//! Programs: prioritised lazy compiles, readiness (`keys_ready`, `usable`), the program cache (disk + LRU).

use super::*;

impl Renderer {
    /// Program keys sorted by how soon the playhead needs them (seconds).
    pub(super) fn priority(&self, movie: &Movie, t: f32, comp_dist: &[f32]) -> Vec<(u64, f32)> {
        let mut v: Vec<(u64, f32)> = vec![];
        for (i, shot) in movie.shots.iter().enumerate() {
            let Some(k) = self.wanted.get(i) else {
                continue;
            };
            let d = distance(movie, shot, t);
            v.push((k.scene, d));
            if let Some(tr) = k.trans {
                v.push((tr, d));
            }
        }
        let n = movie.shots.len();
        for (j, layer) in movie.layers.iter().enumerate() {
            if let Some(k) = self.wanted.get(n + j) {
                let d = layer_distance(movie, layer, t);
                v.push((k.scene, d));
                if let Some(b) = k.trans {
                    v.push((b, d));
                }
            }
        }
        for (b, buf) in movie.buffers.iter().enumerate() {
            if let Some(k) = self.wanted.get(self.buf_base[0] + b) {
                let d = span_distance(movie, buf.start, buf.end, t);
                v.push((k.scene, d));
                if let Some(i) = k.trans {
                    v.push((i, d));
                }
            }
        }
        // Comps (#74): every program of a comp is as near as its nearest user.
        for (c, &d) in comp_dist.iter().enumerate() {
            if !d.is_finite() {
                continue;
            }
            for k in self.seq_items(movie, c + 1) {
                v.push((k.scene, d));
                if let Some(tr) = k.trans {
                    v.push((tr, d));
                }
            }
        }
        // The layer composite is needed as soon as any layer is near.
        if let Some(o) = self.over {
            let d = movie
                .layers
                .iter()
                .map(|l| layer_distance(movie, l, t))
                .chain(
                    comp_dist
                        .iter()
                        .enumerate()
                        .filter(|(c, _)| !movie.comps[*c].movie.layers.is_empty())
                        .map(|(_, d)| *d),
                )
                .fold(f32::INFINITY, f32::min);
            if d.is_finite() {
                v.push((o, d));
                if let Some(om) = self.over_matte {
                    v.push((om, d));
                }
            }
        }
        v.sort_by(|a, b| a.1.total_cmp(&b.1));
        let mut seen = std::collections::HashSet::new();
        v.retain(|(k, _)| seen.insert(*k));
        v
    }

    pub(super) fn state(&self, k: u64) -> Option<&State> {
        self.programs.get(&k).map(|e| &e.state)
    }

    pub(super) fn is_idle(&self, k: u64) -> bool {
        matches!(self.state(k), Some(State::Idle))
    }

    pub(super) fn keys_ready(&self, k: ShotKeys) -> bool {
        let ok = |k: u64| {
            matches!(
                self.programs.get(&k).map(|e| &e.state),
                Some(State::Ready(_))
            )
        };
        ok(k.scene)
            && k.trans.is_none_or(ok)
            && k.channels
                .iter()
                .flatten()
                .all(|c| self.texture_ready(c.tex))
    }

    /// The programs to draw shot `i` with: the current version if it compiled,
    /// otherwise the last working one. With `blocking`, the current version is
    /// compiled first, so the decision is made knowing whether it works.
    ///
    /// Footage (#70): the keys are resolved for film frame `n`, and a frame is usable
    /// only when *that* source frame is ready on every footage channel (or the fallback
    /// version has it): never a neighbouring frame, so playback shows only what `still`
    /// would.
    pub(super) fn usable(&mut self, i: usize, blocking: bool, n: u64) -> Option<ShotKeys> {
        let k = *self.wanted.get(i)?;
        let k = self.resolve(k, n);
        if blocking {
            self.compile_now(k.scene);
            if let Some(t) = k.trans {
                self.compile_now(t);
            }
            for c in k.channels.iter().flatten() {
                self.load_texture_now(c.tex);
            }
        }
        if self.keys_ready(k) {
            return Some(k);
        }
        let fb = self
            .wanted_ids
            .get(i)
            .and_then(|id| self.fallback.get(id))
            .copied();
        if let Some(f) = fb
            .map(|f| self.resolve(f, n))
            .filter(|f| self.keys_ready(*f))
        {
            return Some(f);
        }
        if !blocking
            && k.channels
                .iter()
                .flatten()
                .any(|c| c.footage.is_some() && !self.texture_ready(c.tex))
        {
            self.footage_misses += 1;
        }
        // Nothing working to show: with `blocking`, draw what we have (a failed
        // scene shows black, a failed texture reads black); otherwise wait.
        blocking.then_some(k)
    }

    pub(super) fn start(&mut self, k: u64) {
        let Some(e) = self.programs.get(&k) else {
            return;
        };
        if !matches!(e.state, State::Idle) {
            return;
        }
        let state = match self.begin_compile(&e.src) {
            Ok(p) => State::Pending(p),
            Err(m) => State::Failed(m),
        };
        self.compiles_started += 1;
        self.programs.get_mut(&k).unwrap().state = state;
    }

    /// Make program `k` ready (or failed) now, blocking if needed.
    pub(super) fn compile_now(&mut self, k: u64) {
        self.start(k);
        let Some(e) = self.programs.get_mut(&k) else {
            return;
        };
        if let State::Pending(_) = e.state {
            let State::Pending(p) = std::mem::replace(&mut e.state, State::Idle) else {
                unreachable!()
            };
            let from_binary = p.from_binary;
            self.binary_hits += from_binary as usize;
            let src = e.src.clone();
            let state = match self.finish_compile(p, &src) {
                Ok(prog) => {
                    if !from_binary {
                        self.save_binary(&prog, &src);
                    }
                    self.warm_up(&prog);
                    State::Ready(prog)
                }
                Err(m) => State::Failed(m),
            };
            self.programs.get_mut(&k).unwrap().state = state;
        }
    }

    pub(super) fn collect_garbage(&mut self) {
        let mut keep: std::collections::HashSet<u64> = self
            .copy
            .into_iter()
            .chain(self.over)
            .chain(self.over_matte)
            .collect();
        for k in self.wanted.iter().chain(self.fallback.values()) {
            keep.insert(k.scene);
            if let Some(t) = k.trans {
                keep.insert(t);
            }
        }
        let drop: Vec<u64> = self
            .programs
            .keys()
            .filter(|k| !keep.contains(k))
            .copied()
            .collect();
        for k in drop {
            if let Some(e) = self.programs.remove(&k) {
                self.delete_state(e.state);
            }
        }
        // Footage entries of the current version and the fallbacks, and their frames.
        let live: std::collections::HashSet<u64> = self
            .wanted
            .iter()
            .chain(self.fallback.values())
            .flat_map(|k| k.channels.iter().flatten().filter_map(|c| c.footage))
            .collect();
        self.footage.retain(|k, _| live.contains(k));
        let keep: std::collections::HashSet<u64> = self
            .wanted
            .iter()
            .chain(self.fallback.values())
            .flat_map(|k| k.channels.iter().flatten().map(|c| c.tex))
            .chain(
                self.footage
                    .values()
                    .flat_map(|f| f.frames.values().copied()),
            )
            .collect();
        let drop: Vec<u64> = self
            .textures
            .keys()
            .filter(|k| !keep.contains(k))
            .copied()
            .collect();
        for k in drop {
            if let Some(e) = self.textures.remove(&k) {
                self.delete_texture_state(e.state);
            }
        }
    }

    /// Keep at most MAX_READY compiled programs: drop the least recently used ones
    /// that the playhead won't need soon. They recompile (or reload from the
    /// on-disk cache) when needed again.
    pub(super) fn evict(&mut self, order: &[(u64, f32)]) {
        let ready: Vec<(u64, u64)> = self
            .programs
            .iter()
            .filter(|(_, e)| matches!(e.state, State::Ready(_)))
            .map(|(k, e)| (*k, e.last_used))
            .collect();
        if ready.len() <= MAX_READY {
            return;
        }
        let near: std::collections::HashSet<u64> = order
            .iter()
            .filter(|(_, d)| *d <= LOOKAHEAD * 1.5)
            .map(|(k, _)| *k)
            .collect();
        let mut candidates: Vec<(u64, u64)> = ready
            .into_iter()
            .filter(|(k, _)| {
                !near.contains(k)
                    && Some(*k) != self.copy
                    && Some(*k) != self.over
                    && Some(*k) != self.over_matte
                    && !self
                        .fallback
                        .values()
                        .any(|f| f.scene == *k || f.trans == Some(*k))
            })
            .collect();
        candidates.sort_by_key(|(_, used)| *used);
        let excess = self
            .programs
            .values()
            .filter(|e| matches!(e.state, State::Ready(_)))
            .count()
            - MAX_READY;
        for (k, _) in candidates.into_iter().take(excess) {
            let e = self.programs.get_mut(&k).unwrap();
            let old = std::mem::replace(&mut e.state, State::Idle);
            self.delete_state(old);
        }
    }

    pub(super) fn delete_state(&self, s: State) {
        unsafe {
            match s {
                State::Ready(p) => self.gl.free_program(p.prog),
                State::Pending(p) => {
                    for sh in p.shaders {
                        self.gl.free_shader(sh);
                    }
                    self.gl.free_program(p.prog);
                }
                _ => {}
            }
        }
    }

    pub(super) fn binary_path(&self, src: &Assembled) -> Option<PathBuf> {
        self.binary_dir
            .as_ref()
            .map(|d| d.join(format!("{:016x}.bin", hash(&src.text))))
    }

    pub(super) fn save_binary(&self, p: &Program, src: &Assembled) {
        let Some(path) = self.binary_path(src) else {
            return;
        };
        if let Some(b) = unsafe { self.gl.get_program_binary(p.prog) }
            && !b.buffer.is_empty()
        {
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            let mut bytes = b.format.to_le_bytes().to_vec();
            bytes.extend_from_slice(&b.buffer);
            let _ = std::fs::write(path, bytes);
        }
    }

    /// Kick off a compile: from the on-disk binary cache if possible, otherwise from
    /// source. With KHR_parallel_shader_compile this returns before the driver is done.
    pub(super) fn begin_compile(
        &self,
        src: &Assembled,
    ) -> std::result::Result<Pending, Vec<String>> {
        let gl = &self.gl;
        unsafe {
            if let Some(bytes) = self.binary_path(src).and_then(|p| std::fs::read(p).ok())
                && bytes.len() > 4
                && self.binary_formats.contains(&u32::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3],
                ]))
            {
                let format = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                let prog = gl.new_program().map_err(|e| vec![e])?;
                gl.program_binary(
                    prog,
                    &glow::ProgramBinary {
                        buffer: bytes[4..].to_vec(),
                        format,
                    },
                );
                if gl.get_program_link_status(prog) {
                    return Ok(Pending {
                        prog,
                        shaders: vec![],
                        from_binary: true,
                    });
                }
                gl.free_program(prog); // stale (driver update): compile from source
            }
            let vs_src = vertex_source(src.dialect);
            let vs = gl.new_shader(glow::VERTEX_SHADER).map_err(|e| vec![e])?;
            gl.shader_source(vs, &vs_src);
            gl.compile_shader(vs);
            let fs = gl.new_shader(glow::FRAGMENT_SHADER).map_err(|e| vec![e])?;
            gl.shader_source(fs, &src.text);
            gl.compile_shader(fs);
            let prog = gl.new_program().map_err(|e| vec![e])?;
            gl.attach_shader(prog, vs);
            gl.attach_shader(prog, fs);
            if self.binary_dir.is_some() {
                gl.program_binary_retrievable_hint(prog, true);
            }
            gl.link_program(prog);
            Ok(Pending {
                prog,
                shaders: vec![vs, fs],
                from_binary: false,
            })
        }
    }

    /// Wait for a compile to finish and collect errors and uniform locations.
    pub(super) fn finish_compile(
        &self,
        p: Pending,
        src: &Assembled,
    ) -> std::result::Result<Program, Vec<String>> {
        let gl = &self.gl;
        unsafe {
            let linked = gl.get_program_link_status(p.prog);
            let mut errors = vec![];
            if !linked {
                for (i, sh) in p.shaders.iter().enumerate() {
                    if !gl.get_shader_compile_status(*sh) {
                        let log = gl.get_shader_info_log(*sh);
                        if i == 0 {
                            errors.push(format!("vertex shader: {log}"));
                        } else {
                            errors.extend(remap_log(&log, &src.files));
                        }
                    }
                }
                if errors.is_empty() {
                    errors = remap_log(
                        &format!("link: {}", gl.get_program_info_log(p.prog)),
                        &src.files,
                    );
                }
                if errors.is_empty() {
                    errors.push("compile failed (driver gave no log)".into());
                }
            }
            for sh in &p.shaders {
                gl.detach_shader(p.prog, *sh);
                gl.free_shader(*sh);
            }
            if !linked {
                gl.free_program(p.prog);
                return Err(src.explain(errors));
            }
            let mut locs = HashMap::new();
            let n = gl.get_active_uniforms(p.prog);
            for i in 0..n {
                if let Some(u) = gl.get_active_uniform(p.prog, i) {
                    let name = u.name.trim_end_matches("[0]").to_string();
                    if let Some(l) = gl.get_uniform_location(p.prog, &name) {
                        locs.insert(name, l);
                    }
                }
            }
            // The channel samplers read units 0..N_CHANNELS, always: set once here (this
            // also covers programs loaded from the binary cache, whose uniforms reset).
            let units: Vec<(&glow::UniformLocation, i32)> = (0..N_CHANNELS)
                .filter_map(|n| locs.get(CHANNEL_UNIFORMS[n]).map(|l| (l, n as i32)))
                .collect();
            if !units.is_empty() {
                gl.use_program(Some(p.prog));
                for (l, n) in units {
                    gl.uniform_1_i32(Some(l), n);
                }
                gl.use_program(None);
            }
            Ok(Program { prog: p.prog, locs })
        }
    }

    /// Compile synchronously (sound shaders, which run once).
    pub(super) fn compile(&self, src: &Assembled) -> std::result::Result<Program, Vec<String>> {
        let p = self.begin_compile(src)?;
        self.finish_compile(p, src)
    }

    /// Draw a program once into a tiny viewport so the driver finishes any lazy work
    /// now, not on the first frame the shot appears.
    pub(super) fn warm_up(&mut self, p: &Program) {
        if self.seqs[0].a.is_none() {
            ensure_target(&self.gl, &mut self.seqs[0].a, 8, 8);
        }
        let t = self.seqs[0].a.as_ref().unwrap();
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(t.fbo));
            self.gl.viewport(0, 0, 4, 4);
            self.gl.bind_vertex_array(Some(self.vao));
            self.gl.use_program(Some(p.prog));
            if let Some(l) = p.loc("iResolution") {
                self.gl.uniform_3_f32(Some(l), 4.0, 4.0, 1.0);
            }
            if let Some(l) = p.loc("uOutRes") {
                self.gl.uniform_2_f32(Some(l), 4.0, 4.0);
            }
            // Samplers need something loadable on their units: with nothing bound,
            // macOS logs "unit 0 GLD_TEXTURE_INDEX_2D is unloadable".
            for n in 0..N_CHANNELS as u32 {
                self.gl.active_texture(glow::TEXTURE0 + n);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(self.dummy));
            }
            self.gl.draw_arrays(glow::TRIANGLES, 0, 3);
            for n in (0..N_CHANNELS as u32).rev() {
                self.gl.active_texture(glow::TEXTURE0 + n);
                self.gl.bind_texture(glow::TEXTURE_2D, None);
            }
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.use_program(None);
            self.gl.bind_vertex_array(None);
        }
    }

    pub(super) fn prog(&self, k: u64) -> Option<&Program> {
        match &self.programs.get(&k)?.state {
            State::Ready(p) => Some(p),
            _ => None,
        }
    }
}
