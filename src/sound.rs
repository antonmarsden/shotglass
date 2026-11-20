//! The soundtrack: a movie-level music track, one sound per shot, and an optional
//! mix pass — all GLSL, synthesised on the GPU in chunks as the playhead needs them.
//!
//! ```text
//! music.glsl  (movie time) ───────────────────────────► music bus ─┐
//! shot sounds (shot time, shot uniforms) × fades ─────► sfx bus  ──┼─► mix.glsl ─► out
//!                                                                  └─► (or music + sfx)
//! ```
//!
//! Everything is computed per CHUNK of samples and cached by content, so memory stays
//! bounded whatever the film's length, and editing one shot's sound only
//! re-synthesises the chunks it touches.

use crate::anim::GLSL_EASE;
use crate::gfx::{AUDIO_BLOCK, AudioTicket, AudioTiming, Renderer, ShaderError, TRACK_WIDTH};
use crate::movie::Movie;
use crate::shader::{Assembled, Builder, Dialect};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use web_time::Instant;

/// Samples per chunk (≈0.68 s at 48 kHz).
pub const CHUNK: usize = 32768;

/// Interleaved stereo samples of one chunk, shared with the audio callback.
pub type Chunk = Arc<Vec<f32>>;

/// Most sound blocks queued on the GPU at once while streaming.
const MAX_IN_FLIGHT: usize = 6;

/// Streaming doesn't poll fences until the oldest queued block is this old, then
/// collects everything finished at once: a poll is a driver round trip, which on a
/// threaded driver can wait for the driver thread (up to a whole refresh), so it's
/// done in occasional batches rather than every frame.
const COLLECT_AFTER: std::time::Duration = std::time::Duration::from_millis(30);

/// How synthesis may treat the GPU.
enum Mode {
    /// Wait for each block (headless, or the chunk under a paused playhead).
    Blocking,
    /// Never wait; queue at most this many more blocks, and none after the deadline.
    Async {
        max_submits: usize,
        deadline: Instant,
    },
}

/// Where the time of the last `Soundtrack::stream` call went (for the player's
/// stall diagnostics).
#[derive(Clone, Copy, Debug, Default)]
pub struct StreamStats {
    /// Polling fences and reading finished blocks back.
    pub collect_ms: f32,
    /// Queueing blocks (draw, readback into a buffer, fence; mix input uploads).
    pub submit_ms: f32,
    /// Everything else on the CPU: fades and sums, assembling mix inputs.
    pub cpu_ms: f32,
    pub collected: usize,
    pub submitted: usize,
    /// The GL calls among the above, one by one.
    pub gl: AudioTiming,
}

/// Where a finished block goes.
enum Place {
    /// Samples a.. of the track chunk starting at global sample lo, n samples long.
    Track { lo: usize, n: usize, a: usize },
    /// The final output: scale by the movie volume.
    Mix { volume: f32 },
}

/// A block queued on the GPU, waiting to be collected.
struct InFlight {
    ticket: AudioTicket,
    since: Instant,
    name: String,
    place: Place,
}

impl InFlight {
    fn new(ticket: AudioTicket, name: String, place: Place) -> InFlight {
        InFlight {
            ticket,
            since: Instant::now(),
            name,
            place,
        }
    }
}

/// The movie volume, then clamped to the output range.
fn finish_out(mut out: Vec<f32>, volume: f32) -> Vec<f32> {
    for x in &mut out {
        *x = (*x * volume).clamp(-1.0, 1.0);
    }
    out
}

const COMMON: &str = "\
uniform float iSampleRate;
uniform int iBlockOffset;
uniform int iBlockWidth;
out vec4 shotglass_out;
";

const SAMPLE_INDEX: &str =
    "int s = iBlockOffset + int(gl_FragCoord.y) * iBlockWidth + int(gl_FragCoord.x);";

/// The movie-level music shader (`mainSound`, movie time).
pub fn music_source(d: Dialect, m: &Movie, cues: &str, path: &Path) -> anyhow::Result<Assembled> {
    Ok(Builder::new(d, &format!("{COMMON}{GLSL_EASE}{cues}"), &m.files, &m.root)
        .file(path)?
        .footer(&format!(
            "void main() {{\n  {SAMPLE_INDEX}\n  shotglass_out = vec4(mainSound(s, float(s) / iSampleRate), 0.0, 1.0);\n}}"
        ))
        .build())
}

/// The sound shader of shot `i` (`mainSound`, shot time, the shot's uniforms).
/// Panics if the shot has no sound.
pub fn shot_sound_source(d: Dialect, m: &Movie, cues: &str, i: usize) -> anyhow::Result<Assembled> {
    let shot = &m.shots[i];
    let snd = shot.sound.as_ref().expect("shot has a sound");
    let mut header = format!(
        "{COMMON}{GLSL_EASE}uniform float iShotStart;\nuniform float iShotDuration;\n{cues}"
    );
    let mut assign = String::new();
    for (name, a) in &shot.uniforms {
        header += &format!("{} {name};\n", a.glsl_type());
        header += &a.glsl_fn(&format!("sg_u_{name}"));
        assign += &format!("  {name} = sg_u_{name}(t);\n");
    }
    Ok(Builder::new(d, &header, &m.files, &m.root)
        .file(&snd.path)?
        .footer(&format!(
            "void main() {{\n  {SAMPLE_INDEX}\n  float t = float(s) / iSampleRate - iShotStart;\n{assign}  shotglass_out = vec4(mainSound(s, t), 0.0, 1.0);\n}}"
        ))
        .build())
}

/// The final mix shader (`mainMix`, reading `music()` and `sfx()`).
pub fn mix_source(d: Dialect, m: &Movie, cues: &str, path: &Path) -> anyhow::Result<Assembled> {
    Ok(
        Builder::new(d, &format!("{COMMON}{GLSL_EASE}{cues}{}", mix_helpers()), &m.files, &m.root)
            .file(path)?
            .footer(&format!(
                "void main() {{\n  {SAMPLE_INDEX}\n  shotglass_out = vec4(mainMix(s, float(s) / iSampleRate), 0.0, 1.0);\n}}"
            ))
            .build(),
    )
}

fn mix_helpers() -> String {
    format!(
        "\
uniform sampler2D sg_music;
uniform sampler2D sg_sfx;
uniform int sg_base;
uniform int sg_len;
vec2 sg_fetch(sampler2D t, int i) {{
  i -= sg_base;
  if (i < 0 || i >= sg_len) return vec2(0.0);
  return texelFetch(t, ivec2(i % {w}, i / {w}), 0).rg;
}}
// The music track and the sum of all shot sounds (already faded for transitions).
// Readable up to [sound] mix_lookback seconds back and 50 ms ahead.
vec2 music(int samp) {{ return sg_fetch(sg_music, samp); }}
vec2 sfx(int samp) {{ return sg_fetch(sg_sfx, samp); }}
// Peak level of the sfx bus around samp (30 ms back, 10 ms ahead): for ducking.
float sfxLevel(int samp) {{
  int ms = int(iSampleRate * 0.001);
  float m = 0.0;
  for (int k = -30; k <= 10; k += 2) {{
    vec2 v = sfx(samp + k * ms);
    m = max(m, max(abs(v.x), abs(v.y)));
  }}
  return m;
}}
",
        w = TRACK_WIDTH
    )
}

/// One synthesised track: a program run over a window of global samples.
#[derive(Clone)]
struct TrackDef {
    name: String,
    prog: u64,
    first: usize,
    count: usize,
    shot: (f32, f32),
    key: u64,
}

struct Plan {
    movie: Movie,
    sr: u32,
    total: usize,
    music: Option<TrackDef>,
    shots: Vec<(usize, TrackDef)>,
    mix: Option<TrackDef>,
    lookback: usize,
    lookahead: usize,
    sfx_key: u64,
    out_key: u64,
}

pub struct Soundtrack {
    plan: Option<Plan>,
    /// Samples per chunk (CHUNK by default; tests vary it).
    chunk_size: usize,
    /// (track or bus key, chunk index) → interleaved stereo for that chunk.
    chunks: HashMap<(u64, usize), Arc<Vec<f32>>>,
    /// Names of tracks synthesised since the last call to `take_rendered`.
    rendered: Vec<String>,
    /// Blocks queued on the GPU by `stream`, keyed like `chunks`.
    in_flight: HashMap<(u64, usize), InFlight>,
    stats: StreamStats,
}

impl Default for Soundtrack {
    fn default() -> Self {
        Self::new()
    }
}

impl Soundtrack {
    pub fn new() -> Soundtrack {
        Self::with_chunk(CHUNK)
    }

    /// A soundtrack synthesised in chunks of `chunk` samples (≤ AUDIO_BLOCK).
    pub fn with_chunk(chunk: usize) -> Soundtrack {
        assert!(chunk > 0 && chunk <= AUDIO_BLOCK);
        Soundtrack {
            plan: None,
            chunk_size: chunk,
            chunks: HashMap::new(),
            rendered: vec![],
            in_flight: HashMap::new(),
            stats: StreamStats::default(),
        }
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Identifies the audible result; changes whenever anything that affects it does.
    pub fn version(&self) -> Option<u64> {
        self.plan.as_ref().map(|p| p.out_key)
    }

    pub fn total_chunks(&self) -> usize {
        self.plan
            .as_ref()
            .map_or(0, |p| p.total.div_ceil(self.chunk_size))
    }

    /// Tracks synthesised since last asked (for status messages).
    pub fn take_rendered(&mut self) -> Vec<String> {
        let mut v = std::mem::take(&mut self.rendered);
        let mut seen = HashSet::new();
        v.retain(|n| seen.insert(n.clone()));
        v
    }

    /// Prepare a movie version: assemble and compile its sound shaders. On error the
    /// previous version stays. Synthesis happens later, chunk by chunk.
    pub fn set_movie(
        &mut self,
        r: &mut Renderer,
        m: &Movie,
        sr: u32,
    ) -> Result<(), Vec<ShaderError>> {
        let dialect = r.dialect_for(m).map_err(|e| {
            vec![ShaderError {
                what: "movie.toml".into(),
                messages: vec![e],
            }]
        })?;
        let total = (m.duration * sr as f32).ceil() as usize;
        let cues = m.cue_defines();
        let mut errors = vec![];
        let mut progs = vec![];
        let mut compile =
            |r: &mut Renderer, what: String, src: anyhow::Result<Assembled>| -> Option<u64> {
                let src = match src {
                    Ok(s) => s,
                    Err(e) => {
                        errors.push(ShaderError {
                            what,
                            messages: vec![format!("{e:#}")],
                        });
                        return None;
                    }
                };
                match r.audio_program(&src, &what) {
                    Ok(k) => {
                        progs.push(k);
                        Some(k)
                    }
                    Err(e) => {
                        errors.push(e);
                        None
                    }
                }
            };
        let track =
            |name: &str, prog: u64, first: usize, count: usize, shot: (f32, f32)| TrackDef {
                name: name.to_string(),
                prog,
                first,
                count,
                shot,
                key: hash(&format!("{prog} {first} {count} {sr} {shot:?}")),
            };

        let music = m.music.as_ref().and_then(|path| {
            let what = format!("music ({})", rel(path, &m.root));
            let src = music_source(dialect, m, &cues, path);
            compile(r, what, src).map(|k| track("music", k, 0, total, (0.0, m.duration)))
        });

        let mut shots = vec![];
        for (i, shot) in m.shots.iter().enumerate() {
            let (Some(snd), Some((from, to))) = (&shot.sound, m.sound_window(i)) else {
                continue;
            };
            let first = (from * sr as f32).floor() as usize;
            let last = ((to * sr as f32).ceil() as usize).min(total);
            let what = format!("sound for \"{}\" ({})", shot.id, rel(&snd.path, &m.root));
            let src = shot_sound_source(dialect, m, &cues, i);
            if let Some(k) = compile(r, what, src) {
                let name = format!("sound for \"{}\"", shot.id);
                shots.push((
                    i,
                    track(
                        &name,
                        k,
                        first,
                        last.saturating_sub(first),
                        (shot.start, shot.duration),
                    ),
                ));
            }
        }

        let mix = m.mix.as_ref().and_then(|path| {
            let what = format!("mix ({})", rel(path, &m.root));
            let src = mix_source(dialect, m, &cues, path);
            compile(r, what, src).map(|k| track("mix", k, 0, total, (0.0, m.duration)))
        });

        if !errors.is_empty() {
            return Err(errors);
        }
        // The sfx bus depends on each shot track and on the fades (transition timing).
        let mut sfx_parts = vec![hash(&cues)];
        for (i, t) in &shots {
            let snd = m.shots[*i].sound.as_ref().unwrap();
            sfx_parts.push(t.key);
            sfx_parts.push(hash(&format!(
                "{} {} {} {}",
                snd.gain, snd.lead, snd.tail, i
            )));
        }
        for s in &m.shots {
            if let Some(t) = &s.transition_in {
                sfx_parts.push(hash(&format!("{} {:?}", t.duration, t.ease)));
            }
        }
        let sfx_key = hash(&format!("sfx {sfx_parts:?}"));
        let lookback = (m.mix_lookback.max(0.0) * sr as f32).ceil() as usize;
        let lookahead = (0.05 * sr as f32).ceil() as usize;
        let out_key = hash(&format!(
            "out {:?} {sfx_key} {:?} {} {lookback} {total}",
            music.as_ref().map(|t| t.key),
            mix.as_ref().map(|t| t.key),
            m.volume
        ));
        progs.sort();
        progs.dedup();
        r.retain_audio(&progs);
        self.plan = Some(Plan {
            movie: m.clone(),
            sr,
            total,
            music,
            shots,
            mix,
            lookback,
            lookahead,
            sfx_key,
            out_key,
        });
        self.forget_stale();
        // Blocks queued for the old version that this one doesn't share.
        let live = self.live_keys();
        let stale: Vec<_> = self
            .in_flight
            .keys()
            .filter(|(k, _)| !live.contains(k))
            .copied()
            .collect();
        for key in stale {
            r.cancel_audio(self.in_flight.remove(&key).unwrap().ticket);
        }
        Ok(())
    }

    /// The final output for chunk `c` (interleaved stereo, ≤ chunk_size frames), made
    /// now: waits for the GPU (headless use, or the chunk under a paused playhead).
    pub fn chunk(&mut self, r: &mut Renderer, c: usize) -> Result<Arc<Vec<f32>>, String> {
        Ok(self
            .out_chunk(r, c, &mut Mode::Blocking)?
            .expect("blocking synthesis always finishes"))
    }

    /// Advance synthesis of the output chunks `wanted` (most urgent first) without ever
    /// waiting for the GPU: collect blocks the GPU has finished, then queue at most
    /// `max_submits` new ones (and never more than MAX_IN_FLIGHT at once). Stops early
    /// at `deadline` (CPU work: fades, sums). Returns the chunks that are now complete.
    /// Called once per frame while playing; the output is identical to `chunk`'s.
    pub fn stream(
        &mut self,
        r: &mut Renderer,
        wanted: &[usize],
        max_submits: usize,
        deadline: Instant,
    ) -> Result<Vec<(usize, Chunk)>, String> {
        let start = Instant::now();
        self.stats = StreamStats::default();
        r.audio_timing = AudioTiming::default();
        self.collect(r, deadline)?;
        self.stats.collect_ms = start.elapsed().as_secs_f32() * 1000.0;
        let mut mode = Mode::Async {
            max_submits,
            deadline,
        };
        let mut done = vec![];
        for &c in wanted {
            if let Some(d) = self.out_chunk(r, c, &mut mode)? {
                done.push((c, d));
            }
            if Instant::now() > deadline {
                break;
            }
        }
        let total = start.elapsed().as_secs_f32() * 1000.0;
        self.stats.cpu_ms = (total - self.stats.collect_ms - self.stats.submit_ms).max(0.0);
        self.stats.gl = r.audio_timing;
        Ok(done)
    }

    /// Where the time of the last `stream` call went.
    pub fn stats(&self) -> StreamStats {
        self.stats
    }

    /// Sound blocks queued on the GPU and not yet collected.
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Drop every queued block (before replacing this soundtrack).
    pub fn cancel_all(&mut self, r: &mut Renderer) {
        for (_, j) in self.in_flight.drain() {
            r.cancel_audio(j.ticket);
        }
    }

    /// Store the blocks the GPU has finished. Never waits. Fences pass in submission
    /// order, so this polls oldest first and stops at the first that hasn't passed:
    /// usually one poll per frame (each is a driver round trip).
    fn collect(&mut self, r: &mut Renderer, deadline: Instant) -> Result<(), String> {
        let mut order: Vec<(AudioTicket, (u64, usize))> =
            self.in_flight.iter().map(|(k, f)| (f.ticket, *k)).collect();
        order.sort();
        match order.first() {
            Some((_, k)) if self.in_flight[k].since.elapsed() >= COLLECT_AFTER => {}
            _ => return Ok(()),
        }
        for (ticket, key) in order {
            let Some(res) = r.poll_audio(ticket) else {
                break;
            };
            let job = self.in_flight.remove(&key).unwrap();
            self.store(key, job, res?);
            self.stats.collected += 1;
            if Instant::now() > deadline {
                break;
            }
        }
        Ok(())
    }

    /// Turn a finished block into its cached chunk.
    fn store(&mut self, key: (u64, usize), job: InFlight, d: Vec<f32>) -> Arc<Vec<f32>> {
        self.rendered.push(job.name);
        let out = match job.place {
            Place::Track { lo, n, a } => {
                let mut out = vec![0f32; n * 2];
                out[(a - lo) * 2..(a - lo) * 2 + d.len()].copy_from_slice(&d);
                out
            }
            Place::Mix { volume } => finish_out(d, volume),
        };
        let d = Arc::new(out);
        self.chunks.insert(key, d.clone());
        d
    }

    /// The result of the block for cache entry `key`: cached, collected now, or (if
    /// the mode allows) queued with `job`. None = not ready yet.
    fn block(
        &mut self,
        r: &mut Renderer,
        key: (u64, usize),
        mode: &mut Mode,
        job: impl FnOnce(&mut Renderer) -> Result<InFlight, String>,
    ) -> Result<Option<Arc<Vec<f32>>>, String> {
        if let Some(d) = self.chunks.get(&key) {
            return Ok(Some(d.clone()));
        }
        if let Some(f) = self.in_flight.get(&key) {
            // Streaming: `collect` picks it up once its fence has passed.
            let Mode::Blocking = mode else {
                return Ok(None);
            };
            let res = r.wait_audio(f.ticket);
            let f = self.in_flight.remove(&key).unwrap();
            return Ok(Some(self.store(key, f, res?)));
        }
        match mode {
            Mode::Blocking => {
                let f = job(r)?;
                let d = r.wait_audio(f.ticket)?;
                Ok(Some(self.store(key, f, d)))
            }
            Mode::Async { .. } => {
                if self.can_submit(mode) {
                    if let Mode::Async { max_submits, .. } = mode {
                        *max_submits -= 1;
                    }
                    let t = Instant::now();
                    let f = job(r)?;
                    self.stats.submit_ms += t.elapsed().as_secs_f32() * 1000.0;
                    self.stats.submitted += 1;
                    self.in_flight.insert(key, f);
                }
                Ok(None)
            }
        }
    }

    /// May another block be queued now?
    fn can_submit(&self, mode: &Mode) -> bool {
        match mode {
            Mode::Blocking => true,
            Mode::Async {
                max_submits,
                deadline,
            } => {
                *max_submits > 0
                    && self.in_flight.len() < MAX_IN_FLIGHT
                    && Instant::now() < *deadline
            }
        }
    }

    fn out_chunk(
        &mut self,
        r: &mut Renderer,
        c: usize,
        mode: &mut Mode,
    ) -> Result<Option<Arc<Vec<f32>>>, String> {
        let p = self.plan.as_ref().ok_or("no soundtrack")?;
        let key = (p.out_key, c);
        if let Some(d) = self.chunks.get(&key) {
            return Ok(Some(d.clone()));
        }
        let lo = c * self.chunk_size;
        let hi = ((c + 1) * self.chunk_size).min(p.total);
        if lo >= hi {
            return Ok(Some(Arc::new(vec![])));
        }
        let (mix, lookback, lookahead, total, volume, sr, dur) = (
            p.mix.clone(),
            p.lookback,
            p.lookahead,
            p.total,
            p.movie.volume,
            p.sr,
            p.movie.duration,
        );
        match mix {
            Some(mix) => {
                // A mix block can only be queued once its inputs have arrived; until
                // then this just keeps them coming.
                if !self.in_flight.contains_key(&key) {
                    let in_lo = lo.saturating_sub(lookback);
                    let in_hi = (hi + lookahead).min(total);
                    let music = self.music_range(r, in_lo, in_hi, mode)?;
                    let sfx = self.sfx_range(r, in_lo, in_hi, mode)?;
                    let (Some(music), Some(sfx)) = (music, sfx) else {
                        return Ok(None);
                    };
                    // Copying the inputs out costs; only do it to queue the block.
                    if !self.can_submit(mode) {
                        return Ok(None);
                    }
                    let music = self.splice(&music, in_lo, in_hi);
                    let sfx = self.splice(&sfx, in_lo, in_hi);
                    return self.block(r, key, mode, |r| {
                        let t = r.submit_audio(
                            mix.prog,
                            lo,
                            hi - lo,
                            sr,
                            (0.0, dur),
                            Some((&music, &sfx, in_lo as i64)),
                        )?;
                        Ok(InFlight::new(t, mix.name, Place::Mix { volume }))
                    });
                }
                self.block(r, key, mode, |_| unreachable!("already queued"))
            }
            None => {
                let music = self.music_range(r, lo, hi, mode)?;
                let sfx = self.sfx_range(r, lo, hi, mode)?;
                let (Some(music), Some(sfx)) = (music, sfx) else {
                    return Ok(None);
                };
                let mut o = self.splice(&music, lo, hi);
                let sfx = self.splice(&sfx, lo, hi);
                for (a, b) in o.iter_mut().zip(&sfx) {
                    *a += b;
                }
                let d = Arc::new(finish_out(o, volume));
                self.chunks.insert(key, d.clone());
                Ok(Some(d))
            }
        }
    }

    /// The music chunks covering global samples lo..hi (None: not all ready).
    fn music_range(
        &mut self,
        r: &mut Renderer,
        lo: usize,
        hi: usize,
        mode: &mut Mode,
    ) -> Result<Option<Vec<Chunk>>, String> {
        let Some(t) = self.plan.as_ref().and_then(|p| p.music.clone()) else {
            let silence = Arc::new(vec![0.0; self.chunk_size * 2]);
            return Ok(Some(
                (lo / self.chunk_size..hi.div_ceil(self.chunk_size))
                    .map(|_| silence.clone())
                    .collect(),
            ));
        };
        self.range(lo, hi, |s, c| s.track_chunk(r, &t, c, mode))
    }

    /// The sfx bus (all shot sounds × their fades) chunks covering lo..hi.
    fn sfx_range(
        &mut self,
        r: &mut Renderer,
        lo: usize,
        hi: usize,
        mode: &mut Mode,
    ) -> Result<Option<Vec<Chunk>>, String> {
        self.range(lo, hi, |s, c| s.sfx_chunk(r, c, mode))
    }

    /// The chunks covering lo..hi. Asks for every one (so the missing ones are all
    /// queued) before saying None.
    fn range(
        &mut self,
        lo: usize,
        hi: usize,
        mut chunk: impl FnMut(&mut Self, usize) -> Result<Option<Chunk>, String>,
    ) -> Result<Option<Vec<Chunk>>, String> {
        let mut parts = vec![];
        let mut c = lo / self.chunk_size;
        while c * self.chunk_size < hi {
            parts.push(chunk(self, c)?);
            c += 1;
        }
        Ok(parts.into_iter().collect())
    }

    /// Samples lo..hi out of `parts`, the chunks covering them (from `range`).
    fn splice(&self, parts: &[Chunk], lo: usize, hi: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity((hi - lo) * 2);
        let first = lo / self.chunk_size;
        for (i, d) in parts.iter().enumerate() {
            let c = first + i;
            let a = lo.max(c * self.chunk_size) - c * self.chunk_size;
            let b = hi.min((c + 1) * self.chunk_size) - c * self.chunk_size;
            out.extend_from_slice(&d[a * 2..b * 2]);
        }
        out
    }

    /// One chunk of one track; zero outside the track's window.
    fn track_chunk(
        &mut self,
        r: &mut Renderer,
        t: &TrackDef,
        c: usize,
        mode: &mut Mode,
    ) -> Result<Option<Arc<Vec<f32>>>, String> {
        let p = self.plan.as_ref().unwrap();
        let (sr, total) = (p.sr, p.total);
        let lo = c * self.chunk_size;
        let n = ((c + 1) * self.chunk_size).min(total).saturating_sub(lo);
        let a = lo.max(t.first);
        let b = (lo + n).min(t.first + t.count);
        if a >= b {
            if let Some(d) = self.chunks.get(&(t.key, c)) {
                return Ok(Some(d.clone()));
            }
            let d = Arc::new(vec![0f32; n * 2]);
            self.chunks.insert((t.key, c), d.clone());
            return Ok(Some(d));
        }
        self.block(r, (t.key, c), mode, |r| {
            let tk = r.submit_audio(t.prog, a, b - a, sr, t.shot, None)?;
            Ok(InFlight::new(tk, t.name.clone(), Place::Track { lo, n, a }))
        })
    }

    fn sfx_chunk(
        &mut self,
        r: &mut Renderer,
        c: usize,
        mode: &mut Mode,
    ) -> Result<Option<Arc<Vec<f32>>>, String> {
        let p = self.plan.as_ref().unwrap();
        if let Some(d) = self.chunks.get(&(p.sfx_key, c)) {
            return Ok(Some(d.clone()));
        }
        let (sr, total, sfx_key) = (p.sr, p.total, p.sfx_key);
        let lo = c * self.chunk_size;
        let n = ((c + 1) * self.chunk_size).min(total).saturating_sub(lo);
        let shots = p.shots.clone();
        // Ask for every shot's block first, so they're all queued together.
        let mut parts = vec![];
        for (i, t) in shots {
            if lo + n <= t.first || lo >= t.first + t.count {
                continue;
            }
            parts.push((i, self.track_chunk(r, &t, c, mode)?));
        }
        if parts.iter().any(|(_, d)| d.is_none()) {
            return Ok(None);
        }
        let mut out = vec![0f32; n * 2];
        let m = &self.plan.as_ref().unwrap().movie;
        for (i, d) in parts {
            let d = d.unwrap();
            for k in 0..n {
                let g = m.sound_gain(i, (lo + k) as f32 / sr as f32);
                if g != 0.0 {
                    out[k * 2] += d[k * 2] * g;
                    out[k * 2 + 1] += d[k * 2 + 1] * g;
                }
            }
        }
        let d = Arc::new(out);
        self.chunks.insert((sfx_key, c), d.clone());
        Ok(Some(d))
    }

    /// Drop cached chunks outside `keep` (chunk indices), plus any from old versions.
    pub fn evict(&mut self, keep: std::ops::Range<usize>) {
        self.chunks.retain(|(_, c), _| keep.contains(c));
        self.forget_stale();
    }

    fn forget_stale(&mut self) {
        let live = self.live_keys();
        self.chunks.retain(|(k, _), _| live.contains(k));
    }

    /// Cache keys of the current version.
    fn live_keys(&self) -> HashSet<u64> {
        let Some(p) = &self.plan else {
            return HashSet::new();
        };
        let mut live: HashSet<u64> = [p.out_key, p.sfx_key].into();
        live.extend(p.music.iter().chain(p.mix.iter()).map(|t| t.key));
        live.extend(p.shots.iter().map(|(_, t)| t.key));
        live
    }

    /// Synthesise the whole soundtrack chunk by chunk, handing each to `sink`.
    /// Memory stays bounded: chunks behind the mix's lookback are dropped as it goes.
    pub fn render_all(
        &mut self,
        r: &mut Renderer,
        mut sink: impl FnMut(&[f32]),
    ) -> Result<(), String> {
        let n = self.total_chunks();
        let back = self
            .plan
            .as_ref()
            .map_or(0, |p| p.lookback.div_ceil(self.chunk_size) + 1);
        for c in 0..n {
            let d = self.chunk(r, c)?;
            sink(&d);
            self.evict(c.saturating_sub(back)..n);
        }
        Ok(())
    }
}

fn rel(p: &Path, root: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

fn hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}
