//! Image sequences (#70): exact frames, streaming, prefetch, budgets, broken frames.

use super::*;

/// An image sequence `<prefix>00.png …` of `count` flat frames, `w` × `h`, frame k
/// coloured (10k, 250 − 10k, 0) so a readback says which frame was drawn.
pub(super) fn sequence(dir: &Path, prefix: &str, count: u32, w: u32, h: u32) -> Result<()> {
    for k in 0..count {
        png(dir, &format!("{prefix}{k:02}.png"), w, h, |_, _| {
            [(10 * k) as u8, (250 - 10 * k) as u8, 0, 255]
        })?;
    }
    Ok(())
}

/// The sequence frame a pixel shows (None: black).
pub(super) fn frame_of(p: [u8; 4]) -> Option<u64> {
    (p[0] != 0 || p[1] != 0).then(|| (p[0] as f32 / 10.0).round() as u64)
}

/// A scene showing iChannel0 (texel 0, so no derivatives) and iChannelTime[0] / 2 in blue.
pub(super) fn footage_scene() -> String {
    scene("fragColor = vec4(texelFetch(iChannel0, ivec2(0), 0).rg, iChannelTime[0] / 2.0, 1.0);")
}

/// Play film frames `ns` without blocking (one `pump` and a 1 ms pause per try, at most
/// `tries` per frame), checking each drawn frame shows exactly the source frame
/// `source_frame` says. Returns the tries that drew nothing, per film frame.
pub(super) fn play_footage(
    r: &mut Renderer,
    m: &Movie,
    item: crate::movie::ItemRef,
    ns: impl Iterator<Item = u64>,
    tries: usize,
) -> Result<Vec<(u64, usize)>> {
    let mut stalls = vec![];
    for n in ns {
        let t = m.frame_time(m.wrap_frame(n));
        let mut waited = 0;
        loop {
            r.pump(m, t, false);
            if r.render(m, W, H, &FrameInfo::frame(m, n), false).is_some() {
                let f = r.read_output().ok_or_else(|| anyhow!("no output"))?;
                let want = m.source_frame(item, 0, n);
                let got = frame_of(px(&f, 80, 45));
                if got != want {
                    bail!("film frame {n}: playback drew source frame {got:?}, wanted {want:?}");
                }
                break;
            }
            waited += 1;
            if waited >= tries {
                bail!("film frame {n} never drew without blocking ({waited} tries)");
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        stalls.push((n, waited));
    }
    Ok(stalls)
}

pub(super) fn footage_mapping(gl: &Gl, dir: &Path) -> Result<String> {
    use crate::movie::ItemRef;
    sequence(dir, "f/s_", 24, 4, 2)?;
    // Quarters across: four channels of one 24 fps sequence in a 30 fps film, each
    // drawn as its texel and iChannelTime / 2 in blue.
    write(
        dir,
        &[
            (
                "s.glsl",
                &scene(
                    "int q = int(uv.x * 4.0);\n  \
                 vec4 c = q == 0 ? texelFetch(iChannel0, ivec2(0), 0) : q == 1 ? texelFetch(iChannel1, ivec2(0), 0)\n    \
                 : q == 2 ? texelFetch(iChannel2, ivec2(0), 0) : texelFetch(iChannel3, ivec2(0), 0);\n  \
                 float t = q == 0 ? iChannelTime[0] : q == 1 ? iChannelTime[1] : q == 2 ? iChannelTime[2] : iChannelTime[3];\n  \
                 fragColor = vec4(c.rg, t / 2.0, 1.0);",
                ),
            ),
            (
                // A layer drawing its channel over the bottom rows only (alpha 0 elsewhere).
                "l.glsl",
                &scene(
                    "fragColor = vec4(texelFetch(iChannel0, ivec2(0), 0).rgb, uv.y < 0.1 ? 1.0 : 0.0);",
                ),
            ),
        ],
    )?;
    // A lead shot first, so the second's clock ("shot") starts at film frame 15; a
    // layer from 1.3 s (film frame 39), whose clock is its own.
    let m = movie(
        dir,
        "shotglass = \"0.1.6\"\nfps = 30\n[textures]\nseq = { frames = \"f/s_##.png\", fps = 24 }\n\
         [[shot]]\nid=\"lead\"\nscene=\"s.glsl\"\nduration=0.5\n\
         [[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=3\n\
         channels = [\"seq\", { texture = \"seq\", in = 0.25, speed = 0.5 }, \
         { texture = \"seq\", speed = 2, end = \"loop\", clock = \"movie\" }, \
         { texture = \"seq\", in_frame = 20, end = \"black\" }]\n\
         [[layer]]\nid=\"l\"\nscene=\"l.glsl\"\nfrom = \"a + 0.8\"\nchannels = [{ texture = \"seq\", speed = 0.5 }]\n",
    )?;
    let item = ItemRef::Shot(1);
    // The 24-in-30 pattern: one repeat every five film frames.
    let first: Vec<u64> = (15..27)
        .filter_map(|n| m.source_frame(item, 0, n))
        .collect();
    if first != [0, 0, 1, 2, 3, 4, 4, 5, 6, 7, 8, 8] {
        bail!("source_frame's 24-in-30 pattern is {first:?}");
    }
    let mut r = renderer(gl, &m)?;
    r.set_readback(true);
    let mut black = 0;
    for n in 15..105u64 {
        r.render(&m, W, H, &FrameInfo::frame(&m, n), true)
            .ok_or_else(|| anyhow!("frame {n}: nothing rendered"))?;
        let f = r.read_output().ok_or_else(|| anyhow!("no output"))?;
        for q in 0..4 {
            let p = px(&f, q * 40 + 20, 45);
            let want = m.source_frame(item, q, n);
            let got = frame_of(p);
            if got != want {
                bail!("film frame {n}, iChannel{q}: drew source frame {got:?}, wanted {want:?}");
            }
            let time = want.map_or(0.0, |k| m.frame_map(item, q).unwrap().source_time(k));
            let blue = (time / 2.0 * 255.0).round();
            if (p[2] as f32 - blue).abs() > 1.0 {
                bail!(
                    "film frame {n}, iChannel{q}: iChannelTime reads {}, wanted {time}",
                    p[2] as f32 / 127.5
                );
            }
            black += want.is_none() as usize;
        }
        // The layer's channel, on the layer's clock (the shot's below, before it starts).
        let want = if n >= 39 {
            m.source_frame(ItemRef::Layer(0), 0, n)
        } else {
            m.source_frame(item, 0, n)
        };
        if frame_of(px(&f, 20, 86)) != want {
            bail!(
                "film frame {n}, the layer's iChannel0: drew source frame {:?}, wanted {want:?}",
                frame_of(px(&f, 20, 86))
            );
        }
    }
    if m.source_frame(ItemRef::Layer(0), 0, 39) != Some(0) {
        bail!("a layer's footage clock doesn't start at the layer");
    }
    if black == 0 {
        bail!("end = \"black\" never read black");
    }
    if r.blocking_texture_loads != 0 {
        bail!("footage frames were counted as texture loads");
    }
    // Four channels of one sequence share its frames: one load per frame.
    if r.blocking_footage_loads != 24 {
        bail!(
            "{} frame loads for the 24 frames of one sequence on four channels",
            r.blocking_footage_loads
        );
    }
    Ok("90 film frames × 4 channels and a layer".into())
}

pub(super) fn footage_streaming(gl: &Gl, dir: &Path) -> Result<String> {
    use crate::movie::ItemRef;
    sequence(dir, "f/s_", 24, 4, 2)?;
    write(dir, &[("s.glsl", &footage_scene())])?;
    let m = movie(
        dir,
        "fps = 30\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=3\n\
         channels = [{ frames = \"f/s_##.png\", fps = 24, end = \"loop\" }]\n",
    )?;
    let mut r = renderer(gl, &m)?; // programs compiled; no footage frame loaded
    r.set_readback(true);
    if r.render(&m, W, H, &FrameInfo::frame(&m, 0), false)
        .is_some()
    {
        bail!("a non-blocking render drew before its footage frame was loaded");
    }
    let stalls = play_footage(&mut r, &m, ItemRef::Shot(0), 0..60, 5000)?;
    if r.blocking_footage_loads != 0 || r.blocking_texture_loads != 0 {
        bail!(
            "playback loaded {} footage frame(s) and {} texture(s) on the spot",
            r.blocking_footage_loads,
            r.blocking_texture_loads
        );
    }
    let waited: usize = stalls.iter().map(|(_, w)| w).sum();
    // Seeking while playing: frames asked for where the playhead was (decoding, or
    // decoded and not uploaded) mustn't hold up the ones where it is now. A 1 fps
    // sequence: each second of the film needs its own frame.
    sequence(dir, "g/s_", 24, 4, 2)?;
    write(dir, &[("g.glsl", &footage_scene())])?;
    let g = movie(
        dir,
        "fps = 30\n[[shot]]\nid=\"a\"\nscene=\"g.glsl\"\nduration=24\n\
         channels = [{ frames = \"g/s_##.png\", fps = 1 }]\n",
    )?;
    let mut r = renderer(gl, &g)?;
    r.set_readback(true);
    for s in 0..20u64 {
        r.pump(&g, s as f32 + 0.1, false);
    }
    play_footage(&mut r, &g, ItemRef::Shot(0), [693].into_iter(), 2000)
        .map_err(|e| anyhow!("after 20 seeks: {e:#}"))?;
    if r.blocking_footage_loads != 0 {
        bail!("seeking while playing loaded frames on the spot");
    }
    Ok(format!(
        "60 frames, {waited} waits (all at the start: {})",
        stalls[0].1
    ))
}

pub(super) fn footage_prefetch(gl: &Gl, dir: &Path) -> Result<String> {
    use crate::movie::ItemRef;
    sequence(dir, "f/a_", 24, 4, 2)?;
    write(dir, &[("s.glsl", &footage_scene())])?;
    // A loop film of three footage shots (78 frames): a cut at frame 30, a frozen
    // transition at 48–59 (shot b's source frame 18 held under it, which b's own window
    // has passed by frame 51), the seam at 78.
    let m = movie(
        dir,
        "fps = 30\nloop = true\n\
         [[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\nchannels = [{ frames = \"f/a_##.png\", in_frame = 2 }]\n\
         [[shot]]\nid=\"b\"\nscene=\"s.glsl\"\nduration=1\nchannels = [{ frames = \"f/a_##.png\" }]\n\
         [[shot]]\nid=\"c\"\nscene=\"s.glsl\"\nduration=1\nchannels = [{ frames = \"f/a_##.png\", in_frame = 5 }]\n\
         transition = { type = \"fx/crossfade.glsl\", duration = 0.4, freeze = true }\n",
    )?;
    let mut r = renderer(gl, &m)?;
    r.set_readback(true);
    // No room to spare: only the frames the playhead asks for may stay.
    r.set_texture_budget(1);
    // Warm up mid-shot, then play one try per film frame (as playback does) across the
    // cut and the seam: their first frames must already be there.
    let item = |n: u64| {
        ItemRef::Shot(match m.wrap_frame(n) {
            0..30 => 0,
            30..48 => 1,
            _ => 2,
        })
    };
    let mut stalled = vec![];
    play_footage(&mut r, &m, item(15), 15..16, 5000)?;
    for n in 16..110u64 {
        let t = m.frame_time(m.wrap_frame(n));
        std::thread::sleep(std::time::Duration::from_millis(2));
        r.pump(&m, t, false);
        match r.render(&m, W, H, &FrameInfo::frame(&m, n), false) {
            Some(_) => {
                // Under the transition the picture is a mix; elsewhere one exact frame.
                if matches!(m.at(t), At::Transition { .. }) {
                    continue;
                }
                let f = r.read_output().ok_or_else(|| anyhow!("no output"))?;
                let want = m.source_frame(item(n), 0, n);
                if frame_of(px(&f, 80, 45)) != want {
                    bail!("film frame {n}: wrong source frame (wanted {want:?})");
                }
            }
            None => stalled.push(n),
        }
    }
    if !stalled.is_empty() {
        bail!(
            "playback stalled at film frame(s) {stalled:?} (the cut is 30, the frozen \
             transition 48–59, the seam 78)"
        );
    }
    Ok(String::new())
}

/// Footage a buffer reads (#134) is prefetched like a shot's: playing one try per film
/// frame never stalls on its frames.
pub(super) fn footage_buffer_prefetch(gl: &Gl, dir: &Path) -> Result<String> {
    use crate::movie::ItemRef;
    sequence(dir, "f/a_", 24, 4, 2)?;
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.0, 0.0, 0.0, 1.0);")),
            (
                "copy.glsl",
                &scene("fragColor = texelFetch(iChannel0, ivec2(0), 0);"),
            ),
            (
                "show.glsl",
                &scene("fragColor = vec4(texelFetch(iChannel0, ivec2(0), 0).rg, 0.0, 1.0);"),
            ),
        ],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.9\"\nfps = 30\n\
         [[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\n\
         [[buffer]]\nid=\"fb\"\nscene=\"copy.glsl\"\nsize=[4, 2]\nformat=\"rgba8\"\n\
         channels = [{ frames = \"f/a_##.png\" }]\n\
         [[layer]]\nid=\"show\"\nscene=\"show.glsl\"\nchannels=[\"@fb\"]\n",
    )?;
    let mut r = renderer(gl, &m)?;
    r.set_readback(true);
    r.set_texture_budget(1);
    let item = ItemRef::Buffer(0);
    play_footage(&mut r, &m, item, 2..3, 5000)?;
    let mut stalled = vec![];
    for n in 3..24u64 {
        let t = m.frame_time(n);
        std::thread::sleep(std::time::Duration::from_millis(2));
        r.pump(&m, t, false);
        match r.render(&m, W, H, &FrameInfo::frame(&m, n), false) {
            Some(_) => {
                let f = r.read_output().ok_or_else(|| anyhow!("no output"))?;
                let want = m.source_frame(item, 0, n);
                if frame_of(px(&f, 80, 45)) != want {
                    bail!("film frame {n}: wrong source frame (wanted {want:?})");
                }
            }
            None => stalled.push(n),
        }
    }
    if !stalled.is_empty() {
        bail!("playback stalled on the buffer's footage at film frame(s) {stalled:?}");
    }
    Ok(String::new())
}

pub(super) fn footage_agree(gl: &Gl, dir: &Path) -> Result<String> {
    sequence(dir, "f/s_", 24, 8, 4)?;
    write(
        dir,
        &[(
            "s.glsl",
            &scene("fragColor = vec4(texture(iChannel0, uv).rg, fract(iChannelTime[0]), 1.0);"),
        )],
    )?;
    // 30000/1001 in a 30000/1001 film: the identity, which a `t * fps` rounding breaks.
    let m = movie(
        dir,
        "fps = \"30000/1001\"\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=0.8\n\
         channels = [{ frames = \"f/s_##.png\", fps = \"30000/1001\" }]\n",
    )?;
    let count = m.frame_count();
    let export: Vec<Frame> = {
        let mut r = renderer(gl, &m)?;
        r.set_readback(true);
        (0..count)
            .map(|n| {
                r.render(&m, W, H, &FrameInfo::frame(&m, n), true)
                    .ok_or_else(|| anyhow!("export frame {n}: nothing rendered"))?;
                r.read_output().ok_or_else(|| anyhow!("no output"))
            })
            .collect::<Result<_>>()?
    };
    for (n, f) in export.iter().enumerate() {
        if frame_of(px(f, 80, 45)) != Some(n as u64) {
            bail!(
                "export frame {n} shows source frame {:?}",
                frame_of(px(f, 80, 45))
            );
        }
    }
    // `still` by time, backwards, with a budget so small every frame is evicted between.
    let mut r = renderer(gl, &m)?;
    r.set_readback(true);
    r.set_texture_budget(1);
    for n in (0..count).rev() {
        let t = m.frame_time(n);
        r.render(&m, W, H, &FrameInfo::at(&m, t), true)
            .ok_or_else(|| anyhow!("still {t}: nothing rendered"))?;
        let f = r.read_output().ok_or_else(|| anyhow!("no output"))?;
        expect_same(
            &format!("still at frame {n}'s time vs export"),
            &f,
            &export[n as usize],
            0,
            0.0,
        )?;
        for _ in 0..3 {
            r.pump(&m, m.frame_time((n + count / 2) % count), false);
        }
    }
    Ok(format!("{count} frames"))
}

pub(super) fn footage_budget(gl: &Gl, dir: &Path) -> Result<String> {
    use crate::movie::ItemRef;
    sequence(dir, "f/s_", 24, 32, 32)?;
    write(dir, &[("s.glsl", &footage_scene())])?;
    let m = movie(
        dir,
        "fps = 30\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=3\n\
         channels = [{ frames = \"f/s_##.png\", fps = 30, end = \"loop\" }]\n",
    )?;
    let frame = 32 * 32 * 4;
    let budget = 8 * frame;
    let mut r = renderer(gl, &m)?;
    r.set_readback(true);
    r.set_texture_budget(budget);
    let mut worst = 0;
    for n in 0..72u64 {
        play_footage(&mut r, &m, ItemRef::Shot(0), n..n + 1, 5000)?;
        worst = worst.max(r.texture_bytes());
        if r.texture_bytes() > budget {
            bail!(
                "film frame {n}: {} bytes of textures resident, over the {budget}-byte budget",
                r.texture_bytes()
            );
        }
    }
    // 72 frames shown (24 distinct, three times); evicted textures are reused.
    if r.texture_creates > 20 {
        bail!(
            "{} textures created for 72 frames of footage (evicted ones should be reused)",
            r.texture_creates
        );
    }
    Ok(format!(
        "at most {} of {} KB resident, {} textures created",
        worst / 1024,
        budget / 1024,
        r.texture_creates
    ))
}

pub(super) fn footage_broken(gl: &Gl, dir: &Path) -> Result<String> {
    sequence(dir, "f/s_", 10, 4, 2)?;
    write(dir, &[("s.glsl", &footage_scene())])?;
    let toml = "fps = 10\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\n\
                channels = [{ frames = \"f/s_##.png\" }]\n";
    let m = movie(dir, toml)?;
    let mut r = renderer(gl, &m)?;
    r.set_readback(true);
    let draw = |r: &mut Renderer, m: &Movie, blocking: bool| -> Result<Option<u64>> {
        r.render(m, W, H, &FrameInfo::frame(m, 5), blocking)
            .ok_or_else(|| anyhow!("frame 5: nothing rendered"))?;
        Ok(frame_of(px(&r.read_output().unwrap(), 80, 45)))
    };
    if draw(&mut r, &m, true)? != Some(5) {
        bail!("the first version didn't draw source frame 5");
    }
    let compiles = r.compiles_started;
    // A renderer rewriting frame 5 in place, caught half-written.
    let path = dir.join("f/s_05.png");
    let bytes = std::fs::read(&path)?;
    std::fs::write(&path, &bytes[..bytes.len() / 2])?;
    let m2 = movie(dir, toml)?;
    r.set_movie(&m2).map_err(|_| {
        anyhow!("set_movie refused a version with a broken frame (should fall back)")
    })?;
    // Reload on a healthy frame, then run the fallback cleanup before seeking.
    r.render(&m2, W, H, &FrameInfo::frame(&m2, 4), true)
        .ok_or_else(|| anyhow!("healthy frame 4: nothing rendered"))?;
    let healthy = px(&r.read_output().unwrap(), 80, 45);
    if frame_of(healthy) != Some(4) {
        bail!("the replacement version didn't draw healthy frame 4");
    }
    r.pump(&m2, m2.frame_time(4), false);
    if draw(&mut r, &m2, false)? != Some(5) || draw(&mut r, &m2, true)? != Some(5) {
        bail!("a broken frame didn't fall back to the last working version's frame");
    }
    let errors: String = r.errors().iter().map(|e| e.to_string()).collect();
    if !errors.contains("s_05.png") {
        bail!("the broken frame wasn't reported: {errors:?}");
    }
    if r.compiles_started != compiles {
        bail!(
            "a footage edit compiled {} program(s)",
            r.compiles_started - compiles
        );
    }
    // With no working version to fall back on, the frame reads black and headless
    // commands are told why.
    let mut fresh = renderer(gl, &m2)?;
    fresh.set_readback(true);
    if draw(&mut fresh, &m2, true)?.is_some() || fresh.footage_failure().is_none() {
        bail!("a broken frame without a fallback should read black and be reported");
    }
    // Fixed: the new frame shows.
    png(dir, "f/s_05.png", 4, 2, |_, _| [70, 180, 0, 255])?;
    let m3 = movie(dir, toml)?;
    r.set_movie(&m3).map_err(|e| anyhow!("{e:?}"))?;
    if draw(&mut r, &m3, true)? != Some(7) {
        bail!("the re-rendered frame didn't replace the old one");
    }
    // A missing middle frame is a gap: the new version is refused.
    std::fs::remove_file(dir.join("f/s_03.png"))?;
    match movie(dir, toml) {
        Ok(_) => bail!("a sequence with a missing frame was accepted on load"),
        Err(e) if !format!("{e:#}").contains("frame 3 is missing") => {
            bail!("a missing frame gave the wrong error: {e:#}")
        }
        Err(_) => {}
    }
    Ok(String::new())
}

/// Every footage film in the repo played as the player does, one pump and one
/// non-blocking try per film frame from the start: a frame whose footage isn't resident
/// yet (`footage_misses`) is a prefetch gap (#74, #134 review fixes were such gaps:
/// nested comps, buffers). `tests/films/footage` mixes re-timing, `clock = "movie"`
/// and `end = "black"`; `tests/films/footage-places` reads its own sequence in each
/// place an item can (a cut, a layer and a buffer starting mid-film, a comp shot), so
/// every head is prefetched for itself. Skipped where the films aren't (an installed
/// binary).
pub(super) fn footage_films_no_misses(gl: &Gl, _dir: &Path) -> Result<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/films");
    let mut notes = vec![];
    for name in ["footage", "footage-places", "footage-video"] {
        let d = root.join(name);
        if name == "footage-video" && !crate::video::decode::AVAILABLE {
            notes.push(format!("{name}: no video decoder in this build, skipped"));
            continue;
        }
        if !d.join("movie.toml").exists() {
            notes.push(format!("{name}: not here, skipped"));
            continue;
        }
        let m = Movie::load(&d)?;
        let mut r = renderer(gl, &m)?;
        // The first frame may load (as opening a film does).
        r.render(&m, W, H, &FrameInfo::frame(&m, 0), true)
            .ok_or_else(|| anyhow!("{name}: frame 0 didn't render"))?;
        // Paused on frame 0 for a moment before play, as the player pumps every UI frame.
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(2));
            r.pump(&m, 0.0, false);
        }
        let loads = r.blocking_footage_loads;
        let mut stalled = vec![];
        for n in 1..m.frame_count() {
            std::thread::sleep(std::time::Duration::from_millis(2));
            r.pump(&m, m.frame_time(n), false);
            let misses = r.footage_misses;
            if r.render(&m, W, H, &FrameInfo::frame(&m, n), false)
                .is_none()
                && r.footage_misses > misses
            {
                stalled.push(n);
            }
        }
        if !stalled.is_empty() {
            bail!(
                "{name}: playback waited for footage at film frame(s) {stalled:?} of {}",
                m.frame_count()
            );
        }
        if r.blocking_footage_loads > loads {
            bail!(
                "{name}: {} footage frames loaded on the spot while playing",
                r.blocking_footage_loads - loads
            );
        }
        notes.push(format!("{name}: {} frames", m.frame_count()));
    }
    Ok(notes.join(", "))
}

/// Video (#73): `tests/films/footage-video` played forward (readers decoding on, on
/// the decode pool) gives exactly the frames blocking renders give in a scattered
/// order (every one a seek to its keyframe, from a fresh renderer), bit for bit: a
/// video frame is a function of the file and the frame number, whatever the route.
pub(super) fn video_play_matches_seeks(gl: &Gl, _dir: &Path) -> Result<String> {
    if !crate::video::decode::AVAILABLE {
        return Ok("no video decoder in this build, skipped".into());
    }
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/films/footage-video");
    if !d.join("movie.toml").exists() {
        return Ok("tests/films/footage-video isn't here, skipped".into());
    }
    let m = Movie::load(&d)?;
    let n = m.frame_count();
    let mut a = renderer(gl, &m)?;
    let mut played = vec![];
    for k in 0..n {
        let t = m.frame_time(k);
        a.pump(&m, t, false);
        played.push(play_until_ready(&mut a, &m, t, &format!("video frame {k}"))?.0);
    }
    let mut b = renderer(gl, &m)?;
    // A permutation of 0..n (7 is coprime with the film's 120 frames): far jumps,
    // backwards and forwards, across the clip's keyframes.
    let mut seen = 0;
    for i in 0..n {
        let k = (i * 7 + 3) % n;
        b.render(&m, W, H, &info(m.frame_time(k), 1.0), true)
            .ok_or_else(|| anyhow!("frame {k} didn't render"))?;
        let f = b.read_output().ok_or_else(|| anyhow!("no output"))?;
        expect_same(
            &format!("film frame {k}: played vs sought"),
            &played[k as usize],
            &f,
            0,
            0.0,
        )?;
        seen += 1;
    }
    if let Some(e) = b.footage_failure() {
        bail!("{e}");
    }
    Ok(format!("{seen} frames"))
}

pub(super) fn footage_unpack_state(gl: &Gl, dir: &Path) -> Result<String> {
    use crate::gfx::Tracked;
    use eframe::glow::{self, HasContext};
    // 3 px wide frames (12-byte rows), each texel (x·100, y·100, 10·k).
    for k in 0..4u32 {
        png(dir, &format!("f/u_{k}.png"), 3, 3, |x, y| {
            [(x * 100) as u8, (y * 100) as u8, (10 * k) as u8, 255]
        })?;
    }
    write(
        dir,
        &[(
            "s.glsl",
            &scene("fragColor = vec4(texelFetch(iChannel0, ivec2(fragCoord) % 3, 0).rgb, 1.0);"),
        )],
    )?;
    let m = movie(
        dir,
        "fps = 4\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\n\
         channels = [{ frames = \"f/u_#.png\", vflip = false }]\n",
    )?;
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None);
    r.set_readback(true);
    r.set_upload_budget(12); // one row per band
    r.set_movie(&m).map_err(|e| anyhow!("{e:?}"))?;
    // This check leaves stray state on purpose before every pump (a pump that uploads
    // nothing rightly leaves it alone): no entry-point audit here, only the end check.
    crate::gfx::glstate::set_audit(false);
    // What the sound path's staging buffers (or egui) may leave behind, before every pump.
    let junk = unsafe {
        let b = gl.new_buffer().map_err(|e| anyhow!(e))?;
        gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(b));
        gl.buffer_data_u8_slice(glow::PIXEL_UNPACK_BUFFER, &[9u8; 4096], glow::STATIC_DRAW);
        gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
        b
    };
    let stray = || unsafe {
        gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(junk));
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 8);
        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 5);
        gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 1);
    };
    let mut result = Ok(());
    'frames: for n in 0..4u64 {
        let mut tries = 0;
        let f = loop {
            stray();
            let bands = r.texture_bands;
            r.pump(&m, m.frame_time(n), false);
            let state = r.unpack_state();
            if r.texture_bands > bands && state != [4, 0, 0, 0, 0] {
                result = Err(anyhow!(
                    "unpack state after a footage upload is {state:?}; wanted [4, 0, 0, 0, 0]"
                ));
                break 'frames;
            }
            if r.render(&m, W, H, &FrameInfo::frame(&m, n), false)
                .is_some()
            {
                break r.read_output().unwrap();
            }
            tries += 1;
            if tries > 5000 {
                result = Err(anyhow!("frame {n} never drew"));
                break 'frames;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        for (x, y) in [(0usize, 0usize), (1, 0), (2, 1), (1, 2)] {
            let row = H as usize - 1 - y;
            if let Err(e) = expect_px(
                &format!("frame {n}, texel ({x}, {y})"),
                &f,
                x,
                row,
                [(x * 100) as u8, (y * 100) as u8, (10 * n) as u8],
                1,
            ) {
                result = Err(e);
                break 'frames;
            }
        }
    }
    unsafe {
        gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
        gl.free_buffer(junk);
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
        gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 0);
    }
    if r.texture_bands < 4 * 3 {
        result = result.and(Err(anyhow!(
            "{} upload bands for four 3-row frames (wanted one row per band)",
            r.texture_bands
        )));
    }
    result.map(|_| format!("{} bands", r.texture_bands))
}
