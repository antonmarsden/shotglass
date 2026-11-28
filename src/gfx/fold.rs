//! Feedback buffers (#134), renderer side: the canonical fold, rings, checkpoints and
//! catch-up.
//!
//! A buffer's state at film frame n is its state at the last reset r ≤ n stepped once
//! per film frame from r to n. Buffers step in file order after the picture track and
//! before any layer, all on one cursor: `Fold::frame` is the film frame every active
//! buffer's ring holds. Reaching another frame n picks the cheapest known start (the
//! cursor if it is behind n, the nearest checkpoint ≤ n, or a cold start from the
//! resets) and steps forward to n. Headless renders do it at once; the player (and any
//! non-blocking render) does at most `step_budget` steps per call and shows the frame
//! only when the fold has reached it. Nothing is read back: steps are draws, and
//! checkpoints are GPU copies (`glBlitFramebuffer`) kept under their own budget.

use super::{
    INPUT_TEX, LAYER_TEX, PICTURE_TEX, Renderer, SeqPlan, SeqTargets, ShotKeys, Target, Which, hash,
};

use crate::gfx::Tracked;
use crate::gfx::glstate;
use crate::movie::{Buffer, BufferFormat, Movie, Source};
use eframe::glow::{self, HasContext};

/// `ChanKey::tex` of a buffer read: `BUF_TEX - (b · 65 + back)`, `back` 0..=64.
pub(super) const BUF_TEX: u64 = LAYER_TEX - 60000;

pub(super) fn buf_key(b: usize, back: u32) -> u64 {
    BUF_TEX - (b as u64 * 65 + back as u64)
}

/// A step composite (#138): (the `after` layer, the size it's drawn at).
type StackKey = (usize, (i32, i32));

/// Passes a buffer step binds: (pass key, (texture, width, height)).
type Passes = Vec<(u64, (glow::Texture, usize, usize))>;

/// Steps a non-blocking render (or the player, `set_catchup_slices`) does per call.
pub const STEP_BUDGET: usize = 8;

/// One buffer's states: `history + 1` targets, each holding the state of one film
/// frame (`None`: nothing). The ring is rotated, never copied.
pub(super) struct Ring {
    size: (i32, i32),
    format: BufferFormat,
    slots: Vec<(Target, Option<i64>)>,
    /// The reset the fold it holds started from; `reset - 1` is the `init` state.
    reset: u64,
    /// The first frame of the current run of steps (the reset, or where a pure history
    /// buffer was started mid-span).
    since: u64,
    /// The last frame stepped.
    last: Option<u64>,
}

/// A checkpoint: every non-pure buffer's ring (the states the next step reads) at
/// film frame `frame`, for fold key `key`.
pub(super) struct Checkpoint {
    key: u64,
    frame: u64,
    rings: Vec<Saved>,
    bytes: usize,
}

struct Saved {
    buffer: usize,
    reset: u64,
    format: BufferFormat,
    slots: Vec<(i64, Target)>,
}

/// A catch-up in progress: step frames `next..=target`; buffer b steps on frames ≥
/// `starts[b]` (inside its span).
struct Plan {
    key: u64,
    target: u64,
    next: u64,
    first: u64,
    starts: Vec<u64>,
}

pub(super) struct Fold {
    /// Fold key of the state held (rings, cursor).
    key: u64,
    /// The frame every active buffer's ring is at.
    frame: Option<u64>,
    rings: Vec<Option<Ring>>,
    checkpoints: Vec<Checkpoint>,
    /// Evicted checkpoint targets for reuse.
    spare: Vec<(BufferFormat, Target)>,
    plan: Option<Plan>,
    progress: Option<(u64, u64)>,
    /// Hash of everything a fold reads that isn't a program or texture key (set_movie).
    pub(super) decl: u64,
    /// A buffer reads `@picture` or a comp: shot, transition and comp keys are in the key.
    pub(super) reads_film: bool,
    /// Scratch targets for the picture track a buffer reads, per size.
    pics: Vec<((i32, i32), SeqTargets)>,
    /// Scratch targets for the composite up to a layer that `after` buffers read
    /// (#138), per (layer, size).
    stacks: Vec<(StackKey, SeqTargets)>,
    budget: usize,
    budget_source: &'static str,
    slices: bool,
    step_budget: usize,
    checkpoints_on: bool,
    /// The frame the layers of the frame being drawn read.
    pub(super) n: u64,
    /// The frame the player shows (eviction keeps checkpoints near it); the background
    /// fill steps elsewhere without moving it.
    playhead: u64,
    /// The fold key's parts (hash, first frame that reads it), for per-frame
    /// invalidation (#137).
    parts: Vec<(u64, u64)>,
    /// Progressive scrub (#137, player only): show the nearest ready state.
    preview: bool,
    /// The frame whose states the layers of the frame being drawn read instead of `n`
    /// (an approximate frame), and whether the last frame drawn was approximate.
    approx: Option<u64>,
    approximate: bool,
    /// A `preview_lookback` walk is in progress: its states are not canonical.
    lookback: bool,
    /// Seconds per catch-up step (wall time, smoothed), for the slow-seek hint.
    step_secs: f64,
    /// A warm sequence's seam (v = p, #138): its checkpoint is never evicted.
    seam: Option<u64>,
}

impl Fold {
    pub(super) fn new(gl: &glow::Context, ext: &std::collections::HashSet<String>) -> Fold {
        let (budget, budget_source) = detect_budget(gl, ext);
        Fold {
            key: 0,
            frame: None,
            rings: vec![],
            checkpoints: vec![],
            spare: vec![],
            plan: None,
            progress: None,
            decl: 0,
            reads_film: false,
            pics: vec![],
            stacks: vec![],
            budget,
            budget_source,
            slices: false,
            step_budget: STEP_BUDGET,
            checkpoints_on: true,
            n: 0,
            playhead: 0,
            parts: vec![],
            preview: false,
            approx: None,
            approximate: false,
            lookback: false,
            step_secs: 0.0,
            seam: None,
        }
    }

    /// A fresh fold (a comp's, #138) with `o`'s settings (budget, slices, step budget,
    /// checkpoints on); never progressive scrub, which only the film's fold does.
    pub(super) fn like(o: &Fold) -> Fold {
        Fold {
            key: 0,
            frame: None,
            rings: vec![],
            checkpoints: vec![],
            spare: vec![],
            plan: None,
            progress: None,
            decl: 0,
            reads_film: false,
            pics: vec![],
            stacks: vec![],
            budget: o.budget,
            budget_source: o.budget_source,
            slices: o.slices,
            step_budget: o.step_budget,
            checkpoints_on: o.checkpoints_on,
            n: 0,
            playhead: 0,
            parts: vec![],
            preview: false,
            approx: None,
            approximate: false,
            lookback: false,
            step_secs: 0.0,
            seam: None,
        }
    }

    pub(super) fn clear_progress(&mut self) {
        self.progress = None;
    }

    pub(super) fn ring_target(&self, b: usize, s: usize) -> &Target {
        &self.rings[b].as_ref().unwrap().slots[s].0
    }

    pub(super) fn ring_target_mut(&mut self, b: usize, s: usize) -> &mut Target {
        &mut self.rings[b].as_mut().unwrap().slots[s].0
    }

    /// Does the fold hold any GPU state (rings, checkpoints, spares, pictures)?
    pub(super) fn holds_targets(&self) -> bool {
        self.rings.iter().any(Option::is_some)
            || !self.checkpoints.is_empty()
            || !self.spare.is_empty()
            || !self.pics.is_empty()
            || !self.stacks.is_empty()
    }

    /// Forget every state (the cursor, a plan) and hand back every GL target to delete:
    /// its sequence has no buffers any more.
    pub(super) fn release(&mut self) -> Vec<Target> {
        self.frame = None;
        self.plan = None;
        self.progress = None;
        self.targets()
    }

    /// Every GL target the fold owns (to delete).
    pub(super) fn targets(&mut self) -> Vec<Target> {
        let mut v = vec![];
        for r in std::mem::take(&mut self.rings).into_iter().flatten() {
            v.extend(r.slots.into_iter().map(|(t, _)| t));
        }
        for c in std::mem::take(&mut self.checkpoints) {
            for s in c.rings {
                v.extend(s.slots.into_iter().map(|(_, t)| t));
            }
        }
        v.extend(std::mem::take(&mut self.spare).into_iter().map(|(_, t)| t));
        for (_, s) in std::mem::take(&mut self.pics) {
            v.extend(super::seq_targets(s));
        }
        for (_, s) in std::mem::take(&mut self.stacks) {
            v.extend(super::seq_targets(s));
        }
        v
    }
}

/// The checkpoint budget (#134, "The checkpoint budget"): `SHOTGLASS_BUFFER_CACHE_MB`;
/// else 1/8 of total video memory where the driver reports it (NVX / ATI meminfo),
/// clamped to 256 MB – 2 GB; else 512 MB. Asked once, at startup.
fn detect_budget(
    gl: &glow::Context,
    ext: &std::collections::HashSet<String>,
) -> (usize, &'static str) {
    if let Some(mb) = std::env::var("SHOTGLASS_BUFFER_CACHE_MB")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        return (mb << 20, "SHOTGLASS_BUFFER_CACHE_MB");
    }
    let kb = unsafe {
        if ext.contains("GL_NVX_gpu_memory_info") {
            // GPU_MEMORY_INFO_TOTAL_AVAILABLE_MEMORY_NVX, in KB.
            gl.get_parameter_i32(0x9048) as i64
        } else if ext.contains("GL_ATI_meminfo") {
            // TEXTURE_FREE_MEMORY_ATI: [0] is the pool's size in KB (ATI reports no total).
            let mut v = [0i32; 4];
            gl.get_parameter_i32_slice(0x87FC, &mut v);
            v[0] as i64
        } else {
            0
        }
    };
    if kb > 0 {
        let b = (kb as usize * 1024 / 8).clamp(256 << 20, 2 << 30);
        (b, "video memory")
    } else {
        (512 << 20, "default")
    }
}

/// A float (or RGBA8) colour target for buffer state, at exactly `w` × `h`.
fn new_target(gl: &glow::Context, (w, h): (i32, i32), format: BufferFormat) -> Target {
    let (internal, ty) = match format {
        BufferFormat::Rgba16f => (glow::RGBA16F, glow::HALF_FLOAT),
        BufferFormat::Rgba32f => (glow::RGBA32F, glow::FLOAT),
        BufferFormat::Rgba8 => (glow::RGBA8, glow::UNSIGNED_BYTE),
    };
    unsafe {
        let tex = gl.new_texture().expect("create texture");
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            internal as i32,
            w,
            h,
            0,
            glow::RGBA,
            ty,
            glow::PixelUnpackData::Slice(None),
        );
        for (p, v) in [
            (glow::TEXTURE_MIN_FILTER, glow::NEAREST),
            (glow::TEXTURE_MAG_FILTER, glow::NEAREST),
            (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
            (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
        ] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, p, v as i32);
        }
        gl.bind_texture(glow::TEXTURE_2D, None);
        let fbo = gl.new_framebuffer().expect("create framebuffer");
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(tex),
            0,
        );
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        Target {
            fbo,
            tex,
            alloc: (w, h),
            used: (w, h),
        }
    }
}

fn delete(gl: &glow::Context, t: Target) {
    unsafe {
        gl.free_framebuffer(t.fbo);
        gl.free_texture(t.tex);
    }
}

/// An exact GPU copy of `src` into `dst` (same size and format): no readback.
fn blit(gl: &glow::Context, src: &Target, dst: &Target) {
    let (w, h) = src.alloc;
    unsafe {
        gl.disable(glow::SCISSOR_TEST);
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(src.fbo));
        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(dst.fbo));
        gl.blit_framebuffer(
            0,
            0,
            w,
            h,
            0,
            0,
            w,
            h,
            glow::COLOR_BUFFER_BIT,
            glow::NEAREST,
        );
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
    }
}

// Warm loop films and comps (#138, `loop = "warm"`): the fold runs over *virtual*
// frames v in 0..2p, two periods of p frames: period −1 (v < p) is pre-roll from a cold
// start, period 0 (v ≥ p) is what's shown, real frame v − p. A buffer that spans the
// seam (active from frame 0 to the end) doesn't reset at v = p: frame 0 continues from
// period −1's last frame, so its state at real frame n is the fold from the start of
// the period before. Everything else (other buffers' spans and resets, the inputs a
// step reads at frame v mod p) repeats per period. Without warm, v is the real frame.
// The cursor, plans, rings, checkpoints and the playhead are all in virtual frames.

/// A warm sequence's period.
fn warm(movie: &Movie) -> Option<u64> {
    movie.loop_frames.filter(|_| movie.warm)
}

/// The real frame of virtual frame `v`.
fn real(movie: &Movie, v: u64) -> u64 {
    warm(movie).map_or(v, |p| v % p)
}

/// The virtual frame the fold shows for real frame `n`.
fn virt(movie: &Movie, n: u64) -> u64 {
    warm(movie).map_or(n, |p| p + n % p)
}

/// Does buffer `b` carry state over a warm seam?
fn spans_seam(b: &Buffer, p: u64) -> bool {
    b.start_frame == 0 && b.end_frame >= p
}

fn act(movie: &Movie, b: &Buffer, v: u64) -> bool {
    b.active(real(movie, v))
}

/// The last reset at or before virtual frame `v`.
fn reset_v(movie: &Movie, b: &Buffer, v: u64) -> Option<u64> {
    match warm(movie) {
        Some(p) if v >= p => match b.reset_at(v - p) {
            // The seam isn't a reset: the fold goes on from period −1's.
            Some(0) if spans_seam(b, p) => b.reset_at(p - 1),
            r => r.map(|r| r + p),
        },
        _ => b.reset_at(v),
    }
}

/// The first frame of `b`'s run of active frames containing (or after) `v`.
fn run_start(movie: &Movie, b: &Buffer, v: u64) -> u64 {
    match warm(movie) {
        Some(p) if v >= p && !spans_seam(b, p) => p + b.start_frame,
        _ => b.start_frame,
    }
}

/// The first frame ≥ `s` where `b` is active.
fn next_active(movie: &Movie, b: &Buffer, s: u64) -> Option<u64> {
    let mut runs = vec![(b.start_frame, b.end_frame)];
    if let Some(p) = warm(movie) {
        runs[0].1 = runs[0].1.min(p);
        runs.push((p + b.start_frame, p + b.end_frame.min(p)));
    }
    runs.into_iter().find_map(|(a, e)| {
        let f = s.max(a);
        (f < e).then_some(f)
    })
}

/// Where a pure history buffer must start so every state readers can ask for from
/// frame `from` on (`from - history` and later) is there.
fn pure_start(movie: &Movie, b: &Buffer, from: u64) -> u64 {
    from.saturating_sub(b.history as u64)
        .max(run_start(movie, b, from))
}

/// The earliest frame a fold reaching `n` from nothing must start at: every non-pure
/// buffer active there starts at its last reset, and so on back (a buffer active at
/// that frame needs its own reset, …).
fn cold_start(movie: &Movie, n: u64) -> u64 {
    let mut s = n;
    loop {
        let s2 = movie
            .buffers
            .iter()
            .filter(|b| !b.is_pure() && act(movie, b, s))
            .filter_map(|b| reset_v(movie, b, s))
            .fold(s, u64::min);
        if s2 == s {
            return s;
        }
        s = s2;
    }
}

/// The first frame a plan with these starts steps (n + 1 if nothing steps).
fn first_step(movie: &Movie, starts: &[u64], n: u64) -> u64 {
    movie
        .buffers
        .iter()
        .zip(starts)
        .filter_map(|(b, &s)| next_active(movie, b, s).filter(|&f| f <= n))
        .min()
        .unwrap_or(n + 1)
}

impl Renderer {
    /// The checkpoint budget in bytes and where it came from (a setting, the video
    /// memory, or the default). One budget for every fold (the film's and its comps').
    pub fn buffer_cache_budget(&self) -> (usize, &'static str) {
        (self.folds[0].budget, self.folds[0].budget_source)
    }

    /// Set the checkpoint budget (the player's setting; tests).
    pub fn set_buffer_cache_budget(&mut self, bytes: usize) {
        for f in &mut self.folds {
            f.budget = bytes;
            f.budget_source = "setting";
        }
        self.evict_checkpoints();
    }

    /// Catch up in budgeted slices even when rendering blocking (the player while
    /// paused or scrubbing): at most `set_buffer_step_budget` steps per render, which
    /// returns None until the fold reaches the frame. Headless commands never set it.
    pub fn set_catchup_slices(&mut self, on: bool) {
        for f in &mut self.folds {
            f.slices = on;
        }
    }

    #[allow(dead_code)]
    pub fn set_buffer_step_budget(&mut self, steps: usize) {
        for f in &mut self.folds {
            f.step_budget = steps.max(1);
        }
    }

    /// Take checkpoints while stepping (tests turn them off to force cold folds).
    pub fn set_buffer_checkpoints(&mut self, on: bool) {
        for f in &mut self.folds {
            f.checkpoints_on = on;
        }
        if !on {
            self.drop_checkpoints();
        }
    }

    /// Forget every buffer state (the cursors and the checkpoints): the next frame folds
    /// from the resets (tests: a cold `still`).
    pub fn forget_buffer_state(&mut self) {
        for f in &mut self.folds {
            f.frame = None;
            f.plan = None;
        }
        self.drop_checkpoints();
    }

    /// Progressive scrub (#137): while on (the player, while dragging), a frame whose
    /// buffers aren't at it yet draws at once from the nearest ready state (a
    /// checkpoint or the cursor before it, or a `preview_lookback` walk) and is marked
    /// approximate. Headless commands never set it: `still` and `export` are exact.
    /// The film's buffers only: a comp's (#138) always catch up, since a film buffer
    /// may read the comp and its state must stay canonical.
    pub fn set_scrub_preview(&mut self, on: bool) {
        self.folds[0].preview = on;
    }

    /// Was the last frame drawn approximate (progressive scrub)?
    pub fn buffer_approximate(&self) -> bool {
        self.folds[0].approximate
    }

    /// When the catch-up in progress would take more than about a second (from the
    /// measured step time), what would make seeks here faster.
    pub fn slow_seek_hint(&self) -> Option<String> {
        let secs = self
            .folds
            .iter()
            .filter_map(|f| {
                let (done, total) = f.progress?;
                Some(total.saturating_sub(done) as f64 * f.step_secs)
            })
            .fold(0.0, f64::max);
        (secs > 1.0).then(|| {
            format!(
                "buffer seeks here take ~{secs:.0} s: a smaller buffer size, \
                 cache = \"dense\", or reading a cheaper comp instead of @picture \
                 would shorten them"
            )
        })
    }

    /// Checkpoint bytes held by every fold.
    fn checkpoint_bytes(&self) -> usize {
        self.folds
            .iter()
            .flat_map(|f| &f.checkpoints)
            .map(|c| c.bytes)
            .sum()
    }

    /// Background fill (#137): while paused, walk the film's fold forward from the
    /// cursor taking the checkpoints the player hasn't got yet, a few steps per call,
    /// never blocking (a step whose program or texture isn't ready waits). The player
    /// calls it only when nothing else is pending (compiles, uploads, a catch-up). Stops
    /// at the end of the film or when the budget is full. True if it stepped (call
    /// again). Comps' folds (#138) fill as the film's steps show them.
    pub fn fill_checkpoints(&mut self, movie: &Movie, w: i32, h: i32) -> bool {
        let r = self.fill_checkpoints_inner(movie, w, h);
        glstate::audit(&self.gl, "fill_checkpoints");
        r
    }

    fn fill_checkpoints_inner(&mut self, movie: &Movie, w: i32, h: i32) -> bool {
        let out = (w.max(1), h.max(1));
        let q = 0;
        self.fold_out = out;
        if movie.buffers.is_empty()
            || !self.folds[q].checkpoints_on
            || self.folds[q].preview
            || self.folds[q].approximate
            || self.folds[q].key != self.fold_key(movie, 0, out).0
        {
            return false;
        }
        let used = self.checkpoint_bytes();
        let f = &self.folds[q];
        let Some(c) = f.frame else { return false };
        let each = f.checkpoints.iter().map(|c| c.bytes).max().unwrap_or(0);
        if used + each > f.budget {
            return false;
        }
        let end = warm(movie).map_or(movie.frame_count(), |p| 2 * p);
        let target = match &f.plan {
            Some(p) if p.key == f.key && p.target > f.playhead => p.target,
            _ => {
                if !self.cursor_valid(movie, q, c) {
                    return false;
                }
                let mut g = c + 1;
                loop {
                    if g >= end {
                        return false;
                    }
                    let k = checkpoint_interval(movie, g);
                    if g.is_multiple_of(k)
                        && movie
                            .buffers
                            .iter()
                            .any(|b| !b.is_pure() && act(movie, b, g))
                        && !f.checkpoints.iter().any(|x| x.key == f.key && x.frame == g)
                    {
                        break g;
                    }
                    g += 1;
                }
            }
        };
        // The fill steps elsewhere: every fold's shown frame and playhead (a comp's
        // moves when a film step nests it, #138) stay where the player is.
        let kept: Vec<(u64, u64)> = self.folds.iter().map(|f| (f.n, f.playhead)).collect();
        let steps = self.buffer_steps;
        self.fold_out = out;
        self.advance_buffers(movie, q, target, out, false);
        for (f, (n, playhead)) in self.folds.iter_mut().zip(kept) {
            f.n = n;
            f.playhead = playhead;
            f.progress = None;
        }
        self.fill_steps += self.buffer_steps - steps;
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.use_program(None);
            self.gl.bind_vertex_array(None);
        }
        self.buffer_steps > steps
    }

    /// (frames done, frames to do) of a catch-up in progress (the film's, else a comp's).
    pub fn catchup_progress(&self) -> Option<(u64, u64)> {
        self.folds.iter().find_map(|f| f.progress)
    }

    /// Checkpoints held (every fold), and their GPU bytes.
    pub fn checkpoint_stats(&self) -> (usize, usize) {
        (
            self.folds.iter().map(|f| f.checkpoints.len()).sum(),
            self.checkpoint_bytes(),
        )
    }

    fn drop_checkpoints(&mut self) {
        for q in 0..self.folds.len() {
            for c in std::mem::take(&mut self.folds[q].checkpoints) {
                self.recycle(q, c);
            }
        }
    }

    fn recycle(&mut self, q: usize, c: Checkpoint) {
        for s in c.rings {
            for (_, t) in s.slots {
                self.folds[q].spare.push((s.format, t));
            }
        }
        // A few spares are enough to make filling and evicting cheap.
        while self.folds[q].spare.len() > 16 {
            let (_, t) = self.folds[q].spare.remove(0);
            delete(&self.gl, t);
        }
    }

    /// The keys a buffer's fold uses for wanted item `i`: the current version if its
    /// programs are ready, else the last working one if its are, else the current.
    fn fold_pick(&self, i: usize) -> Option<ShotKeys> {
        let k = *self.wanted.get(i)?;
        let ready = |k: &ShotKeys| {
            self.prog(k.scene).is_some() && k.trans.is_none_or(|t| self.prog(t).is_some())
        };
        if ready(&k) {
            return Some(k);
        }
        match self.wanted_ids.get(i).and_then(|id| self.fallback.get(id)) {
            Some(f) if ready(f) => Some(*f),
            _ => Some(k),
        }
    }

    /// The fold key (#134, "Invalidation"): everything the buffers' folds read, as
    /// parts with the first film frame that reads each (#137: an edit invalidates only
    /// states from that frame on). The declarations (spans, resets, the film's timing)
    /// and the buffer sizes are parts read from frame 0.
    fn fold_key(&self, film: &Movie, q: usize, out: (i32, i32)) -> (u64, Vec<(u64, u64)>) {
        let movie = super::seq_movie(film, q);
        let item = |k: ShotKeys| {
            let mut s = format!("{}:{:?}", k.scene, k.trans);
            for c in k.channels.iter().flatten() {
                s += &format!(",{}/{:?}", c.tex, c.footage);
            }
            hash(&s)
        };
        let mut parts = vec![(self.folds[q].decl, 0)];
        let mut sizes: String = movie
            .buffers
            .iter()
            .map(|b| format!("{:?}", b.size.at(out)))
            .collect();
        if self.folds[q].reads_film {
            // The comps it can read (#138): their buffers' sizes follow the film output
            // when a comp has no `size`, so a resize changes what this fold reads.
            for (c, comp) in film.comps.iter().enumerate() {
                let cout = super::seq_out(film, c + 1, self.fold_out);
                for b in &comp.movie.buffers {
                    sizes += &format!("{c}:{:?}", b.size.at(cout));
                }
            }
        }
        parts.push((hash(&sizes), 0));
        let own_bufs = self.buf_base[q]..self.buf_base[q] + movie.buffers.len();
        for (b, buf) in movie.buffers.iter().enumerate() {
            if let Some(k) = self.fold_pick(own_bufs.start + b) {
                parts.push((item(k), buf.start_frame));
            }
        }
        if self.folds[q].reads_film {
            // Its sequence's shots from their start, and its layers up to the highest
            // `after` a buffer reads the stack at (#138) from theirs; everything any comp
            // draws (its items and its buffers, #138) from frame 0. Not its other layers
            // (they draw after buffers step) or its own buffers (above).
            let base = self.seq_base[q];
            let shots = base..base + movie.shots.len();
            let reach = crate::movie::stack_reach(movie).map_or(0, |r| r + 1);
            let stack = shots.end..shots.end + reach;
            let layers = stack.end..shots.end + movie.layers.len();
            let film = super::seq_movie(film, 0);
            // A comp's fold reads nothing of the film's.
            let film_items = 0..film.shots.len() + film.layers.len();
            let film_bufs = self.buf_base[0]..self.buf_base[0] + film.buffers.len();
            for i in (0..self.wanted.len()).filter(|i| {
                !layers.contains(i)
                    && !own_bufs.contains(i)
                    && (q == 0 || (!film_items.contains(i) && !film_bufs.contains(i)))
            }) {
                if let Some(k) = self.fold_pick(i) {
                    // A shot is read from its start (its incoming transition's, if
                    // any; a frame early to be safe); comp items whenever a comp is:
                    // from frame 0.
                    let first = if shots.contains(&i) {
                        let s = &movie.shots[i - base];
                        let lead = s.transition_in.as_ref().map_or(0.0, |t| t.duration);
                        (movie.frame_at(s.start - lead).max(0) as u64).saturating_sub(1)
                    } else if stack.contains(&i) {
                        let l = &movie.layers[i - stack.start];
                        (movie.frame_at(l.start).max(0) as u64).saturating_sub(1)
                    } else {
                        0
                    };
                    parts.push((item(k), first));
                }
            }
        }
        let key = hash(&format!("{parts:?}"));
        (key, parts)
    }

    /// The first frame whose state an edit from parts `old` to `new` changes.
    fn first_changed(old: &[(u64, u64)], new: &[(u64, u64)]) -> u64 {
        if old.len() != new.len() {
            return 0;
        }
        old.iter()
            .zip(new)
            .filter(|(a, b)| a.0 != b.0)
            .map(|(a, b)| a.1.min(b.1))
            .min()
            .unwrap_or(u64::MAX)
    }

    /// `advance_buffers` for the frame shown (the playhead).
    pub(super) fn advance_to(
        &mut self,
        film: &Movie,
        q: usize,
        n: u64,
        out: (i32, i32),
        blocking: bool,
    ) -> bool {
        // A warm loop's frame n is virtual frame p + n (#138).
        let v = virt(super::seq_movie(film, q), n);
        self.folds[q].playhead = v;
        self.advance_buffers(film, q, v, out, blocking)
    }

    /// Bring every buffer to (virtual) frame `n` (#134) before the frame is planned. False
    /// while that isn't done: a stall (something not ready without `blocking`), or a
    /// catch-up sliced over several renders.
    pub(super) fn advance_buffers(
        &mut self,
        film: &Movie,
        q: usize,
        n: u64,
        out: (i32, i32),
        blocking: bool,
    ) -> bool {
        let movie = super::seq_movie(film, q);
        self.folds[q].seam = warm(movie);
        // A comp's buffers (#138) are sized against the comp's own size, if it has one.
        let out = super::seq_out(film, q, out);
        self.folds[q].n = n;
        self.folds[q].approx = None;
        self.folds[q].approximate = false;
        if movie.buffers.is_empty() {
            return true;
        }
        let (key, parts) = self.fold_key(film, q, out);
        if key != self.folds[q].key {
            // Per-frame invalidation (#137): states before the first frame that reads
            // the change stay valid under the new key; later ones go.
            let f = Self::first_changed(&self.folds[q].parts, &parts);
            let old = self.folds[q].key;
            for c in &mut self.folds[q].checkpoints {
                if c.key == old && c.frame < f {
                    c.key = key;
                }
            }
            self.folds[q].key = key;
            if self.folds[q].frame.is_none_or(|c| c >= f) {
                self.folds[q].frame = None;
            }
            self.folds[q].plan = None;
        }
        self.folds[q].parts = parts;
        if self.folds[q].rings.len() != movie.buffers.len() {
            let old = std::mem::take(&mut self.folds[q].rings);
            for r in old.into_iter().flatten() {
                for (t, _) in r.slots {
                    delete(&self.gl, t);
                }
            }
            self.folds[q]
                .rings
                .resize_with(movie.buffers.len(), || None);
            self.folds[q].frame = None;
            self.folds[q].plan = None;
        }
        if self.folds[q].frame == Some(n)
            && self.folds[q].plan.is_none()
            && self.cursor_valid(movie, q, n)
        {
            self.folds[q].progress = None;
            return true;
        }
        if self.folds[q].preview
            && let Some(ok) = self.lookback(film, q, n, out, blocking)
        {
            return ok;
        }
        let plan = match self.folds[q].plan.take() {
            Some(p) if p.key == key && p.target == n => p,
            _ => {
                let p = self.make_plan(movie, q, n, out);
                if blocking && !self.folds[q].slices && p.next < p.target {
                    self.blocking_catchups += 1;
                }
                p
            }
        };
        if self.folds[q].preview
            && let Some(c) = self.folds[q].frame.filter(|&c| {
                c < n && n - c > self.folds[q].step_budget as u64 && plan.next == c + 1
            })
        {
            // Progressive scrub: draw now from the state at c; refine when it stops.
            self.folds[q].approx = Some(c);
            self.folds[q].approximate = true;
            self.folds[q].plan = Some(plan);
            self.folds[q].progress = None;
            return true;
        }
        let limit = (!blocking || self.folds[q].slices).then_some(self.folds[q].step_budget as u64);
        let mut m = plan.next;
        let mut done = 0u64;
        let started = std::time::Instant::now();
        while m <= n {
            if limit.is_some_and(|l| done >= l) {
                break;
            }
            if !self.step_frame(film, q, m, &plan.starts, out, blocking) {
                break;
            }
            self.folds[q].frame = Some(m);
            m += 1;
            done += 1;
        }
        if done > 0 {
            let per = started.elapsed().as_secs_f64() / done as f64;
            self.folds[q].step_secs = if self.folds[q].step_secs == 0.0 {
                per
            } else {
                0.8 * self.folds[q].step_secs + 0.2 * per
            };
        }
        if m > n {
            self.folds[q].progress = None;
            self.free_idle_rings(movie, q, n, out);
            true
        } else {
            self.folds[q].progress = Some((m - plan.first, (n + 1).saturating_sub(plan.first)));
            self.folds[q].plan = Some(Plan { next: m, ..plan });
            false
        }
    }

    /// `preview_lookback` (#137, progressive scrub only): when every non-pure buffer
    /// active at `n` has one and no known state is within it, step each from black
    /// over only its last lookback frames, all at once, and draw approximate. The
    /// states are never checkpointed and the cursor is dropped, so nothing canonical
    /// continues from them. None: doesn't apply.
    fn lookback(
        &mut self,
        film: &Movie,
        q: usize,
        n: u64,
        out: (i32, i32),
        blocking: bool,
    ) -> Option<bool> {
        let movie = super::seq_movie(film, q);
        let live: Vec<&Buffer> = movie
            .buffers
            .iter()
            .filter(|b| !b.is_pure() && act(movie, b, n))
            .collect();
        if live.is_empty() || live.iter().any(|b| b.preview_lookback.is_none()) {
            return None;
        }
        let lb = live.iter().filter_map(|b| b.preview_lookback).max()?;
        let key = self.folds[q].key;
        let cursor = self.folds[q]
            .frame
            .filter(|&c| c <= n && self.cursor_valid(movie, q, c));
        let known = self.folds[q]
            .checkpoints
            .iter()
            .filter(|c| c.key == key && c.frame <= n)
            .map(|c| c.frame)
            .chain(cursor)
            .max();
        if known.is_some_and(|k| n - k <= lb) || n - cold_start(movie, n) <= lb {
            return None;
        }
        let starts: Vec<u64> = movie
            .buffers
            .iter()
            .map(|b| match b.preview_lookback {
                _ if b.is_pure() => pure_start(movie, b, n),
                Some(l) => n.saturating_sub(l).max(reset_v(movie, b, n).unwrap_or(0)),
                None => n + 1,
            })
            .collect();
        let clear = |fold: &mut Fold| {
            for r in fold.rings.iter_mut().flatten() {
                for s in &mut r.slots {
                    s.1 = None;
                }
                r.last = None;
            }
            fold.frame = None;
            fold.plan = None;
        };
        clear(&mut self.folds[q]);
        self.folds[q].lookback = true;
        let mut ok = true;
        for m in first_step(movie, &starts, n)..=n {
            if !self.step_frame(film, q, m, &starts, out, blocking) {
                ok = false;
                break;
            }
        }
        self.folds[q].lookback = false;
        if ok {
            self.folds[q].approx = Some(n);
            self.folds[q].approximate = true;
            self.folds[q].progress = None;
            self.lookback_walks += 1;
            // The rings hold approximate states: leave them for this frame's layers,
            // but nothing may step on from them.
            self.folds[q].frame = None;
        } else {
            clear(&mut self.folds[q]);
        }
        Some(ok)
    }

    /// Can the fold continue from the cursor at `c`? Every buffer active there was
    /// stepped to `c` with all the states it (and its readers) need.
    fn cursor_valid(&self, movie: &Movie, q: usize, c: u64) -> bool {
        movie.buffers.iter().enumerate().all(|(i, b)| {
            if !act(movie, b, c) {
                return true;
            }
            let Some(r) = self.folds[q].rings.get(i).and_then(Option::as_ref) else {
                return false;
            };
            r.last == Some(c)
                && (Some(r.since) == reset_v(movie, b, c) || r.since + b.history as u64 <= c)
        })
    }

    /// The cheapest way to frame `n`: from the cursor, the nearest checkpoint, or the
    /// resets. A checkpoint start restores its rings now (GPU copies).
    fn make_plan(&mut self, movie: &Movie, q: usize, n: u64, out: (i32, i32)) -> Plan {
        let bufs = &movie.buffers;
        let key = self.folds[q].key;
        let cold: Vec<u64> = {
            let s = cold_start(movie, n);
            bufs.iter()
                .map(|b| {
                    if b.is_pure() {
                        pure_start(movie, b, s)
                    } else {
                        s
                    }
                })
                .collect()
        };
        let mut best = (first_step(movie, &cold, n), cold, None::<usize>, false);
        if let Some(c) = self.folds[q].frame.filter(|&c| c < n)
            && self.cursor_valid(movie, q, c)
        {
            let starts = vec![c + 1; bufs.len()];
            let first = first_step(movie, &starts, n);
            if first >= best.0 {
                best = (first, starts, None, true);
            }
        }
        let ck = self.folds[q]
            .checkpoints
            .iter()
            .enumerate()
            .filter(|(_, c)| c.key == key && c.frame <= n)
            .max_by_key(|(_, c)| c.frame)
            .map(|(i, c)| (i, c.frame));
        if let Some((i, c)) = ck {
            let starts: Vec<u64> = bufs
                .iter()
                .map(|b| {
                    if b.is_pure() {
                        pure_start(movie, b, c + 1)
                    } else {
                        c + 1
                    }
                })
                .collect();
            let first = first_step(movie, &starts, n);
            if first > best.0 {
                best = (first, starts, Some(i), false);
            }
        }
        let (first, starts, ck, cursor) = best;
        if !cursor {
            for r in self.folds[q].rings.iter_mut().flatten() {
                for s in &mut r.slots {
                    s.1 = None;
                }
                r.last = None;
            }
            self.folds[q].frame = None;
            if let Some(i) = ck {
                self.restore(movie, q, i, out);
                // A checkpoint at n itself leaves nothing to step: the cursor is there.
                self.folds[q].frame = Some(self.folds[q].checkpoints[i].frame);
            }
        }
        Plan {
            key,
            target: n,
            next: first,
            first,
            starts,
        }
    }

    /// Copy checkpoint `i` into the rings.
    #[allow(clippy::type_complexity)]
    fn restore(&mut self, movie: &Movie, q: usize, i: usize, out: (i32, i32)) {
        let gl = self.gl.clone();
        let saved: Vec<(usize, u64, Vec<(i64, glow::Framebuffer)>, u64)> = {
            let c = &self.folds[q].checkpoints[i];
            c.rings
                .iter()
                .map(|s| {
                    (
                        s.buffer,
                        s.reset,
                        s.slots.iter().map(|(f, t)| (*f, t.fbo)).collect(),
                        c.frame,
                    )
                })
                .collect()
        };
        for (b, reset, slots, frame) in saved {
            self.ensure_ring(movie, q, b, out);
            let ring = self.folds[q].rings[b].as_mut().unwrap();
            for (k, (f, _)) in slots.iter().enumerate() {
                ring.slots[k].1 = Some(*f);
            }
            ring.reset = reset;
            ring.since = reset;
            ring.last = Some(frame);
            let ring = self.folds[q].rings[b].as_ref().unwrap();
            let c = &self.folds[q].checkpoints[i];
            let s = c.rings.iter().find(|s| s.buffer == b).unwrap();
            for (k, (_, t)) in s.slots.iter().enumerate() {
                blit(&gl, t, &ring.slots[k].0);
            }
        }
        self.buffer_restores += 1;
    }

    /// Make buffer `b`'s ring exist at its size, format and length.
    fn ensure_ring(&mut self, movie: &Movie, q: usize, b: usize, out: (i32, i32)) {
        let buf = &movie.buffers[b];
        let size = buf.size.at(out);
        let len = buf.history as usize + 1;
        let ok = self.folds[q].rings[b]
            .as_ref()
            .is_some_and(|r| r.size == size && r.format == buf.format && r.slots.len() == len);
        if ok {
            return;
        }
        if let Some(r) = self.folds[q].rings[b].take() {
            for (t, _) in r.slots {
                delete(&self.gl, t);
            }
        }
        let slots = (0..len)
            .map(|_| (new_target(&self.gl, size, buf.format), None))
            .collect();
        self.folds[q].rings[b] = Some(Ring {
            size,
            format: buf.format,
            slots,
            reset: buf.start_frame,
            since: buf.start_frame,
            last: None,
        });
    }

    /// Rings of buffers not active at `n` hold nothing anyone reads: free them. So do
    /// the scratch sets for the pictures and composites steps draw (`pics`, `stacks`,
    /// #138) that no buffer active at `n` draws at its current size: a resize, or an
    /// edit to a buffer's size or `after`, would otherwise leave them allocated.
    fn free_idle_rings(&mut self, movie: &Movie, q: usize, n: u64, out: (i32, i32)) {
        for (b, buf) in movie.buffers.iter().enumerate() {
            if !act(movie, buf, n)
                && let Some(r) = self.folds[q].rings[b].take()
            {
                for (t, _) in r.slots {
                    delete(&self.gl, t);
                }
            }
        }
        let live = || movie.buffers.iter().filter(|b| act(movie, b, n));
        let pics: Vec<(i32, i32)> = live()
            .filter(|b| {
                b.channels
                    .iter()
                    .flatten()
                    .any(|c| c.source == Source::Picture)
            })
            .map(|b| b.size.at(out))
            .collect();
        let stacks: Vec<StackKey> = live()
            .filter(|b| b.reads_stack())
            .filter_map(|b| b.after.map(|a| (a, b.size.at(out))))
            .collect();
        let fold = &mut self.folds[q];
        let mut dead = vec![];
        for (k, t) in std::mem::take(&mut fold.pics) {
            if pics.contains(&k) {
                fold.pics.push((k, t));
            } else {
                dead.extend(super::seq_targets(t));
            }
        }
        for (k, t) in std::mem::take(&mut fold.stacks) {
            if stacks.contains(&k) {
                fold.stacks.push((k, t));
            } else {
                dead.extend(super::seq_targets(t));
            }
        }
        for t in dead {
            delete(&self.gl, t);
        }
    }

    /// Scratch sets the folds keep for the pictures and composites buffer steps draw
    /// (tests: they don't pile up).
    pub fn step_scratch_sets(&self) -> usize {
        self.folds
            .iter()
            .map(|f| f.pics.len() + f.stacks.len())
            .sum()
    }

    /// Buffer `b`'s state `back` frames before film frame `m` (0: frame m's, after its
    /// step), as a pass: black (None) while it isn't active or the state isn't there.
    /// Before the reset, past states read the `init` state (frame `reset - 1`).
    pub(super) fn buf_pass(
        &self,
        movie: &Movie,
        q: usize,
        b: usize,
        back: u32,
        m: u64,
    ) -> Option<(glow::Texture, usize, usize)> {
        if !act(movie, movie.buffers.get(b)?, m) {
            return None;
        }
        // An approximate frame reads the states of the frame the rings are at.
        let m = match self.folds[q].approx {
            Some(c) if m == self.folds[q].n => c,
            _ => m,
        };
        let r = self.folds[q].rings.get(b)?.as_ref()?;
        let f = if back == 0 {
            m as i64
        } else {
            (m as i64 - back as i64).max(r.reset as i64 - 1)
        };
        let (t, _) = r.slots.iter().find(|(_, x)| *x == Some(f))?;
        Some((t.tex, r.size.0 as usize, r.size.1 as usize))
    }

    /// The buffer passes a channel list reads on frame `m`: (pass key, texture).
    pub(super) fn buf_passes(
        &self,
        movie: &Movie,
        q: usize,
        channels: &[Option<crate::movie::Channel>],
        m: u64,
    ) -> Vec<(u64, (glow::Texture, usize, usize))> {
        channels
            .iter()
            .flatten()
            .filter_map(|c| match c.source {
                Source::Buffer { index, back, .. } => self
                    .buf_pass(movie, q, index, back, m)
                    .map(|t| (buf_key(index, back), t)),
                _ => None,
            })
            .collect()
    }

    /// Step every buffer due on film frame `m` (active, and `m ≥ starts[b]`), in step
    /// order (file order, buffers with `after` after the layer they name, #138): decide
    /// everything first (programs, textures, the picture, composites and comps they
    /// read), then draw. False, with nothing drawn, if something isn't ready.
    fn step_frame(
        &mut self,
        film: &Movie,
        q: usize,
        m: u64,
        starts: &[u64],
        out: (i32, i32),
        blocking: bool,
    ) -> bool {
        let movie = super::seq_movie(film, q);
        let bufs = &movie.buffers;
        let mut todo: Vec<usize> = (0..bufs.len())
            .filter(|&b| act(movie, &bufs[b], m) && m >= starts[b])
            .collect();
        todo.sort_by_key(|&b| (bufs[b].rank(), b));
        if todo.is_empty() {
            return true;
        }
        let Some(copy) = self.copy else { return false };
        let mut keys = vec![];
        for &b in &todo {
            match self.usable(self.buf_base[q] + b, blocking, real(movie, m)) {
                Some(k) => keys.push(k),
                None => return false,
            }
        }
        // Inputs are read at the real frame (a warm sequence's pre-roll repeats the
        // period); state, resets and iFrame count virtual frames.
        let rm = real(movie, m);
        let t = movie.frame_time(rm);
        let at = movie.at(t);
        let mut insts = vec![];
        let reads_picture = |b: usize| {
            bufs[b]
                .channels
                .iter()
                .flatten()
                .any(|c| c.source == Source::Picture)
        };
        let mut pics: Vec<((i32, i32), SeqPlan)> = vec![];
        for &b in &todo {
            let size = bufs[b].size.at(out);
            if reads_picture(b) && !pics.iter().any(|(s, _)| *s == size) {
                // The sequence's picture track without its layers. Nested folds (a comp
                // stepped while this plans, #138) set and restore it in turn.
                let was = self.plan_layers.replace((q, None));
                let p = self.plan_seq(
                    film,
                    q,
                    at,
                    (t, rm.min(i32::MAX as u64) as i32, rm),
                    size,
                    size,
                    blocking,
                    &mut insts,
                );
                self.plan_layers = was;
                match p {
                    Some(p) => pics.push((size, p)),
                    None => return false,
                }
            }
        }
        // `after` buffers reading the layer stack (#138): the composite up to their
        // layer at their size, scale 1, whatever the player hides; its layers read
        // buffer states at m (those stepping before the layer).
        let mut stacks: Vec<(StackKey, SeqPlan)> = vec![];
        for &b in &todo {
            let size = bufs[b].size.at(out);
            let Some(a) = bufs[b].after.filter(|_| bufs[b].reads_stack()) else {
                continue;
            };
            if stacks.iter().any(|(k, _)| *k == (a, size)) {
                continue;
            }
            let was = self.plan_layers.replace((q, Some(a)));
            let p = self.plan_seq(
                film,
                q,
                at,
                (t, rm.min(i32::MAX as u64) as i32, rm),
                size,
                size,
                blocking,
                &mut insts,
            );
            self.plan_layers = was;
            match p {
                Some(mut p) => {
                    p.step = Some(m);
                    stacks.push(((a, size), p));
                }
                None => return false,
            }
        }
        let mut chans = vec![];
        for &b in &todo {
            let buf = &bufs[b];
            let local = t - buf.start;
            match self.plan_chans(
                film,
                &buf.channels,
                local,
                buf.size.at(out),
                1.0,
                blocking,
                &mut insts,
            ) {
                Some(c) => chans.push(c),
                None => return false,
            }
        }
        // GL work: comp instances, the pictures, then each buffer. Never in strips: the
        // strip count follows the GPU timer, and the state mustn't depend on timing.
        let tiles = std::mem::replace(&mut self.tiles, 1);
        let cur = self.cur;
        if self.inst.len() < insts.len() {
            self.inst.resize_with(insts.len(), || None);
        }
        for (i, (k, p)) in insts.iter().enumerate() {
            super::ensure_target(&self.gl, &mut self.inst[i], k.dest.0, k.dest.1);
            self.draw_seq(film, p, copy, Some(Which::Inst(i)), k.dest);
        }
        let mut pic_tex: Vec<((i32, i32), glow::Texture)> = vec![];
        for (size, p) in &pics {
            let j = match self.folds[q].pics.iter().position(|(s, _)| s == size) {
                Some(j) => j,
                None => {
                    self.folds[q].pics.push((*size, SeqTargets::default()));
                    self.folds[q].pics.len() - 1
                }
            };
            std::mem::swap(&mut self.seqs[q], &mut self.folds[q].pics[j].1);
            super::ensure_target(&self.gl, &mut self.seqs[q].picture, size.0, size.1);
            self.draw_seq(film, p, copy, Some(Which::Picture), *size);
            pic_tex.push((*size, self.seqs[q].picture.as_ref().unwrap().tex));
            std::mem::swap(&mut self.seqs[q], &mut self.folds[q].pics[j].1);
        }
        self.cur = cur;
        // Resets first, for every buffer due: an earlier buffer reading a later one's
        // previous state on that buffer's reset frame then reads black, whichever path
        // reached the frame.
        let mut init = vec![false; todo.len()];
        for (i, &b) in todo.iter().enumerate() {
            let buf = &bufs[b];
            let fresh = reset_v(movie, buf, m) == Some(m)
                || self.folds[q].rings[b]
                    .as_ref()
                    .is_none_or(|r| m == 0 || r.last != Some(m - 1));
            if fresh {
                self.ensure_ring(movie, q, b, out);
                let r = self.folds[q].rings[b].as_mut().unwrap();
                for s in &mut r.slots {
                    s.1 = None;
                }
                r.reset = reset_v(movie, buf, m).unwrap_or(m);
                r.since = m;
                r.last = None;
                init[i] = r.reset == m;
            }
        }
        let mut drawn: Vec<(StackKey, Passes)> = vec![];
        for (i, &b) in todo.iter().enumerate() {
            let buf = &bufs[b];
            let size = buf.size.at(out);
            let local = t - buf.start;
            // Passes besides buffer states and comps: the picture track, and with
            // `after` the composite (`@input`) and held layers (`@<layer>`), drawn
            // now, once every buffer stepping before this one has stepped.
            let mut extra = vec![];
            if let Some((_, tex)) = pic_tex.iter().find(|(s, _)| *s == size) {
                extra.push((PICTURE_TEX, (*tex, size.0 as usize, size.1 as usize)));
            }
            if let Some(a) = buf.after.filter(|_| buf.reads_stack()) {
                let key = (a, size);
                if !drawn.iter().any(|(k, _)| *k == key) {
                    let p = &stacks.iter().find(|(k, _)| *k == key).unwrap().1;
                    let j = match self.folds[q].stacks.iter().position(|(k, _)| *k == key) {
                        Some(j) => j,
                        None => {
                            self.folds[q].stacks.push((key, SeqTargets::default()));
                            self.folds[q].stacks.len() - 1
                        }
                    };
                    std::mem::swap(&mut self.seqs[q], &mut self.folds[q].stacks[j].1);
                    super::ensure_target(&self.gl, &mut self.seqs[q].input, size.0, size.1);
                    let mut v = self.draw_seq(film, p, copy, Some(Which::Input), size);
                    let input = self.seqs[q].input.as_ref().unwrap().tex;
                    v.push((INPUT_TEX, (input, size.0 as usize, size.1 as usize)));
                    std::mem::swap(&mut self.seqs[q], &mut self.folds[q].stacks[j].1);
                    self.cur = cur;
                    drawn.push((key, v));
                }
                extra.extend(
                    drawn
                        .iter()
                        .find(|(k, _)| *k == key)
                        .unwrap()
                        .1
                        .iter()
                        .copied(),
                );
            }
            let mut passes = self.buf_passes(movie, q, &buf.channels, m);
            passes.extend(extra.iter().copied());
            passes.extend(self.inst_passes(&chans[i]));
            let reset = self.folds[q].rings[b].as_ref().unwrap().reset;
            let info = super::FrameInfo {
                t,
                dt: movie.fps_den as f32 / movie.fps_num as f32,
                frame: (m - reset).min(i32::MAX as u64) as i32,
                scale: 1.0,
            };
            let item = super::Item {
                start: buf.start,
                duration: buf.duration(),
                itime: local,
                uniforms: &buf.uniforms,
                opacity: None,
                passes,
            };
            if init[i] {
                // The state before the reset: the init pass, or black.
                let slot = 0;
                self.folds[q].rings[b].as_mut().unwrap().slots[slot].1 = Some(m as i64 - 1);
                match keys[i].trans {
                    Some(k) => {
                        let mut ik = keys[i];
                        ik.scene = k;
                        ik.trans = None;
                        let ipasses: Vec<_> = item
                            .passes
                            .iter()
                            .copied()
                            .filter(|(k, _)| *k != buf_key(b, 1))
                            .collect();
                        let iitem = super::Item {
                            passes: ipasses,
                            ..item_clone(&item)
                        };
                        self.draw_scene(
                            ik,
                            &iitem,
                            local,
                            Which::Ring(q, b, slot),
                            size.0,
                            size.1,
                            &info,
                            movie,
                        );
                    }
                    None => unsafe {
                        let fbo = self.folds[q].rings[b].as_ref().unwrap().slots[slot].0.fbo;
                        self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                        self.gl.viewport(0, 0, size.0, size.1);
                        self.gl.disable(glow::SCISSOR_TEST);
                        self.gl.clear_color(0.0, 0.0, 0.0, 0.0);
                        self.gl.clear(glow::COLOR_BUFFER_BIT);
                    },
                }
            }
            // Re-read the passes: the init state is a new slot.
            let mut passes = self.buf_passes(movie, q, &buf.channels, m);
            passes.extend(extra);
            passes.extend(self.inst_passes(&chans[i]));
            let item = super::Item { passes, ..item };
            let slot = self.free_slot(q, b, m, buf.history);
            self.draw_scene(
                keys[i],
                &item,
                local,
                Which::Ring(q, b, slot),
                size.0,
                size.1,
                &info,
                movie,
            );
            let r = self.folds[q].rings[b].as_mut().unwrap();
            r.slots[slot].1 = Some(m as i64);
            r.last = Some(m);
        }
        self.tiles = tiles;
        self.buffer_steps += todo.len();
        self.maybe_checkpoint(movie, q, m);
        true
    }

    /// A slot to draw frame `m` into: not one holding a state frame m still reads
    /// (`m - history` .. `m - 1`, or the init state at `reset - 1`).
    fn free_slot(&self, q: usize, b: usize, m: u64, history: u32) -> usize {
        let r = self.folds[q].rings[b].as_ref().unwrap();
        let lo = (m as i64 - history as i64).max(r.reset as i64 - 1);
        let hi = m as i64 - 1;
        r.slots
            .iter()
            .position(|(_, f)| f.is_none_or(|f| f < lo || f > hi))
            .expect("a ring has history + 1 slots")
    }

    /// Every `fps` frames (one second), copy the non-pure buffers' rings into a
    /// checkpoint (GPU copies), then keep the checkpoints under the budget.
    fn maybe_checkpoint(&mut self, movie: &Movie, q: usize, m: u64) {
        if !self.folds[q].checkpoints_on || self.folds[q].lookback {
            return;
        }
        // A warm sequence always keeps the state at its seam (v = p, real frame 0), so
        // each wrap of the loop restores it instead of stepping a period again.
        let seam = warm(movie) == Some(m);
        if !m.is_multiple_of(checkpoint_interval(movie, m)) && !seam {
            return;
        }
        let key = self.folds[q].key;
        if self.folds[q]
            .checkpoints
            .iter()
            .any(|c| c.key == key && c.frame == m)
        {
            return;
        }
        let live: Vec<usize> = (0..movie.buffers.len())
            .filter(|&b| !movie.buffers[b].is_pure() && act(movie, &movie.buffers[b], m))
            .collect();
        // A pure buffer can step ahead of the others (it starts `history` back): their
        // rings then hold another frame's state, or none. Only a whole fold at m is saved.
        if live.is_empty()
            || live.iter().any(|&b| {
                self.folds[q].rings[b]
                    .as_ref()
                    .is_none_or(|r| r.last != Some(m))
            })
        {
            return;
        }
        let bytes: usize = live
            .iter()
            .map(|&b| {
                let r = self.folds[q].rings[b].as_ref().unwrap();
                let n = self.saved_frames(q, b, m, movie.buffers[b].history).len();
                n * r.size.0 as usize * r.size.1 as usize * r.format.bytes()
            })
            .sum();
        if bytes > self.folds[q].budget {
            return;
        }
        // Adaptive spacing (#137): with the budget full, don't take one that would be
        // the first evicted (far from the playhead, on a fine grid).
        let used = self.checkpoint_bytes();
        if used + bytes > self.folds[q].budget {
            let playhead = self.folds[q].playhead;
            let keep = self.keep_frame(q, playhead);
            // Scored as eviction does: every fold's against its own key and playhead.
            let worst = (0..self.folds.len())
                .flat_map(|p| {
                    let f = &self.folds[p];
                    let k = self.keep_frame(p, f.playhead);
                    f.checkpoints
                        .iter()
                        .map(move |c| evict_score(c.key == f.key, c.frame, f.playhead, k, f.seam))
                })
                .fold(f64::NEG_INFINITY, f64::max);
            let keep_new = keep.max(Some(m).filter(|&m| m <= playhead));
            if evict_score(true, m, playhead, keep_new, self.folds[q].seam) >= worst {
                return;
            }
        }
        let gl = self.gl.clone();
        let mut rings = vec![];
        for &b in &live {
            let frames = self.saved_frames(q, b, m, movie.buffers[b].history);
            let (size, format, reset) = {
                let r = self.folds[q].rings[b].as_ref().unwrap();
                (r.size, r.format, r.reset)
            };
            let mut slots = vec![];
            for (f, slot) in frames {
                let dst = match self.folds[q]
                    .spare
                    .iter()
                    .position(|(fm, t)| *fm == format && t.alloc == size)
                {
                    Some(i) => self.folds[q].spare.swap_remove(i).1,
                    None => new_target(&gl, size, format),
                };
                blit(
                    &gl,
                    &self.folds[q].rings[b].as_ref().unwrap().slots[slot].0,
                    &dst,
                );
                slots.push((f, dst));
            }
            rings.push(Saved {
                buffer: b,
                reset,
                format,
                slots,
            });
        }
        self.folds[q].checkpoints.push(Checkpoint {
            key,
            frame: m,
            rings,
            bytes,
        });
        self.evict_checkpoints();
    }

    /// The checkpoint eviction keeps longest: the last one at or before the playhead.
    fn keep_frame(&self, q: usize, playhead: u64) -> Option<u64> {
        let key = self.folds[q].key;
        self.folds[q]
            .checkpoints
            .iter()
            .filter(|c| c.key == key && c.frame <= playhead)
            .map(|c| c.frame)
            .max()
    }

    /// The (frame, slot) states the next step after `m` reads.
    fn saved_frames(&self, q: usize, b: usize, m: u64, history: u32) -> Vec<(i64, usize)> {
        let r = self.folds[q].rings[b].as_ref().unwrap();
        let lo = (m as i64 + 1 - history as i64).max(r.reset as i64 - 1);
        r.slots
            .iter()
            .enumerate()
            .filter_map(|(i, (_, f))| f.filter(|f| (lo..=m as i64).contains(f)).map(|f| (f, i)))
            .collect()
    }

    /// Keep the checkpoints under the budget. Stale ones (another fold key) go first,
    /// then the farthest from the playhead, where a checkpoint on a coarser grid (its
    /// second number divisible by a higher power of two) counts as nearer: far from
    /// the playhead the spacing doubles instead of thrashing. The one just before the
    /// playhead stays.
    fn evict_checkpoints(&mut self) {
        // One budget for every fold (#138): the highest score anywhere goes, each fold's
        // checkpoints scored against its own key and playhead.
        loop {
            if self.checkpoint_bytes() <= self.folds[0].budget {
                return;
            }
            let mut worst: Option<(f64, usize, usize)> = None;
            for q in 0..self.folds.len() {
                let f = &self.folds[q];
                let keep = self.keep_frame(q, f.playhead);
                for (i, c) in f.checkpoints.iter().enumerate() {
                    let sc = evict_score(c.key == f.key, c.frame, f.playhead, keep, f.seam);
                    if worst.is_none_or(|w| sc > w.0) {
                        worst = Some((sc, q, i));
                    }
                }
            }
            let Some((_, q, i)) = worst else { return };
            let c = self.folds[q].checkpoints.swap_remove(i);
            self.recycle(q, c);
        }
    }
}

/// Eviction order (higher goes first): stale checkpoints (another fold key), then the
/// farthest from the playhead, where a frame on a coarser grid (divisible by a higher
/// power of two) counts as nearer, so far from the playhead the spacing doubles
/// instead of thrashing (#137). The one just before the playhead (`keep`) stays, and a
/// warm loop's seam (#138).
fn evict_score(
    current: bool,
    frame: u64,
    playhead: u64,
    keep: Option<u64>,
    seam: Option<u64>,
) -> f64 {
    if !current {
        return f64::INFINITY;
    }
    if Some(frame) == keep || Some(frame) == seam {
        return -1.0;
    }
    let d = frame.abs_diff(playhead) as f64;
    let grid = (frame.max(1).trailing_zeros()).min(16);
    d / (1u64 << grid) as f64
}

/// Checkpoint spacing at frame `m`: one second of frames, or every frame while a
/// buffer active there has `cache = "dense"` (#137).
fn checkpoint_interval(movie: &Movie, m: u64) -> u64 {
    if movie.buffers.iter().any(|b| b.dense && act(movie, b, m)) {
        1
    } else {
        (movie.fps.round() as u64).max(1)
    }
}

fn item_clone<'a>(i: &super::Item<'a>) -> super::Item<'a> {
    super::Item {
        start: i.start,
        duration: i.duration,
        itime: i.itime,
        uniforms: i.uniforms,
        opacity: i.opacity,
        passes: i.passes.clone(),
    }
}
