//! Comps (#74): `[comps]`, comp files, the comp graph, comps with buffers, mattes.

use super::*;

/// Deepest comp nesting (each level holds its own targets).
pub const MAX_COMP_DEPTH: usize = 4;

/// `[comps]`: `name = "file.toml"` or `{ file, size = [w, h], loop }`, in name order.
pub(super) fn comp_decls(t: &BTreeMap<String, toml::Value>) -> Result<Vec<CompDecl>> {
    let mut out = vec![];
    for (name, v) in t {
        let what = format!("[comps] {name}");
        if !is_pass_id(name) || name == "input" || name == "picture" {
            bail!(
                "{what}: a comp name may only use letters, digits, _ and - (and isn't input or \
                 picture)"
            );
        }
        let (rel, size, looping, warm) = match v {
            toml::Value::String(s) => (s.clone(), None, false, false),
            toml::Value::Table(t) => {
                let mut file = None;
                let mut size = None;
                let mut looping = false;
                let mut warm = false;
                for (k, val) in t {
                    match k.as_str() {
                        "file" => {
                            file = Some(
                                val.as_str()
                                    .ok_or_else(|| anyhow!("{what}: file must be a path"))?
                                    .to_string(),
                            )
                        }
                        "size" => {
                            let wh: Vec<i64> = val
                                .as_array()
                                .map(|a| a.iter().filter_map(toml::Value::as_integer).collect())
                                .unwrap_or_default();
                            match wh[..] {
                                [w, h] if (1..=16384).contains(&w) && (1..=16384).contains(&h) => {
                                    size = Some((w as u32, h as u32))
                                }
                                _ => bail!("{what}: size must be [width, height] in pixels"),
                            }
                        }
                        "loop" => (looping, warm) = loop_mode(&format!("{what}: "), Some(val))?,
                        other => bail!(
                            "{what}: unknown option `{other}` (a comp has file, size and loop)"
                        ),
                    }
                }
                let Some(file) = file else {
                    bail!("{what}: a comp table needs file = \"comps/….toml\"");
                };
                (file, size, looping, warm)
            }
            _ => bail!("{what}: a comp is \"comps/….toml\" or {{ file = \"…\", size, loop }}"),
        };
        library::check_rel_path(&rel).map_err(|e| anyhow!("{what}: {e}"))?;
        if !rel.ends_with(".toml") {
            bail!("{what}: \"{rel}\": a comp file is a .toml with [[shot]]s and [[layer]]s");
        }
        out.push(CompDecl {
            name: name.clone(),
            rel,
            size,
            looping,
            warm,
        });
    }
    Ok(out)
}

/// A comp file's text: what belongs to the film gets a pointer, not "unknown field".
pub(super) fn parse_comp_file(text: &str) -> Result<CompFile> {
    if let Ok(t) = toml::from_str::<toml::Table>(text) {
        for key in [
            "title", "aspect", "fps", "glsl", "loop", "sound", "marker", "textures", "fonts",
            "texts", "comps",
        ] {
            if t.contains_key(key) {
                bail!(
                    "`{key}` belongs to the film (movie.toml); a comp file has [[shot]], \
                     [[layer]] and channels (size and loop go on its [comps] entry)"
                );
            }
        }
    }
    Ok(toml::from_str(text)?)
}

/// Every place a sequence shows a comp (#74): shots, layers, mattes and the channels of
/// shots, layers and buffers, one entry per use, with what uses it.
pub(super) fn comp_uses(m: &Movie) -> Vec<(usize, String)> {
    let mut v = vec![];
    let mut chans = |what: String, list: &[Option<Channel>]| {
        for c in list.iter().flatten() {
            if let Source::Comp { index, .. } = c.source {
                v.push((index, what.clone()));
            }
        }
    };
    for s in &m.shots {
        chans(format!("shot \"{}\"'s channels", s.id), &s.channels);
    }
    for l in &m.layers {
        chans(format!("layer \"{}\"'s channels", l.id), &l.channels);
    }
    for b in &m.buffers {
        chans(format!("buffer \"{}\"'s channels", b.id), &b.channels);
    }
    for s in &m.shots {
        if let Some(c) = s.comp {
            v.push((c, format!("shot \"{}\"", s.id)));
        }
    }
    for l in &m.layers {
        if let LayerScene::Comp(c) = l.scene {
            v.push((c, format!("layer \"{}\"", l.id)));
        }
        if let Some(Matte {
            source: MatteSource::Comp(c),
            ..
        }) = l.matte
        {
            v.push((c, format!("layer \"{}\"'s matte", l.id)));
        }
    }
    v
}

/// The comps a sequence's shots, layers, buffers, channels and mattes show.
pub(super) fn comps_used(m: &Movie) -> Vec<usize> {
    let mut v: Vec<usize> = comp_uses(m).into_iter().map(|(c, _)| c).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Comps with buffers (#138), directly or through a comp they show, are shown in at
/// most one place in the film and its comps: one fold per comp, over comp-local frames,
/// serves one comp time per film frame. (Two uses could ask for two comp frames at once.)
pub(super) fn check_comp_buffers(film: &Movie, comps: &[Comp]) -> Result<()> {
    fn has_buffers(c: usize, comps: &[Comp]) -> bool {
        let m = &comps[c].movie;
        !m.buffers.is_empty() || comps_used(m).into_iter().any(|d| has_buffers(d, comps))
    }
    let mut uses: Vec<Vec<String>> = vec![vec![]; comps.len()];
    for (c, what) in comp_uses(film) {
        uses[c].push(what);
    }
    for comp in comps {
        for (c, what) in comp_uses(&comp.movie) {
            uses[c].push(format!("comp \"{}\": {what}", comp.name));
        }
    }
    for (c, u) in uses.iter().enumerate() {
        if u.len() > 1 && has_buffers(c, comps) {
            bail!(
                "comp \"{}\" has feedback buffers{}, so it can be shown in one place only \
                 (its state follows one comp time per film frame); it is shown by {}. Make a \
                 second comp file for another use",
                comps[c].name,
                if comps[c].movie.buffers.is_empty() {
                    " (in a comp it shows)"
                } else {
                    ""
                },
                u.join(", ")
            );
        }
    }
    Ok(())
}

/// Comps don't show themselves through others, and nest at most `MAX_COMP_DEPTH` deep.
pub(super) fn check_comp_graph(film: &Movie) -> Result<()> {
    let comps = &film.comps[..];
    fn walk(c: usize, comps: &[Comp], path: &mut Vec<usize>) -> Result<()> {
        if let Some(i) = path.iter().position(|&p| p == c) {
            let mut names: Vec<&str> = path[i..].iter().map(|&p| comps[p].name.as_str()).collect();
            names.push(&comps[c].name);
            bail!("comps show each other in a cycle: {}", names.join(" → "));
        }
        path.push(c);
        if path.len() > MAX_COMP_DEPTH {
            let names: Vec<&str> = path.iter().map(|&p| comps[p].name.as_str()).collect();
            bail!(
                "comps nest {} deep ({}); at most {MAX_COMP_DEPTH} levels",
                path.len(),
                names.join(" → ")
            );
        }
        let m = &comps[c].movie;
        for d in comps_used(m) {
            walk(d, comps, path)?;
        }
        path.pop();
        Ok(())
    }
    for c in 0..comps.len() {
        walk(c, comps, &mut vec![])?;
    }
    // Depth from the film: what it shows is level 1.
    fn depth(c: usize, comps: &[Comp]) -> usize {
        let m = &comps[c].movie;
        1 + comps_used(m)
            .into_iter()
            .map(|d| depth(d, comps))
            .max()
            .unwrap_or(0)
    }
    for c in comps_used(film) {
        if depth(c, comps) > MAX_COMP_DEPTH {
            bail!(
                "comp \"{}\" nests more than {MAX_COMP_DEPTH} levels deep",
                comps[c].name
            );
        }
    }
    Ok(())
}

/// A layer's `matte = { source = "@id", channel = "alpha" | "luma", invert = false }`.
pub(super) fn matte_spec(
    what: &str,
    v: &toml::Value,
    earlier: &[String],
    sc: &SeqCtx,
) -> Result<Matte> {
    let what = format!("{what}: matte");
    let Some(t) = v.as_table() else {
        bail!("{what}: a table {{ source = \"@<layer or comp>\", channel = \"alpha\" }}");
    };
    let mut source = None;
    let mut luma = false;
    let mut invert = false;
    for (k, val) in t {
        match k.as_str() {
            "source" => {
                let s = val.as_str().unwrap_or("");
                let Some(id) = s.strip_prefix('@') else {
                    bail!("{what}: source \"{s}\" must be \"@<earlier layer>\" or \"@<comp>\"");
                };
                source = Some(if let Some(j) = earlier.iter().position(|e| e == id) {
                    MatteSource::Layer(j)
                } else if sc.comps.iter().any(|c| c == id) {
                    MatteSource::Comp(sc.comp_ref(&what, id)?)
                } else {
                    bail!(
                        "{what}: source \"{s}\" is neither an earlier layer nor a comp (layers \
                         below: {}; comps: {})",
                        quoted_list(earlier.iter()),
                        quoted_list(sc.comps.iter())
                    );
                });
            }
            "channel" => {
                luma = match val.as_str() {
                    Some("alpha") => false,
                    Some("luma") => true,
                    _ => bail!("{what}: channel must be \"alpha\" or \"luma\""),
                }
            }
            "invert" => {
                invert = val
                    .as_bool()
                    .ok_or_else(|| anyhow!("{what}: invert must be true or false"))?
            }
            other => bail!("{what}: unknown option `{other}` (source, channel, invert)"),
        }
    }
    let Some(source) = source else {
        bail!("{what}: needs source = \"@<earlier layer or comp>\"");
    };
    Ok(Matte {
        source,
        luma,
        invert,
    })
}
