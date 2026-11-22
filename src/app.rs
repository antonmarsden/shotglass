//! The player window: viewer, timeline, transport controls, live reload, performance.

use crate::audio::{AudioOut, Clock};
use crate::gfx::{FrameInfo, Renderer, ShaderError};
use crate::movie::{At, Movie};
use crate::sound::Soundtrack;
use eframe::egui::{self, Color32, Key, Modifiers, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use eframe::glow;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

/// Seconds of sound synthesised ahead of the playhead while playing.
const SOUND_AHEAD: f32 = 6.0;

pub fn run(path: PathBuf, start: f32, scale: Option<f32>) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1360.0, 860.0])
            .with_title("shotglass"),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "shotglass",
        options,
        Box::new(move |cc| {
            let app = Player::new(cc, path, start, scale).map_err(|e| format!("{e:#}"))?;
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

#[derive(Clone, Copy, PartialEq)]
enum ScaleMode {
    Auto,
    Fixed(f32),
}

/// Slow housekeeping in one frame, and where its time went.
#[derive(Clone, Copy)]
struct Stall {
    sound_ms: f32,
    compile_ms: f32,
    sound: crate::sound::StreamStats,
}

impl Stall {
    fn worse(a: Option<Stall>, b: Option<Stall>) -> Option<Stall> {
        match (a, b) {
            (Some(a), Some(b)) => Some(if b.sound_ms + b.compile_ms > a.sound_ms + a.compile_ms {
                b
            } else {
                a
            }),
            (a, b) => a.or(b),
        }
    }
}

/// What a frame did besides drawing the UI, for the frame graph. Sound work runs
/// after a frame's render, so it lands in the next frame's interval, and is
/// recorded there.
#[derive(Clone, Copy, Default)]
struct Marks {
    rendered: bool,
    gpu_ms: Option<f32>,
    /// Housekeeping in `pump`: compiles and texture uploads.
    compile_ms: f32,
    /// ... of which texture work (collecting decodes, uploading bands).
    texture_ms: f32,
    sound_ms: f32,
    sound: crate::sound::StreamStats,
}

struct Perf {
    last: Option<Instant>,
    frame_ms: VecDeque<f32>,
    /// What happened in each frame of `frame_ms` (same length, same order).
    marks: VecDeque<Marks>,
    /// Work since the last measured frame, pushed with its frame time.
    pending: Marks,
    gpu_ms: VecDeque<f32>,
    dropped: u32,
    /// Dropped frames caused by compiling or sound synthesis, not by rendering.
    stalled: u32,
    /// ... of which sound took most of the housekeeping time.
    stalled_sound: u32,
    /// The slowest sound housekeeping among those stalls.
    worst_sound: Option<Stall>,
    /// This frame did slow housekeeping work.
    stall: Option<Stall>,
    last_render: Option<Instant>,
    stall_since_render: Option<Stall>,
    /// GPU times since the last scale change (for the scaler).
    gpu_recent: VecDeque<f32>,
    misses: u32,
    cooldown: u32,
    calm: u32,
}

impl Perf {
    fn push(q: &mut VecDeque<f32>, v: f32) {
        if q.len() >= 240 {
            q.pop_front();
        }
        q.push_back(v);
    }
    fn since_render_ms(&self) -> f32 {
        self.last_render
            .map_or(0.0, |t| t.elapsed().as_secs_f32() * 1000.0)
    }

    /// Estimated display refresh interval: a low percentile of recent frame times.
    fn vsync_ms(&self) -> f32 {
        if self.frame_ms.len() < 20 {
            return 1000.0 / 60.0;
        }
        let mut v: Vec<f32> = self.frame_ms.iter().copied().collect();
        v.sort_by(f32::total_cmp);
        v[v.len() / 10].clamp(4.0, 34.0)
    }
}

struct Player {
    path: PathBuf,
    movie: Option<Movie>,
    load_error: Option<String>,
    /// Errors assembling sources (missing files): the movie isn't switched.
    shader_errors: Vec<ShaderError>,
    sound_errors: Vec<ShaderError>,
    renderer: Renderer,
    clock: Clock,
    last_synth: String,
    /// The driver reset the GPU (usually a shader that ran for seconds).
    gpu_lost: bool,
    /// When the last frame started (a long gap hints at a driver reset).
    last_ui: Option<Instant>,
    /// Seconds of sound ready ahead of the playhead.
    buffered: f32,
    perf_frame: u64,
    soundtrack: Soundtrack,
    sound_info: String,
    watcher: Option<notify::RecommendedWatcher>,
    /// Library folders outside the film folder that the watcher also follows.
    watched_libs: Vec<PathBuf>,
    /// The project root, when it isn't the film folder (watched, not recursively).
    watched_project: Option<PathBuf>,
    rx: Receiver<()>,
    reload_at: Option<Instant>,
    reloads: u32,
    status: Option<(String, Instant)>,
    tex: Option<(glow::Texture, egui::TextureId)>,
    rendered_key: Option<(u32, i32, i32, u32, u64)>,
    generation: u64,
    scale_mode: ScaleMode,
    auto_scale: f32,
    perf: Perf,
    loop_region: Option<(f32, f32)>,
    /// "repeat movie": on by default for `loop = true` films, which then loop
    /// seamlessly (the clock and sound card wrap); other films seek back to 0.
    loop_all: bool,
    /// The film loaded last was a `loop = true` film (to switch `loop_all` on when
    /// a film starts looping).
    loop_film: bool,
    volume: f32,
    muted: bool,
    last_input: Instant,
    show_panel: bool,
    scrub_resume: Option<bool>,
}

impl Player {
    fn new(
        cc: &eframe::CreationContext<'_>,
        path: PathBuf,
        start: f32,
        scale: Option<f32>,
    ) -> anyhow::Result<Player> {
        let gl = cc
            .gl
            .clone()
            .ok_or_else(|| anyhow::anyhow!("eframe didn't give us an OpenGL context"))?;
        let renderer = Renderer::new(gl)?;
        let (audio, sound_info) = match AudioOut::open() {
            Ok(a) => {
                let s = format!("{} | {} Hz", a.device_name, a.sample_rate);
                (Some(a), s)
            }
            Err(e) => (
                None,
                format!("no audio output ({e}); using the system clock"),
            ),
        };
        let mut clock = Clock::new(audio);
        clock.seek(start);

        let (tx, rx) = channel();
        let ctx = cc.egui_ctx.clone();
        let root = if path.is_dir() {
            path.clone()
        } else {
            path.parent().map(PathBuf::from).unwrap_or_default()
        };
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                use notify::EventKind::*;
                let relevant = matches!(ev.kind, Modify(_) | Create(_) | Remove(_))
                    && ev.paths.iter().any(|p| {
                        matches!(
                            p.extension().and_then(|e| e.to_str()),
                            Some(
                                "glsl"
                                    | "toml"
                                    | "lock"
                                    | "frag"
                                    | "inc"
                                    | "h"
                                    | "png"
                                    | "jpg"
                                    | "jpeg"
                                    | "PNG"
                                    | "JPG"
                                    | "JPEG"
                                    | "ttf"
                                    | "otf"
                                    | "ttc"
                                    | "TTF"
                                    | "OTF"
                                    | "TTC"
                                    | "txt"
                            )
                        )
                    });
                if relevant {
                    let _ = tx.send(());
                    ctx.request_repaint();
                }
            }
        })
        .and_then(|mut w| {
            use notify::Watcher;
            w.watch(&root, notify::RecursiveMode::Recursive).map(|_| w)
        })
        .ok();

        let mut p = Player {
            path,
            movie: None,
            load_error: None,
            shader_errors: vec![],
            sound_errors: vec![],
            renderer,
            clock,
            last_synth: String::new(),
            gpu_lost: false,
            last_ui: None,
            buffered: 0.0,
            perf_frame: 0,
            soundtrack: Soundtrack::new(),
            sound_info,
            watcher,
            watched_libs: vec![],
            watched_project: None,
            rx,
            reload_at: None,
            reloads: 0,
            status: None,
            tex: None,
            rendered_key: None,
            generation: 0,
            scale_mode: scale.map_or(ScaleMode::Auto, ScaleMode::Fixed),
            auto_scale: 1.0,
            perf: Perf {
                last: None,
                frame_ms: VecDeque::new(),
                marks: VecDeque::new(),
                pending: Marks::default(),
                gpu_ms: VecDeque::new(),
                dropped: 0,
                stalled: 0,
                stalled_sound: 0,
                worst_sound: None,
                stall: None,
                last_render: None,
                stall_since_render: None,
                gpu_recent: VecDeque::new(),
                misses: 0,
                cooldown: 0,
                calm: 0,
            },
            loop_region: None,
            loop_all: false,
            loop_film: false,
            volume: 0.8,
            muted: false,
            last_input: Instant::now(),
            show_panel: true,
            scrub_resume: None,
        };
        p.clock.set_gain(p.volume);
        p.reload();
        Ok(p)
    }

    // ------------------------------------------------------------ loading

    fn reload(&mut self) {
        let m = match Movie::load(&self.path) {
            Ok(m) => m,
            Err(e) => {
                self.load_error = Some(format!("{e:#}"));
                return;
            }
        };
        self.load_error = None;
        self.watch_libraries(&m);
        for w in &m.warnings {
            eprintln!("warning: {w}");
        }
        match self.renderer.set_movie(&m) {
            Ok(()) => {
                self.shader_errors.clear();
                self.sound_errors.clear();
                // Compile what's on screen right now; the rest follows in the background.
                let t = self.clock.time();
                self.renderer.pump(&m, t, false);
                let snd = self.update_sound(&m);
                self.reloads += 1;
                if self.reloads > 1 {
                    let mut msg = "reloaded".to_string();
                    if let Some(s) = snd {
                        msg += &format!(" | {s}");
                    }
                    self.say(msg);
                }
                let looping = m.loop_frames.is_some();
                if looping != self.loop_film {
                    self.loop_film = looping;
                    self.loop_all = looping;
                }
                self.movie = Some(m);
                self.generation += 1;
                self.sync_loop();
                if let Some((a, b)) = self.loop_region {
                    let d = self.duration();
                    self.loop_region = Some((a.min(d), b.min(d)));
                }
            }
            Err(errors) => {
                self.shader_errors = errors;
                self.say("shader error | showing the last working version".into());
            }
        }
    }

    /// Rebuild the soundtrack (cached per track). Returns a note about what was re-synthesized.
    /// Prepare the new soundtrack version. Chunks are synthesised as the playhead
    /// needs them (see `stream_sound`); until then the old version keeps playing.
    fn update_sound(&mut self, m: &Movie) -> Option<String> {
        if !m.has_sound() {
            self.soundtrack.cancel_all(&mut self.renderer);
            self.soundtrack = Soundtrack::new();
            self.clock.clear_chunks();
            return None;
        }
        match self
            .soundtrack
            .set_movie(&mut self.renderer, m, self.clock.sample_rate())
        {
            Ok(()) => None,
            Err(errors) => {
                self.sound_errors = errors;
                None
            }
        }
    }

    /// Synthesise sound chunks around the playhead, nearest first, within a small
    /// time budget per frame, and forget chunks far from it. While playing this never
    /// waits for the GPU (see `Soundtrack::stream`).
    fn stream_sound(&mut self) {
        let Some(version) = self.soundtrack.version() else {
            return;
        };
        if !self.clock.has_audio() {
            return;
        }
        let sr = self.clock.sample_rate() as f32;
        let n = self.soundtrack.total_chunks();
        if n == 0 {
            return;
        }
        let t = self.clock.time();
        let c0 = ((t * sr) as usize / self.soundtrack.chunk_size()).min(n - 1);
        let playing = self.clock.playing();
        // A seamless loop: the chunks after the last are the first ones again.
        let looping = self.clock.looping();
        let wrap = |c: usize| if looping { c % n } else { c };
        let chunk = self.soundtrack.chunk_size();
        let per_s = sr / chunk as f32;
        let ahead = if playing {
            (SOUND_AHEAD * per_s).ceil() as usize
        } else {
            1
        };
        let total = self
            .movie
            .as_ref()
            .map_or(0, |m| m.sound_samples(sr as u32));
        // Paused or scrubbing, the chunk under the playhead is made at once (this may
        // wait for the GPU, like a blocking compile). While playing, nothing waits:
        // blocks are queued on the GPU and collected on later frames.
        if !playing && self.clock.chunk_version(c0) != Some(version) {
            match self.soundtrack.chunk(&mut self.renderer, c0) {
                Ok(d) => self.clock.put_chunk(chunk, total, c0, version, d),
                Err(e) => {
                    self.sound_errors = vec![ShaderError {
                        what: "sound".into(),
                        messages: vec![e],
                    }];
                    return;
                }
            }
        }
        let mut wanted: Vec<usize> = if looping {
            (c0..c0 + ahead.min(n - 1) + 1).map(wrap).collect()
        } else {
            (c0..(c0 + ahead + 1).min(n)).collect()
        };
        wanted.retain(|&c| self.clock.chunk_version(c) != Some(version));
        // A block or two per frame; more right after a seek, when the playhead's
        // chunk is missing.
        let blocks = if wanted.first() == Some(&c0) { 4 } else { 2 };
        // Well under the 3 ms that counts as a stall.
        let deadline = Instant::now() + Duration::from_micros(1500);
        match self
            .soundtrack
            .stream(&mut self.renderer, &wanted, blocks, deadline)
        {
            Ok(done) => {
                for (c, d) in done {
                    self.clock.put_chunk(chunk, total, c, version, d);
                }
            }
            Err(e) => {
                self.sound_errors = vec![ShaderError {
                    what: "sound".into(),
                    messages: vec![e],
                }];
            }
        }
        let made = self.soundtrack.take_rendered();
        if !made.is_empty() {
            self.last_synth = made.join(", ");
        }
        let ahead_of_playhead = if looping { c0 + n } else { n };
        self.buffered = (c0..ahead_of_playhead)
            .take_while(|c| self.clock.chunk_version(wrap(*c)) == Some(version))
            .count() as f32
            / per_s;
        if self.perf_frame.is_multiple_of(30) {
            let back = (3.0 * per_s).ceil() as usize;
            let fwd = ahead + (4.0 * per_s).ceil() as usize;
            // Chunks from `back` behind the playhead to `fwd` ahead of it (across the
            // seam of a loop).
            let keep = move |c: usize| {
                if !looping {
                    return c + back >= c0 && c < c0 + fwd;
                }
                let behind = (c0 + n - c) % n;
                let ahead = (c + n - c0) % n;
                behind <= back || ahead < fwd
            };
            self.soundtrack.evict_where(keep);
            self.clock.drop_chunks_where(keep);
        }
    }

    /// Live reload follows library folders too (in-film paths are already covered by
    /// watching the film folder; project libraries, overrides and the cache usually live
    /// elsewhere), and the project root's own files (`shotglass.toml`,
    /// `shotglass.local.toml`, `shotglass.lock`) when the project is above the film.
    fn watch_libraries(&mut self, m: &Movie) {
        use notify::Watcher;
        let Some(w) = self.watcher.as_mut() else {
            return;
        };
        let film = std::path::absolute(&m.root).unwrap_or_else(|_| m.root.clone());
        let want: Vec<PathBuf> = m
            .libraries
            .iter()
            .map(|l| std::path::absolute(&l.root).unwrap_or_else(|_| l.root.clone()))
            .filter(|p| !p.starts_with(&film))
            .collect();
        let project = std::path::absolute(&m.project).unwrap_or_else(|_| m.project.clone());
        let project = Some(project).filter(|p| *p != film);
        if self.watched_project != project {
            if let Some(old) = &self.watched_project {
                let _ = w.unwatch(old);
            }
            if let Some(p) = &project {
                // Not recursive: the other films below it are none of this film's business.
                let _ = w.watch(p, notify::RecursiveMode::NonRecursive);
            }
            self.watched_project = project;
        }
        for p in &self.watched_libs {
            if !want.contains(p) {
                let _ = w.unwatch(p);
            }
        }
        for p in &want {
            if !self.watched_libs.contains(p) {
                let _ = w.watch(p, notify::RecursiveMode::Recursive);
            }
        }
        self.watched_libs = want;
    }

    fn say(&mut self, s: String) {
        self.status = Some((s, Instant::now()));
    }

    fn duration(&self) -> f32 {
        self.movie.as_ref().map_or(0.0, |m| m.duration)
    }

    fn fps(&self) -> f32 {
        self.movie.as_ref().map_or(30.0, |m| m.fps)
    }

    // ------------------------------------------------------------ transport

    fn seek(&mut self, t: f32) {
        self.clock.seek(t.clamp(0.0, self.duration()));
        self.last_input = Instant::now();
    }

    fn step(&mut self, frames: f32) {
        self.clock.pause();
        let fps = self.fps();
        let t = ((self.clock.time() * fps).round() + frames) / fps;
        self.seek(t);
    }

    fn shot_starts(&self) -> Vec<f32> {
        self.movie
            .as_ref()
            .map_or(vec![], |m| m.shots.iter().map(|s| s.start).collect())
    }

    fn jump_shot(&mut self, dir: i32) {
        let t = self.clock.time();
        let mut starts = self.shot_starts();
        starts.push(self.duration());
        let target = if dir < 0 {
            starts
                .iter()
                .rev()
                .find(|&&s| s < t - 0.25)
                .copied()
                .unwrap_or(0.0)
        } else {
            starts
                .iter()
                .find(|&&s| s > t + 0.001)
                .copied()
                .unwrap_or(self.duration())
        };
        self.seek(target);
    }

    fn jump_transition(&mut self, dir: i32) {
        let Some(m) = &self.movie else { return };
        let t = self.clock.time();
        let pre: Vec<f32> = m
            .shots
            .iter()
            .filter(|s| s.transition_in.is_some())
            .map(|s| (s.start - 0.5).max(0.0))
            .collect();
        let target = if dir < 0 {
            pre.iter().rev().find(|&&s| s < t - 0.25).copied()
        } else {
            pre.iter().find(|&&s| s > t + 0.001).copied()
        };
        if let Some(t) = target {
            self.seek(t);
        }
    }

    fn toggle_loop(&mut self) {
        if self.loop_region.is_some() {
            self.loop_region = None;
            self.say("loop off".into());
        } else if let Some(m) = &self.movie {
            let r = m.region_at(self.clock.time());
            self.loop_region = Some(r);
            self.say(format!("looping {:.2}s – {:.2}s", r.0, r.1));
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let k = |m: Modifiers, key: Key| ctx.input_mut(|i| i.consume_key(m, key));
        let none = Modifiers::NONE;
        let shift = Modifiers::SHIFT;
        if k(none, Key::Space) {
            self.toggle_play();
        }
        if k(shift, Key::ArrowLeft) {
            let t = self.clock.time() - 1.0;
            self.seek(t);
        }
        if k(shift, Key::ArrowRight) {
            let t = self.clock.time() + 1.0;
            self.seek(t);
        }
        if k(none, Key::ArrowLeft) {
            self.step(-1.0);
        }
        if k(none, Key::ArrowRight) {
            self.step(1.0);
        }
        if k(none, Key::Home) {
            self.seek(0.0);
        }
        if k(none, Key::End) {
            let d = self.duration();
            self.seek(d);
        }
        if k(none, Key::OpenBracket) || k(none, Key::PageUp) {
            self.jump_shot(-1);
        }
        if k(none, Key::CloseBracket) || k(none, Key::PageDown) {
            self.jump_shot(1);
        }
        if k(none, Key::Comma) {
            self.jump_transition(-1);
        }
        if k(none, Key::Period) {
            self.jump_transition(1);
        }
        if k(none, Key::L) {
            self.toggle_loop();
        }
        if k(none, Key::M) {
            self.muted = !self.muted;
            self.clock
                .set_gain(if self.muted { 0.0 } else { self.volume });
        }
        if k(none, Key::H) || k(none, Key::Tab) {
            self.show_panel = !self.show_panel;
        }
        if k(none, Key::R) {
            self.reload();
        }
        if k(none, Key::Q) {
            self.scale_mode = match self.scale_mode {
                ScaleMode::Auto => ScaleMode::Fixed(1.0),
                ScaleMode::Fixed(s) if s > 0.9 => ScaleMode::Fixed(0.5),
                _ => ScaleMode::Auto,
            };
        }
        for (key, rate) in [(Key::Num1, 0.25), (Key::Num2, 0.5), (Key::Num3, 1.0)] {
            if k(none, key) {
                self.clock.set_rate(rate);
                self.say(format!("speed {rate}x"));
            }
        }
    }

    fn toggle_play(&mut self) {
        if !self.clock.playing() && self.clock.time() >= self.duration() - 1e-3 {
            self.clock.seek(self.loop_region.map_or(0.0, |r| r.0));
        }
        self.clock.toggle();
        self.last_input = Instant::now();
    }

    // ------------------------------------------------------------ frame logic

    /// Loop the clock (seamless: the sound card plays across the wrap) when a loop
    /// film plays with "repeat movie" on and no shot loop.
    fn sync_loop(&mut self) {
        let want = match &self.movie {
            Some(m) if m.loop_frames.is_some() && self.loop_all && self.loop_region.is_none() => {
                Some(m.sound_samples(self.clock.sample_rate()))
            }
            _ => None,
        };
        if want != self.clock.loop_samples() {
            self.clock.set_loop(want);
        }
    }

    fn advance_clock(&mut self) {
        self.sync_loop();
        if !self.clock.playing() {
            return;
        }
        if self.clock.looping() {
            return; // wraps by itself
        }
        let t = self.clock.time();
        if let Some((a, b)) = self.loop_region {
            if t >= b || t < a - 0.5 {
                self.clock.seek(a);
            }
        } else if t >= self.duration() {
            if self.loop_all {
                self.clock.seek(0.0);
            } else {
                self.clock.pause();
                self.clock.seek(self.duration());
            }
        }
    }

    fn effective_scale(&self) -> f32 {
        match self.scale_mode {
            ScaleMode::Fixed(s) => s,
            ScaleMode::Auto => {
                // Paused and idle: render at full quality, there is no deadline.
                if !self.clock.playing() && self.last_input.elapsed() > Duration::from_millis(250) {
                    1.0
                } else {
                    self.auto_scale
                }
            }
        }
    }

    fn measure(&mut self, rendered: bool) {
        let now = Instant::now();
        let dt = self.perf.last.map(|l| (now - l).as_secs_f32() * 1000.0);
        self.perf.last = Some(now);
        let gpu = self.renderer.gpu_ms();
        // Any housekeeping stall since the last rendered frame excuses its lateness.
        self.perf.stall_since_render =
            Stall::worse(self.perf.stall_since_render, self.perf.stall.take());
        if !self.clock.playing() {
            self.perf.last = None;
            self.perf.pending = Marks::default();
            self.perf.last_render = None;
            self.perf.stall_since_render = None;
            return;
        }
        let Some(dt) = dt else { return };
        Perf::push(&mut self.perf.frame_ms, dt);
        let mut mk = std::mem::take(&mut self.perf.pending);
        mk.rendered = rendered;
        mk.gpu_ms = gpu;
        if self.perf.marks.len() >= 240 {
            self.perf.marks.pop_front();
        }
        self.perf.marks.push_back(mk);
        if let Some(g) = gpu {
            Perf::push(&mut self.perf.gpu_ms, g);
            Perf::push(&mut self.perf.gpu_recent, g);
            if g > 500.0 {
                self.say(format!("a frame took {g:.0} ms on the GPU; drawing it in strips to keep the driver happy"));
            }
        }
        let stall = self.perf.stall_since_render;
        let vsync = self.perf.vsync_ms();
        // A film frame is late if it took longer than a film frame to arrive.
        let frame_ms = 1000.0 / self.fps();
        let late = rendered && self.perf.since_render_ms() > (frame_ms.max(vsync) * 1.5);
        if rendered {
            self.perf.last_render = Some(now);
            self.perf.stall_since_render = None;
        }
        if late {
            if let Some(st) = stall {
                self.perf.stalled += 1;
                if st.sound_ms >= st.compile_ms {
                    self.perf.stalled_sound += 1;
                    if self
                        .perf
                        .worst_sound
                        .is_none_or(|w| st.sound_ms > w.sound_ms)
                    {
                        self.perf.worst_sound = Some(st);
                    }
                }
            } else {
                self.perf.dropped += 1;
            }
        }
        if self.scale_mode != ScaleMode::Auto {
            return;
        }
        if self.perf.cooldown > 0 {
            self.perf.cooldown -= 1;
            return;
        }
        // Each film frame may take up to one film-frame interval of GPU time
        // (but not less than a display refresh). Aim for 80% of it.
        let budget = frame_ms.max(vsync);
        let scale = self.auto_scale;
        if self.renderer.has_gpu_timer() {
            // Only GPU time decides, averaged over recent frames so one spike
            // (a compile, the OS) can't change the resolution.
            let recent: Vec<f32> = self.perf.gpu_recent.iter().rev().take(8).copied().collect();
            if recent.len() < 8 || gpu.is_none() {
                return;
            }
            let avg = recent.iter().sum::<f32>() / recent.len() as f32;
            if let Some(next) = next_scale(scale, avg, budget) {
                self.auto_scale = next;
                self.perf.gpu_recent.clear(); // measure afresh at the new scale
                self.perf.cooldown = 10;
            }
        } else if rendered {
            // No GPU timer: go by late film frames, ignoring ones we stalled ourselves.
            if late && stall.is_none() {
                self.perf.misses += 1;
                if self.perf.misses >= 3 {
                    self.auto_scale = (scale * 0.85).max(0.3);
                    self.perf.misses = 0;
                    self.perf.calm = 0;
                    self.perf.cooldown = 30;
                }
            } else {
                self.perf.calm += 1;
                if self.perf.calm > 60 && scale < 1.0 {
                    self.auto_scale = (scale * 1.1).min(1.0);
                    self.perf.calm = 0;
                    self.perf.misses = 0;
                }
            }
        }
    }

    /// Has the GPU driver reset (a shader ran too long)? Asking is a driver round trip
    /// that can cost a whole refresh, so while playing it's only asked after a long
    /// gap between frames (or a very slow GPU frame); paused, about once a second.
    /// Runs at the end of the frame, after the render and sound are queued.
    fn check_gpu(&mut self, gap: Option<Duration>) {
        if self.gpu_lost || self.movie.is_none() {
            return;
        }
        let suspicious = gap.is_some_and(|g| g > Duration::from_millis(500));
        if self
            .renderer
            .context_lost(!self.clock.playing(), suspicious)
        {
            self.gpu_lost = true;
            self.clock.pause();
        }
    }

    fn render_view(&mut self, frame: &mut eframe::Frame, w: i32, h: i32) -> bool {
        if self.gpu_lost {
            return false;
        }
        let Some(m) = &self.movie else { return false };
        let fps = m.fps;
        let playing = self.clock.playing();
        // While playing, render each film frame once (fps from movie.toml) and keep it
        // up until the next: a 30 fps film needs half the GPU work of a 60 Hz redraw,
        // and playback shows exactly the frames `still` would.
        let t = if playing {
            (self.clock.time() * fps).floor() / fps
        } else {
            self.clock.time()
        };
        let scale = self.effective_scale();
        let key = (t.to_bits(), w, h, scale.to_bits(), self.generation);
        if self.rendered_key == Some(key) {
            return false;
        }
        let info = FrameInfo {
            t,
            dt: 1.0 / fps,
            frame: (t * fps) as i32,
            scale,
        };
        // Paused or scrubbing: compile on the spot if needed. Playing: never stall,
        // keep the previous frame up until the shot's program is ready.
        let blocking = !self.clock.playing();
        let Some(tex) = self.renderer.render(m, w, h, &info, blocking) else {
            return false;
        };
        let tex = Some(tex);
        self.rendered_key = Some(key);
        if let Some(tex) = tex
            && self.tex.map(|(t, _)| t) != Some(tex)
        {
            let id = frame.register_native_glow_texture(tex);
            self.tex = Some((tex, id));
        }
        true
    }

    // ------------------------------------------------------------ ui pieces

    fn transport(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let b = |ui: &mut egui::Ui, s: &str, tip: &str| ui.add(egui::Button::new(RichText::new(s).size(15.0)).min_size(Vec2::new(30.0, 24.0))).on_hover_text(tip).clicked();
            if b(ui, "|<", "start (Home)") {
                self.seek(0.0);
            }
            if b(ui, "<<", "previous shot ([)") {
                self.jump_shot(-1);
            }
            if b(ui, "<", "previous frame (Left)") {
                self.step(-1.0);
            }
            let play = if self.clock.playing() { "Pause" } else { "Play" };
            if b(ui, play, "play / pause (Space)") {
                self.toggle_play();
            }
            if b(ui, ">", "next frame (Right)") {
                self.step(1.0);
            }
            if b(ui, ">>", "next shot (])") {
                self.jump_shot(1);
            }
            if b(ui, ">|", "end (End)") {
                let d = self.duration();
                self.seek(d);
            }
            ui.separator();
            let looping = self.loop_region.is_some();
            if ui.selectable_label(looping, "Loop").on_hover_text("loop the shot or transition under the playhead (L, or double-click the timeline)").clicked() {
                self.toggle_loop();
            }
            let tip = if self.loop_film {
                "loop = true: play forever, seamlessly (sound included)"
            } else {
                "start again from the beginning at the end"
            };
            ui.checkbox(&mut self.loop_all, "repeat movie").on_hover_text(tip);
            ui.separator();
            let mut rate = self.clock.rate();
            egui::ComboBox::from_id_salt("rate")
                .width(64.0)
                .selected_text(format!("{rate}x"))
                .show_ui(ui, |ui| {
                    for r in [0.25, 0.5, 1.0] {
                        ui.selectable_value(&mut rate, r, format!("{r}x"));
                    }
                });
            if rate != self.clock.rate() {
                self.clock.set_rate(rate);
            }
            ui.separator();
            let t = self.clock.time();
            let fps = self.fps();
            let loops = if self.clock.looping() {
                format!("   loop {}", self.clock.loops() + 1)
            } else {
                String::new()
            };
            ui.label(
                RichText::new(format!("{}  /  {}   frame {}{loops}", clock_text(t), clock_text(self.duration()), (t * fps).round() as i64))
                    .monospace()
                    .size(14.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut vol = self.volume;
                ui.add(egui::Slider::new(&mut vol, 0.0..=1.0).show_value(false)).on_hover_text("volume");
                if vol != self.volume {
                    self.volume = vol;
                    self.muted = false;
                    self.clock.set_gain(vol);
                }
                let icon = if self.muted { "Unmute" } else { "Mute" };
                if ui.button(icon).on_hover_text("mute (M)").clicked() {
                    self.muted = !self.muted;
                    self.clock.set_gain(if self.muted { 0.0 } else { self.volume });
                }
            });
        });
    }

    fn timeline(&mut self, ui: &mut egui::Ui) {
        let Some(m) = self.movie.clone() else { return };
        let (rect, resp) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), 86.0),
            Sense::click_and_drag(),
        );
        let p = ui.painter_at(rect);
        let d = m.duration.max(1e-3);
        let x_of = |t: f32| rect.left() + 8.0 + (rect.width() - 16.0) * (t / d);
        let t_of = |x: f32| ((x - rect.left() - 8.0) / (rect.width() - 16.0) * d).clamp(0.0, d);
        p.rect_filled(rect, 4.0, Color32::from_gray(22));

        // ruler
        let px_per_s = (rect.width() - 16.0) / d;
        let step = [0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0]
            .into_iter()
            .find(|s| s * px_per_s >= 48.0)
            .unwrap_or(120.0);
        let mut s = 0.0;
        while s <= d + 1e-3 {
            let x = x_of(s);
            p.line_segment(
                [
                    Pos2::new(x, rect.top() + 2.0),
                    Pos2::new(x, rect.top() + 8.0),
                ],
                Stroke::new(1.0, Color32::from_gray(90)),
            );
            p.text(
                Pos2::new(x + 2.0, rect.top() + 1.0),
                egui::Align2::LEFT_TOP,
                format!("{s}s"),
                egui::FontId::monospace(10.0),
                Color32::from_gray(120),
            );
            s += step;
        }

        // shots
        let palette = [
            Color32::from_rgb(74, 98, 140),
            Color32::from_rgb(118, 84, 128),
            Color32::from_rgb(70, 120, 104),
            Color32::from_rgb(140, 104, 66),
            Color32::from_rgb(120, 74, 74),
        ];
        let (y0, y1) = (rect.top() + 20.0, rect.top() + 56.0);
        for shot in &m.shots {
            let r = Rect::from_min_max(
                Pos2::new(x_of(shot.start), y0),
                Pos2::new(x_of(shot.end()), y1),
            );
            let c = palette[shot.index % palette.len()];
            p.rect_filled(r.shrink2(Vec2::new(0.5, 0.0)), 3.0, c);
            let label_x = x_of(shot.settled()) + 4.0;
            // Skip labels that wouldn't fit (long films have narrow blocks).
            if x_of(shot.end()) - label_x < 7.0 * shot.id.len() as f32 + 4.0 {
                continue;
            }
            p.text(
                Pos2::new(label_x, y0 + 4.0),
                egui::Align2::LEFT_TOP,
                &shot.id,
                egui::FontId::proportional(13.0),
                Color32::WHITE,
            );
            p.text(
                Pos2::new(label_x, y1 - 3.0),
                egui::Align2::LEFT_BOTTOM,
                format!("{:.1}s", shot.duration),
                egui::FontId::monospace(10.0),
                Color32::from_white_alpha(150),
            );
        }
        // transitions: hatched overlap
        for shot in &m.shots {
            if let Some(tr) = &shot.transition_in {
                let r = Rect::from_min_max(
                    Pos2::new(x_of(shot.start), y0 - 4.0),
                    Pos2::new(x_of(shot.settled()), y1 + 4.0),
                );
                p.rect_filled(r, 2.0, Color32::from_black_alpha(110));
                let mut x = r.left() - r.height();
                while x < r.right() {
                    let a = Pos2::new(x.max(r.left()), r.bottom() - (x.max(r.left()) - x));
                    let bx = (x + r.height()).min(r.right());
                    let b = Pos2::new(bx, r.bottom() - (bx - x));
                    p.line_segment(
                        [a, b],
                        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 210, 140, 70)),
                    );
                    x += 6.0;
                }
                p.rect_stroke(
                    r,
                    2.0,
                    Stroke::new(1.0, Color32::from_rgb(255, 200, 120)),
                    egui::StrokeKind::Inside,
                );
                let label = if tr.freeze {
                    format!("{} (frozen)", tr.kind)
                } else {
                    tr.kind.clone()
                };
                let spacing = m
                    .shots
                    .iter()
                    .filter(|s| s.transition_in.is_some())
                    .map(|s| x_of(s.start))
                    .collect::<Vec<_>>()
                    .windows(2)
                    .map(|w| w[1] - w[0])
                    .fold(f32::MAX, f32::min);
                if spacing < 6.5 * label.len() as f32 {
                    continue;
                }
                p.text(
                    Pos2::new(r.center().x, y1 + 6.0),
                    egui::Align2::CENTER_TOP,
                    label,
                    egui::FontId::proportional(11.0),
                    Color32::from_rgb(255, 210, 150),
                );
            }
        }
        // markers
        for mk in &m.markers {
            let x = x_of(mk.t);
            p.line_segment(
                [Pos2::new(x, y0), Pos2::new(x, rect.bottom() - 2.0)],
                Stroke::new(1.0, Color32::from_rgb(120, 200, 255)),
            );
            p.text(
                Pos2::new(x + 3.0, rect.bottom() - 2.0),
                egui::Align2::LEFT_BOTTOM,
                &mk.label,
                egui::FontId::proportional(10.0),
                Color32::from_rgb(140, 210, 255),
            );
        }
        // loop region
        if let Some((a, b)) = self.loop_region {
            let r = Rect::from_min_max(
                Pos2::new(x_of(a), rect.top()),
                Pos2::new(x_of(b), rect.bottom()),
            );
            p.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(120, 255, 160, 22));
            p.rect_stroke(
                r,
                0.0,
                Stroke::new(1.0, Color32::from_rgb(120, 230, 150)),
                egui::StrokeKind::Inside,
            );
        }
        // playhead
        let x = x_of(self.clock.time());
        p.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(2.0, Color32::from_rgb(255, 90, 70)),
        );

        // interaction
        if let Some(pos) = resp.interact_pointer_pos() {
            if resp.drag_started() || resp.clicked() {
                if self.scrub_resume.is_none() {
                    self.scrub_resume = Some(self.clock.playing());
                }
                self.clock.pause();
            }
            if resp.dragged() || resp.clicked() || resp.drag_started() {
                self.seek(t_of(pos.x));
            }
        }
        if resp.double_clicked()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            let r = m.region_at(t_of(pos.x));
            self.loop_region = if self.loop_region == Some(r) {
                None
            } else {
                Some(r)
            };
        }
        if resp.drag_stopped() || (resp.clicked() && !resp.dragged()) {
            if let Some(true) = self.scrub_resume.take() {
                self.clock.play();
            }
            self.scrub_resume = None;
        }
        if let Some(pos) = resp.hover_pos() {
            let t = t_of(pos.x);
            resp.on_hover_text(format!("{}  |  {}", clock_text(t), describe(&m, t)));
        }
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        let t = self.clock.time();
        egui::ScrollArea::vertical().show(ui, |ui| {
            if let Some(m) = self.movie.clone() {
                ui.heading(&m.title);
                ui.label(
                    RichText::new(self.path.display().to_string())
                        .small()
                        .weak(),
                );
                ui.add_space(6.0);
                egui::CollapsingHeader::new("Now")
                    .default_open(true)
                    .show(ui, |ui| match m.at(t) {
                        At::Shot { shot, local } => {
                            let s = &m.shots[shot];
                            ui.label(format!("shot  {}", s.id));
                            ui.label(format!(
                                "local {local:.3}s  |  iTime {:.3}",
                                s.scene_time(local)
                            ));
                            ui.add(egui::ProgressBar::new(local / s.duration).desired_height(6.0));
                            uniforms(ui, s, local);
                        }
                        At::Transition {
                            from,
                            to,
                            progress,
                            to_local,
                            ..
                        } => {
                            let tr = m.shots[to].transition_in.as_ref().unwrap();
                            ui.label(format!("{} -> {}", m.shots[from].id, m.shots[to].id));
                            ui.label(format!(
                                "{}{}  |  {:.0}% (eased {:.0}%)",
                                tr.kind,
                                if tr.freeze { " (frozen)" } else { "" },
                                progress * 100.0,
                                tr.ease.apply(progress) * 100.0
                            ));
                            ui.add(egui::ProgressBar::new(progress).desired_height(6.0));
                            uniforms(ui, &m.shots[to], to_local);
                        }
                    });
            }
            egui::CollapsingHeader::new("Performance")
                .default_open(true)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("render scale");
                        let label = match self.scale_mode {
                            ScaleMode::Auto => {
                                format!("auto ({:.0}%)", self.effective_scale() * 100.0)
                            }
                            ScaleMode::Fixed(s) => format!("{:.0}%", s * 100.0),
                        };
                        egui::ComboBox::from_id_salt("scale")
                            .selected_text(label)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.scale_mode, ScaleMode::Auto, "auto");
                                for s in [1.0, 0.75, 0.5, 0.35] {
                                    ui.selectable_value(
                                        &mut self.scale_mode,
                                        ScaleMode::Fixed(s),
                                        format!("{:.0}%", s * 100.0),
                                    );
                                }
                            });
                    });
                    let avg = |q: &VecDeque<f32>| {
                        if q.is_empty() {
                            None
                        } else {
                            Some(q.iter().rev().take(30).sum::<f32>() / q.len().min(30) as f32)
                        }
                    };
                    let fms = avg(&self.perf.frame_ms);
                    ui.label(format!(
                        "frame {}   |   {}",
                        fms.map_or("–".into(), |v| format!("{v:.1} ms")),
                        fms.map_or("–".into(), |v| format!("{:.0} fps", 1000.0 / v))
                    ));
                    ui.label(format!(
                        "gpu {}   |   dropped {}",
                        avg(&self.perf.gpu_ms).map_or("n/a".into(), |v| format!("{v:.2} ms")),
                        self.perf.dropped
                    ))
                    .on_hover_text("frames that missed the display refresh while rendering");
                    if self.perf.stalled > 0 {
                        ui.label(format!(
                            "stalls {} (sound {}, compiling {})",
                            self.perf.stalled,
                            self.perf.stalled_sound,
                            self.perf.stalled - self.perf.stalled_sound
                        ))
                        .on_hover_text("late frames whose housekeeping took over 3 ms");
                    }
                    if let Some(w) = self.perf.worst_sound {
                        let s = w.sound;
                        let g = s.gl;
                        ui.label(format!(
                            "worst sound stall {:.1} ms: fences + reads {:.1} ({}), queueing {:.1} ({}), cpu {:.1}\n\
                             GL: poll {:.1}, read {:.1}, upload {:.1}, draw {:.1}, fence {:.1}{}",
                            w.sound_ms, s.collect_ms, s.collected, s.submit_ms, s.submitted, s.cpu_ms,
                            g.poll_ms, g.read_ms, g.upload_ms, g.draw_ms, g.fence_ms,
                            if self.renderer.persistent_buffers() { " (persistent buffers)" } else { "" }
                        ))
                        .on_hover_text(
                            "where the slowest sound stall's time went: polling fences and reading \
                             finished blocks (count), queueing blocks on the GPU (count), and CPU \
                             work (fades, sums, mix inputs)",
                        );
                    }
                    graph(
                        ui,
                        &self.perf.frame_ms,
                        &self.perf.gpu_ms,
                        &self.perf.marks,
                        self.perf.vsync_ms(),
                    );
                    if ui.small_button("reset counters").clicked() {
                        self.perf.dropped = 0;
                        self.perf.stalled = 0;
                        self.perf.stalled_sound = 0;
                        self.perf.worst_sound = None;
                        self.perf.frame_ms.clear();
                        self.perf.marks.clear();
                        self.perf.gpu_ms.clear();
                    }
                });
            egui::CollapsingHeader::new("Sound")
                .default_open(false)
                .show(ui, |ui| {
                    ui.label(&self.sound_info);
                    if self.clock.playing() {
                        ui.label(if self.clock.audio_locked() {
                            "clock: locked to the sound card"
                        } else {
                            "clock: system timer"
                        });
                    }
                    if let Some(m) = &self.movie {
                        let shots = m.shots.iter().filter(|s| s.sound.is_some()).count();
                        ui.label(format!(
                            "music: {}  |  shot sounds: {shots}  |  mix: {}",
                            if m.music.is_some() { "yes" } else { "no" },
                            if m.mix.is_some() { "mix.glsl" } else { "sum" }
                        ));
                    }
                    ui.label(format!(
                        "buffered ahead {:.1} s  |  underruns {}",
                        self.buffered,
                        self.clock.underruns()
                    ));
                    if !self.last_synth.is_empty() {
                        ui.label(
                            RichText::new(format!("last synthesized: {}", self.last_synth))
                                .small()
                                .weak(),
                        );
                    }
                });
            egui::CollapsingHeader::new("System")
                .default_open(false)
                .show(ui, |ui| {
                    ui.label(RichText::new(&self.renderer.gl_info).small());
                    ui.label(format!("reloads: {}", self.reloads.saturating_sub(1)));
                });
            egui::CollapsingHeader::new("Keys")
                .default_open(false)
                .show(ui, |ui| {
                    for (k, v) in [
                        ("Space", "play / pause"),
                        ("Left Right", "step one frame"),
                        ("Shift+Left/Right", "back / forward 1 s"),
                        ("[ ]", "previous / next shot"),
                        (", .", "previous / next transition"),
                        ("L", "loop shot or transition"),
                        ("1 2 3", "speed 0.25x 0.5x 1x"),
                        ("Q", "cycle render scale"),
                        ("M", "mute"),
                        ("R", "reload now"),
                        ("H / Tab", "hide this panel"),
                    ] {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(k).monospace().strong());
                            ui.label(v);
                        });
                    }
                });
        });
    }
}

impl eframe::App for Player {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // A long gap since the last frame is how a driver reset shows up (the GPU
        // hangs for seconds first): the cue to ask the driver (see `check_gpu`).
        let now = Instant::now();
        let gap = self.last_ui.map(|t| now - t);
        self.last_ui = Some(now);
        // Debounced live reload: editors often write a file in several steps.
        while self.rx.try_recv().is_ok() {
            self.reload_at = Some(Instant::now() + Duration::from_millis(120));
        }
        if self.reload_at.is_some_and(|t| Instant::now() >= t) {
            self.reload_at = None;
            self.reload();
        }
        self.handle_keys(&ctx);
        self.advance_clock();
        self.perf_frame += 1;
        // Housekeeping (shader compiles here, sound after the render) runs on this
        // thread. Time it, so a slow frame caused by it isn't mistaken for the GPU
        // being overloaded.
        let work = Instant::now();
        if let Some(m) = &self.movie {
            // Without parallel compiling, a compile blocks: don't do background ones mid-playback.
            let background = !self.clock.playing() || self.renderer.parallel;
            self.renderer.pump(m, self.clock.time(), background);
        }
        let compile_ms = work.elapsed().as_secs_f32() * 1000.0;
        self.perf.pending.compile_ms += compile_ms;
        if self.movie.is_some() {
            self.perf.pending.texture_ms += self.renderer.texture_ms;
        }
        self.perf.stall = Stall::worse(
            self.perf.stall.take(),
            (compile_ms > 3.0).then_some(Stall {
                sound_ms: 0.0,
                compile_ms,
                sound: Default::default(),
            }),
        );

        egui::Panel::bottom("transport").show(ui, |ui| {
            ui.add_space(4.0);
            self.transport(ui);
            self.timeline(ui);
            ui.add_space(4.0);
        });
        if self.show_panel {
            egui::Panel::right("info")
                .default_size(300.0)
                .resizable(true)
                .show(ui, |ui| self.side_panel(ui));
        }
        let mut rendered = false;
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::from_gray(8))).show(ui, |ui| {
            let avail = ui.available_rect_before_wrap();
            let aspect = self.movie.as_ref().map_or(16.0 / 9.0, |m| m.aspect);
            let size = if avail.width() / avail.height() > aspect {
                Vec2::new(avail.height() * aspect, avail.height())
            } else {
                Vec2::new(avail.width(), avail.width() / aspect)
            };
            let view = Rect::from_center_size(avail.center(), size);
            let ppp = ctx.pixels_per_point();
            let (w, h) = ((view.width() * ppp).round() as i32, (view.height() * ppp).round() as i32);
            if w > 0 && h > 0 {
                rendered = self.render_view(frame, w, h);
                if let Some((_, id)) = self.tex {
                    ui.painter().image(id, view, Rect::from_min_max(Pos2::new(0.0, 1.0), Pos2::new(1.0, 0.0)), Color32::WHITE);
                }
            }
            if self.gpu_lost {
                ui.painter().text(
                    view.center(),
                    egui::Align2::CENTER_CENTER,
                    "The GPU driver reset — a shader probably ran for seconds.\nFix the shader, then restart shotglass.",
                    egui::FontId::proportional(16.0),
                    Color32::from_rgb(255, 160, 140),
                );
            }
            let errors: Vec<String> = self
                .load_error
                .iter()
                .cloned()
                .chain(self.shader_errors.iter().map(|e| e.to_string()))
                .chain(self.renderer.errors().iter().map(|e| e.to_string()))
                .chain(self.sound_errors.iter().map(|e| e.to_string()))
                .collect();
            if !errors.is_empty() {
                let r = Rect::from_min_max(
                    Pos2::new(avail.left() + 12.0, (avail.bottom() - 240.0).max(avail.top() + 12.0)),
                    Pos2::new(avail.right() - 12.0, avail.bottom() - 12.0),
                );
                ui.scope_builder(egui::UiBuilder::new().max_rect(r).layout(egui::Layout::top_down(egui::Align::Min)), |ui| {
                    egui::Frame::NONE
                        .fill(Color32::from_rgba_unmultiplied(40, 8, 6, 235))
                        .inner_margin(10.0)
                        .corner_radius(6.0)
                        .show(ui, |ui| {
                            ui.set_width(r.width() - 20.0);
                            ui.label(
                                RichText::new("Couldn't load the latest change. Showing the last working version; fix and save to reload.")
                                    .color(Color32::from_rgb(255, 180, 160)),
                            );
                            ui.add_space(4.0);
                            egui::ScrollArea::vertical().max_height(r.height() - 60.0).show(ui, |ui| {
                                for e in &errors {
                                    ui.label(RichText::new(e).monospace().color(Color32::from_rgb(255, 140, 120)));
                                }
                            });
                        });
                });
            }
            if let Some((s, at)) = &self.status
                && at.elapsed() < Duration::from_secs(3) {
                    ui.painter().text(
                        view.left_top() + Vec2::new(10.0, 10.0),
                        egui::Align2::LEFT_TOP,
                        s,
                        egui::FontId::proportional(13.0),
                        Color32::from_white_alpha(200),
                    );
                }
            let (ready, total) = self.renderer.progress();
            let (loaded, textures) = self.renderer.texture_progress();
            let mut busy = vec![];
            if ready < total {
                busy.push(format!("compiling shaders {ready}/{total}"));
            }
            if loaded < textures {
                busy.push(format!("loading textures {loaded}/{textures}"));
            }
            if !busy.is_empty() {
                ui.painter().text(
                    view.right_top() + Vec2::new(-10.0, 10.0),
                    egui::Align2::RIGHT_TOP,
                    busy.join("  ·  "),
                    egui::FontId::proportional(12.0),
                    Color32::from_white_alpha(160),
                );
            }
            if self.movie.is_none() && self.load_error.is_none() && self.shader_errors.is_empty() {
                ui.label("loading...");
            }
        });
        self.measure(rendered);
        // Sound goes after the frame's render is queued. Polling a fence or reading a
        // block back is a driver round trip, and on threaded drivers (NVIDIA) that
        // waits for the driver thread, which may be blocked in the previous frame's
        // present until vsync: a whole refresh. Before the render, that delayed it;
        // here it overlaps the wait for the swap. A slow one still excuses the next
        // frame's lateness (as a stall), so the counters stay honest.
        let work = Instant::now();
        self.stream_sound();
        let sound_ms = work.elapsed().as_secs_f32() * 1000.0;
        self.perf.pending.sound_ms += sound_ms;
        let st = self.soundtrack.stats();
        self.perf.pending.sound.submitted += st.submitted;
        self.perf.pending.sound.collected += st.collected;
        self.perf.stall = (sound_ms > 3.0).then(|| Stall {
            sound_ms,
            compile_ms: 0.0,
            sound: self.soundtrack.stats(),
        });
        self.check_gpu(gap);
        let (ready, total) = self.renderer.progress();
        let (loaded, textures) = self.renderer.texture_progress();
        if self.clock.playing() || ready < total || loaded < textures {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
    }
}

/// The auto render scale's next value, given the average GPU time per frame at the
/// current scale and the per-frame budget (ms). None = keep the current scale.
/// Cost grows with pixel count (scale²), so the full-resolution cost is predicted
/// and the largest scale that fits 80% of the budget is chosen. It only lowers the
/// scale when frames are really over 90% of budget, and climbs back in big steps.
fn next_scale(scale: f32, avg_gpu_ms: f32, budget_ms: f32) -> Option<f32> {
    let full = avg_gpu_ms / (scale * scale).max(0.01);
    let target = (0.8 * budget_ms / full.max(1e-3)).sqrt().clamp(0.3, 1.0);
    if avg_gpu_ms > 0.9 * budget_ms && target < scale {
        Some(target)
    } else if target > scale * 1.03 {
        Some((scale + (target - scale) * 0.7).min(1.0))
    } else {
        None
    }
}

fn uniforms(ui: &mut egui::Ui, s: &crate::movie::Shot, local: f32) {
    if s.uniforms.is_empty() {
        return;
    }
    ui.add_space(4.0);
    egui::Grid::new(("u", s.index))
        .num_columns(2)
        .spacing([10.0, 2.0])
        .show(ui, |ui| {
            for (name, a) in &s.uniforms {
                let v = a.eval(local);
                let txt = v[..a.dim()]
                    .iter()
                    .map(|x| format!("{x:.3}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                ui.label(RichText::new(name).monospace());
                ui.label(RichText::new(txt).monospace().weak());
                ui.end_row();
            }
        });
}

const MARK_RENDER: Color32 = Color32::from_rgb(110, 110, 110);
const MARK_QUEUED: Color32 = Color32::from_rgb(190, 120, 255);
const MARK_COLLECTED: Color32 = Color32::from_rgb(90, 220, 220);
const MARK_COMPILE: Color32 = Color32::from_rgb(255, 150, 60);

/// Frame times (and scene GPU times), with ticks under each frame for what it did:
/// rendered a film frame, queued sound blocks, collected sound blocks, compiled.
/// Hover a frame for its numbers.
fn graph(
    ui: &mut egui::Ui,
    frame: &VecDeque<f32>,
    gpu: &VecDeque<f32>,
    marks: &VecDeque<Marks>,
    vsync: f32,
) {
    const N: f32 = 240.0;
    const TICKS: f32 = 4.0 * 3.0; // four rows of 3 px
    const LEGEND: f32 = 13.0;
    let (full, resp) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), 70.0 + TICKS + LEGEND),
        Sense::hover(),
    );
    let rect = Rect::from_min_max(full.min, Pos2::new(full.right(), full.bottom() - LEGEND));
    let p = ui.painter_at(full);
    p.rect_filled(rect, 3.0, Color32::from_gray(18));
    let plot = Rect::from_min_max(rect.min, Pos2::new(rect.right(), rect.bottom() - TICKS));
    let top = (vsync * 3.0).max(20.0);
    let y = |ms: f32| plot.bottom() - (ms / top).min(1.0) * plot.height();
    let x = |len: usize, i: usize| rect.right() - (len - 1 - i) as f32 * rect.width() / N;
    for (ms, c) in [
        (vsync, Color32::from_rgb(90, 160, 90)),
        (vsync * 2.0, Color32::from_rgb(160, 90, 70)),
    ] {
        p.line_segment(
            [
                Pos2::new(rect.left(), y(ms)),
                Pos2::new(rect.right(), y(ms)),
            ],
            Stroke::new(1.0, c),
        );
    }
    for (i, m) in marks.iter().enumerate() {
        let px = x(marks.len(), i);
        for (row, on, c) in [
            (0, m.rendered, MARK_RENDER),
            (1, m.sound.submitted > 0, MARK_QUEUED),
            (2, m.sound.collected > 0, MARK_COLLECTED),
            (3, m.compile_ms > 0.5, MARK_COMPILE),
        ] {
            if on {
                let y0 = plot.bottom() + row as f32 * 3.0;
                p.line_segment(
                    [Pos2::new(px, y0), Pos2::new(px, y0 + 2.5)],
                    Stroke::new(1.5, c),
                );
            }
        }
    }
    let line = |q: &VecDeque<f32>, c: Color32| {
        if q.len() < 2 {
            return;
        }
        let pts: Vec<Pos2> = q
            .iter()
            .enumerate()
            .map(|(i, v)| Pos2::new(x(q.len(), i), y(*v)))
            .collect();
        p.add(egui::Shape::line(pts, Stroke::new(1.0, c)));
    };
    line(gpu, Color32::from_rgb(120, 180, 255));
    line(frame, Color32::from_rgb(255, 220, 140));
    let mut lx = rect.left() + 4.0;
    for (label, c) in [
        ("frame", Color32::from_rgb(255, 220, 140)),
        ("gpu", Color32::from_rgb(120, 180, 255)),
        ("render", MARK_RENDER),
        ("sound queued", MARK_QUEUED),
        ("collected", MARK_COLLECTED),
        ("compile", MARK_COMPILE),
    ] {
        let r = p.text(
            Pos2::new(lx, rect.bottom() + 1.0),
            egui::Align2::LEFT_TOP,
            label,
            egui::FontId::proportional(10.0),
            c,
        );
        lx = r.right() + 6.0;
    }
    // Hover: the frame under the pointer.
    if let Some(pos) = resp.hover_pos()
        && !frame.is_empty()
    {
        let back = ((rect.right() - pos.x) * N / rect.width()).round() as usize;
        if back < frame.len() {
            let i = frame.len() - 1 - back;
            p.line_segment(
                [
                    Pos2::new(x(frame.len(), i), rect.top()),
                    Pos2::new(x(frame.len(), i), rect.bottom()),
                ],
                Stroke::new(1.0, Color32::from_white_alpha(60)),
            );
            let mut text = format!("frame {:.1} ms", frame[i]);
            if let Some(m) = marks.get(i) {
                if let Some(g) = m.gpu_ms {
                    text += &format!(", scene gpu {g:.1} ms");
                }
                text += &format!(
                    "\n{}sound {:.1} ms (queued {}, collected {}), compile {:.1} ms (textures {:.1} ms)",
                    if m.rendered {
                        "rendered a film frame\n"
                    } else {
                        ""
                    },
                    m.sound_ms,
                    m.sound.submitted,
                    m.sound.collected,
                    m.compile_ms,
                    m.texture_ms
                );
            }
            resp.on_hover_text(text);
        }
    }
}

fn describe(m: &Movie, t: f32) -> String {
    match m.at(t) {
        At::Shot { shot, local } => format!("{} +{local:.2}s", m.shots[shot].id),
        At::Transition {
            from, to, progress, ..
        } => {
            format!(
                "{} -> {} {:.0}%",
                m.shots[from].id,
                m.shots[to].id,
                progress * 100.0
            )
        }
    }
}

fn clock_text(t: f32) -> String {
    let t = t.max(0.0);
    format!("{:02}:{:05.2}", (t / 60.0) as u32, t % 60.0)
}

#[cfg(test)]
mod tests {
    use super::next_scale;

    #[test]
    fn stays_at_full_resolution_when_it_fits() {
        // 30 fps film: 33 ms budget. A 20 ms close-up at 100% is fine.
        assert_eq!(next_scale(1.0, 20.0, 33.3), None);
        // Even 29 ms (87% of budget) doesn't trigger a drop.
        assert_eq!(next_scale(1.0, 29.0, 33.3), None);
    }

    #[test]
    fn drops_when_over_budget_and_recovers_fast() {
        let s = next_scale(1.0, 50.0, 33.3).unwrap();
        assert!((0.7..0.75).contains(&s), "{s}");
        // At the lower scale the shot now costs 50·s² ms; a cheap shot follows (2 ms)
        let up = next_scale(s, 2.0, 33.3).unwrap();
        assert!(up > 0.9, "{up}");
        assert_eq!(next_scale(1.0, 2.0, 33.3), None);
    }
}
