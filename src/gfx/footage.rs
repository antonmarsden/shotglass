//! Footage (#70): the prefetch order, frame textures, resolving a frame to its texture.

use super::*;

impl Renderer {
    /// The error of a footage frame the last render needed and couldn't decode (it
    /// read black). Headless commands fail on it, as `load_all` does for textures.
    pub fn footage_failure(&self) -> Option<&str> {
        self.footage_failure.as_deref()
    }

    /// Footage on the GPU (#70): (frames resident, their bytes).
    pub fn footage_resident(&self) -> (usize, usize) {
        self.textures
            .values()
            .filter(|e| e.footage)
            .filter_map(|e| match e.state {
                TexState::Ready { bytes, .. } => Some(bytes),
                _ => None,
            })
            .fold((0, 0), |(n, b), x| (n + 1, b + x))
    }

    /// Footage frames to load, by how soon they're needed (seconds), and this pump's
    /// footage upload budget (#70). For each shot and layer on screen, the frames of
    /// film frames `n − FOOTAGE_BEHIND .. n + W` (W: half a second, at least
    /// `footage::MIN_FRAMES`, fewer when frames are large); for each one starting within
    /// `FOOTAGE_HEADS` seconds (a loop film's first shots near the end of the period
    /// too), its first frames: cuts and loop seams then don't stall. Frames are mapped
    /// through `source_frame`, so repeats, speed and `end` count exactly.
    pub(super) fn footage_order(
        &mut self,
        movie: &Movie,
        t: f32,
        comp_dist: &[f32],
    ) -> Vec<(u64, f32)> {
        let n_now = film_frame(movie, t);
        let dt = movie.fps_den as f64 / movie.fps_num as f64;
        let n_shots = movie.shots.len();
        // (item index in `wanted`, distance, the film frame its frames start from,
        // how many frames: None = a window or head as below)
        let mut items: Vec<(usize, f32, u64, Option<u64>)> = vec![];
        for (i, shot) in movie.shots.iter().enumerate() {
            let d = distance(movie, shot, t);
            if d <= FOOTAGE_HEADS {
                let from = if d == 0.0 {
                    n_now
                } else {
                    movie.frame_at(shot.start).max(0) as u64
                };
                items.push((i, d, from, None));
            }
            // A frozen transition out of this shot shows its frame at the transition's
            // first frame for the whole transition: keep that one frame too.
            if let Some(next) = movie.shots.get(i + 1)
                && next.transition_in.as_ref().is_some_and(|tr| tr.freeze)
            {
                let d = distance(movie, next, t);
                if d <= FOOTAGE_HEADS {
                    items.push((i, d, movie.frame_at(next.start).max(0) as u64, Some(1)));
                }
            }
        }
        for (j, layer) in movie.layers.iter().enumerate() {
            let d = layer_distance(movie, layer, t);
            if d <= FOOTAGE_HEADS {
                let from = if d == 0.0 {
                    n_now
                } else {
                    movie.frame_at(layer.start).max(0) as u64
                };
                items.push((n_shots + j, d, from, None));
            }
        }
        // Buffers (#134): a step reads its footage at the frame it steps, like a layer.
        let buf_items = |m: &Movie, base: usize, tc: f32, n_c: u64, extra: f32| {
            m.buffers
                .iter()
                .enumerate()
                .filter_map(move |(b, x)| {
                    let d = span_distance(m, x.start, x.end, tc);
                    let from = if d == 0.0 {
                        n_c
                    } else {
                        m.frame_at(x.start).max(0) as u64
                    };
                    (d <= FOOTAGE_HEADS).then_some((base + b, d + extra, from, None))
                })
                .collect::<Vec<_>>()
        };
        if let Some(&base) = self.buf_base.first() {
            items.extend(buf_items(movie, base, t, n_now, 0.0));
        }
        // Comps (#74): around each instance drawn last frame (comp time runs on with the
        // film's, as it does when shown at speed 1), and the starts of comps coming up.
        let clocks = comp_clocks(movie, t, &self.comp_seen);
        for (c, (comp, mut clocks)) in movie.comps.iter().zip(clocks).enumerate() {
            let Some(&base) = self.seq_base.get(c + 1) else {
                continue;
            };
            let m = &comp.movie;
            if clocks.is_empty() && comp_dist[c] <= FOOTAGE_HEADS {
                clocks.push((0.0, comp_dist[c]));
            }
            for (tc, extra) in clocks {
                let n_c = m.frame_at(tc).max(0) as u64;
                for (i, shot) in m.shots.iter().enumerate() {
                    let d = distance(m, shot, tc);
                    if d <= FOOTAGE_HEADS {
                        let from = if d == 0.0 {
                            n_c
                        } else {
                            m.frame_at(shot.start).max(0) as u64
                        };
                        items.push((base + i, d + extra, from, None));
                    }
                }
                for (j, layer) in m.layers.iter().enumerate() {
                    let d = layer_distance(m, layer, tc);
                    if d <= FOOTAGE_HEADS {
                        let from = if d == 0.0 {
                            n_c
                        } else {
                            m.frame_at(layer.start).max(0) as u64
                        };
                        items.push((base + m.shots.len() + j, d + extra, from, None));
                    }
                }
                if let Some(&bb) = self.buf_base.get(c + 1) {
                    items.extend(buf_items(m, bb, tc, n_c, extra));
                }
            }
        }
        let uses: Vec<(u64, f32, u64, Option<u64>)> = items
            .iter()
            .filter_map(|&(i, d, from, len)| Some((self.wanted.get(i)?, d, from, len)))
            .flat_map(|(k, d, from, len)| {
                k.channels
                    .iter()
                    .flatten()
                    .filter_map(move |c| c.footage.map(|f| (f, d, from, len)))
            })
            .collect();
        if uses.is_empty() {
            self.footage_budget = self.upload_budget;
            return vec![];
        }
        let half = ((FOOTAGE_WINDOW / dt).ceil() as u64).max(crate::footage::MIN_FRAMES as u64);
        let share = self.texture_budget / 2 / uses.len();
        let mut on_screen = 0;
        let mut out = vec![];
        for (fk, d, from, len) in uses {
            let Some(e) = self.footage.get(&fk) else {
                continue;
            };
            let bytes = e
                .scan
                .size
                .map_or(1, |(w, h)| crate::texture::frame_bytes(w, h, e.mipmap));
            let w = ((share / bytes) as u64).clamp(crate::footage::MIN_FRAMES as u64, half);
            let map = e.map;
            let range: Vec<u64> = if let Some(len) = len {
                (from..from + len).collect()
            } else if d == 0.0 {
                on_screen += bytes;
                (from.saturating_sub(FOOTAGE_BEHIND)..from + w).collect()
            } else {
                (from..from + w.div_ceil(2).max(crate::footage::MIN_FRAMES as u64)).collect()
            };
            for m in range {
                let Some(k) = map.source_frame(m) else {
                    continue;
                };
                let ahead = if m >= from {
                    (m - from) as f64 * dt
                } else {
                    (from - m) as f64 * dt * 2.0
                };
                let tex = self.frame_tex(fk, k);
                out.push((tex, d + ahead as f32));
            }
        }
        self.footage_budget = if self.upload_pinned {
            self.upload_budget
        } else {
            self.upload_budget.max(2 * on_screen)
        };
        out
    }

    /// The texture key of source frame `k` of footage `fk`, adding the frame to the
    /// store (`Idle`) if it isn't there: keyed by its file's path, size and mtime (one
    /// `stat` per version and frame) and how its texels are made. A missing file is a
    /// `Failed` frame.
    pub(super) fn frame_tex(&mut self, fk: u64, k: u64) -> u64 {
        let Some(e) = self.footage.get_mut(&fk) else {
            return FOOTAGE_BLACK;
        };
        let generation = e.generation;
        // Fast path (every render and pump): known, and still in the store.
        if let Some(&t) = e.frames.get(&k)
            && let Some(entry) = self.textures.get_mut(&t)
        {
            entry.generation = entry.generation.max(generation);
            return t;
        }
        let path = e.scan.path(k);
        let tex = match e.frames.get(&k) {
            Some(&t) => t,
            None => {
                let stamp = match std::fs::metadata(&path) {
                    Ok(md) => {
                        let mtime = md
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map_or(0, |d| d.as_nanos());
                        format!("{}\0{mtime}", md.len())
                    }
                    Err(_) => "missing".to_string(),
                };
                // A video's frames share one file: the frame number and the colour
                // conversion are part of the key.
                let video = e
                    .scan
                    .video
                    .as_ref()
                    .map_or(String::new(), |v| format!("\0video {k} {:?}", v.conversion));
                let t = hash(&format!(
                    "frame\0{}{video}\0{stamp}\0{:?}\0{}\0{}\0{}",
                    path.display(),
                    e.scan.size,
                    e.vflip,
                    e.srgb,
                    e.mipmap
                ));
                e.frames.insert(k, t);
                t
            }
        };
        // Another channel (or version) may have this very frame already: share it.
        let entry = self.textures.entry(tex).or_insert_with(|| {
            let label = format!("{}{}", e.folder, e.scan.frame_name(k));
            let state = if path.is_file() {
                TexState::Idle
            } else {
                TexState::Failed(format!(
                    "{label}: not found (it was there when the film loaded)"
                ))
            };
            TexEntry {
                source: match &e.scan.video {
                    Some(v) => TexSource::Video(v.clone(), k),
                    None => TexSource::Frame(path, e.scan.size),
                },
                label,
                vflip: e.vflip,
                srgb: e.srgb,
                mipmap: e.mipmap,
                footage: true,
                generation,
                state,
                last_used: 0,
            }
        });
        entry.generation = entry.generation.max(generation);
        tex
    }

    /// `k` with each footage channel's frame for film frame `n` filled in (`tex` = that
    /// frame's texture, or `FOOTAGE_BLACK`; `time` = its source time).
    pub(super) fn resolve(&mut self, mut k: ShotKeys, n: u64) -> ShotKeys {
        for c in k.channels.iter_mut().flatten() {
            let Some(fk) = c.footage else { continue };
            let Some(map) = self.footage.get(&fk).map(|e| e.map) else {
                c.tex = FOOTAGE_BLACK;
                continue;
            };
            match map.source_frame(n) {
                Some(frame) => {
                    c.tex = self.frame_tex(fk, frame);
                    c.time = map.source_time(frame);
                }
                None => {
                    c.tex = FOOTAGE_BLACK;
                    c.time = 0.0;
                }
            }
        }
        k
    }
}
