//! Textures: decode queue, banded uploads in `pump`, readiness, LRU eviction.

use super::*;

impl Renderer {
    /// Resident texture bytes (images, text and footage), for tests.
    pub fn texture_bytes(&self) -> usize {
        self.textures
            .values()
            .filter_map(|e| match e.state {
                TexState::Ready { bytes, .. } => Some(bytes),
                _ => None,
            })
            .sum()
    }

    /// Textures fully uploaded and held on the GPU.
    pub fn textures_ready(&self) -> usize {
        self.textures
            .values()
            .filter(|e| matches!(e.state, TexState::Ready { .. }))
            .count()
    }

    /// (loaded, total) textures for the current version (failed ones count as loaded).
    pub fn texture_progress(&self) -> (usize, usize) {
        let keys: std::collections::HashSet<u64> = self
            .wanted
            .iter()
            .flat_map(|k| k.channels.iter().flatten().map(|c| c.tex))
            // Another pass's picture or a footage channel (loaded per frame): not a
            // texture to load.
            .filter(|k| !is_pass_key(*k) && !is_footage_marker(*k))
            .collect();
        let done = keys
            .iter()
            .filter(|k| {
                matches!(
                    self.textures.get(*k).map(|e| &e.state),
                    Some(TexState::Ready { .. } | TexState::Failed(_))
                )
            })
            .count();
        (done, keys.len())
    }

    /// Textures sorted by how soon the playhead needs them (seconds).
    pub(super) fn texture_order(
        &self,
        movie: &Movie,
        t: f32,
        comp_dist: &[f32],
    ) -> Vec<(u64, f32)> {
        let mut v: Vec<(u64, f32)> = vec![];
        for (i, shot) in movie.shots.iter().enumerate() {
            let Some(k) = self.wanted.get(i) else {
                continue;
            };
            let d = distance(movie, shot, t);
            v.extend(
                k.channels
                    .iter()
                    .flatten()
                    .filter(|c| !is_pass_key(c.tex) && c.footage.is_none())
                    .map(|c| (c.tex, d)),
            );
        }
        // Comps (#74): their textures are as near as the comp's nearest user.
        for (c, &d) in comp_dist.iter().enumerate() {
            if !d.is_finite() {
                continue;
            }
            for k in self.seq_items(movie, c + 1) {
                v.extend(
                    k.channels
                        .iter()
                        .flatten()
                        .filter(|c| !is_pass_key(c.tex) && c.footage.is_none())
                        .map(|c| (c.tex, d)),
                );
            }
        }
        let n = movie.shots.len();
        let spans = movie
            .layers
            .iter()
            .enumerate()
            .map(|(j, l)| (n + j, layer_distance(movie, l, t)))
            .chain(movie.buffers.iter().enumerate().map(|(b, x)| {
                (
                    self.buf_base[0] + b,
                    span_distance(movie, x.start, x.end, t),
                )
            }));
        for (i, d) in spans {
            let Some(k) = self.wanted.get(i) else {
                continue;
            };
            v.extend(
                k.channels
                    .iter()
                    .flatten()
                    .filter(|c| !is_pass_key(c.tex) && c.footage.is_none())
                    .map(|c| (c.tex, d)),
            );
        }
        v.sort_by(|a, b| a.1.total_cmp(&b.1));
        let mut seen = std::collections::HashSet::new();
        v.retain(|(k, _)| seen.insert(*k));
        v
    }

    /// Texture housekeeping, never blocking: collect finished decodes, upload at most
    /// `upload_budget` bytes of rows (nearest first), start decodes within the
    /// lookahead (everything with `background`), a couple at a time.
    pub(super) fn pump_textures(&mut self, order: &[(u64, f32)], background: bool) {
        if self.textures.is_empty() {
            return;
        }
        if let Some(d) = &self.decoder {
            while let Ok((k, r)) = d.results.try_recv() {
                if let Some(e) = self.textures.get_mut(&k)
                    && matches!(e.state, TexState::Decoding)
                {
                    e.state = match r {
                        Ok(img) => TexState::Decoded(img),
                        Err(m) => TexState::Failed(m),
                    };
                }
            }
        }
        let wanted = |d: f32| background || d <= LOOKAHEAD;
        // Images and text share one budget, footage has its own (#70), so a big still
        // texture never starves the frames under the playhead, nor they it.
        let mut budget = self.upload_budget;
        let mut footage_budget = self.footage_budget;
        for (k, d) in order {
            if (budget == 0 && footage_budget == 0) || !wanted(*d) {
                break;
            }
            let Some(footage) = self.textures.get(k).map(|e| e.footage) else {
                continue;
            };
            if footage {
                if footage_budget > 0 {
                    footage_budget = self.upload_some(*k, footage_budget);
                }
            } else if budget > 0 {
                budget = self.upload_some(*k, budget);
            }
        }
        // Decodes in flight, (images and text, footage frames and their CPU bytes).
        let in_flight = |r: &Renderer| {
            r.textures
                .values()
                .filter(|e| {
                    matches!(
                        e.state,
                        TexState::Decoding | TexState::Decoded(_) | TexState::Uploading { .. }
                    )
                })
                .fold((0, 0, 0), |(n, f, b), e| {
                    if e.footage {
                        let bytes = match &e.source {
                            TexSource::Frame(_, Some((w, h))) => w * h * 4,
                            _ => 0,
                        };
                        (n, f + 1, b + bytes)
                    } else {
                        (n + 1, f, b)
                    }
                })
        };
        let workers = self.decoder.as_ref().map_or(1, |d| d.workers);
        let footage_cap = (workers * FOOTAGE_IN_FLIGHT_PER_WORKER).max(FOOTAGE_MIN_IN_FLIGHT);
        let (mut stills, mut frames, mut frame_bytes) = in_flight(self);
        for (k, d) in order {
            if !wanted(*d) || (stills >= TEXTURES_IN_FLIGHT && frames >= footage_cap) {
                break;
            }
            let Some(e) = self.textures.get_mut(k) else {
                continue;
            };
            if !matches!(e.state, TexState::Idle) {
                continue;
            }
            if e.footage {
                let bytes = match &e.source {
                    TexSource::Frame(_, Some((w, h))) => w * h * 4,
                    _ => 0,
                };
                let over_cpu =
                    frames >= FOOTAGE_MIN_IN_FLIGHT && frame_bytes + bytes > FOOTAGE_CPU_BYTES;
                if frames >= footage_cap || over_cpu {
                    continue;
                }
                frames += 1;
                frame_bytes += bytes;
            } else {
                if stills >= TEXTURES_IN_FLIGHT {
                    continue;
                }
                stills += 1;
            }
            {
                let job = (*k, e.source.clone(), e.label.clone(), e.vflip);
                e.state = TexState::Decoding;
                let d = self.decoder.get_or_insert_with(Decoder::new);
                if d.jobs.send(job).is_err() {
                    self.textures.get_mut(k).unwrap().state =
                        TexState::Failed("the texture decode thread stopped".into());
                }
            }
        }
    }

    /// Upload up to `budget` bytes of texture `k` (at least one row, so a wide image
    /// still makes progress), starting its upload if it's decoded. Returns the budget left.
    pub(super) fn upload_some(&mut self, k: u64, budget: usize) -> usize {
        let Some(e) = self.textures.get_mut(&k) else {
            return budget;
        };
        if !matches!(e.state, TexState::Decoded(_) | TexState::Uploading { .. }) {
            return budget;
        }
        let (srgb, mipmap, label) = (e.srgb, e.mipmap, e.label.clone());
        let (tex, img, row) = match std::mem::replace(&mut e.state, TexState::Idle) {
            TexState::Decoded(img) => match self.begin_upload(&img, srgb, mipmap, &label) {
                Ok(tex) => (tex, img, 0),
                Err(m) => {
                    self.textures.get_mut(&k).unwrap().state = TexState::Failed(m);
                    return budget;
                }
            },
            TexState::Uploading { tex, img, row } => (tex, img, row),
            other => {
                self.textures.get_mut(&k).unwrap().state = other;
                return budget;
            }
        };
        let row_bytes = img.width * 4;
        let rows = (budget / row_bytes).clamp(1, img.height - row);
        self.upload_rows(tex, &img, row, rows, mipmap);
        let row = row + rows;
        let state = if row == img.height {
            TexState::Ready {
                tex,
                w: img.width,
                h: img.height,
                bytes: crate::texture::frame_bytes(img.width, img.height, mipmap),
                meta: img.meta,
            }
        } else {
            TexState::Uploading { tex, img, row }
        };
        self.textures.get_mut(&k).unwrap().state = state;
        budget.saturating_sub(rows * row_bytes)
    }

    /// Make texture `k` ready (or failed) now: decode on this thread if needed and
    /// upload the rest at once. Only for paused/scrubbing and headless renders.
    pub(super) fn load_texture_now(&mut self, k: u64) {
        let Some(e) = self.textures.get_mut(&k) else {
            return;
        };
        match &e.state {
            TexState::Ready { .. } | TexState::Failed(_) => return,
            TexState::Idle | TexState::Decoding => {
                // A worker may be decoding it too; its result is ignored once this is done.
                e.state = match e.source.load(&e.label, e.vflip) {
                    Ok(img) => TexState::Decoded(img),
                    Err(m) => TexState::Failed(m),
                };
            }
            _ => {}
        }
        if e.footage {
            self.blocking_footage_loads += 1;
        } else {
            self.blocking_texture_loads += 1;
        }
        self.upload_some(k, usize::MAX);
    }

    /// Allocate level 0 (`RGBA8`, or `SRGB8_ALPHA8` with `srgb`) for `img`, or reuse a
    /// spare texture of that size and format (footage frames turn over constantly:
    /// `tex_sub_image_2d` into a reused texture, no reallocation; GL orders the new
    /// upload after earlier draws that sampled the old contents, so no fence).
    pub(super) fn begin_upload(
        &mut self,
        img: &DecodedImage,
        srgb: bool,
        mipmap: bool,
        label: &str,
    ) -> std::result::Result<glow::Texture, String> {
        let max = self.max_texture_size;
        if img.width > max || img.height > max {
            return Err(format!(
                "{label}: {}×{} is larger than this GPU's limit of {max} (GL_MAX_TEXTURE_SIZE)",
                img.width, img.height
            ));
        }
        let slot = (img.width, img.height, srgb, mipmap);
        if let Some(i) = self.spare.iter().position(|(k, _)| *k == slot) {
            return Ok(self.spare.swap_remove(i).1);
        }
        self.texture_creates += 1;
        let gl = &self.gl;
        unsafe {
            let tex = gl.new_texture()?;
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            let internal = if srgb {
                glow::SRGB8_ALPHA8
            } else {
                glow::RGBA8
            };
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                internal as i32,
                img.width as i32,
                img.height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            // Sampling state comes from sampler objects; these only keep the texture
            // complete if one is ever sampled without.
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                if mipmap {
                    glow::LINEAR_MIPMAP_LINEAR
                } else {
                    glow::LINEAR
                } as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
            Ok(tex)
        }
    }

    /// Submit rows `row .. row + rows` of level 0 from client memory (the driver copies
    /// them before returning: nothing to wait for), and the mipmaps after the last band.
    /// The unpack state is set for the band and put back to GL's defaults, and no
    /// unpack buffer is bound (with one bound, the slice would be read as an offset).
    pub(super) fn upload_rows(
        &mut self,
        tex: glow::Texture,
        img: &DecodedImage,
        row: usize,
        rows: usize,
        mipmap: bool,
    ) {
        let gl = &self.gl;
        let stride = img.width * 4;
        unsafe {
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
            gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 0);
            gl.pixel_store_i32(glow::UNPACK_SKIP_PIXELS, 0);
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                row as i32,
                img.width as i32,
                rows as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&img.rgba[row * stride..(row + rows) * stride])),
            );
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            if mipmap && row + rows == img.height {
                gl.generate_mipmap(glow::TEXTURE_2D);
            }
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
        self.texture_bands += 1;
    }

    pub(super) fn texture_ready(&self, k: u64) -> bool {
        is_pass_key(k)
            || is_footage_marker(k)
            || matches!(
                self.textures.get(&k).map(|e| &e.state),
                Some(TexState::Ready { .. })
            )
    }

    /// Keep resident texture memory under the budget: drop the least recently used
    /// textures that the playhead won't need soon and no fallback holds. They come
    /// back through the same decode and upload, identically.
    pub(super) fn evict_textures(&mut self, order: &[(u64, f32)]) {
        // Footage frames the playhead no longer wants (it moved on, or jumped) that
        // aren't uploaded yet leave the store, so they hold neither CPU memory nor an
        // in-flight slot; a decode still running for one is dropped when it returns.
        if self.textures.values().any(|e| e.footage) {
            let wanted: std::collections::HashSet<u64> = order.iter().map(|(k, _)| *k).collect();
            let stale: Vec<u64> = self
                .textures
                .iter()
                .filter(|(k, e)| {
                    e.footage
                        && !wanted.contains(k)
                        && matches!(
                            e.state,
                            TexState::Idle
                                | TexState::Decoding
                                | TexState::Decoded(_)
                                | TexState::Uploading { .. }
                        )
                })
                .map(|(k, _)| *k)
                .collect();
            for k in stale {
                if let Some(e) = self.textures.remove(&k) {
                    self.delete_texture_state(e.state);
                }
            }
        }
        let mut total: usize = self
            .textures
            .values()
            .map(|e| match e.state {
                TexState::Ready { bytes, .. } => bytes,
                _ => 0,
            })
            .sum();
        if total <= self.texture_budget {
            return;
        }
        let near: std::collections::HashSet<u64> = order
            .iter()
            .filter(|(_, d)| *d <= LOOKAHEAD * 1.5)
            .map(|(k, _)| *k)
            .collect();
        let held: std::collections::HashSet<u64> = self
            .fallback
            .values()
            .flat_map(|f| f.channels.iter().flatten().map(|c| c.tex))
            .collect();
        // Footage frames the playhead has passed go first (they turn over anyway),
        // then the least recently used.
        let mut candidates: Vec<(u64, (bool, u64), usize)> = self
            .textures
            .iter()
            .filter_map(|(k, e)| match e.state {
                TexState::Ready { bytes, .. } if !near.contains(k) && !held.contains(k) => {
                    Some((*k, (!e.footage, e.last_used), bytes))
                }
                _ => None,
            })
            .collect();
        candidates.sort_by_key(|(_, used, _)| *used);
        for (k, _, bytes) in candidates {
            if total <= self.texture_budget {
                break;
            }
            let e = self.textures.get_mut(&k).unwrap();
            let footage = e.footage;
            let slot = (e.srgb, e.mipmap);
            let old = std::mem::replace(&mut e.state, TexState::Idle);
            if footage {
                // Footage frames leave the store (their footage entry remembers the key
                // and re-adds them if needed again); their texture waits for the next
                // frame of the same size.
                self.textures.remove(&k);
                if let TexState::Ready { tex, w, h, .. } = old
                    && self.spare.len() < SPARE_TEXTURES
                {
                    self.spare.push(((w, h, slot.0, slot.1), tex));
                    total -= bytes;
                    continue;
                }
            }
            self.delete_texture_state(old);
            total -= bytes;
        }
    }

    pub(super) fn delete_texture_state(&self, s: TexState) {
        if let TexState::Ready { tex, .. } | TexState::Uploading { tex, .. } = s {
            unsafe { self.gl.free_texture(tex) };
        }
    }
}
