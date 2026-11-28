//! The render path: `render_at`, drawing a sequence, the scene pass, layer composites.

use super::*;

impl Renderer {
    /// Render the movie at `info.t` into the output texture (w × h).
    /// With `blocking = false` it returns None when a shot's program isn't compiled
    /// yet (keep showing the previous frame); with `true` it compiles on the spot.
    pub fn render(
        &mut self,
        movie: &Movie,
        w: i32,
        h: i32,
        info: &FrameInfo,
        blocking: bool,
    ) -> Option<glow::Texture> {
        self.render_at(movie, w, h, info, movie.at(info.t), blocking)
    }

    /// Like `render`, but for an explicit timeline position (tests use this to
    /// render a transition at an exact progress).
    pub fn render_at(
        &mut self,
        movie: &Movie,
        w: i32,
        h: i32,
        info: &FrameInfo,
        at: At,
        blocking: bool,
    ) -> Option<glow::Texture> {
        let r = self.render_at_inner(movie, w, h, info, at, blocking);
        self.audit("render_at");
        r
    }

    pub(super) fn render_at_inner(
        &mut self,
        movie: &Movie,
        w: i32,
        h: i32,
        info: &FrameInfo,
        at: At,
        blocking: bool,
    ) -> Option<glow::Texture> {
        let copy = self.copy?;
        let (w, h) = (w.max(1), h.max(1));
        let sw = ((w as f32 * info.scale).round() as i32).clamp(1, w);
        let sh = ((h as f32 * info.scale).round() as i32).clamp(1, h);
        self.poll_timer();
        if invalid_at(movie, at) {
            return None;
        }
        // Decide everything first (#74): the film's needed shots and active layers, and,
        // depth first, every comp instance they show, each ready (or its fallback). A
        // frame shows only when all of it is, so playback never shows what `still`
        // wouldn't. Films without comps plan exactly the old frame.
        let mut insts: Vec<(InstKey, SeqPlan)> = vec![];
        let n = film_frame(movie, info.t);
        self.comp_req.clear();
        // Buffers (#134) step to frame n first (catching up if needed): a frame shows
        // only once every active buffer holds its state at n.
        self.fold_out = (w, h);
        // A comp's catch-up (#138) is reported only while this frame still waits for
        // it: a comp no longer shown (a seek out of its span) mustn't hold the player.
        for f in self.folds.iter_mut().skip(1) {
            f.clear_progress();
        }
        if !self.advance_to(movie, 0, n, (w, h), blocking) {
            self.comp_seen = std::mem::take(&mut self.comp_req);
            self.unbind_after_steps();
            return None;
        }
        let root = self.plan_seq(
            movie,
            0,
            at,
            (info.t, info.frame, n),
            (w, h),
            (sw, sh),
            blocking,
            &mut insts,
        );
        self.comp_seen = std::mem::take(&mut self.comp_req);
        let Some(root) = root else {
            // A comp's buffers (#138) may have stepped while this frame was planned.
            self.unbind_after_steps();
            return None;
        };
        let plans: Vec<&SeqPlan> = insts.iter().map(|(_, p)| p).chain([&root]).collect();
        let all_keys: Vec<ShotKeys> = plans
            .iter()
            .flat_map(|p| {
                p.shots
                    .iter()
                    .map(|s| s.0)
                    .chain(p.layers.iter().map(|l| l.2))
            })
            .collect();
        // A footage frame that failed to decode reads black (a broken still texture
        // too); headless commands refuse such a frame (`footage_failure`).
        self.footage_failure = all_keys
            .iter()
            .flat_map(|k| k.channels.iter().flatten())
            .filter(|c| c.footage.is_some())
            .find_map(|c| match &self.textures.get(&c.tex)?.state {
                TexState::Failed(m) => Some(m.clone()),
                _ => None,
            });
        let frame_no = self.frame_no;
        for k in &all_keys {
            for x in [Some(k.scene), k.trans].into_iter().flatten() {
                if let Some(e) = self.programs.get_mut(&x) {
                    e.last_used = frame_no;
                }
            }
            for c in k.channels.iter().flatten() {
                if let Some(e) = self.textures.get_mut(&c.tex) {
                    e.last_used = frame_no;
                }
            }
        }
        if let Some(timer) = &mut self.timer {
            timer.begin(&self.gl);
        }
        ensure_target(&self.gl, &mut self.out, w, h);
        if self.inst.len() < insts.len() {
            self.inst.resize_with(insts.len(), || None);
        }
        // Comp instances first (each is complete before anything reads it), then the film.
        for (i, (k, p)) in insts.iter().enumerate() {
            ensure_target(&self.gl, &mut self.inst[i], k.dest.0, k.dest.1);
            self.draw_seq(movie, p, copy, Some(Which::Inst(i)), k.dest);
        }
        self.draw_seq(movie, &root, copy, None, (w, h));
        self.cur = 0;
        if let Some(timer) = &mut self.timer {
            timer.end(&self.gl);
        }
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.use_program(None);
            self.gl.bind_vertex_array(None);
        }
        self.out.as_ref().map(|t| t.tex)
    }

    /// Leave GL as a finished frame does after buffer steps drew into their targets and
    /// the frame wasn't drawn, or the player's UI paints into the last ring's
    /// framebuffer (a flicker, and corrupt state).
    pub(super) fn unbind_after_steps(&self) {
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.use_program(None);
            self.gl.bind_vertex_array(None);
        }
    }

    /// Draw a planned sequence (#74) into `dest` (None: the output, flipped for
    /// readback when asked) at `dsize`: the picture track, then its layers. Returns the
    /// held layers' outputs as passes (`LAYER_TEX - j`: a buffer step reads them, #138).
    #[allow(clippy::type_complexity)]
    pub(super) fn draw_seq(
        &mut self,
        film: &Movie,
        p: &SeqPlan,
        copy: u64,
        dest: Option<Which>,
        dsize: (i32, i32),
    ) -> Vec<(u64, (glow::Texture, usize, usize))> {
        let m = seq_movie(film, p.seq);
        self.cur = p.seq;
        let (w, h) = p.full;
        let (sw, sh) = p.scaled;
        let info = FrameInfo {
            t: p.t,
            dt: m.fps_den as f32 / m.fps_num as f32,
            frame: p.frame,
            scale: sw as f32 / w as f32,
        };
        let s = self.cur;
        ensure_target(&self.gl, &mut self.seqs[s].a, w, h);
        ensure_target(&self.gl, &mut self.seqs[s].b, w, h);
        // With layers, the picture track goes into a working picture first.
        let over = p.over;
        let pic_dest = if over.is_some() {
            Some(Which::Picture)
        } else {
            dest
        };
        let pic_size = if over.is_some() { (w, h) } else { dsize };
        if over.is_some() {
            let t = &mut self.seqs[s];
            ensure_target(&self.gl, &mut t.picture, w, h);
            ensure_target(&self.gl, &mut t.work[0], w, h);
            ensure_target(&self.gl, &mut t.work[1], w, h);
            ensure_target(&self.gl, &mut t.layer, w, h);
            t.held.resize_with(m.layers.len(), || None);
        }
        match p.at {
            At::Shot { shot, local } => {
                let (k, inst) = p.shots[0];
                let from = match inst {
                    Some(i) => Which::Inst(i),
                    None => {
                        let mut item = Item::shot(&m.shots[shot], local);
                        item.passes = self.inst_passes(&p.chans[0]);
                        self.draw_scene(k, &item, local, Which::A, sw, sh, &info, m);
                        Which::A
                    }
                };
                self.composite_into(copy, from, from, 0.0, pic_size, pic_dest, None);
            }
            At::Transition {
                from,
                to,
                from_local,
                to_local,
                progress,
            } => {
                // checked by invalid_at before any GL work
                let Some(tr) = m.shots[to].transition_in.as_ref() else {
                    unreachable!()
                };
                let (k0, inst0) = p.shots[0];
                let (k1, inst1) = p.shots[1];
                let from_target = if let Some(i) = inst0 {
                    Which::Inst(i)
                } else if tr.freeze {
                    let key = (to, self.generation, k0);
                    if self.seqs[s].freeze_key != Some(key)
                        || self.seqs[s]
                            .freeze
                            .as_ref()
                            .is_none_or(|f| f.alloc != (w, h))
                    {
                        ensure_target(&self.gl, &mut self.seqs[s].freeze, w, h);
                        let freeze_local = m.shots[to].start - m.shots[from].start;
                        let mut item = Item::shot(&m.shots[from], freeze_local);
                        item.passes = self.inst_passes(&p.chans[0]);
                        // The frame at the transition's start, iFrame and iMovieTime too,
                        // so a redrawn freeze (one that reads a comp) stays still.
                        let finfo = FrameInfo {
                            scale: info.scale,
                            ..FrameInfo::at(m, m.shots[to].start)
                        };
                        self.draw_scene(k0, &item, freeze_local, Which::Freeze, sw, sh, &finfo, m);
                        // A frame that reads a comp isn't kept: the comp may move.
                        self.seqs[s].freeze_key = p.chans[0].is_empty().then_some(key);
                    }
                    Which::Freeze
                } else {
                    let mut item = Item::shot(&m.shots[from], from_local);
                    item.passes = self.inst_passes(&p.chans[0]);
                    self.draw_scene(k0, &item, from_local, Which::A, sw, sh, &info, m);
                    Which::A
                };
                let to_target = match inst1 {
                    Some(i) => Which::Inst(i),
                    None => {
                        let mut item = Item::shot(&m.shots[to], to_local);
                        item.passes = self.inst_passes(&p.chans[1]);
                        self.draw_scene(k1, &item, to_local, Which::B, sw, sh, &info, m);
                        Which::B
                    }
                };
                let eased = tr.ease.apply(progress);
                let tp = k1.trans.filter(|k| self.prog(*k).is_some()).unwrap_or(copy);
                self.composite_into(tp, from_target, to_target, eased, pic_size, pic_dest, None);
            }
        }
        let Some(over) = over else { return vec![] };
        // Each layer bottom to top: draw it at its scale (premultiplied), then
        // composite it over the picture so far (the engine's source-over, or its blend
        // pass; × opacity, × matte) into a working picture; the picture track stays in
        // its own target for `@picture`. A layer that later layers read (or use as a
        // matte) draws into a target of exactly its size and stays there for them. The
        // last working picture is copied to `dest`.
        // A buffer step's composite (#138) also holds the layers buffers read.
        let readers = m.layers.iter().map(|l| &l.channels).chain(
            m.buffers
                .iter()
                .filter(|_| p.step.is_some())
                .map(|b| &b.channels),
        );
        let read: std::collections::HashSet<usize> = readers
            .flat_map(|c| c.iter().flatten())
            .filter_map(|c| match c.source {
                crate::movie::Source::Layer { index, .. } => Some(index),
                _ => None,
            })
            .chain(m.layers.iter().filter_map(|l| match l.matte?.source {
                crate::movie::MatteSource::Layer(j) => Some(j),
                _ => None,
            }))
            .collect();
        let full = (w as usize, h as usize);
        let picture = self.seqs[s].picture.as_ref().unwrap().tex;
        let mut held: Vec<(u64, (glow::Texture, usize, usize))> = vec![];
        let mut cur = Which::Picture;
        let n_shots = p.shots.len();
        for (li, &(j, local, k, inst, matte_inst)) in p.layers.iter().enumerate() {
            let layer = &m.layers[j];
            let (lw, lh) = match layer.scale {
                None => (sw, sh),
                Some(sc) => (
                    ((w as f32 * sc).round() as i32).clamp(1, w),
                    ((h as f32 * sc).round() as i32).clamp(1, h),
                ),
            };
            let into = match inst {
                Some(i) => Which::Inst(i),
                None => {
                    let below = self.target(cur).tex;
                    let mut passes = vec![
                        (INPUT_TEX, (below, full.0, full.1)),
                        (PICTURE_TEX, (picture, full.0, full.1)),
                    ];
                    passes.extend(held.iter().copied());
                    passes.extend(self.inst_passes(&p.chans[n_shots + li]));
                    let bn = p.step.unwrap_or(self.folds[p.seq].n);
                    passes.extend(self.buf_passes(m, p.seq, &layer.channels, bn));
                    let item = Item {
                        start: layer.start,
                        duration: layer.duration(),
                        itime: layer.scene_time(local),
                        uniforms: &layer.uniforms,
                        opacity: Some(layer.opacity_at(local)),
                        passes,
                    };
                    let into = if read.contains(&j) {
                        ensure_target(&self.gl, &mut self.seqs[s].held[j], lw, lh);
                        Which::Held(j)
                    } else {
                        Which::Layer
                    };
                    self.draw_scene(k, &item, local, into, lw, lh, &info, m);
                    into
                }
            };
            if read.contains(&j) {
                let t = self.target(into);
                held.push((
                    LAYER_TEX - j as u64,
                    (t.tex, t.used.0 as usize, t.used.1 as usize),
                ));
            }
            // The matte (#74): an earlier layer's held output (black when it isn't
            // drawn this frame) or a comp instance.
            let matte = layer.matte.map(|mt| {
                let tex = match mt.source {
                    crate::movie::MatteSource::Layer(i) => held
                        .iter()
                        .find(|(key, _)| *key == LAYER_TEX - i as u64)
                        .map_or(self.clear, |(_, (t, _, _))| *t),
                    crate::movie::MatteSource::Comp(_) => {
                        self.inst[matte_inst.unwrap()].as_ref().unwrap().tex
                    }
                };
                (tex, mt.luma, mt.invert)
            });
            let next = match cur {
                Which::Work(0) => Which::Work(1),
                _ => Which::Work(0),
            };
            let base_over = match matte {
                Some(_) => self.over_matte.unwrap_or(over),
                None => over,
            };
            let prog = k
                .trans
                .filter(|b| self.prog(*b).is_some())
                .unwrap_or(base_over);
            self.composite_into(
                prog,
                cur,
                into,
                layer.opacity_at(local),
                (w, h),
                Some(next),
                matte,
            );
            cur = next;
        }
        self.composite_into(copy, cur, cur, 0.0, dsize, dest, None);
        held
    }

    pub(super) fn target(&self, w: Which) -> &Target {
        match w {
            Which::Inst(i) => self.inst[i].as_ref().unwrap(),
            Which::Ring(q, b, s) => self.folds[q].ring_target(b, s),
            _ => {
                let s = &self.seqs[self.cur];
                match w {
                    Which::A => s.a.as_ref().unwrap(),
                    Which::B => s.b.as_ref().unwrap(),
                    Which::Freeze => s.freeze.as_ref().unwrap(),
                    Which::Work(i) => s.work[i].as_ref().unwrap(),
                    Which::Layer => s.layer.as_ref().unwrap(),
                    Which::Picture => s.picture.as_ref().unwrap(),
                    Which::Held(j) => s.held[j].as_ref().unwrap(),
                    Which::Input => s.input.as_ref().unwrap(),
                    Which::Inst(_) | Which::Ring(..) => unreachable!(),
                }
            }
        }
    }

    pub(super) fn target_mut(&mut self, w: Which) -> &mut Target {
        match w {
            Which::Inst(i) => self.inst[i].as_mut().unwrap(),
            Which::Ring(q, b, s) => self.folds[q].ring_target_mut(b, s),
            _ => {
                let s = &mut self.seqs[self.cur];
                match w {
                    Which::A => s.a.as_mut().unwrap(),
                    Which::B => s.b.as_mut().unwrap(),
                    Which::Freeze => s.freeze.as_mut().unwrap(),
                    Which::Work(i) => s.work[i].as_mut().unwrap(),
                    Which::Layer => s.layer.as_mut().unwrap(),
                    Which::Picture => s.picture.as_mut().unwrap(),
                    Which::Held(j) => s.held[j].as_mut().unwrap(),
                    Which::Input => s.input.as_mut().unwrap(),
                    Which::Inst(_) | Which::Ring(..) => unreachable!(),
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn draw_scene(
        &mut self,
        keys: ShotKeys,
        item: &Item<'_>,
        local: f32,
        into: Which,
        sw: i32,
        sh: i32,
        info: &FrameInfo,
        movie: &Movie,
    ) {
        let gl = self.gl.clone();
        let t = self.target_mut(into);
        t.used = (sw, sh);
        let fbo = t.fbo;
        let Some(p) = self.prog(keys.scene) else {
            // Failed with no working version: a shot shows black, a layer nothing.
            let a = if item.opacity.is_some() { 0.0 } else { 1.0 };
            unsafe {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                gl.viewport(0, 0, sw, sh);
                gl.clear_color(0.0, 0.0, 0.0, a);
                gl.clear(glow::COLOR_BUFFER_BIT);
            }
            return;
        };
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.viewport(0, 0, sw, sh);
            gl.disable(glow::BLEND);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            gl.bind_vertex_array(Some(self.vao));
            gl.use_program(Some(p.prog));
            let f1 = |n: &str, v: f32| {
                if let Some(l) = p.loc(n) {
                    gl.uniform_1_f32(Some(l), v)
                }
            };
            if let Some(l) = p.loc("iResolution") {
                gl.uniform_3_f32(Some(l), sw as f32, sh as f32, 1.0);
            }
            f1("iTime", item.itime);
            f1("iTimeDelta", info.dt);
            f1("iFrameRate", movie.fps);
            f1("iSampleRate", 44100.0);
            f1("iMovieTime", item.start + local);
            f1("iShotTime", local);
            f1("iShotDuration", item.duration);
            f1("iShotProgress", (local / item.duration).clamp(0.0, 1.0));

            if let Some(l) = p.loc("iFrame") {
                gl.uniform_1_i32(Some(l), info.frame);
            }
            if let Some(l) = p.loc("iMouse") {
                gl.uniform_4_f32(Some(l), 0.0, 0.0, 0.0, 0.0);
            }
            if let Some(l) = p.loc("iDate") {
                gl.uniform_4_f32(Some(l), 2026.0, 0.0, 1.0, item.start + local);
            }
            // Channels: the texture and its sampler on unit n (the iChannelN uniforms
            // were set to n at link time), or black with resolution 0.
            let mut res = [0f32; 3 * N_CHANNELS];
            let mut meta = [0f32; 4 * N_CHANNELS];
            let mut times = [0f32; N_CHANNELS];
            let mut sampled = false;
            for (n, c) in keys.channels.iter().enumerate() {
                // Footage: the shown frame's source time; past the end with
                // `end = "black"`, black with the clip's resolution.
                if let Some(c) = c
                    && let Some(fk) = c.footage
                {
                    times[n] = c.time;
                    if c.tex == FOOTAGE_BLACK
                        && let Some((w, h)) = self.footage.get(&fk).and_then(|f| f.scan.size)
                    {
                        res[n * 3..n * 3 + 3].copy_from_slice(&[w as f32, h as f32, 1.0]);
                    }
                }
                let ready = c.and_then(|c| {
                    if is_pass_key(c.tex) {
                        // Another pass's picture, exactly its size; black (resolution 0)
                        // when it isn't drawn this frame (an inactive or hidden layer).
                        let &(_, (tex, w, h)) = item.passes.iter().find(|(k, _)| *k == c.tex)?;
                        return Some((tex, w, h, [0.0; 4], self.sampler(c)));
                    }
                    match self.textures.get(&c.tex)?.state {
                        TexState::Ready {
                            tex, w, h, meta, ..
                        } => Some((tex, w, h, meta, self.sampler(c))),
                        _ => None,
                    }
                });
                if ready.is_none() && p.loc(CHANNEL_UNIFORMS[n]).is_none() {
                    continue;
                }
                gl.active_texture(glow::TEXTURE0 + n as u32);
                match ready {
                    Some((tex, w, h, m, sampler)) => {
                        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                        gl.bind_sampler(n as u32, Some(sampler));
                        res[n * 3..n * 3 + 3].copy_from_slice(&[w as f32, h as f32, 1.0]);
                        meta[n * 4..n * 4 + 4].copy_from_slice(&m);
                        sampled = true;
                    }
                    None => {
                        gl.bind_texture(glow::TEXTURE_2D, Some(self.dummy));
                        gl.bind_sampler(n as u32, None);
                    }
                }
            }
            if let Some(l) = p.loc("iChannelResolution") {
                gl.uniform_3_f32_slice(Some(l), &res);
            }
            if let Some(l) = p.loc("iChannelMeta") {
                gl.uniform_4_f32_slice(Some(l), &meta);
            }
            if let Some(l) = p.loc("iChannelTime") {
                gl.uniform_1_f32_slice(Some(l), &times);
            }
            for (name, a) in item.uniforms {
                if let Some(l) = p.loc(name) {
                    set_anim(&gl, l, a, local);
                }
            }
            // A very slow frame is drawn in horizontal strips, flushed one by one, so
            // no single GPU job runs long enough to trip the driver's watchdog
            // (Windows resets the GPU after ~2 s). Each strip is its own viewport, as
            // for sound blocks: gl_FragCoord stays window-relative, so the picture is
            // the same. Not a scissor: the Apple Software Renderer left parts of
            // scissored strips undrawn (stale rows from the previous frame).
            let tiles = self.tiles.clamp(1, sh.max(1) as u32) as i32;
            if tiles == 1 {
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            } else {
                for i in 0..tiles {
                    let y0 = sh * i / tiles;
                    let y1 = sh * (i + 1) / tiles;
                    gl.viewport(0, y0, sw, y1 - y0);
                    gl.draw_arrays(glow::TRIANGLES, 0, 3);
                    gl.flush();
                }
                gl.viewport(0, 0, sw, sh);
            }
            // Sampler objects override texture state on their unit: unbind them, or
            // the composite pass (and egui) would sample through them.
            if sampled {
                for n in 0..N_CHANNELS as u32 {
                    gl.bind_sampler(n, None);
                }
            }
            gl.active_texture(glow::TEXTURE0);
        }
    }

    pub(super) fn sampler(&self, c: ChanKey) -> glow::Sampler {
        self.samplers
            .iter()
            .find(|(k, _)| *k == (c.filter, c.wrap))
            .map(|(_, s)| *s)
            .expect("a sampler for every (filter, wrap)")
    }

    /// A composite pass (copy, transition, over, blend) into a working picture or
    /// comp instance (`dest`, never flipped) or, with None, into the output (flipped
    /// for readback when asked). `matte` (#74): (texture, luma, invert) for the matted
    /// over pass or a blend assembled with a matte.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn composite_into(
        &mut self,
        prog: u64,
        from: Which,
        to: Which,
        progress: f32,
        (w, h): (i32, i32),
        dest: Option<Which>,
        matte: Option<(glow::Texture, bool, bool)>,
    ) {
        let gl = self.gl.clone();
        let (ft, tt) = (self.target(from), self.target(to));
        let scale = |t: &Target| {
            (
                t.used.0 as f32 / t.alloc.0 as f32,
                t.used.1 as f32 / t.alloc.1 as f32,
            )
        };
        let maxv = |t: &Target| {
            (
                (t.used.0 as f32 - 0.5) / t.alloc.0 as f32,
                (t.used.1 as f32 - 0.5) / t.alloc.1 as f32,
            )
        };
        let (fs, fm, ftex) = (scale(ft), maxv(ft), ft.tex);
        let (ts, tm, ttex) = (scale(tt), maxv(tt), tt.tex);
        let flip = dest.is_none() && self.flip_output;
        let out = match dest {
            None => {
                self.out_flipped = self.flip_output;
                self.out.as_ref().unwrap().fbo
            }
            Some(d) => {
                let t = self.target_mut(d);
                t.used = (w, h);
                t.fbo
            }
        };
        let Some(p) = self.prog(prog) else { return };
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(out));
            gl.viewport(0, 0, w, h);
            gl.bind_vertex_array(Some(self.vao));
            gl.use_program(Some(p.prog));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(ftex));
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(ttex));
            if let Some(l) = p.loc("uFrom") {
                gl.uniform_1_i32(Some(l), 0);
            }
            if let Some(l) = p.loc("uTo") {
                gl.uniform_1_i32(Some(l), 1);
            }
            let v2 = |n: &str, v: (f32, f32)| {
                if let Some(l) = p.loc(n) {
                    gl.uniform_2_f32(Some(l), v.0, v.1)
                }
            };
            v2("uFromScale", fs);
            v2("uToScale", ts);
            v2("uFromMax", fm);
            v2("uToMax", tm);
            v2("uOutRes", (w as f32, h as f32));
            if let Some(l) = p.loc("progress") {
                gl.uniform_1_f32(Some(l), progress);
            }
            if let Some(l) = p.loc("ratio") {
                gl.uniform_1_f32(Some(l), w as f32 / h as f32);
            }
            if let Some(l) = p.loc("sg_flip") {
                gl.uniform_1_i32(Some(l), flip as i32);
            }
            if let Some((tex, luma, invert)) = matte {
                gl.active_texture(glow::TEXTURE2);
                gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                if let Some(l) = p.loc("sg_matte") {
                    gl.uniform_1_i32(Some(l), 2);
                }
                if let Some(l) = p.loc("sg_matte_luma") {
                    gl.uniform_1_f32(Some(l), luma as i32 as f32);
                }
                if let Some(l) = p.loc("sg_matte_invert") {
                    gl.uniform_1_f32(Some(l), invert as i32 as f32);
                }
            }
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            if matte.is_some() {
                gl.active_texture(glow::TEXTURE2);
                gl.bind_texture(glow::TEXTURE_2D, None);
            }
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }
}
