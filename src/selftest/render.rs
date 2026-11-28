//! Renderer basics: transitions, scale, strips, readback, YUV, errors, program cache, GL objects.

use super::*;

/// What the driver actually compiled: pictures and sound report `__VERSION__`, which
/// the driver sets from the `#version` line the harness emitted.
pub(super) fn version_line(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            (
                "v.glsl",
                &scene("fragColor = vec4(float(__VERSION__) / 1000.0, 0.0, 0.0, 1.0);"),
            ),
            (
                "music.glsl",
                "vec2 mainSound(int samp, float time) {\n  return vec2(float(__VERSION__) / 1000.0);\n}\n",
            ),
        ],
    )?;
    let film = |glsl: Option<u32>| -> Result<Movie> {
        let line = glsl.map_or(String::new(), |v| format!("glsl = \"{v}\"\n"));
        movie(
            dir,
            &format!(
                "{line}[sound]\nmusic = \"music.glsl\"\n[[shot]]\nid=\"v\"\nscene=\"v.glsl\"\nduration=0.1\n"
            ),
        )
    };
    // What the driver compiled each part as (picture, sound), or why it refused the film.
    let compiled = |m: &Movie| -> Result<std::result::Result<(u32, u32), String>> {
        let mut r = Renderer::new(gl.clone())?;
        r.set_cache_dir(None);
        if let Err(e) = r.load_all(m) {
            return Ok(Err(e.iter().map(|e| e.to_string()).collect()));
        }
        let f = shot(
            &mut r,
            m,
            At::Shot {
                shot: 0,
                local: 0.0,
            },
            1.0,
        )?;
        let picture = (px(&f, W as usize / 2, H as usize / 2)[0] as f32 / 255.0 * 1000.0).round();
        let mut st = Soundtrack::new();
        if let Err(e) = st.set_movie(&mut r, m, 48000) {
            return Ok(Err(e.iter().map(|e| e.to_string()).collect()));
        }
        let mut first = None;
        st.render_all(&mut r, |d| {
            first.get_or_insert(d[0]);
        })
        .map_err(|e| anyhow!(e))?;
        let sound = (first.ok_or_else(|| anyhow!("no sound"))? * 1000.0).round();
        Ok(Ok((picture as u32, sound as u32)))
    };
    // 8-bit colour carries the version to within ±2 (4 per step); sound is exact.
    let near = |got: u32, want: u32| got.abs_diff(want) <= 2;
    let probe = Renderer::new(gl.clone())?;
    let driver = probe.driver;
    drop(probe);

    let baseline = if driver.es { 300 } else { 330 };
    let (p, snd) = compiled(&film(None)?)?.map_err(|e| anyhow!("baseline film refused: {e}"))?;
    if !near(p, baseline) || snd != baseline {
        bail!(
            "a film without `glsl` compiled as {p} (picture) / {snd} (sound), want {baseline} \
             on this GLSL {} driver",
            driver.max
        );
    }
    let mut notes = vec![format!("baseline {baseline}")];
    for v in [410, 430] {
        let got = compiled(&film(Some(v))?)?;
        let possible = !driver.es && driver.max >= v;
        match (possible, got) {
            (true, Ok((p, snd))) if near(p, v) && snd == v => notes.push(format!("{v} ok")),
            (true, Ok((p, snd))) => {
                bail!("glsl = \"{v}\" compiled as {p} (picture) / {snd} (sound)")
            }
            (true, Err(e)) => bail!(
                "glsl = \"{v}\" refused on a GLSL {} driver: {e}",
                driver.max
            ),
            (false, Ok(_)) => bail!("glsl = \"{v}\" accepted on a GLSL {} driver", driver.max),
            (false, Err(e)) if e.contains("glsl") => notes.push(format!("{v} refused")),
            (false, Err(e)) => bail!("glsl = \"{v}\" refused without saying why: {e}"),
        }
    }
    Ok(notes.join(", "))
}

pub(super) fn transition_endpoints(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            ("a.glsl", &scene("fragColor = vec4(uv.x, uv.y, 0.25, 1.0);")),
            (
                "b.glsl",
                &scene("fragColor = vec4(0.9 - uv.y, 0.3, uv.x * 0.8, 1.0);"),
            ),
        ],
    )?;
    let kinds = [
        ("fx/crossfade.glsl", ""),
        ("fx/wipe.glsl", ""),
        ("fx/wipe.glsl", "params = { direction = [-0.4, 1.0] }, "),
        (
            "fx/wipe.glsl",
            "params = { direction = [0.0, -1.0], softness = 0.2 }, ",
        ),
    ];
    for (kind, params) in kinds {
        let m = movie(
            dir,
            &format!(
                "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=3\n[[shot]]\nid=\"b\"\nscene=\"b.glsl\"\nduration=3\n\
                 transition = {{ type = \"{kind}\", {params}duration = 1 }}\n"
            ),
        )?;
        let mut r = renderer(gl, &m)?;
        let a = shot(
            &mut r,
            &m,
            At::Shot {
                shot: 0,
                local: 2.0,
            },
            1.0,
        )?;
        let b = shot(
            &mut r,
            &m,
            At::Shot {
                shot: 1,
                local: 0.0,
            },
            1.0,
        )?;
        let tr = |p| At::Transition {
            from: 0,
            to: 1,
            from_local: 2.0,
            to_local: 0.0,
            progress: p,
        };
        let p0 = shot(&mut r, &m, tr(0.0), 1.0)?;
        let p1 = shot(&mut r, &m, tr(1.0), 1.0)?;
        // A pixel can sit exactly on a wipe's edge: allow a handful.
        expect_same(&format!("{kind} {params}at progress 0"), &p0, &a, 2, 0.002)?;
        expect_same(&format!("{kind} {params}at progress 1"), &p1, &b, 2, 0.002)?;
    }
    Ok(format!("{} variants", kinds.len()))
}

pub(super) fn crossfade_and_freeze(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            (
                "a.glsl",
                &scene("fragColor = vec4(fract(iTime * 0.37), 0.5 * uv.x, 0.2, 1.0);"),
            ),
            (
                "b.glsl",
                &scene("fragColor = vec4(0.1, fract(iTime * 0.23), 0.7 * uv.y, 1.0);"),
            ),
        ],
    )?;
    for freeze in [false, true] {
        let m = movie(
            dir,
            &format!(
                "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=3\n[[shot]]\nid=\"b\"\nscene=\"b.glsl\"\nduration=3\n\
                 transition = {{ type = \"fx/crossfade.glsl\", duration = 1, ease = \"linear\", freeze = {freeze} }}\n"
            ),
        )?;
        let mut r = renderer(gl, &m)?;
        // B starts at 2.0; at 2.5 the crossfade is half way.
        let a_local = if freeze { 2.0 } else { 2.5 };
        let a = shot(
            &mut r,
            &m,
            At::Shot {
                shot: 0,
                local: a_local,
            },
            1.0,
        )?;
        let b = shot(
            &mut r,
            &m,
            At::Shot {
                shot: 1,
                local: 0.5,
            },
            1.0,
        )?;
        let got = shot(&mut r, &m, m.at(2.5), 1.0)?;
        let mut expected = a.clone();
        for (e, y) in expected.cells_mut().zip(b.cells()) {
            for c in 0..4 {
                e[c] = ((e[c] as f32 + y[c] as f32) * 0.5).round() as u8;
            }
        }
        expect_same(
            if freeze {
                "frozen crossfade"
            } else {
                "live crossfade"
            },
            &got,
            &expected,
            2,
            0.0,
        )?;
    }
    Ok(String::new())
}

pub(super) fn render_scale_edges(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            ("a.glsl", &scene("fragColor = vec4(0.8, 0.2, 0.4, 1.0);")),
            ("b.glsl", &scene("fragColor = vec4(0.1, 0.6, 0.3, 1.0);")),
        ],
    )?;
    let m = movie(
        dir,
        "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=3\n[[shot]]\nid=\"b\"\nscene=\"b.glsl\"\nduration=3\n\
         transition = { type = \"fx/crossfade.glsl\", duration = 1, ease = \"linear\" }\n",
    )?;
    let mut r = renderer(gl, &m)?;
    let solid = |c: [f32; 3]| -> Frame {
        let q = |x: f32| (x * 255.0).round() as u8;
        Frame::init(W as usize, H as usize, [q(c[0]), q(c[1]), q(c[2]), 255])
    };
    for scale in [0.77, 0.5, 0.35] {
        let a = shot(
            &mut r,
            &m,
            At::Shot {
                shot: 0,
                local: 1.0,
            },
            scale,
        )?;
        expect_same(
            &format!("shot at scale {scale}"),
            &a,
            &solid([0.8, 0.2, 0.4]),
            2,
            0.0,
        )?;
        let mid = shot(&mut r, &m, m.at(2.5), scale)?;
        expect_same(
            &format!("transition at scale {scale}"),
            &mid,
            &solid([0.45, 0.4, 0.35]),
            2,
            0.0,
        )?;
    }
    Ok(String::new())
}

pub(super) fn tiled_drawing(gl: &Gl, dir: &Path) -> Result<String> {
    // The strip count is pinned (otherwise the GPU timer puts it back to 1 on the
    // next frame and nothing is tested), the picture changes with time, and every
    // tiled frame is drawn over a different one: a strip the driver skips shows up as
    // stale pixels instead of hiding behind an identical previous frame. (The Apple
    // Software Renderer dropped parts of scissored strips at 320x180 with 40 and 64
    // strips: the demo's golden strip showed rows from the frame before.)
    write(
        dir,
        &[(
            "s.glsl",
            &scene("fragColor = vec4(uv, fract(uv.x * 7.0 + uv.y * 3.0 + iTime), 1.0);"),
        )],
    )?;
    let m = movie(
        dir,
        "[[shot]]
id=\"a\"
scene=\"s.glsl\"
duration=2
",
    )?;
    let mut r = renderer(gl, &m)?;
    let draw = |r: &mut Renderer, (w, h): (i32, i32), tiles: u32, local: f32, scale: f32| {
        r.set_tiles(Some(tiles));
        let t = m.shots[0].start + local;
        r.render_at(&m, w, h, &info(t, scale), At::Shot { shot: 0, local }, true)
            .ok_or_else(|| anyhow!("nothing rendered"))?;
        r.read_output().ok_or_else(|| anyhow!("no output"))
    };
    for size in [(W, H), (320, 180)] {
        for scale in [1.0, 0.6] {
            let want = draw(&mut r, size, 1, 0.5, scale)?;
            for tiles in [2, 7, 40, 64] {
                draw(&mut r, size, 1, 1.25, scale)?;
                let got = draw(&mut r, size, tiles, 0.5, scale)?;
                expect_same(
                    &format!("{tiles} strips at {}x{} scale {scale}", size.0, size.1),
                    &got,
                    &want,
                    0,
                    0.0,
                )?;
            }
        }
    }
    r.set_tiles(None);
    Ok(String::new())
}

/// The export's YUV pass (#116) against the CPU reference (`yuv::reference`, itself
/// pinned to BT.709 limited-range values by unit tests), at 10 and 8 bits, flipped and
/// plain outputs, through the pipelined loop the export uses: frames come out in
/// order, and the loop waits on the GPU only when its ring is full (on a fast driver,
/// never before the final drain).
pub(super) fn yuv_pass(gl: &Gl, dir: &Path) -> Result<String> {
    use crate::yuv::{YuvReader, reference, samples};
    // Hard colours in the corners (red, blue, white, black), gradients elsewhere, a
    // one-pixel red column at x = 0 for chroma siting, and a different picture each
    // frame, so a stale or reordered frame can't pass.
    write(
        dir,
        &[(
            "a.glsl",
            &scene(
                "vec2 p = floor(fragCoord);\n  \
                 fragColor = vec4(fract(uv.x * 3.0 + float(iFrame) * 0.1), uv.y, fract(p.x * 0.37 + p.y * 0.11), 1.0);\n  \
                 if (p.x < 8.0 && p.y < 8.0) fragColor = vec4(1.0, 0.0, 0.0, 1.0);\n  \
                 if (p.x >= iResolution.x - 8.0 && p.y < 8.0) fragColor = vec4(0.0, 0.0, 1.0, 1.0);\n  \
                 if (p.x < 8.0 && p.y >= iResolution.y - 8.0) fragColor = vec4(1.0);\n  \
                 if (p.x >= iResolution.x - 8.0 && p.y >= iResolution.y - 8.0) fragColor = vec4(0.0, 0.0, 0.0, 1.0);\n  \
                 if (p.x == 0.0 && p.y > 20.0 && p.y < 40.0) fragColor = vec4(1.0, 0.0, 0.0, 1.0);",
            ),
        )],
    )?;
    let m = movie(dir, "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=1\n")?;
    let (w, h) = (W as usize, H as usize);
    let frames = 12u64;
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let mut notes = vec![];
    for (depth, flipped) in [(10u8, true), (8, false)] {
        let mut r = renderer(gl, &m)?;
        r.set_readback(flipped);
        let mut want = vec![];
        for n in 0..frames {
            r.render(&m, W, H, &FrameInfo::frame(&m, n), true)
                .ok_or_else(|| anyhow!("frame {n} didn't render"))?;
            want.push(reference(&r.read_output().unwrap(), depth));
        }
        let mut reader = YuvReader::new(&r, w, h, depth, 4)?;
        let mut next = 0;
        let mut off_by_one = 0;
        crate::export::render_yuv(
            &mut r,
            &m,
            (w, h),
            0..frames,
            &mut reader,
            &cancel,
            |rd, f| {
                if f.n != next {
                    bail!("{depth}-bit: got frame {} when expecting {next}", f.n);
                }
                let got = samples(&f.data, depth);
                let want = &want[f.n as usize];
                if got.len() != want.len() {
                    bail!("{depth}-bit: {} samples, want {}", got.len(), want.len());
                }
                for (i, (g, e)) in got.iter().zip(want).enumerate() {
                    let d = g.abs_diff(*e);
                    if d > 1 {
                        let plane = if i < w * h { "Y" } else { "CbCr" };
                        bail!(
                            "{depth}-bit, {}: frame {}, {plane} sample {i}: {g}, want {e}",
                            if flipped {
                                "flipped output"
                            } else {
                                "plain output"
                            },
                            f.n
                        );
                    }
                    off_by_one += d as usize;
                }
                rd.recycle(f.data);
                next += 1;
                Ok(())
            },
        )?;
        if next != frames {
            bail!("{depth}-bit: {next} of {frames} frames came out");
        }
        // The corners are exact: no rounding ties there.
        let got = {
            let mut reader = YuvReader::new(&r, w, h, depth, 1)?;
            let tex = r
                .render(&m, W, H, &FrameInfo::frame(&m, 0), true)
                .ok_or_else(|| anyhow!("frame 0 didn't render"))?;
            reader.submit(&r, tex, 0)?;
            samples(&reader.collect(true)?.unwrap().data, depth)
        };
        let y = |x: usize, yy: usize| got[yy * w + x];
        let corners = [
            // fragCoord.y = 0 is the bottom row: red and blue are at the bottom.
            ("red", y(2, h - 3), if depth == 10 { 250 } else { 63 }),
            ("blue", y(w - 3, h - 3), if depth == 10 { 127 } else { 32 }),
            ("white", y(2, 2), if depth == 10 { 940 } else { 235 }),
            ("black", y(w - 3, 2), if depth == 10 { 64 } else { 16 }),
        ];
        for (what, g, e) in corners {
            if g != e {
                bail!("{depth}-bit {what} Y = {g}, want {e}");
            }
        }
        // The last chroma row (bottom), Cr, column 1 (centred on luma column 2).
        let cr_red = got[w * h + (h / 2 - 1) * w + w / 2 + 1];
        if cr_red != if depth == 10 { 960 } else { 240 } {
            bail!("{depth}-bit red Cr = {cr_red}");
        }
        notes.push(format!(
            "{depth}-bit: {} waits, {off_by_one} samples off by one",
            reader.waits
        ));
        if reader.waits > 4 {
            bail!(
                "{depth}-bit: the loop waited for the GPU {} times in {frames} frames (ring of 4)",
                reader.waits
            );
        }
    }
    Ok(notes.join("; "))
}

pub(super) fn readback_into(gl: &Gl, dir: &Path) -> Result<String> {
    // Top and bottom differ, so an upside-down readback can't pass.
    write(
        dir,
        &[
            (
                "a.glsl",
                &scene(
                    "fragColor = vec4(uv.x, uv.y * uv.y, fract(uv.x * 5.0 + uv.y * 11.0), 1.0);",
                ),
            ),
            (
                "b.glsl",
                &scene("fragColor = vec4(1.0 - uv.y, 0.3, uv.x * uv.y, 1.0);"),
            ),
        ],
    )?;
    let m = movie(
        dir,
        "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=2\n[[shot]]\nid=\"b\"\nscene=\"b.glsl\"\nduration=2\n\
         transition = { type = \"fx/wipe.glsl\", duration = 1 }\n",
    )?;
    let defaults = [4, 0, 0, 0, 0];
    let mut plain = renderer(gl, &m)?;
    let mut flipped = renderer(gl, &m)?;
    flipped.set_readback(true);
    if plain.pack_state() != defaults {
        bail!(
            "pack state isn't the default before any readback: {:?}",
            plain.pack_state()
        );
    }
    let (dw, dh) = (W as usize + 37, H as usize + 23);
    let bg = Frame::init(dw, dh, [7, 8, 9, 10]);
    let mut notes = 0;
    // A plain shot (copy pass) and a mid-wipe transition (transition pass).
    for (what, at) in [("shot", m.at(0.5)), ("transition", m.at(1.5))] {
        let want_frame = shot(&mut plain, &m, at, 1.0)?;
        for (x, y) in [(0, 0), (13, 11), (37, 23)] {
            let mut want = bg.clone();
            want.view_mut((x, y), (x + W as usize, y + H as usize))
                .copy_from_toodee(&want_frame);
            for (name, r) in [("plain", &mut plain), ("flipped composite", &mut flipped)] {
                r.render_at(&m, W, H, &info(at_time(&m, at), 1.0), at, true)
                    .ok_or_else(|| anyhow!("nothing rendered"))?;
                let mut dst = bg.clone();
                let junk = dirty_pack_state(gl)?;
                r.read_output_into(&mut dst, (x, y));
                let state = r.pack_state();
                unsafe { gl.free_buffer(junk) };
                let window = (x, y, x + W as usize, y + H as usize);
                if dst.view((window.0, window.1), (window.2, window.3)) != want_frame {
                    bail!("{what}, {name}, at ({x}, {y}): the window differs from read_output");
                }
                if dst != want {
                    bail!("{what}, {name}, at ({x}, {y}): pixels outside the window changed");
                }
                if state != defaults {
                    bail!("{what}, {name}: pack state left at {state:?}");
                }
                // read_output (used by `still`) agrees as well.
                expect_same(
                    &format!("{what}, {name}: read_output"),
                    &r.read_output().unwrap(),
                    &want_frame,
                    0,
                    0.0,
                )?;
                notes += 1;
            }
        }
    }
    // A window that doesn't fit must panic before GL writes anything. The first
    // probe overflows the right edge but stays inside the buffer's memory, so the
    // bounds check is the only thing that can catch it (and dropping it is safe to
    // try); toodee isn't involved, since the flipped renderer needs no row flip.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {})); // the panics are expected: keep them quiet
    let mut accepted = vec![];
    for (size, at) in [((W + 1, H + 5), (2, 0)), ((W, H + 1), (0, 2))] {
        let mut small = Frame::init(size.0 as usize, size.1 as usize, [0; 4]);
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            flipped.read_output_into(&mut small, at)
        }));
        if res.is_ok() {
            accepted.push(format!("{W}x{H} at {at:?} in {}x{}", size.0, size.1));
        }
    }
    std::panic::set_hook(hook);
    if !accepted.is_empty() {
        bail!(
            "read_output_into accepted windows that don't fit: {}",
            accepted.join(", ")
        );
    }
    Ok(format!("{notes} readbacks"))
}

pub(super) fn error_check_rate(gl: &Gl, dir: &Path) -> Result<String> {
    // glGetError makes the CPU wait for threaded drivers (NVIDIA): the player must
    // not call it every frame. 120 frames (~2 s of 60 Hz playback) → at most a few.
    write(
        dir,
        &[("s.glsl", &scene("fragColor = vec4(uv, 0.3, 1.0);"))],
    )?;
    let m = movie(dir, "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=4\n")?;
    let mut r = renderer(gl, &m)?;
    // Playing: never, unless there's a sign of trouble (even a once-a-second check
    // shows as a regular hitch in the frame graph).
    for i in 0..120 {
        r.render(&m, W, H, &info(i as f32 / 60.0, 1.0), true);
        if r.context_lost(false, false) {
            bail!("reported a lost context on a healthy GPU");
        }
    }
    if r.error_checks != 0 {
        bail!(
            "{} driver error checks in 120 frames of playback; expected none",
            r.error_checks
        );
    }
    // A sign of trouble (a long frame gap) does ask, so a reset is still noticed.
    if r.context_lost(false, true) || r.error_checks != 1 {
        bail!("a suspicious frame didn't ask the driver (or got a lost context)");
    }
    // Paused: about once a second (so a reset while paused is still noticed).
    let start = Instant::now();
    let before = r.error_checks;
    let mut frames = 0;
    while start.elapsed().as_secs_f32() < 1.3 {
        r.render(&m, W, H, &info(frames as f32 / 60.0, 1.0), true);
        if r.context_lost(true, false) {
            bail!("reported a lost context on a healthy GPU");
        }
        frames += 1;
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let secs = start.elapsed().as_secs_f32();
    let paused = r.error_checks - before;
    let allowed = 1 + secs.ceil() as usize;
    if paused == 0 || paused > allowed {
        bail!(
            "{paused} driver error checks in {frames} paused frames ({secs:.1} s); \
             expected 1 to {allowed}"
        );
    }
    Ok(format!(
        "none while playing, {paused} in {secs:.1} s paused"
    ))
}

pub(super) fn error_mapping(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            (
                "lib/bad.glsl",
                "// helper\n// line two\nfloat broken() { return undefinedThing; }\n",
            ),
            (
                "s.glsl",
                &format!(
                    "// scene\n#include \"lib/bad.glsl\"\n{}",
                    scene("fragColor = vec4(broken());")
                ),
            ),
            (
                "t.glsl",
                "// scene\nvoid mainImage(out vec4 fragColor, in vec2 fragCoord) {\n  vec2 uv = fragCoord / iResolution.xy;\n  // line four\n  fragColor = vec4(alsoUndefined);\n}\n",
            ),
        ],
    )?;
    let mut notes = vec![];
    for (scene_file, expect) in [("s.glsl", "lib/bad.glsl:3:"), ("t.glsl", "t.glsl:5:")] {
        let m = movie(
            dir,
            &format!("[[shot]]\nid=\"a\"\nscene=\"{scene_file}\"\nduration=1\n"),
        )?;
        let mut r = Renderer::new(gl.clone())?;
        r.set_cache_dir(None);
        let errors = match r.load_all(&m) {
            Ok(_) => bail!("{scene_file} compiled, but it has an error"),
            Err(e) => e,
        };
        let text: String = errors.iter().map(|e| e.to_string()).collect();
        if !text.contains(expect) {
            bail!("expected an error at {expect}, got:\n{text}");
        }
        notes.push(expect.trim_end_matches(':').to_string());
    }
    Ok(notes.join(", "))
}

/// A film-local library (`path = …`) under its local name `kit`: a working include
/// renders (and only compiles if each file is included once), `type = "kit/split"` runs
/// the library's `split.glsl` with the film's params, and an error inside the library is
/// reported as `kit/noise.glsl:3`, not by its folder on disk.
pub(super) fn library_errors(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            (
                "vendor/kit-core/library.toml",
                "name = \"shotglass-kit/core\"\nversion = \"0.4.0\"\nlicences = [\"MIT\"]\n",
            ),
            (
                "vendor/kit-core/hash.glsl",
                "float kit_level() { return 0.75; }\n",
            ),
            (
                "vendor/kit-core/fx.glsl",
                "#include \"hash.glsl\"\nvec3 kit_fx() { return vec3(kit_level(), 0.0, 0.25); }\n",
            ),
            (
                "vendor/kit-core/noise.glsl",
                "// kit noise\n// line two\nfloat kit_noise() { return notDeclaredAnywhere; }\n",
            ),
            (
                "vendor/kit-core/split.glsl",
                "#ifndef edge\n#define edge 0.5\n#endif\n\
                 vec4 transition(vec2 uv) { return uv.x < edge ? getFromColor(uv) : getToColor(uv); }\n",
            ),
            (
                "green.glsl",
                &scene("fragColor = vec4(0.0, 1.0, 0.0, 1.0);"),
            ),
            (
                "good.glsl",
                &format!(
                    "#include <kit/hash.glsl>\n#include <kit/fx.glsl>\n{}",
                    scene("fragColor = vec4(kit_fx(), 1.0);")
                ),
            ),
            (
                "bad.glsl",
                &format!(
                    "// scene\n#include <kit/noise.glsl>\n{}",
                    scene("fragColor = vec4(kit_noise());")
                ),
            ),
        ],
    )?;
    let toml = |scene_file: &str| {
        format!(
            "[libraries]\nkit = {{ path = \"vendor/kit-core\", version = \"0.4\" }}\n\
             [[shot]]\nid=\"a\"\nscene=\"{scene_file}\"\nduration=1\n"
        )
    };
    let m = movie(dir, &toml("good.glsl"))?;
    let mut r = renderer(gl, &m)?;
    let f = shot(
        &mut r,
        &m,
        At::Shot {
            shot: 0,
            local: 0.5,
        },
        1.0,
    )?;
    let p = px(&f, W as usize / 2, H as usize / 2);
    if p[0].abs_diff(191) > 2 || p[1] > 2 || p[2].abs_diff(64) > 2 {
        bail!("library include rendered {p:?}, expected about [191, 0, 64]");
    }

    // A library transition by name; `edge` comes from the film, overriding the default.
    let m = movie(
        dir,
        &format!(
            "{}[[shot]]\nid=\"b\"\nscene=\"green.glsl\"\nduration=1\n\
             transition = {{ type = \"kit/split\", duration = 0.5, params = {{ edge = 0.25 }} }}\n",
            toml("good.glsl")
        ),
    )?;
    let mut r = renderer(gl, &m)?;
    let f = shot(
        &mut r,
        &m,
        At::Transition {
            from: 0,
            to: 1,
            from_local: 0.75,
            to_local: 0.25,
            progress: 0.5,
        },
        1.0,
    )?;
    let (left, mid) = (
        px(&f, W as usize / 8, H as usize / 2),
        px(&f, W as usize / 2, H as usize / 2),
    );
    if left[0].abs_diff(191) > 2 || mid[1] < 253 || mid[0] > 2 {
        bail!(
            "kit/split at edge 0.25 rendered {left:?} at x = 1/8 and {mid:?} at x = 1/2, \
             expected the first shot then green"
        );
    }

    let m = movie(dir, &toml("bad.glsl"))?;
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None);
    let errors = match r.load_all(&m) {
        Ok(_) => bail!("bad.glsl compiled, but its library has an error"),
        Err(e) => e,
    };
    let text: String = errors.iter().map(|e| e.to_string()).collect();
    let expect = "kit/noise.glsl:3:";
    if !text.contains(expect) {
        bail!("expected an error at {expect}, got:\n{text}");
    }
    Ok("kit/split, kit/noise.glsl:3".into())
}

pub(super) fn keep_last_working(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[("s.glsl", &scene("fragColor = vec4(1.0, 0.0, 0.0, 1.0);"))],
    )?;
    let toml = "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\n";
    let m = movie(dir, toml)?;
    let mut r = renderer(gl, &m)?;
    let red = shot(
        &mut r,
        &m,
        At::Shot {
            shot: 0,
            local: 1.0,
        },
        1.0,
    )?;
    if px(&red, 0, 0)[0] < 250 || px(&red, 0, 0)[1] > 5 {
        bail!("first version didn't render red");
    }
    write(
        dir,
        &[("s.glsl", &scene("fragColor = vec4(oops, 1.0, 0.0, 1.0);"))],
    )?;
    let m2 = movie(dir, toml)?;
    r.set_movie(&m2)
        .map_err(|_| anyhow!("set_movie refused a compile error (should defer it)"))?;
    let still = shot(
        &mut r,
        &m2,
        At::Shot {
            shot: 0,
            local: 1.0,
        },
        1.0,
    )?;
    expect_same("after a broken edit", &still, &red, 1, 0.0)?;
    if r.errors().is_empty() {
        bail!("the broken edit didn't report an error");
    }
    write(
        dir,
        &[("s.glsl", &scene("fragColor = vec4(0.0, 1.0, 0.0, 1.0);"))],
    )?;
    let m3 = movie(dir, toml)?;
    r.set_movie(&m3).map_err(|e| anyhow!("{e:?}"))?;
    let green = shot(
        &mut r,
        &m3,
        At::Shot {
            shot: 0,
            local: 1.0,
        },
        1.0,
    )?;
    if px(&green, 0, 0)[1] < 250 || px(&green, 0, 0)[0] > 5 {
        bail!("the fixed version didn't replace the old one");
    }
    if !r.errors().is_empty() {
        bail!("errors remained after the fix");
    }
    Ok(String::new())
}

pub(super) fn program_cache(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[(
            "s.glsl",
            &scene("fragColor = vec4(uv, 0.5 + 0.5 * sin(uv.x * 20.0), 1.0);"),
        )],
    )?;
    let m = movie(dir, "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\n")?;
    let cache: PathBuf = dir.join("cache");
    let make = |cache_on: bool| -> Result<Renderer> {
        let mut r = Renderer::new(gl.clone())?;
        if cache_on {
            r.set_cache_dir(Some(cache.clone()));
            if r.shader_cache_dir().is_none() {
                return Ok(r);
            }
        }
        r.load_all(&m)
            .map_err(|e| anyhow!("{}", e.iter().map(|e| e.to_string()).collect::<String>()))?;
        Ok(r)
    };
    let mut r1 = make(true)?;
    if r1.shader_cache_dir().is_none() {
        return Ok("skipped: this driver offers no program binary formats".into());
    }
    let img1 = shot(
        &mut r1,
        &m,
        At::Shot {
            shot: 0,
            local: 0.5,
        },
        1.0,
    )?;
    let files: Vec<_> = std::fs::read_dir(&cache)?.flatten().collect();
    if files.is_empty() {
        bail!("nothing was written to the program cache");
    }
    drop(r1);
    let mut r2 = make(true)?;
    let img2 = shot(
        &mut r2,
        &m,
        At::Shot {
            shot: 0,
            local: 0.5,
        },
        1.0,
    )?;
    if r2.binary_hits == 0 {
        bail!("the second load didn't use the cache");
    }
    expect_same("cached program", &img2, &img1, 0, 0.0)?;
    drop(r2);
    for f in &files {
        std::fs::write(
            f.path(),
            b"\x01\x02\x03\x04 definitely not a program binary",
        )?;
    }
    let mut r3 = make(true)?;
    let img3 = shot(
        &mut r3,
        &m,
        At::Shot {
            shot: 0,
            local: 0.5,
        },
        1.0,
    )?;
    expect_same("after cache corruption", &img3, &img1, 0, 0.0)?;
    Ok(format!("{} cached program(s)", files.len()))
}

pub(super) fn eviction(gl: &Gl, dir: &Path) -> Result<String> {
    // 71 shots joined by 70 distinct transitions (one program each) — more than the
    // renderer keeps, so early ones get evicted and must come back correctly.
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.0, 0.0, 0.0, 1.0);")),
            (
                "k.glsl",
                "vec4 transition(vec2 uv) { return vec4(k / 100.0, 0.5, 0.25, 1.0); }\n",
            ),
        ],
    )?;
    let n = 71;
    let mut toml = String::new();
    for i in 0..n {
        toml += &format!("[[shot]]\nid=\"s{i}\"\nscene=\"s.glsl\"\nduration=2\n");
        if i > 0 {
            toml += &format!(
                "transition = {{ type = \"k.glsl\", duration = 1, params = {{ k = {i} }} }}\n"
            );
        }
    }
    let m = movie(dir, &toml)?;
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None);
    r.set_movie(&m).map_err(|e| anyhow!("{e:?}"))?;
    let check = |r: &mut Renderer, i: usize| -> Result<()> {
        let at = m.at(m.shots[i].start + 0.5);
        let frame = shot(r, &m, at, 1.0)?;
        let want = (i as f32 / 100.0 * 255.0).round() as i32;
        let red = px(&frame, 0, 0)[0];
        if (red as i32 - want).abs() > 1 {
            bail!("transition into shot {i}: red {red} (wanted {want})");
        }
        Ok(())
    };
    let mut peak = 0;
    for i in 1..n {
        check(&mut r, i)?;
        for _ in 0..31 {
            r.pump(&m, m.shots[i].start, false);
        }
        peak = peak.max(r.ready_count());
    }
    for i in 1..6 {
        check(&mut r, i)?; // evicted by now; recompiled on demand
    }
    if peak > 64 + 8 {
        bail!("{peak} programs held at once; eviction isn't working");
    }
    Ok(format!("peak {peak} programs held for {} needed", n))
}

pub(super) fn lazy_compile(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[("s.glsl", &scene("fragColor = vec4(uv.y, 0.3, uv.x, 1.0);"))],
    )?;
    let m = movie(dir, "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\n")?;
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None);
    r.set_movie(&m).map_err(|e| anyhow!("{e:?}"))?;
    let mut frames = 0;
    let got = loop {
        if let Some(_t) = r.render(&m, W, H, &info(0.5, 1.0), false) {
            break r.read_output().unwrap();
        }
        frames += 1;
        if frames > 2000 {
            bail!("the shot never became ready without blocking");
        }
        r.pump(&m, 0.5, false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    let want = shot(
        &mut r,
        &m,
        At::Shot {
            shot: 0,
            local: 0.5,
        },
        1.0,
    )?;
    expect_same("non-blocking render", &got, &want, 0, 0.0)?;
    Ok(format!("ready after {frames} frame(s)"))
}

/// The GL object ledger (`gfx::ledger`) across a renderer's life. GL handles are plain
/// numbers, so a target dropped from a `Vec` leaks silently (#74, #138 review fixes).
/// One renderer cycles through edits that add and remove buffers, layers, a comp with
/// buffers and a texture, at several output sizes; the second cycle must end on the
/// same counts as the first (caches have filled by then), and dropping the renderer
/// must bring every kind back to where it started.
pub(super) fn gl_object_churn(gl: &Gl, dir: &Path) -> Result<String> {
    let base = GlObjects::live();
    // acc sized by `scale`: its checkpoints (and the spares they're evicted into)
    // follow the output size.
    let bufs = buffer_film_with(dir, "bufs", |t| t.replace("size=[40, 24]", "scale=0.25"))?;
    let cdir = dir.join("comp");
    buffer_film(&cdir, "\"@hist\"")?;
    write(
        &cdir,
        &[
            (
                "pat.glsl",
                &scene("fragColor = vec4(fract(uv.x + iTime * 0.9), uv.y, 0.5, 1.0);"),
            ),
            (
                "showacc.glsl",
                &scene("fragColor = vec4(texture(iChannel0, uv).rgb, 1.0);"),
            ),
            ("c.toml", COMP_BUF_BODY),
        ],
    )?;
    let comp = movie(
        &cdir,
        "shotglass = \"0.1.10\"
[comps]
c = \"c.toml\"
         [[shot]]
id=\"s\"
comp=\"c\"
duration=2
",
    )?;
    let pdir = dir.join("plain");
    write(
        &pdir,
        &[("s.glsl", &scene("fragColor = vec4(uv, 0.5, 1.0);"))],
    )?;
    let plain = movie(
        &pdir,
        "shotglass = \"0.1.9\"
[[shot]]
id=\"a\"
scene=\"s.glsl\"
duration=2
",
    )?;
    let mut r = renderer(gl, &bufs)?;
    let draw = |r: &mut Renderer, m: &Movie, w: i32, h: i32, what: &str| -> Result<()> {
        if let Err(e) = r.set_movie(m) {
            bail!(
                "{what}: {}",
                e.iter().map(|e| e.to_string()).collect::<String>()
            );
        }
        for n in [0, 10, 40] {
            r.render(m, w, h, &FrameInfo::frame(m, n), true)
                .ok_or_else(|| anyhow!("{what}, frame {n} at {w}x{h}: nothing rendered"))?;
        }
        Ok(())
    };
    // Framebuffers belong to render targets only (caches keep textures and programs),
    // so after each step the renderer holds exactly as many as a fresh renderer that
    // drew just that film at that size: nothing kept for what was removed or resized.
    // Checkpoints are left out: a budgeted cache that keeps stale-key entries until
    // eviction on purpose (the spares they're evicted into aren't budgeted, so count).
    let fbos = |o: &GlObjects| o.counts[1];
    let step = |r: &mut Renderer, m: &Movie, w: i32, h: i32, what: &str| -> Result<()> {
        draw(r, m, w, h, what)?;
        let held = fbos(&GlObjects::live()) - fbos(&base) - r.checkpoint_targets() as i64;
        let before = GlObjects::live();
        let mut f = renderer(gl, m)?;
        draw(&mut f, m, w, h, what)?;
        let fresh = fbos(&GlObjects::live()) - fbos(&before) - f.checkpoint_targets() as i64;
        drop(f);
        if held != fresh {
            bail!("{what} at {w}x{h}: {held} framebuffers held after the edits, {fresh} fresh");
        }
        Ok(())
    };
    let cycle = |r: &mut Renderer| -> Result<GlObjects> {
        r.set_buffer_cache_budget(1 << 30);
        step(r, &bufs, W, H, "buffers")?;
        // Lowering the budget evicts a checkpoint into the fold's spare pool; the next
        // step's output size can't reuse it, so it must go rather than wait there.
        r.set_buffer_cache_budget(20_000);
        r.set_buffer_cache_budget(1 << 30);
        step(r, &bufs, W / 2, H / 2, "buffers at half size")?;
        step(r, &comp, W, H, "comp with buffers")?;
        step(r, &plain, W - 8, H, "plain film")?;
        step(r, &comp, W / 2, H, "comp at another size")?;
        step(r, &bufs, W, H, "buffers again")?;
        Ok(GlObjects::live())
    };
    let first = cycle(&mut r)?;
    let second = cycle(&mut r)?;
    if first != second {
        bail!(
            "a second cycle of edits and resizes ended on other GL object counts: {}",
            second.diff(&first)
        );
    }
    drop(r);
    let end = GlObjects::live();
    if end != base {
        bail!(
            "a dropped renderer left GL objects behind: {}",
            end.diff(&base)
        );
    }
    Ok(format!(
        "{} GL objects live mid-cycle, all freed",
        first.counts.iter().sum::<i64>() - base.counts.iter().sum::<i64>()
    ))
}
