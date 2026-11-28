//! Text blocks and glyph runs.

use super::*;

/// The fixture font, embedded so the selftest runs from an installed binary too.
pub(super) const TEST_FONT: &[u8] =
    include_bytes!("../../tests/films/text/fonts/NotoSans-subset.ttf");

pub(super) fn text_textures(gl: &Gl, dir: &Path) -> Result<String> {
    std::fs::create_dir_all(dir.join("fonts"))?;
    std::fs::write(dir.join("fonts/sans.ttf"), TEST_FONT)?;
    png(dir, "t/grey.png", 2, 2, |_, _| [128, 128, 128, 255])?;
    // Left half: the text block stretched over it (coverage in red, distance in green).
    // Right half: iChannelMeta[0] (em, padding, pitch) / 255, plus iChannelMeta[1] (an
    // image: must be 0) as blue.
    write(
        dir,
        &[(
            "s.glsl",
            &scene(
                "vec4 t = texture(iChannel0, vec2(uv.x * 2.0, uv.y));\n  \
                 vec3 meta = vec3(iChannelMeta[0].x, iChannelMeta[0].w, iChannelMeta[0].z) / 255.0;\n  \
                 meta.b += dot(iChannelMeta[1], vec4(1.0));\n  \
                 fragColor = vec4(uv.x < 0.5 ? vec3(t.r, t.g, t.a) : meta, 1.0);",
            ),
        )],
    )?;
    let toml = |words: &str| {
        format!(
            "shotglass = \"0.1.3\"\n[fonts]\nsans = {{ file = \"fonts/sans.ttf\", licence = \"OFL-1.1\" }}\n\
             [texts]\ntitle = {{ text = \"{words}\", font = \"sans\", size = 120, sdf_spread = 6 }}\n\
             [[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\nchannels = [\"title\", \"t/grey.png\"]\n"
        )
    };
    let m = movie(dir, &toml("HI"))?;
    // Blocking render (as still/export): the reference.
    let want = {
        let mut r = renderer(gl, &m)?;
        shot(&mut r, &m, at_shot(0, 0.5), 1.0)?
    };
    // iChannelMeta: em 120, padding 6, pitch 1.2 × 120 = 144; an image's meta is 0.
    expect_px("iChannelMeta", &want, 120, 45, [120, 6, 144], 1)?;
    // The block: padding is empty (coverage 0, distance far outside); the letters are ink.
    expect_px("text padding", &want, 1, 1, [0, 0, 0], 2)?;
    let inked = (0..80)
        .flat_map(|c| (0..90).map(move |r| (c, r)))
        .filter(|&(c, r)| px(&want, c, r)[0] == 255)
        .count();
    if inked < 300 {
        bail!("the text block has {inked} fully covered pixels; expected the letters' ink");
    }
    for (c, r) in (0..80).flat_map(|c| (0..90).map(move |r| (c, r))) {
        let p = px(&want, c, r);
        if p[0] != p[2] {
            bail!("text alpha differs from coverage at ({c}, {r}): {p:?}");
        }
    }
    // Playing from cold: frames are held (never drawn half-made) until the block is
    // rasterised and uploaded, nothing loads on the spot, and the frame matches still.
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None);
    r.set_movie(&m).map_err(|e| anyhow!("{e:?}"))?;
    let (got, frames) = play_until_ready(&mut r, &m, 0.5, "text shot")?;
    if frames == 0 {
        bail!("a non-blocking render drew before the text was rasterised");
    }
    if r.blocking_texture_loads != 0 {
        bail!(
            "playback rasterised {} text(s) on the spot",
            r.blocking_texture_loads
        );
    }
    expect_same("non-blocking render of a text", &got, &want, 0, 0.0)?;
    // New words: a new texture, no program compiled.
    let compiles = r.compiles_started;
    let m2 = movie(dir, &toml("Hi there"))?;
    r.set_movie(&m2).map_err(|e| anyhow!("{e:?}"))?;
    let changed = shot(&mut r, &m2, at_shot(0, 0.5), 1.0)?;
    if diff(&changed, &want, 0).1 == 0.0 {
        bail!("editing the text didn't change the picture");
    }
    if r.compiles_started != compiles {
        bail!(
            "a text edit compiled {} program(s)",
            r.compiles_started - compiles
        );
    }
    // A broken font refuses the edit; the last working version stays up.
    std::fs::write(dir.join("fonts/sans.ttf"), b"<html>not a font</html>")?;
    match movie(dir, &toml("Bye")) {
        Ok(_) => bail!("a broken font was accepted on load"),
        Err(e) if !format!("{e:#}").contains("not a font") => {
            bail!("the broken font wasn't explained: {e:#}")
        }
        Err(_) => {}
    }
    let after = shot(&mut r, &m2, at_shot(0, 0.5), 1.0)?;
    expect_same("after a refused text edit", &after, &changed, 0, 0.0)?;
    Ok(format!("ready after {frames} frame(s)"))
}

pub(super) fn glyph_runs(gl: &Gl, dir: &Path) -> Result<String> {
    std::fs::create_dir_all(dir.join("fonts"))?;
    std::fs::write(dir.join("fonts/sans.ttf"), TEST_FONT)?;
    // Frame row r (top first) shows texel row r, read with texelFetch: columns 0–79 the
    // atlas (iChannel0), 80–149 the table (iChannel1, from texel 0). The top half shows
    // RGB, the bottom half alpha (in red) of texel rows 0–44. Columns 150+ show
    // iChannelMeta: (atlas em, atlas spread, table header texels) / 255.
    write(
        dir,
        &[(
            "s.glsl",
            &scene(
                "ivec2 p = ivec2(fragCoord);\n  \
                 int r = int(iResolution.y) - 1 - p.y;\n  \
                 bool alphaRow = r >= 45;\n  \
                 int tr = alphaRow ? r - 45 : r;\n  \
                 bool atlas = p.x < 80;\n  \
                 ivec2 at = ivec2(atlas ? p.x : p.x - 80, tr);\n  \
                 ivec2 size = ivec2(atlas ? iChannelResolution[0].xy : iChannelResolution[1].xy);\n  \
                 vec4 t = atlas ? texelFetch(iChannel0, at, 0) : texelFetch(iChannel1, at, 0);\n  \
                 if (any(greaterThanEqual(at, size))) t = vec4(0.0);\n  \
                 fragColor = vec4(alphaRow ? vec3(t.a, 0.0, 0.0) : t.rgb, 1.0);\n  \
                 if (p.x >= 150) fragColor = vec4(iChannelMeta[0].x, iChannelMeta[0].y, iChannelMeta[1].y, 255.0) / 255.0;",
            ),
        )],
    )?;
    let toml = |words: &str| {
        format!(
            "shotglass = \"0.1.8\"\n[fonts]\nsans = {{ file = \"fonts/sans.ttf\", licence = \"OFL-1.1\" }}\n\
             [texts]\nrun = {{ text = \"{words}\", font = \"sans\", size = 40, glyphs = true }}\n\
             [[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\n\
             channels = [\"run\", {{ text = \"run\", part = \"glyphs\" }}]\n"
        )
    };
    let m = movie(dir, &toml("HIx"))?;
    let run = m.shots[0].channels[0]
        .as_ref()
        .and_then(|c| c.text())
        .ok_or_else(|| anyhow!("iChannel0 isn't the text"))?
        .glyph_run_shared()?;
    let (atlas, table) = (&run.atlas.image, &run.table);
    if atlas.num_rows() < 2 || atlas.cells().all(|p| p[3] == atlas[(0, 0)][3]) {
        bail!("the fixture atlas is flat; the orientation check would prove nothing");
    }
    let texel = |img: &toodee::TooDee<[u8; 4]>, c: usize, r: usize| {
        if c < img.num_cols() && r < img.num_rows() {
            img[(c, r)]
        } else {
            [0; 4]
        }
    };
    let want_px = |c: usize, r: usize| -> [u8; 3] {
        if c >= 150 {
            return [48, 4, crate::glyphrun::HEADER_TEXELS as u8];
        }
        let alpha = r >= 45;
        let tr = if alpha { r - 45 } else { r };
        let t = if c < 80 {
            texel(atlas, c, tr)
        } else {
            texel(table, c - 80, tr)
        };
        if alpha {
            [t[3], 0, 0]
        } else {
            [t[0], t[1], t[2]]
        }
    };
    let check = |what: &str, f: &Frame| -> Result<()> {
        for r in 0..H as usize {
            for c in 0..W as usize {
                let (got, want) = (px(f, c, r), want_px(c, r));
                if got[..3] != want {
                    bail!("{what}: pixel ({c}, {r}) is {got:?}, the CPU texel says {want:?}");
                }
            }
        }
        Ok(())
    };
    // Blocking render (as still/export): every byte as built, the right way up.
    let want = {
        let mut r = renderer(gl, &m)?;
        shot(&mut r, &m, at_shot(0, 0.5), 1.0)?
    };
    check("blocking render", &want)?;
    // Playing from cold: held until both parts are ready, nothing loaded on the spot.
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None);
    r.set_movie(&m).map_err(|e| anyhow!("{e:?}"))?;
    let (got, frames) = play_until_ready(&mut r, &m, 0.5, "glyph run shot")?;
    if frames == 0 {
        bail!("a non-blocking render drew before the glyph run was built");
    }
    if r.blocking_texture_loads != 0 {
        bail!(
            "playback built {} glyph run part(s) on the spot",
            r.blocking_texture_loads
        );
    }
    check("non-blocking render", &got)?;
    // New words: new textures, no program compiled.
    let compiles = r.compiles_started;
    let m2 = movie(dir, &toml("Hi there"))?;
    r.set_movie(&m2).map_err(|e| anyhow!("{e:?}"))?;
    let changed = shot(&mut r, &m2, at_shot(0, 0.5), 1.0)?;
    if diff(&changed, &want, 0).1 == 0.0 {
        bail!("editing the glyph run didn't change the picture");
    }
    if r.compiles_started != compiles {
        bail!(
            "a glyph run edit compiled {} program(s)",
            r.compiles_started - compiles
        );
    }
    Ok(format!(
        "ready after {frames} frame(s); atlas {}×{}",
        atlas.num_cols(),
        atlas.num_rows()
    ))
}
