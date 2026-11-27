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

use super::{LAYER_TEX, PICTURE_TEX, Renderer, SeqPlan, SeqTargets, ShotKeys, Target, Which, hash};
use crate::movie::{Buffer, BufferFormat, Movie, Source};
use eframe::glow::{self, HasContext};

/// `ChanKey::tex` of a buffer read: `BUF_TEX - (b · 65 + back)`, `back` 0..=64.
pub(super) const BUF_TEX: u64 = LAYER_TEX - 60000;

pub(super) fn buf_key(b: usize, back: u32) -> u64 {
    BUF_TEX - (b as u64 * 65 + back as u64)
}

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
    budget: usize,
    budget_source: &'static str,
    slices: bool,
    step_budget: usize,
    checkpoints_on: bool,
    /// The frame the layers of the frame being drawn read.
    pub(super) n: u64,
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
            budget,
            budget_source,
            slices: false,
            step_budget: STEP_BUDGET,
            checkpoints_on: true,
            n: 0,
        }
    }

    pub(super) fn ring_target(&self, b: usize, s: usize) -> &Target {
        &self.rings[b].as_ref().unwrap().slots[s].0
    }

    pub(super) fn ring_target_mut(&mut self, b: usize, s: usize) -> &mut Target {
        &mut self.rings[b].as_mut().unwrap().slots[s].0
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
        let tex = gl.create_texture().expect("create texture");
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
        let fbo = gl.create_framebuffer().expect("create framebuffer");
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
        gl.delete_framebuffer(t.fbo);
        gl.delete_texture(t.tex);
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

/// Where a pure history buffer must start so every state readers can ask for from
/// frame `from` on (`from - history` and later) is there.
fn pure_start(b: &Buffer, from: u64) -> u64 {
    from.saturating_sub(b.history as u64).max(b.start_frame)
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
            .filter(|b| !b.is_pure() && b.active(s))
            .filter_map(|b| b.reset_at(s))
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
        .filter_map(|(b, &s)| {
            let f = s.max(b.start_frame);
            (f <= n && f < b.end_frame).then_some(f)
        })
        .min()
        .unwrap_or(n + 1)
}

impl Renderer {
    /// The checkpoint budget in bytes and where it came from (a setting, the video
    /// memory, or the default).
    pub fn buffer_cache_budget(&self) -> (usize, &'static str) {
        (self.fold.budget, self.fold.budget_source)
    }

    /// Set the checkpoint budget (the player's setting; tests).
    #[allow(dead_code)]
    pub fn set_buffer_cache_budget(&mut self, bytes: usize) {
        self.fold.budget = bytes;
        self.fold.budget_source = "setting";
        self.evict_checkpoints(self.fold.n);
    }

    /// Catch up in budgeted slices even when rendering blocking (the player while
    /// paused or scrubbing): at most `set_buffer_step_budget` steps per render, which
    /// returns None until the fold reaches the frame. Headless commands never set it.
    pub fn set_catchup_slices(&mut self, on: bool) {
        self.fold.slices = on;
    }

    #[allow(dead_code)]
    pub fn set_buffer_step_budget(&mut self, steps: usize) {
        self.fold.step_budget = steps.max(1);
    }

    /// Take checkpoints while stepping (tests turn them off to force cold folds).
    pub fn set_buffer_checkpoints(&mut self, on: bool) {
        self.fold.checkpoints_on = on;
        if !on {
            self.drop_checkpoints();
        }
    }

    /// Forget every buffer state (the cursor and the checkpoints): the next frame folds
    /// from the resets (tests: a cold `still`).
    pub fn forget_buffer_state(&mut self) {
        self.fold.frame = None;
        self.fold.plan = None;
        self.drop_checkpoints();
    }

    /// (frames done, frames to do) of a catch-up in progress.
    pub fn catchup_progress(&self) -> Option<(u64, u64)> {
        self.fold.progress
    }

    /// Checkpoints held, and their GPU bytes.
    pub fn checkpoint_stats(&self) -> (usize, usize) {
        (
            self.fold.checkpoints.len(),
            self.fold.checkpoints.iter().map(|c| c.bytes).sum(),
        )
    }

    fn drop_checkpoints(&mut self) {
        for c in std::mem::take(&mut self.fold.checkpoints) {
            self.recycle(c);
        }
    }

    fn recycle(&mut self, c: Checkpoint) {
        for s in c.rings {
            for (_, t) in s.slots {
                self.fold.spare.push((s.format, t));
            }
        }
        // A few spares are enough to make filling and evicting cheap.
        while self.fold.spare.len() > 16 {
            let (_, t) = self.fold.spare.remove(0);
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

    /// The fold key (#134, "Invalidation"): everything the buffers' folds read. Phase 1
    /// invalidates every buffer on any change.
    fn fold_key(&self, movie: &Movie, out: (i32, i32)) -> u64 {
        let mut s = format!("{}\0", self.fold.decl);
        let mut add = |k: ShotKeys| {
            s += &format!("{}:{:?}", k.scene, k.trans);
            for c in k.channels.iter().flatten() {
                s += &format!(",{}/{:?}", c.tex, c.footage);
            }
            s.push(';');
        };
        for b in 0..movie.buffers.len() {
            if let Some(k) = self.fold_pick(self.buf_base + b) {
                add(k);
            }
        }
        if self.fold.reads_film {
            let layers = movie.shots.len()..movie.shots.len() + movie.layers.len();
            for i in (0..self.buf_base).filter(|i| !layers.contains(i)) {
                if let Some(k) = self.fold_pick(i) {
                    add(k);
                }
            }
        }
        for b in &movie.buffers {
            s += &format!("{:?}", b.size.at(out));
        }
        hash(&s)
    }

    /// Bring every buffer to film frame `n` (#134) before the frame is planned. False
    /// while that isn't done: a stall (something not ready without `blocking`), or a
    /// catch-up sliced over several renders.
    pub(super) fn advance_buffers(
        &mut self,
        movie: &Movie,
        n: u64,
        out: (i32, i32),
        blocking: bool,
    ) -> bool {
        self.fold.n = n;
        if movie.buffers.is_empty() {
            return true;
        }
        let key = self.fold_key(movie, out);
        if key != self.fold.key {
            self.fold.key = key;
            self.fold.frame = None;
            self.fold.plan = None;
        }
        if self.fold.rings.len() != movie.buffers.len() {
            let old = std::mem::take(&mut self.fold.rings);
            for r in old.into_iter().flatten() {
                for (t, _) in r.slots {
                    delete(&self.gl, t);
                }
            }
            self.fold.rings.resize_with(movie.buffers.len(), || None);
            self.fold.frame = None;
            self.fold.plan = None;
        }
        if self.fold.frame == Some(n) && self.fold.plan.is_none() && self.cursor_valid(movie, n) {
            self.fold.progress = None;
            return true;
        }
        let plan = match self.fold.plan.take() {
            Some(p) if p.key == key && p.target == n => p,
            _ => {
                let p = self.make_plan(movie, n, out);
                if blocking && !self.fold.slices && p.next < p.target {
                    self.blocking_catchups += 1;
                }
                p
            }
        };
        let limit = (!blocking || self.fold.slices).then_some(self.fold.step_budget as u64);
        let mut m = plan.next;
        let mut done = 0u64;
        while m <= n {
            if limit.is_some_and(|l| done >= l) {
                break;
            }
            if !self.step_frame(movie, m, &plan.starts, out, blocking) {
                break;
            }
            self.fold.frame = Some(m);
            m += 1;
            done += 1;
        }
        if m > n {
            self.fold.progress = None;
            self.free_idle_rings(movie, n);
            true
        } else {
            self.fold.progress = Some((m - plan.first, (n + 1).saturating_sub(plan.first)));
            self.fold.plan = Some(Plan { next: m, ..plan });
            false
        }
    }

    /// Can the fold continue from the cursor at `c`? Every buffer active there was
    /// stepped to `c` with all the states it (and its readers) need.
    fn cursor_valid(&self, movie: &Movie, c: u64) -> bool {
        movie.buffers.iter().enumerate().all(|(i, b)| {
            if !b.active(c) {
                return true;
            }
            let Some(r) = self.fold.rings.get(i).and_then(Option::as_ref) else {
                return false;
            };
            r.last == Some(c) && (Some(r.since) == b.reset_at(c) || r.since + b.history as u64 <= c)
        })
    }

    /// The cheapest way to frame `n`: from the cursor, the nearest checkpoint, or the
    /// resets. A checkpoint start restores its rings now (GPU copies).
    fn make_plan(&mut self, movie: &Movie, n: u64, out: (i32, i32)) -> Plan {
        let bufs = &movie.buffers;
        let key = self.fold.key;
        let cold: Vec<u64> = {
            let s = cold_start(movie, n);
            bufs.iter()
                .map(|b| if b.is_pure() { pure_start(b, s) } else { s })
                .collect()
        };
        let mut best = (first_step(movie, &cold, n), cold, None::<usize>, false);
        if let Some(c) = self.fold.frame.filter(|&c| c < n)
            && self.cursor_valid(movie, c)
        {
            let starts = vec![c + 1; bufs.len()];
            let first = first_step(movie, &starts, n);
            if first >= best.0 {
                best = (first, starts, None, true);
            }
        }
        let ck = self
            .fold
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
                        pure_start(b, c + 1)
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
            for r in self.fold.rings.iter_mut().flatten() {
                for s in &mut r.slots {
                    s.1 = None;
                }
                r.last = None;
            }
            self.fold.frame = None;
            if let Some(i) = ck {
                self.restore(movie, i, out);
                // A checkpoint at n itself leaves nothing to step: the cursor is there.
                self.fold.frame = Some(self.fold.checkpoints[i].frame);
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
    fn restore(&mut self, movie: &Movie, i: usize, out: (i32, i32)) {
        let gl = self.gl.clone();
        let saved: Vec<(usize, u64, Vec<(i64, glow::Framebuffer)>, u64)> = {
            let c = &self.fold.checkpoints[i];
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
            self.ensure_ring(movie, b, out);
            let ring = self.fold.rings[b].as_mut().unwrap();
            for (k, (f, _)) in slots.iter().enumerate() {
                ring.slots[k].1 = Some(*f);
            }
            ring.reset = reset;
            ring.since = reset;
            ring.last = Some(frame);
            let ring = self.fold.rings[b].as_ref().unwrap();
            let c = &self.fold.checkpoints[i];
            let s = c.rings.iter().find(|s| s.buffer == b).unwrap();
            for (k, (_, t)) in s.slots.iter().enumerate() {
                blit(&gl, t, &ring.slots[k].0);
            }
        }
        self.buffer_restores += 1;
    }

    /// Make buffer `b`'s ring exist at its size, format and length.
    fn ensure_ring(&mut self, movie: &Movie, b: usize, out: (i32, i32)) {
        let buf = &movie.buffers[b];
        let size = buf.size.at(out);
        let len = buf.history as usize + 1;
        let ok = self.fold.rings[b]
            .as_ref()
            .is_some_and(|r| r.size == size && r.format == buf.format && r.slots.len() == len);
        if ok {
            return;
        }
        if let Some(r) = self.fold.rings[b].take() {
            for (t, _) in r.slots {
                delete(&self.gl, t);
            }
        }
        let slots = (0..len)
            .map(|_| (new_target(&self.gl, size, buf.format), None))
            .collect();
        self.fold.rings[b] = Some(Ring {
            size,
            format: buf.format,
            slots,
            reset: buf.start_frame,
            since: buf.start_frame,
            last: None,
        });
    }

    /// Rings of buffers not active at `n` hold nothing anyone reads: free them.
    fn free_idle_rings(&mut self, movie: &Movie, n: u64) {
        for (b, buf) in movie.buffers.iter().enumerate() {
            if !buf.active(n)
                && let Some(r) = self.fold.rings[b].take()
            {
                for (t, _) in r.slots {
                    delete(&self.gl, t);
                }
            }
        }
    }

    /// Buffer `b`'s state `back` frames before film frame `m` (0: frame m's, after its
    /// step), as a pass: black (None) while it isn't active or the state isn't there.
    /// Before the reset, past states read the `init` state (frame `reset - 1`).
    pub(super) fn buf_pass(
        &self,
        movie: &Movie,
        b: usize,
        back: u32,
        m: u64,
    ) -> Option<(glow::Texture, usize, usize)> {
        if !movie.buffers.get(b)?.active(m) {
            return None;
        }
        let r = self.fold.rings.get(b)?.as_ref()?;
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
        channels: &[Option<crate::movie::Channel>],
        m: u64,
    ) -> Vec<(u64, (glow::Texture, usize, usize))> {
        channels
            .iter()
            .flatten()
            .filter_map(|c| match c.source {
                Source::Buffer { index, back, .. } => self
                    .buf_pass(movie, index, back, m)
                    .map(|t| (buf_key(index, back), t)),
                _ => None,
            })
            .collect()
    }

    /// Step every buffer due on film frame `m` (active, and `m ≥ starts[b]`), in file
    /// order: decide everything first (programs, textures, the picture and comps they
    /// read), then draw. False, with nothing drawn, if something isn't ready.
    fn step_frame(
        &mut self,
        movie: &Movie,
        m: u64,
        starts: &[u64],
        out: (i32, i32),
        blocking: bool,
    ) -> bool {
        let bufs = &movie.buffers;
        let todo: Vec<usize> = (0..bufs.len())
            .filter(|&b| bufs[b].active(m) && m >= starts[b])
            .collect();
        if todo.is_empty() {
            return true;
        }
        let Some(copy) = self.copy else { return false };
        let mut keys = vec![];
        for &b in &todo {
            match self.usable(self.buf_base + b, blocking, m) {
                Some(k) => keys.push(k),
                None => return false,
            }
        }
        let t = movie.frame_time(m);
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
                self.plan_no_layers = true;
                let p = self.plan_seq(
                    movie,
                    0,
                    at,
                    (t, m.min(i32::MAX as u64) as i32, m),
                    size,
                    size,
                    blocking,
                    &mut insts,
                );
                self.plan_no_layers = false;
                match p {
                    Some(p) => pics.push((size, p)),
                    None => return false,
                }
            }
        }
        let mut chans = vec![];
        for &b in &todo {
            let buf = &bufs[b];
            let local = t - buf.start;
            match self.plan_chans(
                movie,
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
        // GL work: comp instances, the pictures, then each buffer.
        if self.inst.len() < insts.len() {
            self.inst.resize_with(insts.len(), || None);
        }
        for (i, (k, p)) in insts.iter().enumerate() {
            super::ensure_target(&self.gl, &mut self.inst[i], k.dest.0, k.dest.1);
            self.draw_seq(movie, p, copy, Some(Which::Inst(i)), k.dest);
        }
        let mut pic_tex: Vec<((i32, i32), glow::Texture)> = vec![];
        for (size, p) in &pics {
            let j = match self.fold.pics.iter().position(|(s, _)| s == size) {
                Some(j) => j,
                None => {
                    self.fold.pics.push((*size, SeqTargets::default()));
                    self.fold.pics.len() - 1
                }
            };
            std::mem::swap(&mut self.seqs[0], &mut self.fold.pics[j].1);
            super::ensure_target(&self.gl, &mut self.seqs[0].picture, size.0, size.1);
            self.draw_seq(movie, p, copy, Some(Which::Picture), *size);
            pic_tex.push((*size, self.seqs[0].picture.as_ref().unwrap().tex));
            std::mem::swap(&mut self.seqs[0], &mut self.fold.pics[j].1);
        }
        self.cur = 0;
        // Resets first, for every buffer due: an earlier buffer reading a later one's
        // previous state on that buffer's reset frame then reads black, whichever path
        // reached the frame.
        let mut init = vec![false; todo.len()];
        for (i, &b) in todo.iter().enumerate() {
            let buf = &bufs[b];
            let fresh = buf.resets.binary_search(&m).is_ok()
                || self.fold.rings[b]
                    .as_ref()
                    .is_none_or(|r| m == 0 || r.last != Some(m - 1));
            if fresh {
                self.ensure_ring(movie, b, out);
                let r = self.fold.rings[b].as_mut().unwrap();
                for s in &mut r.slots {
                    s.1 = None;
                }
                r.reset = buf.reset_at(m).unwrap_or(m);
                r.since = m;
                r.last = None;
                init[i] = r.reset == m;
            }
        }
        for (i, &b) in todo.iter().enumerate() {
            let buf = &bufs[b];
            let size = buf.size.at(out);
            let local = t - buf.start;
            let mut passes = self.buf_passes(movie, &buf.channels, m);
            if let Some((_, tex)) = pic_tex.iter().find(|(s, _)| *s == size) {
                passes.push((PICTURE_TEX, (*tex, size.0 as usize, size.1 as usize)));
            }
            passes.extend(self.inst_passes(&chans[i]));
            let reset = self.fold.rings[b].as_ref().unwrap().reset;
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
                self.fold.rings[b].as_mut().unwrap().slots[slot].1 = Some(m as i64 - 1);
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
                            Which::Ring(b, slot),
                            size.0,
                            size.1,
                            &info,
                            movie,
                        );
                    }
                    None => unsafe {
                        let fbo = self.fold.rings[b].as_ref().unwrap().slots[slot].0.fbo;
                        self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                        self.gl.viewport(0, 0, size.0, size.1);
                        self.gl.disable(glow::SCISSOR_TEST);
                        self.gl.clear_color(0.0, 0.0, 0.0, 0.0);
                        self.gl.clear(glow::COLOR_BUFFER_BIT);
                    },
                }
            }
            // Re-read the passes: the init state is a new slot.
            let mut passes = self.buf_passes(movie, &buf.channels, m);
            passes.extend(
                item.passes
                    .iter()
                    .copied()
                    .filter(|(k, _)| *k == PICTURE_TEX),
            );
            passes.extend(self.inst_passes(&chans[i]));
            let item = super::Item { passes, ..item };
            let slot = self.free_slot(b, m, buf.history);
            self.draw_scene(
                keys[i],
                &item,
                local,
                Which::Ring(b, slot),
                size.0,
                size.1,
                &info,
                movie,
            );
            let r = self.fold.rings[b].as_mut().unwrap();
            r.slots[slot].1 = Some(m as i64);
            r.last = Some(m);
        }
        self.buffer_steps += todo.len();
        self.maybe_checkpoint(movie, m);
        true
    }

    /// A slot to draw frame `m` into: not one holding a state frame m still reads
    /// (`m - history` .. `m - 1`, or the init state at `reset - 1`).
    fn free_slot(&self, b: usize, m: u64, history: u32) -> usize {
        let r = self.fold.rings[b].as_ref().unwrap();
        let lo = (m as i64 - history as i64).max(r.reset as i64 - 1);
        let hi = m as i64 - 1;
        r.slots
            .iter()
            .position(|(_, f)| f.is_none_or(|f| f < lo || f > hi))
            .expect("a ring has history + 1 slots")
    }

    /// Every `fps` frames (one second), copy the non-pure buffers' rings into a
    /// checkpoint (GPU copies), then keep the checkpoints under the budget.
    fn maybe_checkpoint(&mut self, movie: &Movie, m: u64) {
        if !self.fold.checkpoints_on {
            return;
        }
        let k = (movie.fps.round() as u64).max(1);
        if !m.is_multiple_of(k) {
            return;
        }
        let key = self.fold.key;
        if self
            .fold
            .checkpoints
            .iter()
            .any(|c| c.key == key && c.frame == m)
        {
            return;
        }
        let live: Vec<usize> = (0..movie.buffers.len())
            .filter(|&b| !movie.buffers[b].is_pure() && movie.buffers[b].active(m))
            .collect();
        // A pure buffer can step ahead of the others (it starts `history` back): their
        // rings then hold another frame's state, or none. Only a whole fold at m is saved.
        if live.is_empty()
            || live.iter().any(|&b| {
                self.fold.rings[b]
                    .as_ref()
                    .is_none_or(|r| r.last != Some(m))
            })
        {
            return;
        }
        let bytes: usize = live
            .iter()
            .map(|&b| {
                let r = self.fold.rings[b].as_ref().unwrap();
                let n = self.saved_frames(b, m, movie.buffers[b].history).len();
                n * r.size.0 as usize * r.size.1 as usize * r.format.bytes()
            })
            .sum();
        if bytes > self.fold.budget {
            return;
        }
        let gl = self.gl.clone();
        let mut rings = vec![];
        for &b in &live {
            let frames = self.saved_frames(b, m, movie.buffers[b].history);
            let (size, format, reset) = {
                let r = self.fold.rings[b].as_ref().unwrap();
                (r.size, r.format, r.reset)
            };
            let mut slots = vec![];
            for (f, slot) in frames {
                let dst = match self
                    .fold
                    .spare
                    .iter()
                    .position(|(fm, t)| *fm == format && t.alloc == size)
                {
                    Some(i) => self.fold.spare.swap_remove(i).1,
                    None => new_target(&gl, size, format),
                };
                blit(
                    &gl,
                    &self.fold.rings[b].as_ref().unwrap().slots[slot].0,
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
        self.fold.checkpoints.push(Checkpoint {
            key,
            frame: m,
            rings,
            bytes,
        });
        self.evict_checkpoints(m);
    }

    /// The (frame, slot) states the next step after `m` reads.
    fn saved_frames(&self, b: usize, m: u64, history: u32) -> Vec<(i64, usize)> {
        let r = self.fold.rings[b].as_ref().unwrap();
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
    fn evict_checkpoints(&mut self, playhead: u64) {
        let key = self.fold.key;
        loop {
            let total: usize = self.fold.checkpoints.iter().map(|c| c.bytes).sum();
            if total <= self.fold.budget || self.fold.checkpoints.is_empty() {
                return;
            }
            let keep = self
                .fold
                .checkpoints
                .iter()
                .filter(|c| c.key == key && c.frame <= playhead)
                .map(|c| c.frame)
                .max();
            let score = |c: &Checkpoint| -> f64 {
                if c.key != key {
                    return f64::INFINITY;
                }
                if Some(c.frame) == keep {
                    return -1.0;
                }
                let d = c.frame.abs_diff(playhead) as f64;
                let grid = (c.frame.max(1).trailing_zeros()).min(16);
                d / (1u64 << grid) as f64
            };
            let i = (0..self.fold.checkpoints.len())
                .max_by(|&a, &b| {
                    score(&self.fold.checkpoints[a]).total_cmp(&score(&self.fold.checkpoints[b]))
                })
                .unwrap();
            let c = self.fold.checkpoints.swap_remove(i);
            self.recycle(c);
        }
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
