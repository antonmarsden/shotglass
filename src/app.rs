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

pub fn run(path: PathBuf, start: f32) -> anyhow::Result<()> {
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
            let app = Player::new(cc, path, start).map_err(|e| format!("{e:#}"))?;
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

struct Perf {
    last: Option<Instant>,
    frame_ms: VecDeque<f32>,
    gpu_ms: VecDeque<f32>,
    dropped: u32,
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
    shader_errors: Vec<ShaderError>,
    renderer: Renderer,
    clock: Clock,
    sound_key: Option<u64>,
    soundtrack: Soundtrack,
    sound_info: String,
    _watcher: Option<notify::RecommendedWatcher>,
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
    loop_all: bool,
    volume: f32,
    muted: bool,
    last_input: Instant,
    show_panel: bool,
    scrub_resume: Option<bool>,
}

impl Player {
    fn new(cc: &eframe::CreationContext<'_>, path: PathBuf, start: f32) -> anyhow::Result<Player> {
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
                            Some("glsl" | "toml" | "frag" | "inc" | "h")
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
            renderer,
            clock,
            sound_key: None,
            soundtrack: Soundtrack::new(),
            sound_info,
            _watcher: watcher,
            rx,
            reload_at: None,
            reloads: 0,
            status: None,
            tex: None,
            rendered_key: None,
            generation: 0,
            scale_mode: ScaleMode::Auto,
            auto_scale: 1.0,
            perf: Perf {
                last: None,
                frame_ms: VecDeque::new(),
                gpu_ms: VecDeque::new(),
                dropped: 0,
                cooldown: 0,
                calm: 0,
            },
            loop_region: None,
            loop_all: false,
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
        let t0 = Instant::now();
        match self.renderer.load(&m) {
            Ok(n) => {
                self.shader_errors.clear();
                let pic_ms = t0.elapsed().as_secs_f32() * 1000.0;
                let snd = self.update_sound(&m);
                self.reloads += 1;
                if self.reloads > 1 {
                    let mut msg = format!("reloaded | {n} shader(s) compiled in {pic_ms:.0} ms");
                    if let Some(s) = snd {
                        msg += &format!(" | {s}");
                    }
                    self.say(msg);
                }
                self.movie = Some(m);
                self.generation += 1;
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
    fn update_sound(&mut self, m: &Movie) -> Option<String> {
        if !m.has_sound() {
            self.sound_key = None;
            self.clock.set_samples(vec![]);
            return None;
        }
        let t0 = Instant::now();
        match self
            .soundtrack
            .build(&mut self.renderer, m, self.clock.sample_rate())
        {
            Ok(built) => {
                if self.sound_key != Some(built.key) {
                    self.clock.set_samples(built.samples);
                    self.sound_key = Some(built.key);
                }
                (!built.rendered.is_empty()).then(|| {
                    format!(
                        "sound: {} in {:.0} ms",
                        built.rendered.join(", "),
                        t0.elapsed().as_secs_f32() * 1000.0
                    )
                })
            }
            Err(errors) => {
                self.shader_errors.extend(errors);
                None
            }
        }
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

    fn advance_clock(&mut self) {
        if !self.clock.playing() {
            return;
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
        if !self.clock.playing() {
            self.perf.last = None;
            return;
        }
        let Some(dt) = dt else { return };
        Perf::push(&mut self.perf.frame_ms, dt);
        if let Some(g) = gpu {
            Perf::push(&mut self.perf.gpu_ms, g);
        }
        let vsync = self.perf.vsync_ms();
        let over = dt > vsync * 1.5;
        if over {
            self.perf.dropped += 1;
        }
        if self.scale_mode != ScaleMode::Auto || !rendered {
            return;
        }
        if self.perf.cooldown > 0 {
            self.perf.cooldown -= 1;
            return;
        }
        // Prefer the GPU timer: it warns before frames actually drop.
        let busy = gpu.map(|g| g / vsync);
        let too_slow = over || busy.is_some_and(|b| b > 0.85);
        if too_slow {
            let k = busy.map_or(0.85, |b| (0.75 / b).sqrt().clamp(0.7, 0.95));
            self.auto_scale = (self.auto_scale * k).max(0.3);
            self.perf.cooldown = 12;
            self.perf.calm = 0;
        } else if busy.is_none_or(|b| b < 0.55) {
            self.perf.calm += 1;
            if self.perf.calm > 90 && self.auto_scale < 1.0 {
                self.auto_scale = (self.auto_scale * 1.06).min(1.0);
                self.perf.calm = 0;
                self.perf.cooldown = 20;
            }
        }
    }

    fn render_view(&mut self, frame: &mut eframe::Frame, w: i32, h: i32) -> bool {
        let Some(m) = &self.movie else { return false };
        let t = self.clock.time();
        let scale = self.effective_scale();
        let key = (t.to_bits(), w, h, scale.to_bits(), self.generation);
        if !self.clock.playing() && self.rendered_key == Some(key) {
            return false;
        }
        let fps = m.fps;
        let info = FrameInfo {
            t,
            dt: 1.0 / 60.0,
            frame: (t * fps) as i32,
            scale,
        };
        let tex = self.renderer.render(m, w, h, &info);
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
            ui.checkbox(&mut self.loop_all, "repeat movie");
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
            ui.label(
                RichText::new(format!("{}  /  {}   frame {}", clock_text(t), clock_text(self.duration()), (t * fps).round() as i64))
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
                    ));
                    graph(
                        ui,
                        &self.perf.frame_ms,
                        &self.perf.gpu_ms,
                        self.perf.vsync_ms(),
                    );
                    if ui.small_button("reset counters").clicked() {
                        self.perf.dropped = 0;
                        self.perf.frame_ms.clear();
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
            let errors: Vec<String> = self
                .load_error
                .iter()
                .cloned()
                .chain(self.shader_errors.iter().map(|e| e.to_string()))
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
            if self.movie.is_none() && self.load_error.is_none() && self.shader_errors.is_empty() {
                ui.label("loading...");
            }
        });
        self.measure(rendered);
        if self.clock.playing() {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
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

fn graph(ui: &mut egui::Ui, frame: &VecDeque<f32>, gpu: &VecDeque<f32>, vsync: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 70.0), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 3.0, Color32::from_gray(18));
    let top = (vsync * 3.0).max(20.0);
    let y = |ms: f32| rect.bottom() - (ms / top).min(1.0) * rect.height();
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
    let line = |q: &VecDeque<f32>, c: Color32| {
        if q.len() < 2 {
            return;
        }
        let n = 240.0;
        let pts: Vec<Pos2> = q
            .iter()
            .enumerate()
            .map(|(i, v)| {
                Pos2::new(
                    rect.right() - (q.len() - 1 - i) as f32 * rect.width() / n,
                    y(*v),
                )
            })
            .collect();
        p.add(egui::Shape::line(pts, Stroke::new(1.0, c)));
    };
    line(gpu, Color32::from_rgb(120, 180, 255));
    line(frame, Color32::from_rgb(255, 220, 140));
    p.text(
        rect.left_top() + Vec2::new(4.0, 2.0),
        egui::Align2::LEFT_TOP,
        "frame",
        egui::FontId::proportional(10.0),
        Color32::from_rgb(255, 220, 140),
    );
    p.text(
        rect.left_top() + Vec2::new(40.0, 2.0),
        egui::Align2::LEFT_TOP,
        "gpu",
        egui::FontId::proportional(10.0),
        Color32::from_rgb(120, 180, 255),
    );
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
