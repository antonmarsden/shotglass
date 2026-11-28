//! iChannel textures: binding, sampling, streaming, edits, eviction, unpack state.

use super::*;

pub(super) fn channel_binding(gl: &Gl, dir: &Path) -> Result<String> {
    png(dir, "t/quad.png", 2, 2, quadrants)?;
    png(dir, "t/magenta.png", 3, 5, |_, _| [200, 0, 200, 255])?;
    // Quarters across: iChannel0 stretched (orientation), iChannel1 (unbound),
    // iChannel2, and iChannelResolution as colours.
    write(
        dir,
        &[(
            "s.glsl",
            &scene(
                "float q = floor(uv.x * 4.0);\n  \
                 vec3 r = vec3(iChannelResolution[0].xy, iChannelResolution[1].x + iChannelResolution[2].x + iChannelResolution[2].y + iChannelResolution[2].z * 2.0) / 10.0;\n  \
                 vec3 c = q < 1.0 ? texture(iChannel0, vec2(uv.x * 4.0, uv.y)).rgb\n    \
                 : q < 2.0 ? texture(iChannel1, uv).rgb + 0.2 * iChannelResolution[1].z\n    \
                 : q < 3.0 ? texture(iChannel2, uv).rgb : r;\n  \
                 fragColor = vec4(c, 1.0);",
            ),
        )],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.1\"\n[textures]\nquad = { file = \"t/quad.png\", filter = \"nearest\" }\n\
         [[shot]]\nid=\"up\"\nscene=\"s.glsl\"\nduration=1\n\
         channels = [\"quad\", \"\", { file = \"t/magenta.png\", filter = \"nearest\" }]\n\
         [[shot]]\nid=\"down\"\nscene=\"s.glsl\"\nduration=1\n\
         channels = [{ file = \"t/quad.png\", filter = \"nearest\", vflip = false }, \"\", { file = \"t/magenta.png\", filter = \"nearest\" }]\n",
    )?;
    let mut r = renderer(gl, &m)?;
    let up = shot(&mut r, &m, at_shot(0, 0.5), 1.0)?;
    // Default vflip: the image's top-left texel at the frame's top-left, as on Shadertoy.
    expect_px("iChannel0 top-left", &up, 10, 10, [255, 0, 0], 1)?;
    expect_px("iChannel0 top-right", &up, 30, 10, [0, 255, 0], 1)?;
    expect_px("iChannel0 bottom-left", &up, 10, 80, [0, 0, 255], 1)?;
    expect_px("iChannel0 bottom-right", &up, 30, 80, [255, 255, 255], 1)?;
    expect_px(
        "unbound iChannel1 (black, resolution 0)",
        &up,
        60,
        45,
        [0, 0, 0],
        1,
    )?;
    expect_px("iChannel2", &up, 100, 45, [200, 0, 200], 1)?;
    // iChannelResolution: [0] = (2, 2, 1), [1] = 0, [2] = (3, 5, 1) → b = (3 + 5 + 2) / 10.
    expect_px("iChannelResolution", &up, 140, 45, [51, 51, 255], 1)?;
    let down = shot(&mut r, &m, at_shot(1, 0.5), 1.0)?;
    // vflip = false: the top-left texel lands at the bottom-left.
    expect_px("vflip = false, bottom-left", &down, 10, 80, [255, 0, 0], 1)?;
    expect_px("vflip = false, top-left", &down, 10, 10, [0, 0, 255], 1)?;
    Ok(String::new())
}

pub(super) fn channel_sampling(gl: &Gl, dir: &Path) -> Result<String> {
    // 4×1 greys: texel centres at u = 0.125, 0.375, 0.625, 0.875.
    png(dir, "ramp.png", 4, 1, |x, _| {
        let v = [0, 85, 170, 255][x as usize];
        [v, v, v, 255]
    })?;
    // Eighths across, each one grey value.
    write(
        dir,
        &[
            (
                "a.glsl",
                &scene(
                    "int k = int(uv.x * 8.0);\n  float v = 0.0;\n  \
                     if (k == 0) v = texture(iChannel0, vec2(0.24, 0.5)).r;      // nearest: texel 0\n  \
                     if (k == 1) v = texture(iChannel1, vec2(0.24, 0.5)).r;      // linear: between 0 and 1\n  \
                     if (k == 2) v = texture(iChannel0, vec2(1.375, 0.5)).r;     // repeat → 0.375\n  \
                     if (k == 3) v = texture(iChannel1, vec2(1.375, 0.5)).r;     // clamp → edge\n  \
                     if (k == 4) v = texture(iChannel2, vec2(1.375, 0.5)).r;     // mirror → 0.625\n  \
                     if (k == 5) v = textureLod(iChannel3, vec2(0.375, 0.5), 0.0).r;\n  \
                     if (k == 6) v = textureLod(iChannel3, vec2(0.375, 0.5), 2.0).r;  // 1×1 mip\n  \
                     if (k == 7) v = textureLod(iChannel3, vec2(0.25, 0.5), 1.0).r;   // 2×1 mip\n  \
                     fragColor = vec4(vec3(v), 1.0);",
                ),
            ),
            (
                "b.glsl",
                &scene(
                    "int k = int(uv.x * 8.0);\n  float v = 0.0;\n  \
                     if (k == 0) v = texture(iChannel0, vec2(0.375, 0.5)).r;\n  \
                     if (k == 1) v = textureLod(iChannel1, vec2(0.375, 0.5), 2.0).r;\n  \
                     fragColor = vec4(vec3(v), 1.0);",
                ),
            ),
        ],
    )?;
    let m = movie(
        dir,
        "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=1\n\
         channels = [{ file = \"ramp.png\", filter = \"nearest\" }, { file = \"ramp.png\", filter = \"linear\", wrap = \"clamp\" }, \
         { file = \"ramp.png\", filter = \"linear\", wrap = \"mirror\" }, \"ramp.png\"]\n\
         [[shot]]\nid=\"b\"\nscene=\"b.glsl\"\nduration=1\n\
         channels = [{ file = \"ramp.png\", filter = \"nearest\", srgb = true }, { file = \"ramp.png\", srgb = true }]\n",
    )?;
    let mut r = renderer(gl, &m)?;
    let a = shot(&mut r, &m, at_shot(0, 0.5), 1.0)?;
    let col = |k: usize| k * W as usize / 8 + 10;
    for (k, what, want, tol) in [
        (0, "nearest", 0, 1),
        (1, "linear", 39, 3),
        (2, "repeat", 85, 1),
        (3, "clamp", 255, 1),
        (4, "mirror", 170, 1),
        (5, "mipmap, level 0", 85, 1),
        (6, "mipmap, 1×1 level (the driver's filter)", 128, 4),
        (7, "mipmap, 2×1 level (the driver's filter)", 43, 4),
    ] {
        expect_px(what, &a, col(k), 45, [want; 3], tol)?;
    }
    let b = shot(&mut r, &m, at_shot(1, 0.5), 1.0)?;
    // sRGB 85 decodes to linear 0.0908 (23/255). Its 1×1 mip is the linear-space average
    // of (0, 0.0908, 0.402, 1.0) = 0.373 (95/255); a gamma-space average would be 54.
    expect_px("srgb", &b, col(0), 45, [23; 3], 2)?;
    let srgb_mip = px(&b, col(1), 45)[0];
    let mut note = String::new();
    if !(85..=105).contains(&srgb_mip) {
        if (45..=65).contains(&srgb_mip) {
            note = format!(
                "this driver averages sRGB mipmaps in gamma space ({srgb_mip}, linear would be 95)"
            );
        } else {
            bail!("srgb mipmap: {srgb_mip}, wanted about 95 (a linear-space average)");
        }
    }
    // One file, five samplers: two uploads (plain and sRGB), not five.
    if r.textures_ready() != 2 {
        bail!(
            "{} textures uploaded for one file used plain and as sRGB (wanted 2)",
            r.textures_ready()
        );
    }
    Ok(note)
}

pub(super) fn texture_streaming(gl: &Gl, dir: &Path) -> Result<String> {
    png(dir, "big.png", 512, 512, |x, y| {
        [(x / 2) as u8, (y / 2) as u8, ((x ^ y) & 255) as u8, 255]
    })?;
    write(
        dir,
        &[(
            "s.glsl",
            &scene("fragColor = vec4(texture(iChannel0, uv).rgb, 1.0);"),
        )],
    )?;
    let m = movie(
        dir,
        "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\nchannels = [\"big.png\"]\n",
    )?;
    let want = {
        let mut r = renderer(gl, &m)?;
        shot(&mut r, &m, at_shot(0, 0.5), 1.0)?
    };
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None);
    r.set_upload_budget(64 << 10); // 1 MB of texels: 16+ frames of uploads
    r.set_movie(&m).map_err(|e| anyhow!("{e:?}"))?;
    let mut frames = 0;
    let got = loop {
        if let Some(_t) = r.render(&m, W, H, &info(0.5, 1.0), false) {
            break r.read_output().unwrap();
        }
        if r.textures_ready() > 0 && frames > 0 {
            bail!("the texture was ready but the shot didn't draw");
        }
        frames += 1;
        if frames > 5000 {
            bail!("the shot never became ready without blocking");
        }
        r.pump(&m, 0.5, false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    if frames == 0 {
        bail!("a non-blocking render drew before the texture was loaded");
    }
    if r.blocking_texture_loads != 0 {
        bail!(
            "playback loaded {} texture(s) on the spot",
            r.blocking_texture_loads
        );
    }
    if r.texture_bands < 16 {
        bail!(
            "the upload took {} band(s); the per-frame budget should spread it over 16+",
            r.texture_bands
        );
    }
    expect_same("non-blocking render with a texture", &got, &want, 0, 0.0)?;
    Ok(format!(
        "ready after {frames} frame(s), {} upload bands",
        r.texture_bands
    ))
}

pub(super) fn channels_high(gl: &Gl, dir: &Path) -> Result<String> {
    png(dir, "t/quad.png", 2, 2, quadrants)?;
    png(dir, "t/magenta.png", 3, 5, |_, _| [200, 0, 200, 255])?;
    // Quarters across: iChannel7 stretched (orientation), iChannel5 (unbound),
    // iChannel4, and iChannelResolution[4] / [5] / [7] as colours.
    write(
        dir,
        &[(
            "s.glsl",
            &scene(
                "float q = floor(uv.x * 4.0);\n  \
                 vec3 r = vec3(iChannelResolution[7].xy, iChannelResolution[5].x + iChannelResolution[4].x + iChannelResolution[4].y + iChannelResolution[4].z * 2.0) / 10.0;\n  \
                 vec3 c = q < 1.0 ? texture(iChannel7, vec2(uv.x * 4.0, uv.y)).rgb\n    \
                 : q < 2.0 ? texture(iChannel5, uv).rgb + 0.2 * iChannelResolution[5].z\n    \
                 : q < 3.0 ? texture(iChannel4, uv).rgb : r;\n  \
                 fragColor = vec4(c, 1.0);",
            ),
        )],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.3\"\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\n\
         channels = [\"\", \"\", \"\", \"\", { file = \"t/magenta.png\", filter = \"nearest\" }, \"\", \"\", \
         { file = \"t/quad.png\", filter = \"nearest\" }]\n",
    )?;
    let mut r = renderer(gl, &m)?;
    let f = shot(&mut r, &m, at_shot(0, 0.5), 1.0)?;
    expect_px("iChannel7 top-left", &f, 10, 10, [255, 0, 0], 1)?;
    expect_px("iChannel7 bottom-right", &f, 30, 80, [255, 255, 255], 1)?;
    expect_px(
        "unbound iChannel5 (black, resolution 0)",
        &f,
        60,
        45,
        [0, 0, 0],
        1,
    )?;
    expect_px("iChannel4", &f, 100, 45, [200, 0, 200], 1)?;
    // [7] = (2, 2, 1), [5] = 0, [4] = (3, 5, 1) → b = (3 + 5 + 2) / 10.
    expect_px("iChannelResolution[4..7]", &f, 140, 45, [51, 51, 255], 1)?;
    Ok(String::new())
}

pub(super) fn texture_edits(gl: &Gl, dir: &Path) -> Result<String> {
    png(dir, "t.png", 4, 4, |_, _| [255, 0, 0, 255])?;
    write(
        dir,
        &[(
            "s.glsl",
            &scene("fragColor = vec4(texture(iChannel0, uv).rgb, 1.0);"),
        )],
    )?;
    let toml = "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\nchannels = [\"t.png\"]\n";
    let m = movie(dir, toml)?;
    let mut r = renderer(gl, &m)?;
    let red = shot(&mut r, &m, at_shot(0, 1.0), 1.0)?;
    expect_px("first version", &red, 80, 45, [255, 0, 0], 1)?;
    let compiles = r.compiles_started;
    // An editor saving in pieces: half the file.
    let bytes = std::fs::read(dir.join("t.png"))?;
    std::fs::write(dir.join("t.png"), &bytes[..bytes.len() / 2])?;
    let m2 = movie(dir, toml)?;
    r.set_movie(&m2)
        .map_err(|_| anyhow!("set_movie refused a broken image (should fall back)"))?;
    let playing = r
        .render(&m2, W, H, &info(1.0, 1.0), false)
        .ok_or_else(|| anyhow!("playback showed nothing instead of the last working texture"))?;
    let _ = playing;
    let still = shot(&mut r, &m2, at_shot(0, 1.0), 1.0)?;
    expect_same("after a broken texture edit", &still, &red, 1, 0.0)?;
    let errors: String = r.errors().iter().map(|e| e.to_string()).collect();
    if !errors.contains("t.png") || !errors.contains("still being saved") {
        bail!("the broken texture wasn't reported: {errors:?}");
    }
    png(dir, "t.png", 8, 8, |_, _| [0, 255, 0, 255])?;
    let m3 = movie(dir, toml)?;
    r.set_movie(&m3).map_err(|e| anyhow!("{e:?}"))?;
    let green = shot(&mut r, &m3, at_shot(0, 1.0), 1.0)?;
    expect_px("fixed texture", &green, 80, 45, [0, 255, 0], 1)?;
    if !r.errors().is_empty() {
        bail!("errors remained after the fix");
    }
    if r.compiles_started != compiles {
        bail!(
            "texture-only edits compiled {} program(s)",
            r.compiles_started - compiles
        );
    }
    // A missing file refuses the whole new version; the old one keeps drawing.
    std::fs::remove_file(dir.join("t.png"))?;
    if movie(dir, toml).is_ok() {
        bail!("a missing texture file was accepted on load");
    }
    if r.set_movie(&m3).is_ok() {
        bail!("set_movie accepted a version whose texture file is gone");
    }
    let after = shot(&mut r, &m3, at_shot(0, 1.0), 1.0)?;
    expect_same("after a refused version", &after, &green, 0, 0.0)?;
    Ok(String::new())
}

pub(super) fn texture_eviction(gl: &Gl, dir: &Path) -> Result<String> {
    png(dir, "a.png", 16, 16, |x, y| {
        [(x * 16) as u8, (y * 16) as u8, 90, 255]
    })?;
    png(dir, "b.png", 16, 16, |_, _| [10, 20, 30, 255])?;
    write(
        dir,
        &[(
            "s.glsl",
            &scene("fragColor = vec4(texture(iChannel0, uv).rgb, 1.0);"),
        )],
    )?;
    let m = movie(
        dir,
        "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\nchannels = [\"a.png\"]\n\
         [[shot]]\nid=\"b\"\nscene=\"s.glsl\"\nduration=60\nchannels = [\"b.png\"]\n",
    )?;
    let mut r = renderer(gl, &m)?;
    r.set_texture_budget(1);
    let first = shot(&mut r, &m, at_shot(0, 1.0), 1.0)?;
    shot(&mut r, &m, at_shot(1, 40.0), 1.0)?;
    for _ in 0..31 {
        r.pump(&m, 42.0, false);
    }
    if r.textures_ready() != 1 {
        bail!(
            "{} textures resident over a 1-byte budget (wanted only the one in use)",
            r.textures_ready()
        );
    }
    let again = shot(&mut r, &m, at_shot(0, 1.0), 1.0)?;
    expect_same("evicted texture after re-upload", &again, &first, 0, 0.0)?;
    Ok(String::new())
}

pub(super) fn texture_freeze(gl: &Gl, dir: &Path) -> Result<String> {
    png(dir, "t.png", 2, 2, |_, _| [255, 0, 0, 255])?;
    write(
        dir,
        &[
            (
                "a.glsl",
                &scene("fragColor = vec4(texture(iChannel0, uv).rgb, 1.0);"),
            ),
            ("b.glsl", &scene("fragColor = vec4(0.0, 1.0, 0.0, 1.0);")),
            (
                "fx/mix.glsl",
                "vec4 transition(vec2 uv) { return mix(getFromColor(uv), getToColor(uv), progress); }\n",
            ),
        ],
    )?;
    let toml = "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=2\nchannels = [\"t.png\"]\n\
                [[shot]]\nid=\"b\"\nscene=\"b.glsl\"\nduration=2\n\
                transition = { type = \"fx/mix.glsl\", duration = 1, freeze = true }\n";
    let at = At::Transition {
        from: 0,
        to: 1,
        from_local: 1.0,
        to_local: 0.0,
        progress: 0.0,
    };
    let m = movie(dir, toml)?;
    let mut r = renderer(gl, &m)?;
    let f = shot(&mut r, &m, at, 1.0)?;
    expect_px("frozen outgoing shot", &f, 80, 45, [255, 0, 0], 1)?;
    png(dir, "t.png", 3, 3, |_, _| [0, 0, 255, 255])?;
    let m2 = movie(dir, toml)?;
    r.set_movie(&m2).map_err(|e| anyhow!("{e:?}"))?;
    // Playing: the edited texture isn't loaded yet, so the freeze uses the last
    // working one; once it's loaded, the frozen frame must be redrawn with it.
    let t = m2.shots[1].start;
    r.render_at(&m2, W, H, &info(t, 1.0), at, false)
        .ok_or_else(|| anyhow!("nothing drawn while the edited texture loads"))?;
    let f = r.read_output().unwrap();
    expect_px(
        "frozen shot while the edit loads",
        &f,
        80,
        45,
        [255, 0, 0],
        1,
    )?;
    for _ in 0..2000 {
        r.pump(&m2, t, false);
        if r.texture_progress().0 == 1 && r.textures_ready() == 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    for _ in 0..31 {
        r.pump(&m2, t, false); // drops the fallback (and the old texture)
    }
    r.render_at(&m2, W, H, &info(t, 1.0), at, false)
        .ok_or_else(|| anyhow!("nothing drawn after the edited texture loaded"))?;
    let f = r.read_output().unwrap();
    expect_px(
        "frozen shot after the edit loaded",
        &f,
        80,
        45,
        [0, 0, 255],
        1,
    )?;
    let f = shot(&mut r, &m2, at, 1.0)?;
    expect_px(
        "frozen shot after a texture edit",
        &f,
        80,
        45,
        [0, 0, 255],
        1,
    )?;
    Ok(String::new())
}

pub(super) fn texture_unpack_state(gl: &Gl, dir: &Path) -> Result<String> {
    use crate::gfx::Tracked;
    use eframe::glow::{self, HasContext};
    // 3 px wide: 12-byte rows, which a stray alignment of 8 or row length would misread.
    png(dir, "t.png", 3, 3, |x, y| {
        [(x * 100) as u8, (y * 100) as u8, 77, 255]
    })?;
    write(
        dir,
        &[(
            "s.glsl",
            &scene("fragColor = vec4(texelFetch(iChannel0, ivec2(fragCoord) % 3, 0).rgb, 1.0);"),
        )],
    )?;
    let m = movie(
        dir,
        "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\nchannels = [{ file = \"t.png\", vflip = false }]\n",
    )?;
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None);
    r.set_movie(&m).map_err(|e| anyhow!("{e:?}"))?;
    // What other code (the sound path's staging buffers, egui) may leave behind.
    let junk = unsafe {
        let b = gl.new_buffer().map_err(|e| anyhow!(e))?;
        gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(b));
        gl.buffer_data_u8_slice(glow::PIXEL_UNPACK_BUFFER, &[9u8; 4096], glow::STATIC_DRAW);
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 8);
        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 5);
        gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 1);
        b
    };
    let f = shot(&mut r, &m, at_shot(0, 0.5), 1.0)?;
    let state = r.unpack_state();
    unsafe { gl.free_buffer(junk) };
    // Rows of the frame are top first; texelFetch row 0 is the frame's bottom row,
    // and vflip = false keeps the file's top row as texel row 0.
    for (x, y) in [(0usize, 0usize), (1, 0), (2, 1), (1, 2)] {
        let row = H as usize - 1 - y;
        expect_px(
            &format!("texel ({x}, {y})"),
            &f,
            x,
            row,
            [(x * 100) as u8, (y * 100) as u8, 77],
            1,
        )?;
    }
    if state != [4, 0, 0, 0, 0] {
        bail!(
            "unpack state after an upload is {state:?} (alignment, row length, skip rows, \
             skip pixels, unpack buffer); wanted [4, 0, 0, 0, 0]"
        );
    }
    Ok(String::new())
}
