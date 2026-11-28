//! Per-frame planning: sequences, channels and comp instances (#74), comp clocks and distances.

use super::*;

impl Renderer {
    /// Plan sequence `seq` (0: the film; 1 + c: comp c) at `(t, iFrame, footage frame)`,
    /// its local clock (#74): keys for its needed shots and active layers, and the comp
    /// instances they show (pushed to `insts` before their user, memoised by (comp,
    /// time, size, scale)). None when something isn't ready and `blocking` is off.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn plan_seq(
        &mut self,
        film: &Movie,
        seq: usize,
        at: At,
        (t, frame, n): (f32, i32, u64),
        full: (i32, i32),
        scaled: (i32, i32),
        blocking: bool,
        insts: &mut Vec<(InstKey, SeqPlan)>,
    ) -> Option<SeqPlan> {
        let m = seq_movie(film, seq);
        let base = *self.seq_base.get(seq)?;
        let scale = scaled.0 as f32 / full.0 as f32;
        // The film frame on screen picks each footage channel's source frame (#70); in a
        // comp, the comp's own frame. A frozen transition's outgoing shot shows the
        // frame where the transition starts.
        let needed: Vec<(usize, u64, f32)> = match at {
            At::Shot { shot, local } => vec![(shot, n, local)],
            At::Transition {
                from,
                to,
                from_local,
                to_local,
                ..
            } => {
                let freeze = m.shots[to].transition_in.as_ref().is_some_and(|t| t.freeze);
                let (from_n, from_local) = if freeze {
                    (
                        m.frame_at(m.shots[to].start).max(0) as u64,
                        m.shots[to].start - m.shots[from].start,
                    )
                } else {
                    (n, from_local)
                };
                vec![(from, from_n, from_local), (to, n, to_local)]
            }
        };
        let mut shots = vec![];
        let mut chans = vec![];
        for &(i, n, local) in &needed {
            let k = self.usable(base + i, blocking, n)?;
            let shot = &m.shots[i];
            let itime = shot.scene_time(local);
            let inst = match shot.comp {
                // Resolved at full size, as a film's picture is into the output: a comp
                // shot is then exactly the comp rendered as a film.
                Some(c) => {
                    Some(self.plan_inst(film, c, itime, full, scale, full, blocking, insts)?)
                }
                None => None,
            };
            chans.push(self.plan_chans(
                film,
                &shot.channels,
                itime,
                full,
                scale,
                blocking,
                insts,
            )?);
            shots.push((k, inst));
        }
        // Layers (#69): a frame is usable only when every active layer is, so playback
        // never shows a frame without a layer that `still` and `export` would draw.
        // A buffer step's picture (#138) never depends on preview settings: hidden
        // layers are drawn there.
        let (layers_here, step) = match self.plan_layers {
            Some((q, upto)) if q == seq => (
                upto.map_or(vec![], |u| {
                    m.layers_at(t)
                        .into_iter()
                        .filter(|(j, _)| *j <= u)
                        .collect()
                }),
                true,
            ),
            _ => (m.layers_at(t), false),
        };
        let active: Vec<(usize, f32)> = layers_here
            .into_iter()
            .filter(|(j, _)| step || seq > 0 || !self.hidden.contains(&m.layers[*j].id))
            .collect();
        let mut layers = vec![];
        for &(j, local) in &active {
            let k = self.usable(base + m.shots.len() + j, blocking, n)?;
            let layer = &m.layers[j];
            let itime = layer.scene_time(local);
            let (w, h) = full;
            let lsize = match layer.scale {
                None => scaled,
                Some(s) => (
                    ((w as f32 * s).round() as i32).clamp(1, w),
                    ((h as f32 * s).round() as i32).clamp(1, h),
                ),
            };
            let inst = match layer.scene {
                LayerScene::Comp(c) => {
                    let s = lsize.0 as f32 / w as f32;
                    Some(self.plan_inst(film, c, itime, full, s, lsize, blocking, insts)?)
                }
                _ => None,
            };
            let matte = match layer.matte.map(|mt| mt.source) {
                Some(crate::movie::MatteSource::Comp(c)) => {
                    let dest = comp_dest(film, c, full);
                    Some(self.plan_inst(film, c, itime, full, scale, dest, blocking, insts)?)
                }
                _ => None,
            };
            chans.push(self.plan_chans(
                film,
                &layer.channels,
                itime,
                full,
                scale,
                blocking,
                insts,
            )?);
            layers.push((j, local, k, inst, matte));
        }
        let over = if active.is_empty() {
            None
        } else {
            let o = self.over?;
            if blocking {
                self.compile_now(o);
            }
            self.prog(o)?;
            if active.iter().any(|(j, _)| m.layers[*j].matte.is_some()) {
                let om = self.over_matte?;
                if blocking {
                    self.compile_now(om);
                }
                self.prog(om)?;
            }
            Some(o)
        };
        Some(SeqPlan {
            seq,
            at,
            t,
            frame,
            full,
            scaled,
            shots,
            chans,
            layers,
            over,
            step: None,
        })
    }

    /// The comp instances an item's `@<comp>` channels read: (pass key, instance).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn plan_chans(
        &mut self,
        film: &Movie,
        channels: &[Option<crate::movie::Channel>; N_CHANNELS],
        itime: f32,
        full: (i32, i32),
        scale: f32,
        blocking: bool,
        insts: &mut Vec<(InstKey, SeqPlan)>,
    ) -> Option<Vec<(u64, usize)>> {
        let mut v = vec![];
        for (n, c) in channels.iter().enumerate() {
            let Some(crate::movie::Channel {
                source:
                    crate::movie::Source::Comp {
                        index, time_offset, ..
                    },
                ..
            }) = c
            else {
                continue;
            };
            let dest = comp_dest(film, *index, full);
            let i = self.plan_inst(
                film,
                *index,
                itime + time_offset,
                full,
                scale,
                dest,
                blocking,
                insts,
            )?;
            v.push((COMP_TEX - n as u64, i));
        }
        Some(v)
    }

    /// Plan comp `c` shown at comp time `tc`, drawn at `full` × `scale` (a sized comp:
    /// its size at scale 1) into a `dest`-sized target; its index in `insts`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn plan_inst(
        &mut self,
        film: &Movie,
        c: usize,
        tc: f32,
        full: (i32, i32),
        scale: f32,
        dest: (i32, i32),
        blocking: bool,
        insts: &mut Vec<(InstKey, SeqPlan)>,
    ) -> Option<usize> {
        let comp = &film.comps[c];
        let (full, scale) = match comp.size {
            Some((cw, ch)) => ((cw as i32, ch as i32), 1.0),
            None => (full, scale),
        };
        let t = comp.local(tc);
        let key = InstKey {
            comp: c,
            t: t.to_bits(),
            full,
            scale: scale.to_bits(),
            dest,
        };
        if let Some(i) = insts.iter().position(|(k, _)| *k == key) {
            return Some(i);
        }
        self.comp_req.push((c, t));
        let m = &comp.movie;
        let scaled = (
            ((full.0 as f32 * scale).round() as i32).clamp(1, full.0),
            ((full.1 as f32 * scale).round() as i32).clamp(1, full.1),
        );
        // Comp time is f32 arithmetic (`time_offset + local × time_scale`): within 0.001
        // frame of a frame boundary it is that frame, so a comp shown at speed 1 steps
        // exactly one comp frame per film frame.
        let f = t as f64 * m.fps_num as f64 / m.fps_den as f64;
        let frame = if (f - f.round()).abs() < 1e-3 {
            f.round() as i64
        } else {
            m.frame_at(t)
        }
        .clamp(0, i32::MAX as i64) as u64;
        let frame = m.wrap_frame(frame).min(i32::MAX as u64) as i32;
        // Buffers in the comp (#138) fold over comp-local frames: bring them to this
        // comp frame first (a comp with buffers is shown in one place, so there's one
        // comp frame per film frame), or the instance isn't ready.
        if !m.buffers.is_empty() {
            let out = self.fold_out;
            if !self.advance_to(film, c + 1, frame.max(0) as u64, out, blocking) {
                return None;
            }
        }
        let plan = self.plan_seq(
            film,
            c + 1,
            m.at(t),
            (t, frame, frame.max(0) as u64),
            full,
            scaled,
            blocking,
            insts,
        )?;
        insts.push((key, plan));
        Some(insts.len() - 1)
    }

    /// The keys of sequence `s`'s shots, layers (#74) and buffers (#138).
    pub(super) fn seq_items(&self, film: &Movie, s: usize) -> Vec<ShotKeys> {
        let Some(&base) = self.seq_base.get(s) else {
            return vec![];
        };
        let m = seq_movie(film, s);
        let n = m.shots.len() + m.layers.len();
        let bufs = self
            .buf_base
            .get(s)
            .map_or(0..0, |&b| b..b + m.buffers.len());
        self.wanted
            .iter()
            .skip(base)
            .take(n)
            .chain(self.wanted.get(bufs).unwrap_or_default())
            .copied()
            .collect()
    }

    /// Comp instances as an item's channel passes: (pass key, (texture, w, h)).
    pub(super) fn inst_passes(
        &self,
        list: &[(u64, usize)],
    ) -> Vec<(u64, (glow::Texture, usize, usize))> {
        list.iter()
            .map(|&(k, i)| {
                let t = self.inst[i].as_ref().unwrap();
                (k, (t.tex, t.alloc.0 as usize, t.alloc.1 as usize))
            })
            .collect()
    }
}

/// The cues a sequence's shaders get (#74): the film's, and for a comp its own
/// (`COMP_DURATION`, `COMP_CUE_*`, comp-local seconds) after them.
pub fn seq_cues(film: &Movie, comp: Option<&Movie>) -> String {
    let mut s = film.cue_defines();
    if let Some(c) = comp {
        s += &c.comp_cue_defines();
    }
    s
}

/// How soon the playhead at `t` needs each comp (#74): the nearest film shot or layer
/// that shows it (as a shot, layer, channel or matte), through nested comps.
pub(super) fn comp_distances(film: &Movie, t: f32) -> Vec<f32> {
    let mut d = vec![f32::INFINITY; film.comps.len()];
    if d.is_empty() {
        return d;
    }
    let uses = |shots: &[Shot], layers: &[Layer], f: &mut dyn FnMut(usize, usize)| {
        for (i, s) in shots.iter().enumerate() {
            let chans = s.channels.iter().flatten();
            for c in s.comp.into_iter().chain(chans.filter_map(comp_of)) {
                f(i, c);
            }
        }
        for (j, l) in layers.iter().enumerate() {
            let mut cs: Vec<usize> = l.channels.iter().flatten().filter_map(comp_of).collect();
            if let LayerScene::Comp(c) = l.scene {
                cs.push(c);
            }
            if let Some(crate::movie::Matte {
                source: crate::movie::MatteSource::Comp(c),
                ..
            }) = l.matte
            {
                cs.push(c);
            }
            for c in cs {
                f(shots.len() + j, c);
            }
        }
    };
    for b in &film.buffers {
        for c in b.channels.iter().flatten().filter_map(comp_of) {
            d[c] = d[c].min(span_distance(film, b.start, b.end, t));
        }
    }
    uses(&film.shots, &film.layers, &mut |i, c| {
        let di = if i < film.shots.len() {
            distance(film, &film.shots[i], t)
        } else {
            layer_distance(film, &film.layers[i - film.shots.len()], t)
        };
        d[c] = d[c].min(di);
    });
    for _ in 0..crate::movie::MAX_COMP_DEPTH {
        for (c, comp) in film.comps.iter().enumerate() {
            let dc = d[c];
            if dc.is_finite() {
                uses(&comp.movie.shots, &comp.movie.layers, &mut |_, x| {
                    d[x] = d[x].min(dc)
                });
                for b in &comp.movie.buffers {
                    for x in b.channels.iter().flatten().filter_map(comp_of) {
                        d[x] = d[x].min(dc);
                    }
                }
            }
        }
    }
    d
}

/// Footage clocks grouped by comp (#74): last frame's instances, then nearby direct
/// users in film order. A user ahead of the playhead counts from its start.
pub(super) fn comp_clocks(film: &Movie, t: f32, seen: &[(usize, f32)]) -> Vec<Vec<(f32, f32)>> {
    let mut clocks = vec![vec![]; film.comps.len()];
    if clocks.is_empty() {
        return clocks;
    }
    for &(c, tc) in seen {
        if let Some(clocks) = clocks.get_mut(c) {
            clocks.push((tc, 0.0));
        }
    }
    let mut add = |c: usize, tc, d| {
        if d <= FOOTAGE_HEADS {
            clocks[c].push((film.comps[c].local(tc), d));
        }
    };
    for s in &film.shots {
        let d = distance(film, s, t);
        let itime = s.scene_time((t - s.start).clamp(0.0, s.duration));
        for c in s.comp.into_iter() {
            add(c, itime, d);
        }
        for ch in s.channels.iter().flatten() {
            if let crate::movie::Source::Comp {
                index, time_offset, ..
            } = ch.source
            {
                add(index, itime + time_offset, d);
            }
        }
    }
    for l in &film.layers {
        let d = layer_distance(film, l, t);
        let itime = l.scene_time((t - l.start).clamp(0.0, l.duration()));
        if let LayerScene::Comp(c) = l.scene {
            add(c, itime, d);
        }
        if let Some(crate::movie::Matte {
            source: crate::movie::MatteSource::Comp(c),
            ..
        }) = l.matte
        {
            add(c, itime, d);
        }
        for ch in l.channels.iter().flatten() {
            if let crate::movie::Source::Comp {
                index, time_offset, ..
            } = ch.source
            {
                add(index, itime + time_offset, d);
            }
        }
    }
    clocks
}

pub(super) fn comp_of(c: &crate::movie::Channel) -> Option<usize> {
    match c.source {
        crate::movie::Source::Comp { index, .. } => Some(index),
        _ => None,
    }
}

/// Sequence `s` of a film: 0 is the film, 1 + c its comp c.
pub(super) fn seq_movie(film: &Movie, s: usize) -> &Movie {
    match s {
        0 => film,
        _ => &film.comps[s - 1].movie,
    }
}

/// The output size sequence `s`'s buffers are sized against (#138): the film's output
/// `out`, or a comp's own `size` if it has one. Never the reader's size, so a comp's
/// state doesn't depend on who shows it.
pub(super) fn seq_out(film: &Movie, s: usize, out: (i32, i32)) -> (i32, i32) {
    match s {
        0 => out,
        _ => film.comps[s - 1]
            .size
            .map_or(out, |(w, h)| (w as i32, h as i32)),
    }
}

/// The size of comp `c`'s output when read as a channel or matte: its `size`, or the
/// reader's full size.
pub(super) fn comp_dest(film: &Movie, c: usize, full: (i32, i32)) -> (i32, i32) {
    match film.comps[c].size {
        Some((w, h)) => (w as i32, h as i32),
        None => full,
    }
}
