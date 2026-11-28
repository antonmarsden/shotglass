//! Comps and mattes (#74).

use super::*;

/// A comp (#74) with a dissolve and a layer, written both as a comp file and as a film.
pub(super) const COMP_BODY: &str = "[[shot]]\nid=\"a\"\nscene=\"ca.glsl\"\nduration=1\n\
     [[shot]]\nid=\"b\"\nscene=\"cb.glsl\"\nduration=1\n\
     transition = { type = \"fx/crossfade.glsl\", duration = 0.5 }\n\
     [[layer]]\nid=\"t\"\nscene=\"ct.glsl\"\nfrom=0.25\nto=1.25\nfade=[0.25, 0.25]\n";

pub(super) fn comp_files(dir: &Path) -> Result<()> {
    write(
        dir,
        &[
            (
                "ca.glsl",
                &scene("fragColor = vec4(uv, fract(iTime * 0.37 + float(iFrame) * 0.01), 1.0);"),
            ),
            (
                "cb.glsl",
                &scene("fragColor = vec4(1.0 - uv.x, iMovieTime / 2.0, COMP_DURATION / 4.0, 1.0);"),
            ),
            (
                "ct.glsl",
                &scene("fragColor = vec4(1.0, 1.0, 0.0, uv.y < 0.5 ? 0.7 : 0.0);"),
            ),
            ("c.toml", COMP_BODY),
        ],
    )
}

pub(super) fn comps_render(gl: &Gl, dir: &Path) -> Result<String> {
    comp_files(dir)?;
    // The comp as a film of its own (COMP_DURATION is defined for it by hand).
    let alone_dir = dir.join("alone");
    write(&alone_dir, FX)?;
    for f in ["ca.glsl", "ct.glsl"] {
        std::fs::copy(dir.join(f), alone_dir.join(f))?;
    }
    std::fs::write(
        alone_dir.join("cb.glsl"),
        format!(
            "#define COMP_DURATION 1.5\n{}",
            std::fs::read_to_string(dir.join("cb.glsl"))?
        ),
    )?;
    crate::project::write_fixture(&alone_dir, &format!("shotglass = \"0.1.4\"\n{COMP_BODY}"))?;
    let alone = Movie::load(&alone_dir)?;
    // The film: the comp as a shot (the whole comp), then as a channel at its size and
    // shifted in time, and a loop comp that wraps.
    write(
        dir,
        &[
            (
                "read.glsl",
                &scene(
                    "vec4 c = texture(iChannel0, vec2(0.25, 0.25));\n  \
                     fragColor = vec4(iChannelResolution[0].x / 255.0, iChannelResolution[0].y / 255.0, c.g, 1.0);",
                ),
            ),
            (
                "clock.glsl",
                &scene("fragColor = vec4(0.0, iTime / 4.0, 0.0, 1.0);"),
            ),
            (
                "clock.toml",
                "[[shot]]\nid=\"k\"\nscene=\"clock.glsl\"\nduration=2\n",
            ),
        ],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.7\"\n[comps]\nc = \"c.toml\"\n\
         small = { file = \"clock.toml\", size = [40, 20] }\n\
         wrap = { file = \"clock.toml\", loop = true }\n\
         [[shot]]\nid=\"s\"\ncomp=\"c\"\nduration=1.5\n\
         [[shot]]\nid=\"r\"\nscene=\"read.glsl\"\nduration=1\nchannels = [{ source = \"@small\", time_offset = 1 }]\n\
         [[shot]]\nid=\"w\"\nscene=\"read.glsl\"\nduration=3\ntime_offset=1\nchannels = [\"@wrap\"]\n",
    )?;
    let mut r = renderer(gl, &m)?;
    let mut a = renderer(gl, &alone)?;
    for t in [0.1, 0.3, 0.6, 0.75, 0.9, 1.2, 1.45] {
        let got = shot(&mut r, &m, at_shot(0, t), 1.0)?;
        let want = shot(&mut a, &alone, alone.at(t), 1.0)?;
        expect_same(
            &format!("comp shot at {t} s vs the comp as a film"),
            &got,
            &want,
            0,
            0.0,
        )?;
        // At render scale 0.5 too, and drawn in strips. The Apple Software
        // Renderer rounds the comp's extra copy pass 1/255 off on a few pixels
        // (macOS CI: 0.55%, max 1); everywhere else it must match exactly.
        let got = shot(&mut r, &m, at_shot(0, t), 0.5)?;
        let want = shot(&mut a, &alone, alone.at(t), 0.5)?;
        let tol = if inexact_sound(gl).is_some() { 1 } else { 0 };
        expect_same(
            &format!("comp shot at {t} s, scale 0.5"),
            &got,
            &want,
            tol,
            0.0,
        )?;
    }
    r.set_tiles(Some(3));
    let tiled = shot(&mut r, &m, at_shot(0, 0.75), 1.0)?;
    r.set_tiles(None);
    let single = shot(&mut r, &m, at_shot(0, 0.75), 1.0)?;
    expect_same("comp drawn in strips", &tiled, &single, 0, 0.0)?;
    // @small: 40×20 whatever the output, at the reader's iTime (0.5) + 1.
    expect_px(
        "@comp at its size and time",
        &shot(&mut r, &m, at_shot(1, 0.5), 1.0)?,
        80,
        45,
        [40, 20, 96],
        1,
    )?;
    expect_px(
        "@comp at its size, render scale 0.5",
        &shot(&mut r, &m, at_shot(1, 0.5), 0.5)?,
        80,
        45,
        [40, 20, 96],
        1,
    )?;
    // @wrap (unsized: the output's size), loop: iTime 1 + 2.5 = 3.5 wraps to 1.5.
    expect_px(
        "@comp loops",
        &shot(&mut r, &m, at_shot(2, 2.5), 1.0)?,
        80,
        45,
        [160, 90, 96],
        1,
    )?;
    // Held outside its range: comp "small" read at 4 s shows its last frame (iTime 59/30).
    let m2 = movie(
        dir,
        "shotglass = \"0.1.7\"\n[comps]\nsmall = { file = \"clock.toml\", size = [40, 20] }\n\
         [[shot]]\nid=\"r\"\nscene=\"read.glsl\"\nduration=1\nchannels = [{ source = \"@small\", time_offset = 4 }]\n",
    )?;
    let mut r2 = renderer(gl, &m2)?;
    let want = (59.0 / 30.0 / 4.0 * 255.0f32).round() as u8;
    expect_px(
        "@comp holds its last frame",
        &shot(&mut r2, &m2, at_shot(0, 0.5), 1.0)?,
        80,
        45,
        [40, 20, want],
        1,
    )?;
    Ok(String::new())
}

pub(super) fn mattes(gl: &Gl, dir: &Path) -> Result<String> {
    // Picture blue. "m" (opacity 0: only a matte) is straight (0.4, 0.4, 0.4, 0.5):
    // premultiplied, alpha 0.5 and luma 0.2. Red layers through it, one per second.
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.0, 0.0, 1.0, 1.0);")),
            ("m.glsl", &scene("fragColor = vec4(0.4, 0.4, 0.4, 0.5);")),
            ("red.glsl", &scene("fragColor = vec4(1.0, 0.0, 0.0, 1.0);")),
            (
                "half.glsl",
                &scene("fragColor = vec4(vec3(uv.x < 0.5 ? 1.0 : 0.0), 1.0);"),
            ),
            (
                "half.toml",
                "[[shot]]\nid=\"h\"\nscene=\"half.glsl\"\nduration=1\n",
            ),
            ("fx/multiply.glsl", MULTIPLY),
        ],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.7\"\n[comps]\nhalf = \"half.toml\"\n\
         [[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=7\n\
         [[layer]]\nid=\"m\"\nscene=\"m.glsl\"\nto=6\nopacity=0\n\
         [[layer]]\nid=\"alpha\"\nscene=\"red.glsl\"\nfrom=0\nto=1\nmatte={ source=\"@m\" }\n\
         [[layer]]\nid=\"luma\"\nscene=\"red.glsl\"\nfrom=1\nto=2\nmatte={ source=\"@m\", channel=\"luma\" }\n\
         [[layer]]\nid=\"inv\"\nscene=\"red.glsl\"\nfrom=2\nto=3\nmatte={ source=\"@m\", channel=\"luma\", invert=true }\n\
         [[layer]]\nid=\"comp\"\nscene=\"red.glsl\"\nfrom=3\nto=4\nmatte={ source=\"@half\", channel=\"luma\" }\n\
         [[layer]]\nid=\"blend\"\nscene=\"red.glsl\"\nfrom=4\nto=5\nopacity=0.5\nblend=\"fx/multiply.glsl\"\nmatte={ source=\"@half\", channel=\"luma\" }\n\
         [[layer]]\nid=\"gone\"\nscene=\"red.glsl\"\nfrom=6\nto=7\nmatte={ source=\"@m\" }\n",
    )?;
    let mut r = renderer(gl, &m)?;
    let at = |r: &mut Renderer, t: f32| shot(r, &m, at_shot(0, t), 1.0);
    expect_px(
        "alpha matte 0.5",
        &at(&mut r, 0.5)?,
        80,
        45,
        [128, 0, 128],
        1,
    )?;
    expect_px("luma matte 0.2", &at(&mut r, 1.5)?, 80, 45, [51, 0, 204], 1)?;
    expect_px(
        "inverted luma 0.8",
        &at(&mut r, 2.5)?,
        80,
        45,
        [204, 0, 51],
        1,
    )?;
    let c = at(&mut r, 3.5)?;
    expect_px("comp matte (white half)", &c, 20, 45, [255, 0, 0], 1)?;
    expect_px("comp matte (black half)", &c, 140, 45, [0, 0, 255], 1)?;
    // Multiply of red over blue is black; at opacity 0.5 × matte 1: half-way.
    let b = at(&mut r, 4.5)?;
    expect_px("matted blend (white half)", &b, 20, 45, [0, 0, 128], 1)?;
    expect_px("matted blend (black half)", &b, 140, 45, [0, 0, 255], 1)?;
    // A matte layer that isn't drawn hides the layer it mattes.
    expect_px(
        "matte source inactive",
        &at(&mut r, 6.5)?,
        80,
        45,
        [0, 0, 255],
        1,
    )?;
    Ok(String::new())
}

pub(super) fn comps_ready_and_fallback(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.0, 0.0, 1.0, 1.0);")),
            ("in.glsl", &scene("fragColor = vec4(0.0, 1.0, 0.0, 1.0);")),
            (
                "in.toml",
                "[[shot]]\nid=\"x\"\nscene=\"in.glsl\"\nduration=1\n",
            ),
        ],
    )?;
    let toml = "shotglass = \"0.1.7\"\n[comps]\ninner = \"in.toml\"\n\
                [[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\n\
                [[shot]]\nid=\"b\"\ncomp=\"inner\"\nduration=1\n";
    let m = movie(dir, toml)?;
    let want = {
        let mut r = renderer(gl, &m)?;
        shot(&mut r, &m, at_shot(1, 0.5), 1.0)?
    };
    expect_px("comp shot", &want, 80, 45, [0, 255, 0], 1)?;
    // Playback never draws the comp shot before the comp's own shot is compiled.
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None);
    r.set_movie(&m).map_err(|e| anyhow!("{e:?}"))?;
    if r.render(&m, W, H, &info(1.5, 1.0), false).is_some() {
        bail!("playback drew a comp shot before the comp's programs were compiled");
    }
    let (got, frames) = play_until_ready(&mut r, &m, 1.5, "comp shot")?;
    expect_same("non-blocking comp shot", &got, &want, 0, 0.0)?;
    if r.blocking_texture_loads != 0 || r.blocking_footage_loads != 0 {
        bail!("playback loaded on the spot");
    }
    // A broken edit inside the comp keeps its last working version (fallback key
    // "inner/x"), and the error names the comp.
    write(dir, &[("in.glsl", &scene("fragColor = vec4(oops);"))])?;
    let m2 = movie(dir, toml)?;
    r.set_movie(&m2)
        .map_err(|_| anyhow!("set_movie refused a comp compile error (should defer it)"))?;
    let still = shot(&mut r, &m2, at_shot(1, 0.5), 1.0)?;
    expect_same("after a broken comp edit", &still, &want, 0, 0.0)?;
    if !r
        .errors()
        .iter()
        .any(|e| e.what.contains("comp \"inner\": shot \"x\""))
    {
        bail!("the broken comp shot wasn't reported: {:?}", r.errors());
    }
    // A missing comp file refuses the whole version.
    std::fs::remove_file(dir.join("in.toml"))?;
    if movie(dir, toml).is_ok() {
        bail!("a missing comp file loaded");
    }
    Ok(format!("ready after {frames} frame(s)"))
}

pub(super) fn comps_footage(gl: &Gl, dir: &Path) -> Result<String> {
    use crate::movie::ItemRef;
    sequence(dir, "f/s_", 24, 4, 2)?;
    write(
        dir,
        &[
            ("s.glsl", &footage_scene()),
            (
                "ft.toml",
                "[[shot]]\nid=\"f\"\nscene=\"s.glsl\"\nduration=3\n\
                 channels = [{ frames = \"f/s_##.png\", fps = 24, end = \"loop\" }]\n",
            ),
        ],
    )?;
    // The comp shot starts the comp half a second in: comp frame = film frame + 15.
    let m = movie(
        dir,
        "shotglass = \"0.1.7\"\nfps = 30\n[comps]\nft = \"ft.toml\"\n\
         [[shot]]\nid=\"a\"\ncomp=\"ft\"\nduration=2\ntime_offset=0.5\n",
    )?;
    let cm = &m.comps[0].movie;
    let mut r = renderer(gl, &m)?;
    r.set_readback(true);
    if r.render(&m, W, H, &FrameInfo::frame(&m, 0), false)
        .is_some()
    {
        bail!("a non-blocking render drew before the comp's footage frame was loaded");
    }
    let mut waits = 0;
    for n in 0..45u64 {
        let t = m.frame_time(n);
        let mut tries = 0;
        loop {
            r.pump(&m, t, false);
            if r.render(&m, W, H, &FrameInfo::frame(&m, n), false)
                .is_some()
            {
                let f = r.read_output().ok_or_else(|| anyhow!("no output"))?;
                let want = cm.source_frame(ItemRef::Shot(0), 0, n + 15);
                let got = frame_of(px(&f, 80, 45));
                if got != want {
                    bail!("film frame {n}: the comp drew source frame {got:?}, wanted {want:?}");
                }
                break;
            }
            tries += 1;
            if tries >= 5000 {
                bail!("film frame {n} never drew without blocking");
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        waits += tries;
    }
    if r.blocking_footage_loads != 0 {
        bail!(
            "playback loaded {} footage frame(s) on the spot",
            r.blocking_footage_loads
        );
    }
    // Nested: the footage comp inside another comp, 0.8 s in (past the window at 0).
    // Nothing on the film uses it directly, so prefetch must learn its time from the
    // stalled frame's plan.
    write(
        dir,
        &[(
            "outer.toml",
            "[[shot]]\nid=\"o\"\ncomp=\"ft\"\nduration=3\ntime_offset=0.8\n",
        )],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.7\"\nfps = 30\n[comps]\nft = \"ft.toml\"\nouter = \"outer.toml\"\n\
         [[shot]]\nid=\"a\"\ncomp=\"outer\"\nduration=2\n",
    )?;
    let cm = &m.comps[0].movie;
    let mut r = renderer(gl, &m)?;
    r.set_readback(true);
    for n in 0..10u64 {
        let t = m.frame_time(n);
        let mut tries = 0;
        loop {
            r.pump(&m, t, false);
            if r.render(&m, W, H, &FrameInfo::frame(&m, n), false)
                .is_some()
            {
                let f = r.read_output().ok_or_else(|| anyhow!("no output"))?;
                let want = cm.source_frame(ItemRef::Shot(0), 0, n + 24);
                let got = frame_of(px(&f, 80, 45));
                if got != want {
                    bail!("nested, film frame {n}: source frame {got:?}, wanted {want:?}");
                }
                break;
            }
            tries += 1;
            if tries >= 5000 {
                bail!("nested, film frame {n} never drew without blocking");
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    Ok(format!("45 frames, {waits} waits; nested 10 frames"))
}

pub(super) fn comps_removed_free(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.0, 0.0, 1.0, 1.0);")),
            ("in.glsl", &scene("fragColor = vec4(0.0, 1.0, 0.0, 1.0);")),
            (
                "in.toml",
                "[[shot]]\nid=\"x\"\nscene=\"in.glsl\"\nduration=1\n",
            ),
        ],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.7\"\n[comps]\ninner = \"in.toml\"\n\
         [[shot]]\nid=\"a\"\ncomp=\"inner\"\nduration=1\n",
    )?;
    let mut r = renderer(gl, &m)?;
    shot(&mut r, &m, at_shot(0, 0.5), 1.0)?;
    let texs = r.seq_textures(1);
    if texs.is_empty() {
        bail!("the comp's sequence allocated no targets");
    }
    let m2 = movie(
        dir,
        "shotglass = \"0.1.7\"\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\n",
    )?;
    r.set_movie(&m2).map_err(|e| anyhow!("{e:?}"))?;
    let live = texs
        .iter()
        .filter(|t| unsafe { gl.is_texture(**t) })
        .count();
    if live != 0 {
        bail!(
            "{live} of {} texture(s) of the removed comp still exist",
            texs.len()
        );
    }
    Ok(format!("{} textures freed", texs.len()))
}

pub(super) fn comps_freeze(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            ("c.glsl", &scene("fragColor = vec4(0.0, 1.0, 0.0, 1.0);")),
            (
                "a.glsl",
                &scene(
                    "fragColor = vec4(float(iFrame % 256) / 255.0, texture(iChannel0, uv).g, 0.0, 1.0);",
                ),
            ),
            ("b.glsl", &scene("fragColor = vec4(0.0, 0.0, 1.0, 1.0);")),
            (
                "hold.glsl",
                "vec4 transition(vec2 uv) { return getFromColor(uv); }\n",
            ),
            (
                "c.toml",
                "[[shot]]\nid=\"x\"\nscene=\"c.glsl\"\nduration=1\n",
            ),
        ],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.7\"\nfps = 30\n[comps]\nc = \"c.toml\"\n\
         [[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=2\nchannels = [\"@c\"]\n\
         [[shot]]\nid=\"b\"\nscene=\"b.glsl\"\nduration=2\n\
         transition = { type = \"hold.glsl\", duration = 1.0, freeze = true }\n",
    )?;
    let mut r = renderer(gl, &m)?;
    r.set_readback(true);
    let mut got = vec![];
    for t in [1.2, 1.6] {
        if r.render(&m, W, H, &info(t, 1.0), true).is_none() {
            bail!("nothing rendered at {t}");
        }
        let f = r.read_output().ok_or_else(|| anyhow!("no output"))?;
        got.push(px(&f, 80, 45));
    }
    // The transition starts at 1.0 s: film frame 30, whatever the frame drawn.
    for (t, p) in [1.2, 1.6].iter().zip(&got) {
        if p[0] != 30 || p[1] != 255 {
            bail!("frozen shot at {t}: {p:?}, wanted iFrame 30 and the comp's green");
        }
    }
    Ok("iFrame 30 at 1.2 s and 1.6 s".into())
}
