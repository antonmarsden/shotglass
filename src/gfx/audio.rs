//! Sound on the GPU: audio programs, submit → PBO + fence → poll/wait, buffer recycling.

use super::*;

impl Renderer {
    /// Compile (or reuse) an audio program. Returns its key for `render_audio`.
    pub fn audio_program(
        &mut self,
        src: &Assembled,
        what: &str,
    ) -> std::result::Result<u64, ShaderError> {
        let k = hash(&src.text);
        if !self.audio.contains_key(&k) {
            let p = self.compile(src).map_err(|m| ShaderError {
                what: what.into(),
                messages: m,
            })?;
            self.audio.insert(k, p);
        }
        Ok(k)
    }

    /// Delete audio programs that aren't in `keep`.
    pub fn retain_audio(&mut self, keep: &[u64]) {
        let drop: Vec<u64> = self
            .audio
            .keys()
            .filter(|k| !keep.contains(k))
            .copied()
            .collect();
        for k in drop {
            if let Some(p) = self.audio.remove(&k) {
                unsafe { self.gl.free_program(p.prog) };
            }
        }
    }

    /// Queue audio program `key` for global samples `first .. first + count`
    /// (count ≤ AUDIO_BLOCK). `inputs` = (music, sfx, global index of their first sample),
    /// bound for the mix pass as `sg_music` / `sg_sfx`.
    ///
    /// Never waits for the GPU: the block is drawn, read back into a pixel pack buffer
    /// and fenced. Collect it with `poll_audio` (never blocks) or `wait_audio` (blocks).
    /// Only WebGL 2 features are used (fences, PBOs), so a web build can do the same.
    pub fn submit_audio(
        &mut self,
        key: u64,
        first: usize,
        count: usize,
        sample_rate: u32,
        shot: (f32, f32),
        inputs: Option<(&[f32], &[f32], i64)>,
    ) -> std::result::Result<AudioTicket, String> {
        let r = self.submit_audio_inner(key, first, count, sample_rate, shot, inputs);
        self.audit("submit_audio");
        r
    }

    pub(super) fn submit_audio_inner(
        &mut self,
        key: u64,
        first: usize,
        count: usize,
        sample_rate: u32,
        shot: (f32, f32),
        inputs: Option<(&[f32], &[f32], i64)>,
    ) -> std::result::Result<AudioTicket, String> {
        const W: i32 = 1024;
        assert!(count <= AUDIO_BLOCK);
        let rows = (count as i32 + W - 1) / W;
        let gl = self.gl.clone();
        let Some(prog) = self.audio.get(&key) else {
            return Err("audio program not compiled".into());
        };
        unsafe {
            if self.audio_target.is_none() {
                let tex = gl.new_texture()?;
                gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA32F as i32,
                    W,
                    (AUDIO_BLOCK as i32) / W,
                    0,
                    glow::RGBA,
                    glow::FLOAT,
                    glow::PixelUnpackData::Slice(None),
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MIN_FILTER,
                    glow::NEAREST as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MAG_FILTER,
                    glow::NEAREST as i32,
                );
                gl.bind_texture(glow::TEXTURE_2D, None);
                let fbo = gl.new_framebuffer().inspect_err(|_| gl.free_texture(tex))?;
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                gl.framebuffer_texture_2d(
                    glow::FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0,
                    glow::TEXTURE_2D,
                    Some(tex),
                    0,
                );
                let ok =
                    gl.check_framebuffer_status(glow::FRAMEBUFFER) == glow::FRAMEBUFFER_COMPLETE;
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                if !ok {
                    gl.free_framebuffer(fbo);
                    gl.free_texture(tex);
                    return Err(
                        "this GPU can't render to float textures, so GLSL sound is unavailable"
                            .into(),
                    );
                }
                self.audio_target = Some((fbo, tex));
            }
            let (fbo, _) = self.audio_target.unwrap();
            gl.use_program(Some(prog.prog));
            let mut bindings = AudioSubmit {
                gl: &gl,
                input_tex: vec![],
            };
            let mut staging = None;
            let t_upload = std::time::Instant::now();
            if let Some((music, sfx, base)) = inputs {
                let rows = (music.len() / 2).max(1).div_ceil(TRACK_WIDTH);
                if rows > self.max_texture_size {
                    return Err(format!(
                        "the soundtrack is too long for one texture ({rows} rows, GPU max {})",
                        self.max_texture_size
                    ));
                }
                if self.persistent {
                    // Write the inputs into mapped memory and copy them into textures
                    // on the GPU: the driver never reads our memory, so nothing waits.
                    let row = TRACK_WIDTH * 8;
                    let st = take_buf(
                        &gl,
                        &mut self.audio_staging,
                        self.persistent,
                        false,
                        (2 * rows * row) as i32,
                    )?;
                    for (k, data) in [music, sfx].into_iter().enumerate() {
                        let bytes =
                            std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 4);
                        let off = k * rows * row;
                        let dst = std::slice::from_raw_parts_mut(st.ptr.add(off), rows * row);
                        dst[..bytes.len()].copy_from_slice(bytes);
                        dst[bytes.len()..].fill(0);
                    }
                    // Allocate before binding the unpack buffer: tex_image_2d uses a
                    // null client pointer to reserve storage for the texture.
                    for unit in 0..self.audio_inputs.len() {
                        if self.audio_inputs[unit].is_some_and(|(_, r)| r >= rows) {
                            continue;
                        }
                        gl.active_texture(glow::TEXTURE0 + unit as u32);
                        let tex = new_track_texture(&gl, rows)?;
                        if let Some((old, _)) = self.audio_inputs[unit].replace((tex, rows)) {
                            gl.free_texture(old);
                        }
                    }
                    gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(st.buf));
                    for (unit, name) in ["sg_music", "sg_sfx"].into_iter().enumerate() {
                        gl.active_texture(glow::TEXTURE0 + unit as u32);
                        let (tex, _) = self.audio_inputs[unit].unwrap();
                        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                        gl.tex_sub_image_2d(
                            glow::TEXTURE_2D,
                            0,
                            0,
                            0,
                            TRACK_WIDTH as i32,
                            rows as i32,
                            glow::RG,
                            glow::FLOAT,
                            glow::PixelUnpackData::BufferOffset((unit * rows * row) as u32),
                        );
                        if let Some(l) = prog.loc(name) {
                            gl.uniform_1_i32(Some(l), unit as i32);
                        }
                    }
                    gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
                    staging = Some(st);
                } else {
                    for (unit, (name, data)) in [("sg_music", music), ("sg_sfx", sfx)]
                        .into_iter()
                        .enumerate()
                    {
                        // select the unit first: upload_track binds the new texture to it
                        gl.active_texture(glow::TEXTURE0 + unit as u32);
                        let tex = upload_track(&gl, data, self.max_texture_size)?;
                        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                        if let Some(l) = prog.loc(name) {
                            gl.uniform_1_i32(Some(l), unit as i32);
                        }
                        bindings.input_tex.push(tex);
                    }
                }
                if let Some(l) = prog.loc("sg_len") {
                    gl.uniform_1_i32(Some(l), (music.len() / 2) as i32);
                }
                if let Some(l) = prog.loc("sg_base") {
                    gl.uniform_1_i32(Some(l), base as i32);
                }
            }
            self.audio_timing.upload_ms += ms_since(t_upload);
            let t_draw = std::time::Instant::now();
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.viewport(0, 0, W, rows);
            gl.disable(glow::BLEND);
            gl.bind_vertex_array(Some(self.vao));
            let f1 = |n: &str, v: f32| {
                if let Some(l) = prog.loc(n) {
                    gl.uniform_1_f32(Some(l), v)
                }
            };
            f1("iSampleRate", sample_rate as f32);
            f1("iShotStart", shot.0);
            f1("iShotDuration", shot.1);
            if let Some(l) = prog.loc("iBlockWidth") {
                gl.uniform_1_i32(Some(l), W);
            }
            if let Some(l) = prog.loc("iBlockOffset") {
                gl.uniform_1_i32(Some(l), first as i32);
            }
            // Normally one draw. If the last block was slow (a heavy mainSound on a
            // slow GPU), draw in bands of a few rows, flushed one by one, so no single
            // job runs long enough to trip the driver's watchdog. Flushing costs
            // batching, so it's only done when needed. gl_FragCoord stays
            // window-relative, so the sample index is the same either way.
            let submitted = std::time::Instant::now();
            let band = if self.audio_banded { 4 } else { rows };
            let mut y0 = 0;
            while y0 < rows {
                let h = band.min(rows - y0);
                gl.viewport(0, y0, W, h);
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
                if self.audio_banded {
                    gl.flush();
                }
                y0 += h;
            }
            // Read back into a buffer on the GPU: queued like a draw, no wait.
            let pbo = take_buf(
                &gl,
                &mut self.audio_pbos,
                self.persistent,
                true,
                W * rows * 16,
            )?;
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(pbo.buf));
            gl.read_pixels(
                0,
                0,
                W,
                rows,
                glow::RGBA,
                glow::FLOAT,
                glow::PixelPackData::BufferOffset(0),
            );
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            self.audio_timing.draw_ms += ms_since(t_draw);
            let t_fence = std::time::Instant::now();
            let fence = AudioFence {
                gl: gl.clone(),
                sync: gl.new_fence(glow::SYNC_GPU_COMMANDS_COMPLETE, 0)?,
            };
            // Make sure the fence reaches the GPU without anyone waiting on it.
            gl.flush();
            self.audio_timing.fence_ms += ms_since(t_fence);
            self.audio_ticket += 1;
            self.audio_jobs.insert(
                self.audio_ticket,
                AudioJob {
                    pbo,
                    staging,
                    fence,
                    count,
                    submitted,
                },
            );
            Ok(AudioTicket(self.audio_ticket))
        }
    }

    /// The samples of a submitted block if the GPU has finished it, without waiting.
    /// None = not yet; the ticket stays valid. Some = done; the ticket is used up.
    pub fn poll_audio(&mut self, t: AudioTicket) -> Option<std::result::Result<Vec<f32>, String>> {
        let r = self.poll_audio_inner(t);
        self.audit("poll_audio");
        r
    }

    pub(super) fn poll_audio_inner(
        &mut self,
        t: AudioTicket,
    ) -> Option<std::result::Result<Vec<f32>, String>> {
        let Some(job) = self.audio_jobs.get(&t.0) else {
            return Some(Err("unknown sound block".into()));
        };
        let t_poll = std::time::Instant::now();
        let status = unsafe { self.gl.client_wait_sync(job.fence.sync, 0, 0) };
        self.audio_timing.poll_ms += ms_since(t_poll);
        match status {
            glow::ALREADY_SIGNALED | glow::CONDITION_SATISFIED => {}
            glow::WAIT_FAILED => {
                self.cancel_audio(t);
                return Some(Err("waiting for a sound block failed".into()));
            }
            _ => return None,
        }
        let job = self.audio_jobs.remove(&t.0).unwrap();
        Some(self.finish_audio(job, false))
    }

    /// The samples of a submitted block, waiting for the GPU if needed. This is a
    /// GPU→CPU sync (counted in `blocking_audio_reads`): only for headless use, or
    /// for the chunk under a paused playhead.
    pub fn wait_audio(&mut self, t: AudioTicket) -> std::result::Result<Vec<f32>, String> {
        let r = self.wait_audio_inner(t);
        self.audit("wait_audio");
        r
    }

    pub(super) fn wait_audio_inner(
        &mut self,
        t: AudioTicket,
    ) -> std::result::Result<Vec<f32>, String> {
        let job = self.audio_jobs.remove(&t.0).ok_or("unknown sound block")?;
        self.blocking_audio_reads += 1;
        loop {
            match unsafe {
                self.gl
                    .client_wait_sync(job.fence.sync, glow::SYNC_FLUSH_COMMANDS_BIT, 100_000_000)
            } {
                glow::ALREADY_SIGNALED | glow::CONDITION_SATISFIED => break,
                glow::WAIT_FAILED => {
                    return Err("waiting for a sound block failed".into());
                }
                _ => {}
            }
        }
        self.finish_audio(job, true)
    }

    /// Forget a submitted block (its result is no longer wanted).
    pub fn cancel_audio(&mut self, t: AudioTicket) {
        // The GPU may still be using these buffers, so only completed jobs
        // can return their storage to the pools.
        self.audio_jobs.remove(&t.0);
    }

    /// Read a finished block's samples and recycle its buffers.
    pub(super) fn finish_audio(
        &mut self,
        job: AudioJob,
        waited: bool,
    ) -> std::result::Result<Vec<f32>, String> {
        let n = job.count;
        let mut out = Vec::with_capacity(n * 2);
        let t_read = std::time::Instant::now();
        unsafe {
            let mapping = if job.pbo.ptr.is_null() {
                // The fence has passed, so mapping doesn't wait for the GPU (it is still
                // a driver round trip). WebGL 2 would use getBufferSubData here.
                let gl = &job.pbo.gl;
                gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(job.pbo.buf));
                Some(AudioReadMapping {
                    buffer: &job.pbo,
                    ptr: gl.map_buffer_range(
                        glow::PIXEL_PACK_BUFFER,
                        0,
                        (n * 16) as i32,
                        glow::MAP_READ_BIT,
                    ),
                })
            } else {
                // Persistent and coherent: the fence makes the samples available.
                None
            };
            let p = mapping.as_ref().map_or(job.pbo.ptr, |m| m.ptr);
            if p.is_null() {
                return Err("mapping a finished sound block failed".into());
            }
            // RGBA32F texels. Mappings are aligned on recent GL versions.
            if (p as usize).is_multiple_of(std::mem::align_of::<f32>()) {
                let px = std::slice::from_raw_parts(p as *const [f32; 4], n);
                for t in px {
                    out.push(sanitize(t[0]));
                    out.push(sanitize(t[1]));
                }
            } else {
                let px = std::slice::from_raw_parts(p as *const [u8; 16], n);
                for t in px {
                    let f = |i: usize| f32::from_ne_bytes(t[i..i + 4].try_into().unwrap());
                    out.push(sanitize(f(0)));
                    out.push(sanitize(f(4)));
                }
            }
        }
        self.audio_timing.read_ms += ms_since(t_read);
        // Normally one draw per block; after a slow one (a heavy mainSound on a slow
        // GPU), bands (see submit_audio). Judged by the time from submit to result, with
        // no timer queries (each would be another driver round trip). A blocking wait
        // measures the block itself, as before. A polled block's time also includes the
        // frames until it was polled and the scene queued ahead of it, so the
        // thresholds are looser: only a clearly heavy mainSound switches banding on.
        let ms = job.submitted.elapsed().as_secs_f32() * 1000.0;
        let (on, off) = if waited { (50.0, 20.0) } else { (250.0, 100.0) };
        self.audio_banded = if self.audio_banded { ms > off } else { ms > on };
        self.recycle_audio_buffers(job);
        Ok(out)
    }

    pub(super) fn recycle_audio_buffers(&mut self, job: AudioJob) {
        for (pool, b) in [(true, Some(job.pbo)), (false, job.staging)] {
            let Some(b) = b else { continue };
            let pool = if pool {
                &mut self.audio_pbos
            } else {
                &mut self.audio_staging
            };
            if pool.len() < 8 {
                pool.push(b);
            }
        }
    }

    /// Use persistently mapped buffers for sound streaming (the default where the
    /// driver has GL 4.4 / ARB_buffer_storage), or not (tests). Returns whether they're on.
    pub fn set_persistent_buffers(&mut self, on: bool) -> bool {
        self.persistent = on && self.can_persist;
        self.persistent
    }

    pub fn persistent_buffers(&self) -> bool {
        self.persistent
    }

    /// Sound blocks submitted and not yet collected.
    pub fn audio_in_flight(&self) -> usize {
        self.audio_jobs.len()
    }
}
