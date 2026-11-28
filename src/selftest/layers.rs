//! Layers: composite, readiness, scale, blends and references, hidden layers.

use super::*;

pub(super) fn layers_compose(gl: &Gl, dir: &Path) -> Result<String> {
    // A solid picture (0.2, 0.4, 0.6); "half" (red, alpha 0.5, opacity 0.5) over
    // [0.5, 1.5); "invert" reads the picture below (@input on iChannel3) from 1.0 on.
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.2, 0.4, 0.6, 1.0);")),
            ("half.glsl", &scene("fragColor = vec4(1.0, 0.0, 0.0, 0.5);")),
            (
                "invert.glsl",
                &scene(
                    "fragColor = vec4(1.0 - texture(iChannel3, fragCoord / iChannelResolution[3].xy).rgb, 1.0);",
                ),
            ),
            ("fade.glsl", &scene("fragColor = vec4(1.0, 1.0, 1.0, 1.0);")),
        ],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.4\"\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=4\n\
         [[layer]]\nid=\"half\"\nscene=\"half.glsl\"\nfrom=0.5\nto=1.5\nopacity=0.5\n\
         [[layer]]\nid=\"invert\"\nscene=\"invert.glsl\"\nfrom=1.0\nto=2.0\nchannels=[\"\", \"\", \"\", \"@input\"]\n\
         [[layer]]\nid=\"fade\"\nscene=\"fade.glsl\"\nfrom=2.0\nto=4.0\nfade=[1.0, 1.0]\n",
    )?;
    let mut r = renderer(gl, &m)?;
    // @input is another pass's picture, not a texture: nothing to load, nothing pending
    // (the player's "loading textures" would otherwise never finish).
    if r.texture_progress() != (0, 0) {
        bail!(
            "an @input channel counts as a texture: {:?}",
            r.texture_progress()
        );
    }
    let at = |r: &mut Renderer, t: f32| shot(r, &m, at_shot(0, t), 1.0);
    // Before any layer: the picture as it is.
    expect_px("no layer", &at(&mut r, 0.25)?, 80, 45, [51, 102, 153], 1)?;
    // Premultiplied over at opacity 0.5: a = 0.25 → 0.25·red + 0.75·below.
    expect_px("half over", &at(&mut r, 0.75)?, 80, 45, [102, 77, 115], 1)?;
    // @input sees everything below (half included) and inverts it.
    expect_px(
        "invert of half over",
        &at(&mut r, 1.25)?,
        80,
        45,
        [153, 178, 140],
        1,
    )?;
    // Half's span is half-open: at 1.5 only invert is left.
    expect_px("span ends", &at(&mut r, 1.75)?, 80, 45, [204, 153, 102], 1)?;
    // A 1 s smooth fade: halfway in, half the white; fully in at the middle of the span.
    expect_px("mid fade", &at(&mut r, 2.5)?, 80, 45, [153, 178, 204], 2)?;
    expect_px("faded in", &at(&mut r, 3.0)?, 80, 45, [255, 255, 255], 1)?;
    Ok(String::new())
}

pub(super) fn layers_ready_and_fallback(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.0, 0.0, 1.0, 1.0);")),
            (
                "l.glsl",
                &scene("fragColor = vec4(1.0, 0.0, 0.0, uv.x < 0.5 ? 1.0 : 0.0);"),
            ),
        ],
    )?;
    let toml = "shotglass = \"0.1.4\"\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\n\
                [[layer]]\nid=\"left\"\nscene=\"l.glsl\"\n";
    let m = movie(dir, toml)?;
    let want = {
        let mut r = renderer(gl, &m)?;
        shot(&mut r, &m, at_shot(0, 1.0), 1.0)?
    };
    expect_px("layer drawn (left)", &want, 20, 45, [255, 0, 0], 1)?;
    expect_px("layer transparent (right)", &want, 140, 45, [0, 0, 255], 1)?;
    // The shot alone first, so its program is ready; then the layer arrives (a new id:
    // no fallback). Until the layer compiles, playback holds the frame: never the
    // picture without its layer. Then exactly the blocking frame.
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None);
    let alone = movie(dir, "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\n")?;
    r.set_movie(&alone).map_err(|e| anyhow!("{e:?}"))?;
    shot(&mut r, &alone, at_shot(0, 1.0), 1.0)?;
    let m = movie(dir, toml)?;
    r.set_movie(&m).map_err(|e| anyhow!("{e:?}"))?;
    if r.render(&m, W, H, &info(1.0, 1.0), false).is_some() {
        bail!("playback drew the shot before its new layer was compiled");
    }
    let (got, frames) = play_until_ready(&mut r, &m, 1.0, "layered shot")?;
    expect_same("non-blocking render with a layer", &got, &want, 0, 0.0)?;
    // A compile error in the layer: the last working layer stays on screen.
    write(dir, &[("l.glsl", &scene("fragColor = vec4(oops);"))])?;
    let m2 = movie(dir, toml)?;
    r.set_movie(&m2)
        .map_err(|_| anyhow!("set_movie refused a layer compile error (should defer it)"))?;
    let still = shot(&mut r, &m2, at_shot(0, 1.0), 1.0)?;
    expect_same("after a broken layer edit", &still, &want, 0, 0.0)?;
    if !r.errors().iter().any(|e| e.what.contains("layer \"left\"")) {
        bail!("the broken layer wasn't reported: {:?}", r.errors());
    }
    Ok(format!("ready after {frames} frame(s)"))
}

pub(super) fn layers_scale(gl: &Gl, dir: &Path) -> Result<String> {
    // Each layer writes its own iResolution.x / 255 into red: what it was drawn at.
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.0, 0.0, 0.0, 1.0);")),
            (
                "res.glsl",
                &scene(
                    "fragColor = vec4(iResolution.x / 255.0, 0.0, 0.0, uv.y > 0.5 ? 1.0 : 0.0);",
                ),
            ),
            (
                "res2.glsl",
                &scene(
                    "fragColor = vec4(0.0, iResolution.x / 255.0, 0.0, uv.y <= 0.5 ? 1.0 : 0.0);",
                ),
            ),
        ],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.4\"\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\n\
         [[layer]]\nid=\"fixed\"\nscene=\"res.glsl\"\nscale=0.5\n\
         [[layer]]\nid=\"auto\"\nscene=\"res2.glsl\"\n",
    )?;
    let mut r = renderer(gl, &m)?;
    // At render scale 1 (W = 160): fixed draws at 80, auto at 160.
    let full = shot(&mut r, &m, at_shot(0, 0.5), 1.0)?;
    expect_px("scale 0.5 at render scale 1", &full, 80, 20, [80, 0, 0], 1)?;
    expect_px("auto at render scale 1", &full, 80, 70, [0, 160, 0], 1)?;
    // At render scale 0.5: fixed still 80, auto follows to 80.
    let half = shot(&mut r, &m, at_shot(0, 0.5), 0.5)?;
    expect_px(
        "scale 0.5 at render scale 0.5",
        &half,
        80,
        20,
        [80, 0, 0],
        1,
    )?;
    expect_px("auto at render scale 0.5", &half, 80, 70, [0, 80, 0], 1)?;
    Ok(String::new())
}

/// A film-local multiply blend, written against the blend interface: the layer arrives
/// premultiplied with its opacity applied, so unpremultiply, multiply, mix by coverage.
pub(super) const MULTIPLY: &str = "vec4 blend(vec2 uv) {\n  vec4 b = getBase(uv);\n  vec4 l = getLayer(uv);\n  vec3 m = b.rgb * l.rgb / max(l.a, 1e-5);\n  return vec4(mix(b.rgb, m, l.a), 1.0);\n}\n";

pub(super) fn layers_blend_and_refs(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.2, 0.4, 0.6, 1.0);")),
            (
                "green.glsl",
                &scene("fragColor = vec4(0.5, 1.0, 0.5, 1.0);"),
            ),
            ("white.glsl", &scene("fragColor = vec4(1.0);")),
            (
                "pic.glsl",
                &scene(
                    "fragColor = vec4(texture(iChannel0, fragCoord / iChannelResolution[0].xy).rgb, 1.0);",
                ),
            ),
            ("red.glsl", &scene("fragColor = vec4(1.0, 0.0, 0.0, 1.0);")),
            (
                "copy.glsl",
                &scene(
                    "fragColor = vec4(iChannelResolution[1].x / 255.0, texture(iChannel1, uv).r, 0.0, 1.0);",
                ),
            ),
            ("fx/multiply.glsl", MULTIPLY),
        ],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.5\"\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=5\n\
         [[layer]]\nid=\"mul\"\nscene=\"green.glsl\"\nto=1.0\nblend=\"fx/multiply.glsl\"\n\
         [[layer]]\nid=\"half\"\nscene=\"green.glsl\"\nfrom=1.0\nto=2.0\nopacity=0.5\nblend=\"fx/multiply.glsl\"\n\
         [[layer]]\nid=\"white\"\nscene=\"white.glsl\"\nfrom=2.0\nto=3.0\n\
         [[layer]]\nid=\"pic\"\nscene=\"pic.glsl\"\nfrom=2.0\nto=3.0\nchannels=[\"@picture\"]\n\
         [[layer]]\nid=\"src\"\nscene=\"red.glsl\"\nfrom=3.0\nto=4.0\nscale=0.5\nopacity=0.0\n\
         [[layer]]\nid=\"copy\"\nscene=\"copy.glsl\"\nfrom=3.0\nto=5.0\nchannels=[\"\", \"@src\"]\n",
    )?;
    let mut r = renderer(gl, &m)?;
    let at = |r: &mut Renderer, t: f32| shot(r, &m, at_shot(0, t), 1.0);
    // Multiply at opacity 1: (0.2, 0.4, 0.6) × (0.5, 1, 0.5).
    expect_px(
        "multiply blend",
        &at(&mut r, 0.5)?,
        80,
        45,
        [26, 102, 77],
        1,
    )?;
    // At opacity 0.5 the blend sees alpha 0.5: halfway between the base and the product.
    expect_px(
        "multiply at opacity 0.5",
        &at(&mut r, 1.5)?,
        80,
        45,
        [38, 102, 115],
        1,
    )?;
    // @picture sees the picture track past the white layer below.
    expect_px(
        "@picture under a white layer",
        &at(&mut r, 2.5)?,
        80,
        45,
        [51, 102, 153],
        1,
    )?;
    // @src: the earlier layer's own output (red), though it composites at opacity 0,
    // at its own size (scale 0.5 of 160 = 80).
    expect_px("@<layer>", &at(&mut r, 3.5)?, 80, 45, [80, 255, 0], 1)?;
    // After src's span: copy still runs and reads black, resolution 0.
    expect_px(
        "@<layer> while it's inactive",
        &at(&mut r, 4.5)?,
        80,
        45,
        [0, 0, 0],
        1,
    )?;
    Ok(String::new())
}

pub(super) fn layers_hidden(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.2, 0.4, 0.6, 1.0);")),
            ("red.glsl", &scene("fragColor = vec4(1.0, 0.0, 0.0, 1.0);")),
            (
                "copy.glsl",
                &scene(
                    "fragColor = vec4(0.0, texture(iChannel0, uv).r, 0.0, uv.x < 0.5 ? 1.0 : 0.0);",
                ),
            ),
        ],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.5\"\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\n\
         [[layer]]\nid=\"red\"\nscene=\"red.glsl\"\n\
         [[layer]]\nid=\"copy\"\nscene=\"copy.glsl\"\nchannels=[\"@red\"]\n",
    )?;
    let mut r = renderer(gl, &m)?;
    let both = shot(&mut r, &m, at_shot(0, 0.5), 1.0)?;
    expect_px("both layers: copy (left)", &both, 20, 45, [0, 255, 0], 1)?;
    expect_px("both layers: red (right)", &both, 140, 45, [255, 0, 0], 1)?;
    // Mute "red": the picture shows through on the right; copy reads black on the left.
    r.set_hidden_layers(["red".to_string()].into());
    let muted = shot(&mut r, &m, at_shot(0, 0.5), 1.0)?;
    expect_px("red muted: copy reads black", &muted, 20, 45, [0, 0, 0], 1)?;
    expect_px("red muted: the picture", &muted, 140, 45, [51, 102, 153], 1)?;
    // Unmuted again: the same frame as before.
    r.set_hidden_layers(Default::default());
    let back = shot(&mut r, &m, at_shot(0, 0.5), 1.0)?;
    expect_same("after unmuting", &back, &both, 0, 0.0)?;
    Ok(String::new())
}
