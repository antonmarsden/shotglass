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
use crate::gfx::{AUDIO_BLOCK, Renderer, ShaderError, TRACK_WIDTH};
use crate::movie::Movie;
use crate::shader::{Assembled, Builder, Dialect};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

/// Samples per chunk (≈0.68 s at 48 kHz).
pub const CHUNK: usize = 32768;

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
    Ok(Builder::new(d, &format!("{COMMON}{GLSL_EASE}{cues}"), &m.root)
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
    Ok(Builder::new(d, &header, &m.root)
        .file(&snd.path)?
        .footer(&format!(
            "void main() {{\n  {SAMPLE_INDEX}\n  float t = float(s) / iSampleRate - iShotStart;\n{assign}  shotglass_out = vec4(mainSound(s, t), 0.0, 1.0);\n}}"
        ))
        .build())
}

/// The final mix shader (`mainMix`, reading `music()` and `sfx()`).
pub fn mix_source(d: Dialect, m: &Movie, cues: &str, path: &Path) -> anyhow::Result<Assembled> {
    Ok(
        Builder::new(d, &format!("{COMMON}{GLSL_EASE}{cues}{}", mix_helpers()), &m.root)
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
        Ok(())
    }

    /// The final output for chunk `c` (interleaved stereo, ≤ chunk_size frames).
    pub fn chunk(&mut self, r: &mut Renderer, c: usize) -> Result<Arc<Vec<f32>>, String> {
        let p = self.plan.as_ref().ok_or("no soundtrack")?;
        let key = (p.out_key, c);
        if let Some(d) = self.chunks.get(&key) {
            return Ok(d.clone());
        }
        let lo = c * self.chunk_size;
        let hi = ((c + 1) * self.chunk_size).min(p.total);
        if lo >= hi {
            return Ok(Arc::new(vec![]));
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
        let mut out = match mix {
            Some(mix) => {
                let in_lo = lo.saturating_sub(lookback);
                let in_hi = (hi + lookahead).min(total);
                let music = self.music_range(r, in_lo, in_hi)?;
                let sfx = self.sfx_range(r, in_lo, in_hi)?;
                self.rendered.push(mix.name.clone());
                r.render_audio(
                    mix.prog,
                    lo,
                    hi - lo,
                    sr,
                    (0.0, dur),
                    Some((&music, &sfx, in_lo as i64)),
                )?
            }
            None => {
                let mut o = self.music_range(r, lo, hi)?;
                let sfx = self.sfx_range(r, lo, hi)?;
                for (a, b) in o.iter_mut().zip(&sfx) {
                    *a += b;
                }
                o
            }
        };
        for x in &mut out {
            *x = (*x * volume).clamp(-1.0, 1.0);
        }
        let d = Arc::new(out);
        self.chunks.insert(key, d.clone());
        Ok(d)
    }

    /// Music for global samples lo..hi.
    fn music_range(&mut self, r: &mut Renderer, lo: usize, hi: usize) -> Result<Vec<f32>, String> {
        let Some(t) = self.plan.as_ref().and_then(|p| p.music.clone()) else {
            return Ok(vec![0.0; (hi - lo) * 2]);
        };
        self.range(lo, hi, |s, c| s.track_chunk(r, &t, c))
    }

    /// The sfx bus (all shot sounds × their fades) for global samples lo..hi.
    fn sfx_range(&mut self, r: &mut Renderer, lo: usize, hi: usize) -> Result<Vec<f32>, String> {
        self.range(lo, hi, |s, c| s.sfx_chunk(r, c))
    }

    fn range(
        &mut self,
        lo: usize,
        hi: usize,
        mut chunk: impl FnMut(&mut Self, usize) -> Result<Arc<Vec<f32>>, String>,
    ) -> Result<Vec<f32>, String> {
        let mut out = Vec::with_capacity((hi - lo) * 2);
        let mut c = lo / self.chunk_size;
        while c * self.chunk_size < hi {
            let d = chunk(self, c)?;
            let a = lo.max(c * self.chunk_size) - c * self.chunk_size;
            let b = hi.min((c + 1) * self.chunk_size) - c * self.chunk_size;
            out.extend_from_slice(&d[a * 2..b * 2]);
            c += 1;
        }
        Ok(out)
    }

    /// One chunk of one track; zero outside the track's window.
    fn track_chunk(
        &mut self,
        r: &mut Renderer,
        t: &TrackDef,
        c: usize,
    ) -> Result<Arc<Vec<f32>>, String> {
        if let Some(d) = self.chunks.get(&(t.key, c)) {
            return Ok(d.clone());
        }
        let p = self.plan.as_ref().unwrap();
        let (sr, total) = (p.sr, p.total);
        let lo = c * self.chunk_size;
        let n = ((c + 1) * self.chunk_size).min(total).saturating_sub(lo);
        let mut out = vec![0f32; n * 2];
        let a = lo.max(t.first);
        let b = (lo + n).min(t.first + t.count);
        if a < b {
            let d = r.render_audio(t.prog, a, b - a, sr, t.shot, None)?;
            out[(a - lo) * 2..(b - lo) * 2].copy_from_slice(&d);
            self.rendered.push(t.name.clone());
        }
        let d = Arc::new(out);
        self.chunks.insert((t.key, c), d.clone());
        Ok(d)
    }

    fn sfx_chunk(&mut self, r: &mut Renderer, c: usize) -> Result<Arc<Vec<f32>>, String> {
        let p = self.plan.as_ref().unwrap();
        if let Some(d) = self.chunks.get(&(p.sfx_key, c)) {
            return Ok(d.clone());
        }
        let (sr, total, sfx_key) = (p.sr, p.total, p.sfx_key);
        let lo = c * self.chunk_size;
        let n = ((c + 1) * self.chunk_size).min(total).saturating_sub(lo);
        let shots = p.shots.clone();
        let mut out = vec![0f32; n * 2];
        for (i, t) in shots {
            if lo + n <= t.first || lo >= t.first + t.count {
                continue;
            }
            let d = self.track_chunk(r, &t, c)?;
            let m = &self.plan.as_ref().unwrap().movie;
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
        Ok(d)
    }

    /// Drop cached chunks outside `keep` (chunk indices), plus any from old versions.
    pub fn evict(&mut self, keep: std::ops::Range<usize>) {
        self.chunks.retain(|(_, c), _| keep.contains(c));
        self.forget_stale();
    }

    fn forget_stale(&mut self) {
        let Some(p) = &self.plan else { return };
        let mut live: HashSet<u64> = [p.out_key, p.sfx_key].into();
        live.extend(p.music.iter().chain(p.mix.iter()).map(|t| t.key));
        live.extend(p.shots.iter().map(|(_, t)| t.key));
        self.chunks.retain(|(k, _), _| live.contains(k));
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
