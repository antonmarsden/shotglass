//! Building a sequence (the film or a comp): shots, layers, buffers and their spans; loop periods.

use super::*;

/// Shots, then layers (bottom to top), of the film or a comp: (shots, layers,
/// duration, loop frames).
pub(super) type SeqParts = (Vec<Shot>, Vec<Layer>, Vec<Buffer>, f32, Option<u64>);

/// Builds a [`SeqParts`].
pub(super) fn build_sequence(
    sc: &SeqCtx,
    used: &mut std::collections::HashSet<String>,
    shot_files: Vec<ShotFile>,
    layer_files: Vec<LayerFile>,
    buffer_files: Vec<BufferFile>,
    channels: Option<&[toml::Value]>,
    looping: bool,
) -> Result<SeqParts> {
    let path_rule = |what: String, p: &str| -> Result<()> {
        library::check_rel_path(p).map_err(|e| anyhow!("{what}: {e}"))
    };
    if shot_files.is_empty() {
        match sc.comp {
            None => bail!("a movie needs at least one [[shot]]"),
            Some(_) => bail!("a comp needs at least one [[shot]]"),
        }
    }
    let mut channel_list = |what: &str,
                            list: &[toml::Value],
                            on_layer: Option<LayerRefs>|
     -> Result<[Option<Channel>; N_CHANNELS]> {
        if list.len() > N_CHANNELS {
            bail!(
                "{what}channels has {} entries; there are {N_CHANNELS} (iChannel0..{})",
                list.len(),
                N_CHANNELS - 1
            );
        }
        let mut out: [Option<Channel>; N_CHANNELS] = Default::default();
        for (n, v) in list.iter().enumerate() {
            let ctx = format!("{what}channels[{n}]");
            if let toml::Value::String(s) = v
                && let ChannelSource::Name(name) = ChannelSource::of(s)
            {
                used.insert(name.to_string());
            }
            for key in ["texture", "text"] {
                if let Some(name) = v.get(key).and_then(toml::Value::as_str) {
                    used.insert(name.to_string());
                }
            }
            out[n] = texture_spec(
                &ctx,
                v,
                Some(sc.names),
                sc.specs,
                sc.root,
                on_layer,
                sc.comps,
                sc.buffers,
            )?;
        }
        Ok(out)
    };
    let default_channels = match channels {
        Some(list) => channel_list("", list, None)?,
        None => Default::default(),
    };
    let mut shots: Vec<Shot> = Vec::new();
    let mut ids = std::collections::HashSet::new();
    for (index, s) in shot_files.into_iter().enumerate() {
        let ctx = || format!("shot \"{}\"", s.id);
        let comp = match (&s.scene, &s.comp) {
            (Some(_), Some(_)) => bail!("{}: give scene or comp, not both", ctx()),
            (None, None) => bail!(
                "{}: a shot needs scene = \"…\" (a film file) or comp = \"…\" (a name \
                     from [comps])",
                ctx()
            ),
            (Some(p), None) => {
                path_rule(format!("{}: scene", ctx()), p)?;
                None
            }
            (None, Some(name)) => Some(sc.comp_ref(&ctx(), name)?),
        };
        if comp.is_some() && (s.channels.is_some() || !s.uniforms.is_empty()) {
            bail!(
                "{}: a comp shot has no shader of its own, so no channels or uniforms (the \
                     comp's shots and layers have theirs)",
                ctx()
            );
        }
        if sc.comp.is_some()
            && (s.sound.is_some()
                || s.sound_lead.is_some()
                || s.sound_tail.is_some()
                || s.sound_gain.is_some())
        {
            bail!(
                "{}: comps are picture only; put the sound on the film's shot that shows \
                     the comp",
                ctx()
            );
        }
        let channels = match &s.channels {
            Some(list) => channel_list(&format!("{}: ", ctx()), list, None)?,
            None if comp.is_some() => Default::default(),
            None => default_channels.clone(),
        };
        if !ids.insert(s.id.clone()) {
            bail!("duplicate shot id \"{}\"", s.id);
        }
        if !is_pass_id(&s.id) {
            bail!("shot id \"{}\" may only use letters, digits, _ and -", s.id);
        }
        if let Some(p) = &s.sound {
            path_rule(format!("{}: sound", ctx()), p)?;
        }
        if let Some(t) = &s.transition {
            match transition_kind(&t.kind) {
                TransitionKind::File(p) => path_rule(format!("{}: transition", ctx()), p)?,
                TransitionKind::Library(lib, file) => {
                    if !sc.specs.iter().any(|l| l.name == lib) {
                        bail!(
                            "{}: transition \"{}\" names library \"{lib}\", but [libraries] \
                                 has {}",
                            ctx(),
                            t.kind,
                            if sc.specs.is_empty() {
                                "none".to_string()
                            } else {
                                sc.specs
                                    .iter()
                                    .map(|l| format!("\"{}\"", l.name))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            }
                        );
                    }
                    path_rule(format!("{}: transition", ctx()), &file)?;
                }
                TransitionKind::Name(name) => {
                    bail!("{}: {}", ctx(), bare_transition_error(name, sc.specs))
                }
            }
        }
        if s.duration.is_nan() || s.duration <= 0.0 {
            bail!("{}: duration must be > 0", ctx());
        }
        let mut uniforms = Vec::new();
        for (name, v) in &s.uniforms {
            if !is_ident(name) {
                bail!("{}: \"{name}\" is not a valid GLSL name", ctx());
            }
            let a = Anim::from_toml(v).with_context(|| format!("{}: uniform {name}", ctx()))?;
            uniforms.push((name.clone(), a));
        }
        let transition_in = match s.transition {
            None => None,
            Some(t) => {
                if index == 0 {
                    bail!("{}: the first shot can't have a transition in", ctx());
                }
                if t.duration.is_nan() || t.duration <= 0.0 {
                    bail!("{}: transition duration must be > 0", ctx());
                }
                let mut params = Vec::new();
                for (name, v) in &t.params {
                    if !is_ident(name) {
                        bail!(
                            "{}: transition param \"{name}\" is not a valid GLSL name",
                            ctx()
                        );
                    }
                    params.push((
                        name.clone(),
                        vector(v).with_context(|| format!("{}: param {name}", ctx()))?,
                    ));
                }
                Some(Transition {
                    kind: t.kind,
                    duration: t.duration,
                    freeze: t.freeze,
                    ease: match t.ease {
                        Some(e) => Ease::parse(&e)?,
                        None => Ease::Smooth,
                    },
                    params,
                })
            }
        };
        // Shots overlap by the incoming transition's length.
        let start = match shots.last() {
            None => 0.0,
            Some(p) => {
                let d = transition_in.as_ref().map_or(0.0, |t| t.duration);
                let prev_in = p.transition_in.as_ref().map_or(0.0, |t| t.duration);
                if d > s.duration {
                    bail!(
                        "{}: transition ({d}s) is longer than the shot ({}s)",
                        ctx(),
                        s.duration
                    );
                }
                if prev_in + d > p.duration + 1e-4 {
                    bail!(
                        "{}: transition ({d}s) overlaps the previous transition; shot \"{}\" is {}s \
                             but its two transitions need {}s",
                        ctx(),
                        p.id,
                        p.duration,
                        prev_in + d
                    );
                }
                p.end() - d
            }
        };
        let lead = s.sound_lead.unwrap_or(0.0);
        let tail = s.sound_tail.unwrap_or(0.0);
        if lead < 0.0 || tail < 0.0 || lead.is_nan() || tail.is_nan() {
            bail!("{}: sound_lead and sound_tail must be >= 0", ctx());
        }
        if s.sound.is_none()
            && (s.sound_lead.is_some() || s.sound_tail.is_some() || s.sound_gain.is_some())
        {
            bail!(
                "{}: sound_lead/sound_tail/sound_gain need sound = \"file.glsl\"",
                ctx()
            );
        }
        let sound = s.sound.map(|p| ShotSound {
            path: sc.root.join(p),
            lead,
            tail,
            gain: s.sound_gain.unwrap_or(1.0),
        });
        shots.push(Shot {
            channels,
            sound,
            id: s.id,
            index,
            start,
            duration: s.duration,
            scene: match &s.scene {
                Some(p) => sc.root.join(p),
                None => sc.root.join(sc.comp_rel(comp)),
            },
            comp,
            time_offset: s.time_offset.unwrap_or(0.0),
            time_scale: s.time_scale.unwrap_or(1.0),
            uniforms,
            transition_in,
        });
    }
    let duration = shots.last().unwrap().end();
    let loop_frames = match looping {
        true => Some(loop_frames(duration, sc.fps.0, sc.fps.1)?),
        false => None,
    };
    // Layers, bottom to top. Ids share the shot namespace (fallback keys, cue names).
    let mut layers: Vec<Layer> = vec![];
    let layer_ids: Vec<String> = layer_files.iter().map(|l| l.id.clone()).collect();
    // `type` and `blend` name a pass the way a transition's `type` does.
    let pass_kind = |what: &str, key: &str, k: &str, example: &str| -> Result<()> {
        match transition_kind(k) {
            TransitionKind::File(p) => path_rule(format!("{what}: {key}"), p),
            TransitionKind::Library(lib, file) => {
                if !sc.specs.iter().any(|s| s.name == lib) {
                    bail!(
                        "{what}: {key} \"{k}\" names library \"{lib}\", but [libraries] has {}",
                        quoted_list(sc.specs.iter().map(|s| &s.name))
                    );
                }
                path_rule(format!("{what}: {key}"), &file)
            }
            TransitionKind::Name(n) => bail!(
                "{what}: {key} \"{n}\" needs a library (\"{example}/{n}\") or a film file \
                     (\"fx/{n}.glsl\")"
            ),
        }
    };
    for (index, l) in layer_files.into_iter().enumerate() {
        let what = format!("layer \"{}\"", l.id);
        if !is_pass_id(&l.id) {
            bail!(
                "layer id \"{}\" may only use letters, digits, _ and -",
                l.id
            );
        }
        if shots.iter().any(|s| s.id == l.id) || layers.iter().any(|o| o.id == l.id) {
            bail!(
                "{what}: the id is taken; shots and layers share ids (they name cues and \
                     fallbacks)"
            );
        }
        if sc.comps.contains(&l.id) {
            bail!(
                "{what}: [comps] has a \"{}\" too; layers and comps share @ names",
                l.id
            );
        }
        if l.transition_in.is_some() || l.transition_out.is_some() {
            bail!("{what}: layer transitions arrive in a later phase; use fade = [in, out]");
        }
        let blend = match l.blend.as_deref() {
            None | Some("over") => None,
            Some(b) => {
                pass_kind(&what, "blend", b, "blend")?;
                Some(b.to_string())
            }
        };
        let scene = match (&l.scene, &l.kind, &l.comp) {
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
                bail!("{what}: give one of scene, type and comp")
            }
            (None, None, None) => bail!(
                "{what}: a layer needs scene = \"…\" (a film file), type = \"…\" (a \
                     library pass like \"fx/grain\") or comp = \"…\" (a name from [comps])"
            ),
            (None, None, Some(name)) => {
                if l.channels.is_some() || !l.uniforms.is_empty() || !l.params.is_empty() {
                    bail!(
                        "{what}: a comp layer has no shader of its own, so no channels, \
                             uniforms or params"
                    );
                }
                LayerScene::Comp(sc.comp_ref(&what, name)?)
            }
            (Some(p), None, None) => {
                path_rule(format!("{what}: scene"), p)?;
                LayerScene::File(sc.root.join(p))
            }
            (None, Some(k), None) => {
                pass_kind(&what, "type", k, "fx")?;
                LayerScene::Pass(k.clone())
            }
        };
        let start = match &l.from {
            None => 0.0,
            Some(v) => span_point(&format!("{what}: from"), v, true, &shots, &layers)?,
        };
        let end = match &l.to {
            None => duration,
            Some(v) => span_point(&format!("{what}: to"), v, false, &shots, &layers)?,
        };
        if !(0.0..duration).contains(&start) || !(start < end && end <= duration + 1e-4) {
            bail!(
                "{what}: the span {}–{} s must lie within the film (0–{} s), from before to{}",
                glsl_float(start),
                glsl_float(end),
                glsl_float(duration),
                if loop_frames.is_some() {
                    "; a loop film's layers stay inside one period"
                } else {
                    ""
                }
            );
        }
        let end = end.min(duration);
        let (fade_in, fade_out) = match l.fade {
            None => (0.0, 0.0),
            Some([a, b]) => {
                if !(a >= 0.0 && b >= 0.0) || a + b > end - start + 1e-4 {
                    bail!(
                        "{what}: fade = [{a}, {b}] must be ≥ 0 and fit the span ({} s)",
                        glsl_float(end - start)
                    );
                }
                (a, b)
            }
        };
        let opacity = match &l.opacity {
            None => Anim::from_toml(&toml::Value::Float(1.0))?,
            Some(v) => {
                let a = Anim::from_toml(v).with_context(|| format!("{what}: opacity"))?;
                if a.dim() != 1 {
                    bail!("{what}: opacity is one number (or keyframes of one number)");
                }
                a
            }
        };
        let scale = match &l.scale {
            None => None,
            Some(toml::Value::String(s)) if s == "auto" => None,
            Some(v) => {
                let x = v
                    .as_float()
                    .or_else(|| v.as_integer().map(|i| i as f64))
                    .unwrap_or(-1.0) as f32;
                if !(x > 0.0 && x <= 1.0) {
                    bail!(
                        "{what}: scale must be \"auto\" or a fraction of the output size in (0, 1]"
                    );
                }
                Some(x)
            }
        };
        let mut uniforms = Vec::new();
        for (name, v) in &l.uniforms {
            if !is_ident(name) {
                bail!("{what}: \"{name}\" is not a valid GLSL name");
            }
            let a = Anim::from_toml(v).with_context(|| format!("{what}: uniform {name}"))?;
            uniforms.push((name.clone(), a));
        }
        let mut params = Vec::new();
        for (name, v) in &l.params {
            if !is_ident(name) {
                bail!("{what}: param \"{name}\" is not a valid GLSL name");
            }
            params.push((
                name.clone(),
                vector(v).with_context(|| format!("{what}: param {name}"))?,
            ));
        }
        let channels = match &l.channels {
            Some(list) => channel_list(
                &format!("{what}: "),
                list,
                Some(LayerRefs {
                    earlier: &layer_ids[..index],
                    later: &layer_ids[index..],
                    on_buffer: None,
                }),
            )?,
            None => Default::default(),
        };
        let matte = match &l.matte {
            None => None,
            Some(v) => Some(matte_spec(&what, v, &layer_ids[..index], sc)?),
        };
        layers.push(Layer {
            id: l.id,
            start,
            end,
            scene,
            uniforms,
            params,
            channels,
            opacity,
            fade: (fade_in, fade_out),
            scale,
            time_offset: l.time_offset.unwrap_or(0.0),
            time_scale: l.time_scale.unwrap_or(1.0),
            blend,
            matte,
        });
    }
    let buffers = build_buffers(
        sc,
        &mut channel_list,
        buffer_files,
        &shots,
        &layers,
        duration,
        loop_frames,
    )?;
    Ok((shots, layers, buffers, duration, loop_frames))
}

/// `t` in [0, period): the f64 remainder, kept below the period after rounding.
pub fn wrap_time(t: f64, period: f64) -> f64 {
    let w = t.rem_euclid(period);
    if w >= period { 0.0 } else { w }
}

/// The frame count of a `loop = true` film: its duration must be a whole number of
/// frames, so the frame after the last is frame 0.
pub(super) fn loop_frames(duration: f32, num: u32, den: u32) -> Result<u64> {
    let exact = duration as f64 * num as f64 / den as f64;
    let n = exact.round();
    if n >= 1.0 && (exact - n).abs() <= 1e-3 {
        return Ok(n as u64);
    }
    let secs = |f: f64| f * den as f64 / num as f64;
    let (lo, hi) = (exact.floor().max(1.0), exact.ceil().max(1.0));
    let fps = if den == 1 {
        num.to_string()
    } else {
        format!("{num}/{den}")
    };
    bail!(
        "loop = true needs a film that lasts a whole number of frames (so the frame after the \
         last is frame 0), but it lasts {duration}s, {exact:.3} frames at {fps} fps; make it \
         {:.4}s ({lo} frames) or {:.4}s ({hi} frames)",
        secs(lo),
        secs(hi)
    )
}

/// One end of a layer's span: movie seconds, or `"<id>[.start|.settled|.end]"` with an
/// optional `+ s` / `- s`. A bare shot id means its start for `from` and its end for
/// `to`; layers (earlier ones only) have `.start` and `.end`.
pub(super) fn span_point(
    what: &str,
    v: &toml::Value,
    is_from: bool,
    shots: &[Shot],
    layers: &[Layer],
) -> Result<f32> {
    if let Some(x) = v.as_float().or_else(|| v.as_integer().map(|i| i as f64)) {
        return Ok(x as f32);
    }
    let Some(s) = v.as_str() else {
        bail!("{what}: a number of seconds or \"<shot>[.start|.settled|.end] ± s\"");
    };
    let (base, offset) = match s
        .find(['+', '-'])
        .filter(|&i| i > 0 && s[..i].ends_with(' '))
    {
        Some(i) => {
            let n: f32 = s[i + 1..].trim().parse().map_err(|_| {
                anyhow!(
                    "{what}: \"{s}\": the offset after {} must be seconds",
                    &s[i..=i]
                )
            })?;
            (s[..i].trim(), if &s[i..=i] == "-" { -n } else { n })
        }
        None => (s.trim(), 0.0),
    };
    let (id, part) = base.split_once('.').unwrap_or((base, ""));
    let t = if let Some(shot) = shots.iter().find(|x| x.id == id) {
        match part {
            "" if is_from => shot.start,
            "" => shot.end(),
            "start" => shot.start,
            "settled" => shot.settled(),
            "end" => shot.end(),
            p => bail!("{what}: \"{s}\": a shot has .start, .settled and .end, not .{p}"),
        }
    } else if let Some(layer) = layers.iter().find(|x| x.id == id) {
        match part {
            "" if is_from => layer.start,
            "" => layer.end,
            "start" => layer.start,
            "end" => layer.end,
            p => bail!("{what}: \"{s}\": a layer has .start and .end, not .{p}"),
        }
    } else {
        bail!(
            "{what}: \"{s}\": no shot or earlier layer called \"{id}\" (shots: {}{})",
            shots
                .iter()
                .map(|x| x.id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            if layers.is_empty() {
                String::new()
            } else {
                format!(
                    "; layers: {}",
                    layers
                        .iter()
                        .map(|x| x.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        );
    };
    Ok(t + offset)
}
