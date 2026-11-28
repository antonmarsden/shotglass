//! Feedback buffers (#134, #138): `[[buffer]]` → `Buffer`s, `after` ranks, memory checks.

use super::*;

/// The film's `[[buffer]]`s (#134), after its shots and layers (spans and resets may
/// name either).
#[allow(clippy::type_complexity)]
pub(super) fn build_buffers(
    sc: &SeqCtx,
    channel_list: &mut dyn FnMut(
        &str,
        &[toml::Value],
        Option<LayerRefs>,
    ) -> Result<[Option<Channel>; N_CHANNELS]>,
    files: Vec<BufferFile>,
    shots: &[Shot],
    layers: &[Layer],
    duration: f32,
    loop_frames: Option<u64>,
) -> Result<Vec<Buffer>> {
    let (num, den) = (sc.fps.0 as f64, sc.fps.1 as f64);
    // The first film frame at or after `t` (frame m shows at m·den/num).
    let frame_ceil = |t: f32| -> u64 { ((t as f64 * num / den) - 1e-6).ceil().max(0.0) as u64 };
    let n_frames = loop_frames.unwrap_or_else(|| frame_ceil(duration));
    let layer_ids: Vec<String> = layers.iter().map(|l| l.id.clone()).collect();
    let mut out: Vec<Buffer> = vec![];
    for (i, b) in files.into_iter().enumerate() {
        let what = format!("buffer \"{}\"", b.id);
        if !is_pass_id(&b.id) || b.id == "prev" || b.id == "input" || b.id == "picture" {
            bail!(
                "buffer id \"{}\" may only use letters, digits, _ and - (and isn't prev, \
                 input or picture)",
                b.id
            );
        }
        if shots.iter().any(|s| s.id == b.id)
            || layers.iter().any(|l| l.id == b.id)
            || out.iter().any(|o| o.id == b.id)
            || sc.comps.contains(&b.id)
        {
            bail!(
                "{what}: the id is taken; shots, layers, buffers and comps share ids (they \
                 name cues, fallbacks and @ sources)"
            );
        }
        library::check_rel_path(&b.scene).map_err(|e| anyhow!("{what}: scene: {e}"))?;
        if let Some(p) = &b.init {
            library::check_rel_path(p).map_err(|e| anyhow!("{what}: init: {e}"))?;
        }
        // `after = "<layer>"` (#138): a layer of the same sequence (film or comp).
        let after = match &b.after {
            None => None,
            Some(id) => match layer_ids.iter().position(|l| l == id) {
                Some(j) => Some(j),
                None => bail!(
                    "{what}: after = \"{id}\" names no layer{} (a buffer steps after a layer of \
                     its own film or comp)",
                    if layer_ids.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "; the layers are {}",
                            layer_ids
                                .iter()
                                .map(|l| format!("\"{l}\""))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    }
                ),
            },
        };
        let start = match &b.from {
            None => 0.0,
            Some(v) => span_point(&format!("{what}: from"), v, true, shots, layers)?,
        };
        let end = match &b.to {
            None => duration,
            Some(v) => span_point(&format!("{what}: to"), v, false, shots, layers)?,
        };
        if !(0.0..duration).contains(&start) || !(start < end && end <= duration + 1e-4) {
            bail!(
                "{what}: the span {}–{} s must lie within the film (0–{} s), from before to",
                glsl_float(start),
                glsl_float(end),
                glsl_float(duration)
            );
        }
        let end = end.min(duration);
        let start_frame = frame_ceil(start);
        let end_frame = frame_ceil(end).min(n_frames);
        if start_frame >= end_frame {
            bail!("{what}: the span covers no film frame");
        }
        let size = match (&b.size, &b.scale) {
            (Some(_), Some(_)) => bail!("{what}: give size or scale, not both"),
            (Some([w, h]), None) => {
                if !(1..=8192).contains(w) || !(1..=8192).contains(h) {
                    bail!("{what}: size = [{w}, {h}] must be 1–8192 pixels each way");
                }
                BufferSize::Fixed(*w, *h)
            }
            (None, Some(v)) => {
                if v.as_str() == Some("auto") {
                    bail!(
                        "{what}: scale can't be \"auto\": a buffer's state would then change \
                         with the player's render scale; give a fraction of the output size \
                         or size = [w, h]"
                    );
                }
                let x = v
                    .as_float()
                    .or_else(|| v.as_integer().map(|i| i as f64))
                    .unwrap_or(-1.0) as f32;
                if !(x > 0.0 && x <= 1.0) {
                    bail!("{what}: scale must be a fraction of the output size in (0, 1]");
                }
                BufferSize::Scale(x)
            }
            (None, None) => BufferSize::Scale(1.0),
        };
        let decl = &sc.buffers[i];
        let point = |v: &toml::Value| -> Result<u64> {
            let t = span_point(&format!("{what}: reset"), v, true, shots, layers)?;
            let f = frame_ceil(t);
            if !(start_frame..end_frame).contains(&f) {
                bail!(
                    "{what}: reset at {} s is outside the buffer's span ({}–{} s)",
                    glsl_float(t),
                    glsl_float(start),
                    glsl_float(end)
                );
            }
            Ok(f)
        };
        let mut resets = vec![start_frame];
        match &b.reset {
            None => {}
            Some(toml::Value::String(s)) if s == "cuts" => {
                resets.extend(
                    shots
                        .iter()
                        .map(|s| frame_ceil(s.start))
                        .filter(|f| (start_frame..end_frame).contains(f)),
                );
            }
            Some(toml::Value::Array(list)) => {
                for v in list {
                    resets.push(point(v)?);
                }
            }
            Some(_) => bail!(
                "{what}: reset is \"cuts\" or a list of shot or layer ids, \"<id> ± s\" points \
                 and seconds"
            ),
        }
        resets.sort_unstable();
        resets.dedup();
        let mut uniforms = Vec::new();
        for (name, v) in &b.uniforms {
            if !is_ident(name) {
                bail!("{what}: \"{name}\" is not a valid GLSL name");
            }
            let a = Anim::from_toml(v).with_context(|| format!("{what}: uniform {name}"))?;
            uniforms.push((name.clone(), a));
        }
        let mut params = Vec::new();
        for (name, v) in &b.params {
            if !is_ident(name) {
                bail!("{what}: param \"{name}\" is not a valid GLSL name");
            }
            params.push((
                name.clone(),
                vector(v).with_context(|| format!("{what}: param {name}"))?,
            ));
        }
        // With `after`, the layers up to it are below the buffer (it may read them and
        // `@input`); the rest draw after it steps.
        let below = after.map_or(0, |j| j + 1);
        let channels = match &b.channels {
            Some(list) => channel_list(
                &format!("{what}: "),
                list,
                Some(LayerRefs {
                    earlier: &layer_ids[..below],
                    later: &layer_ids[below..],
                    on_buffer: Some(i),
                }),
            )?,
            None => Default::default(),
        };
        let reads_stack = channels
            .iter()
            .flatten()
            .any(|c| matches!(c.source, Source::Input | Source::Layer { .. }));
        let stack_reads_state = reads_stack
            && layers[..below].iter().any(|l| {
                l.channels
                    .iter()
                    .flatten()
                    .any(|c| matches!(c.source, Source::Buffer { .. }))
            });
        let dense = match b.cache.as_deref() {
            None | Some("second") => false,
            Some("dense") => true,
            Some(c) => bail!("{what}: cache = \"{c}\": use \"dense\" (or leave it out)"),
        };
        let preview_lookback = match b.preview_lookback {
            None => None,
            Some(s) if s > 0.0 && s.is_finite() => {
                Some(((s * sc.fps.0 as f64 / sc.fps.1 as f64).ceil() as u64).max(1))
            }
            Some(s) => bail!("{what}: preview_lookback = {s}: seconds, more than 0"),
        };
        out.push(Buffer {
            id: b.id,
            scene: sc.root.join(&b.scene),
            init: b.init.as_ref().map(|p| sc.root.join(p)),
            start,
            end,
            start_frame,
            end_frame,
            resets,
            after,
            stack_reads_state,
            size,
            format: decl.format,
            history: decl.history,
            dense,
            preview_lookback,
            channels,
            uniforms,
            params,
        });
    }
    // Required memory: every ring active on the worst frame (the most buffers at once).
    let out_size = memory_check_out(sc.aspect, sc.size);
    for b in &out {
        let at = b.start_frame;
        let live: Vec<&Buffer> = out.iter().filter(|o| o.active(at)).collect();
        let total: usize = live.iter().map(|o| o.ring_bytes(out_size)).sum();
        if total > BUFFER_MEMORY_LIMIT {
            let list: Vec<String> = live
                .iter()
                .map(|o| {
                    let (w, h) = o.size.at(out_size);
                    format!(
                        "\"{}\" {w}×{h} {} × {} states = {} MB",
                        o.id,
                        BufferFormat::NAMES[o.format as usize],
                        o.history + 1,
                        o.ring_bytes(out_size) >> 20
                    )
                })
                .collect();
            bail!(
                "buffers need {} MB of GPU memory at once (limit {} MB; `scale` buffers counted \
                 at a {}-pixel-high output): {}; use a smaller size or scale, rgba8, or a \
                 shorter history",
                total >> 20,
                BUFFER_MEMORY_LIMIT >> 20,
                MEMORY_CHECK_HEIGHT,
                list.join(", ")
            );
        }
    }
    Ok(out)
}

/// The highest layer a buffer's step draws (#138): the largest `after` of the buffers
/// that read the layer stack (`@input`, `@<layer>`). Layers up to it are part of those
/// folds; layers above it never are.
pub fn stack_reach(m: &Movie) -> Option<usize> {
    m.buffers
        .iter()
        .filter(|b| b.reads_stack())
        .filter_map(|b| b.after)
        .max()
}

/// The buffers of a warm loop sequence (#138) that carry state over its seam: active
/// from frame 0 to the end of the period.
pub fn warm_buffers(m: &Movie) -> impl Iterator<Item = &Buffer> {
    let p = m.loop_frames.filter(|_| m.warm);
    m.buffers
        .iter()
        .filter(move |b| p.is_some_and(|p| b.start_frame == 0 && b.end_frame >= p))
}

/// The output `scale` buffers are sized against in the memory checks: a comp's own
/// `size` (#138), else a 1080-pixel-high output of the aspect.
pub(super) fn memory_check_out(aspect: f32, size: Option<(u32, u32)>) -> (i32, i32) {
    match size {
        Some((w, h)) => (w as i32, h as i32),
        None => (
            (MEMORY_CHECK_HEIGHT * aspect).round() as i32,
            MEMORY_CHECK_HEIGHT as i32,
        ),
    }
}

/// Required buffer memory across the film and its comps (#138): each sequence's worst
/// frame, summed (comps can be active at any film frame). Only comps add anything here:
/// `build_buffers` already checked each sequence on its own.
pub(super) fn check_buffer_memory(film: &Movie) -> Result<()> {
    let worst = |m: &Movie, size: Option<(u32, u32)>| -> usize {
        let out = memory_check_out(m.aspect, size);
        m.buffers
            .iter()
            .map(|b| {
                m.buffers
                    .iter()
                    .filter(|o| o.active(b.start_frame))
                    .map(|o| o.ring_bytes(out))
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0)
    };
    if film.comps.iter().all(|c| c.movie.buffers.is_empty()) {
        return Ok(());
    }
    let parts: Vec<(String, usize)> = std::iter::once(("the film".to_string(), worst(film, None)))
        .chain(
            film.comps
                .iter()
                .map(|c| (format!("comp \"{}\"", c.name), worst(&c.movie, c.size))),
        )
        .filter(|(_, b)| *b > 0)
        .collect();
    let total: usize = parts.iter().map(|(_, b)| b).sum();
    if total > BUFFER_MEMORY_LIMIT {
        bail!(
            "buffers in the film and its comps need up to {} MB of GPU memory at once (limit \
             {} MB; `scale` buffers counted at a {}-pixel-high output): {}; use smaller sizes, \
             rgba8, or shorter histories",
            total >> 20,
            BUFFER_MEMORY_LIMIT >> 20,
            MEMORY_CHECK_HEIGHT,
            parts
                .iter()
                .map(|(w, b)| format!("{w} {} MB", b >> 20))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

/// `[[buffer]]` ids, formats and histories (#134), known before any channel is parsed.
pub(super) fn buffer_decls(files: &[BufferFile]) -> Result<Vec<BufDecl>> {
    if files.len() > MAX_BUFFERS {
        bail!(
            "a film has at most {MAX_BUFFERS} buffers (this one has {})",
            files.len()
        );
    }
    files
        .iter()
        .map(|b| {
            let what = format!("buffer \"{}\"", b.id);
            let format = match b.format.as_deref() {
                None => BufferFormat::Rgba16f,
                Some(f) => BufferFormat::parse(f).ok_or_else(|| {
                    anyhow!(
                        "{what}: format \"{f}\" isn't one of {:?}",
                        BufferFormat::NAMES
                    )
                })?,
            };
            let history = match b.history {
                None => 1,
                Some(n) if (1..=MAX_HISTORY as i64).contains(&n) => n as u32,
                Some(n) => bail!(
                    "{what}: history = {n}: keep 1 to {MAX_HISTORY} states (memory limits it \
                     further)"
                ),
            };
            Ok(BufDecl {
                id: b.id.clone(),
                history,
                format,
                after: b.after.clone(),
            })
        })
        .collect()
}
