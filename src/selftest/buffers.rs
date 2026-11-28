//! Feedback buffers (#134, #137, #138): canonical folds, seeks, scrub, fill, comps, warm loops, `after`.

use super::*;

/// `frame = -3` on the fixture's layer (see `buffer_film`).
pub(super) const HIST3: &str = "{ source = \"@hist\", frame = -3 }";

/// The buffer fixture (#134): two identical shots cut at 1 s (30 fps). "acc" (rgba32f,
/// reads its own past) folds its previous state, the picture and a texture through
/// `fract` with its iFrame, so a missed, repeated or misnumbered step shows; it has an
/// init pass and resets at the cut. "hist" (rgba8, half size) is a pure history of the
/// picture, 4 deep, with a flat init pass. One opaque fixed-scale layer shows acc (left
/// half) and `hist` read as `hist_chan` (right half).
pub(super) fn buffer_film(dir: &Path, hist_chan: &str) -> Result<Movie> {
    write(
        dir,
        &[
            (
                "s.glsl",
                &scene("fragColor = vec4(uv.x, fract(iTime * 0.7), uv.y, 1.0);"),
            ),
            (
                "seed.glsl",
                &scene("fragColor = vec4(uv * 0.5, 0.25, 1.0);"),
            ),
            (
                "acc.glsl",
                &scene(
                    "vec4 p = texture(iChannel0, uv);\n  vec4 q = texture(iChannel1, uv);\n  \
                     vec4 t = texture(iChannel2, uv);\n  \
                     fragColor = vec4(fract(p.rgb * 1.37 + q.rgb * 0.31 + t.rgb * 0.11 + \
                     float(iFrame) * 0.013), 1.0);",
                ),
            ),
            ("copy.glsl", &scene("fragColor = texture(iChannel0, uv);")),
            (
                "hinit.glsl",
                &scene("fragColor = vec4(0.2, 0.6, 0.4, 1.0);"),
            ),
            (
                "show.glsl",
                &scene(
                    "fragColor = uv.x < 0.5 ? texture(iChannel0, vec2(uv.x * 2.0, uv.y)) \
                     : texture(iChannel1, vec2(uv.x * 2.0 - 1.0, uv.y));\n  fragColor.a = 1.0;",
                ),
            ),
        ],
    )?;
    if !dir.join("t.png").exists() {
        png(dir, "t.png", 8, 8, |x, y| {
            [(x * 32) as u8, (y * 32) as u8, 128, 255]
        })?;
    }
    movie(
        dir,
        &format!(
            "shotglass = \"0.1.9\"\n\
             [[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\n\
             [[shot]]\nid=\"b\"\nscene=\"s.glsl\"\nduration=1\n\
             [[buffer]]\nid=\"acc\"\nscene=\"acc.glsl\"\ninit=\"seed.glsl\"\nsize=[40, 24]\n\
             format=\"rgba32f\"\nreset=\"cuts\"\nchannels=[\"@prev\", \"@picture\", \"t.png\"]\n\
             [[buffer]]\nid=\"hist\"\nscene=\"copy.glsl\"\ninit=\"hinit.glsl\"\nscale=0.5\n\
             format=\"rgba8\"\nhistory=4\nchannels=[\"@picture\"]\n\
             [[layer]]\nid=\"show\"\nscene=\"show.glsl\"\nscale=1\n\
             channels=[\"@acc\", {hist_chan}]\n"
        ),
    )
}

/// Film frame `n` rendered blocking (as `still`), read back.
pub(super) fn buf_frame(r: &mut Renderer, m: &Movie, n: u64, scale: f32) -> Result<Frame> {
    let mut i = FrameInfo::frame(m, n);
    i.scale = scale;
    r.render(m, W, H, &i, true)
        .ok_or_else(|| anyhow!("frame {n}: nothing rendered"))?;
    r.read_output().ok_or_else(|| anyhow!("no output"))
}

/// Plays film frame `n` without blocking (pumping between tries) until it draws.
pub(super) fn buf_play(r: &mut Renderer, m: &Movie, n: u64) -> Result<(Frame, usize)> {
    let i = FrameInfo::frame(m, n);
    let mut held = 0;
    loop {
        if r.render(m, W, H, &i, false).is_some() {
            return Ok((r.read_output().unwrap(), held));
        }
        held += 1;
        if held > 5000 {
            bail!("frame {n} never became ready without blocking");
        }
        r.pump(m, i.t, false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// One half of a frame (columns), for comparing one panel of the fixture's layer.
pub(super) fn half(f: &Frame, left: bool) -> Frame {
    let w = f.num_cols() / 2;
    let x0 = if left { 0 } else { w };
    let mut out = Frame::new(w, f.num_rows());
    for row in 0..f.num_rows() {
        for col in 0..w {
            out[(col, row)] = f[(x0 + col, row)];
        }
    }
    out
}

/// `buffers_warm_loop`'s buffers and layer (#138): a float accumulator with an init pass
/// over the whole film, and a 4-deep pure history read 3 frames back.
pub(super) const WARM_BODY: &str = "[[buffer]]\nid=\"acc\"\nscene=\"acc.glsl\"\ninit=\"seed.glsl\"\nsize=[40, 24]\n\
     format=\"rgba32f\"\nchannels=[\"@prev\", \"@picture\", \"t.png\"]\n\
     [[buffer]]\nid=\"hist\"\nscene=\"copy.glsl\"\ninit=\"hinit.glsl\"\nscale=0.5\n\
     format=\"rgba8\"\nhistory=4\nchannels=[\"@picture\"]\n\
     [[layer]]\nid=\"show\"\nscene=\"show.glsl\"\nscale=1\n\
     channels=[\"@acc\", { source = \"@hist\", frame = -3 }]\n";

pub(super) fn buffers_warm_loop(gl: &Gl, dir: &Path) -> Result<String> {
    const P: u64 = 30;
    let film = |sub: &str, head: &str, shots: &str| -> Result<Movie> {
        let d = dir.join(sub);
        buffer_film(&d, HIST3)?;
        // 24 fps: the seam (frame 30) isn't on the once-a-second checkpoint grid.
        movie(
            &d,
            &format!("shotglass = \"0.1.10\"\nfps = 24\n{head}\n{shots}{WARM_BODY}"),
        )
    };
    let one = "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1.25\n";
    let warm = film("warm", "loop = \"warm\"", one)?;
    if warm.loop_frames != Some(P) || !warm.warm {
        bail!("the fixture isn't a warm 30-frame loop");
    }
    // The reference: the period played twice, unlooped, with no reset between (shot-
    // local time repeats; the buffers don't read film time).
    let twice = film(
        "twice",
        "",
        "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1.25\n\
         [[shot]]\nid=\"b\"\nscene=\"s.glsl\"\nduration=1.25\n",
    )?;
    let plain = film("plain", "loop = true", one)?;
    let mut u = renderer(gl, &twice)?;
    let want: Vec<Frame> = (0..2 * P)
        .map(|n| buf_frame(&mut u, &twice, n, 1.0))
        .collect::<Result<_>>()?;
    let mut r = renderer(gl, &warm)?;
    let mut fwd = vec![];
    for n in 0..P {
        let f = buf_frame(&mut r, &warm, n, 1.0)?;
        expect_fold(
            gl,
            &format!(
                "warm frame {n} vs the unlooped second period's frame {}",
                P + n
            ),
            &f,
            &want[(P + n) as usize],
            0,
            0.0,
        )?;
        fwd.push(f);
    }
    let mut pl = renderer(gl, &plain)?;
    let cold0 = buf_frame(&mut pl, &plain, 0, 1.0)?;
    if cold0 == fwd[0] {
        bail!("frame 0 of the warm loop equals the plain loop's (a reset)");
    }
    expect_fold(
        gl,
        "plain loop frame 0 vs the first period",
        &cold0,
        &want[0],
        0,
        0.0,
    )?;
    // Every path agrees: seeks, cold stills, folds without checkpoints.
    for n in [20u64, 0, 29, 3, 15, 1] {
        expect_fold(
            gl,
            &format!("warm frame {n} after seeking"),
            &buf_frame(&mut r, &warm, n, 1.0)?,
            &fwd[n as usize],
            0,
            0.0,
        )?;
    }
    for n in [0u64, 2, 29] {
        let mut c = renderer(gl, &warm)?;
        c.set_buffer_checkpoints(false);
        expect_fold(
            gl,
            &format!("warm frame {n}, cold, no checkpoints"),
            &buf_frame(&mut c, &warm, n, 1.0)?,
            &fwd[n as usize],
            0,
            0.0,
        )?;
    }
    // Playing over the wrap: never blocks, and the wrap restores the seam's checkpoint
    // instead of stepping a whole period of pre-roll again.
    let mut p = renderer(gl, &warm)?;
    for n in 0..P {
        let (f, _) = buf_play(&mut p, &warm, n)?;
        expect_fold(
            gl,
            &format!("warm frame {n} played"),
            &f,
            &fwd[n as usize],
            0,
            0.0,
        )?;
    }
    let (steps, restores) = (p.buffer_steps, p.buffer_restores);
    let mut wrap_steps = 0;
    for n in 0..4 {
        let (f, _) = buf_play(&mut p, &warm, n)?;
        expect_fold(
            gl,
            &format!("warm frame {n} after the wrap"),
            &f,
            &fwd[n as usize],
            0,
            0.0,
        )?;
        if n == 0 {
            wrap_steps = p.buffer_steps - steps;
        }
    }
    // The seam restored, then only the pure history's frames before it.
    if p.buffer_restores == restores || wrap_steps > 5 {
        bail!(
            "the wrap stepped {wrap_steps} buffer steps and restored {} checkpoints (want the \
             seam restored, a few steps)",
            p.buffer_restores - restores
        );
    }
    if p.blocking_catchups != 0 {
        bail!("playback caught up blocking {} times", p.blocking_catchups);
    }
    // With room for only two checkpoints (acc: 40×24 rgba32f, one state each), the
    // seam's survives the play-through: eviction keeps it, and the wrap restores it.
    let mut t = renderer(gl, &warm)?;
    t.set_buffer_cache_budget(2 * 40 * 24 * 16);
    for n in 0..P {
        buf_play(&mut t, &warm, n)?;
    }
    let steps = t.buffer_steps;
    let (f, _) = buf_play(&mut t, &warm, 0)?;
    expect_fold(
        gl,
        "frame 0 after the wrap, small budget",
        &f,
        &fwd[0],
        0,
        0.0,
    )?;
    if t.buffer_steps - steps > 5 {
        bail!(
            "with a two-checkpoint budget the wrap stepped {} (the seam's checkpoint was evicted)",
            t.buffer_steps - steps
        );
    }
    Ok(format!(
        "30 warm frames = the unlooped second period; paths agree; the wrap frame took {wrap_steps} steps"
    ))
}

/// The comp of `buffers_in_comps` (#138): a moving picture, a float accumulator with
/// an init pass and a reset at 1 s (comp time), and a layer that shows it.
pub(super) const COMP_BUF_BODY: &str = "[[shot]]\nid=\"p\"\nscene=\"pat.glsl\"\nduration=2\n\
     [[buffer]]\nid=\"acc\"\nscene=\"acc.glsl\"\ninit=\"seed.glsl\"\nsize=[40, 24]\n\
     format=\"rgba32f\"\nreset=[1]\nchannels=[\"@prev\", \"@picture\", \"t.png\"]\n\
     [[layer]]\nid=\"show\"\nscene=\"showacc.glsl\"\nchannels=[\"@acc\"]\n";

pub(super) fn buffers_in_comps(gl: &Gl, dir: &Path) -> Result<String> {
    let files = |d: &Path| -> Result<()> {
        buffer_film(d, HIST3)?;
        write(
            d,
            &[
                (
                    "pat.glsl",
                    &scene(
                        "fragColor = vec4(fract(uv.x + iTime * 0.9), uv.y, fract(iTime * 0.37), 1.0);",
                    ),
                ),
                (
                    "showacc.glsl",
                    &scene("fragColor = vec4(texture(iChannel0, uv).rgb, 1.0);"),
                ),
                ("c.toml", COMP_BUF_BODY),
            ],
        )
    };
    // The comp's content as a film of its own: the reference for comp-local frames.
    let alone_dir = dir.join("alone");
    files(&alone_dir)?;
    let alone = movie(
        &alone_dir,
        &format!("shotglass = \"0.1.10\"\n{COMP_BUF_BODY}"),
    )?;
    let mut a = renderer(gl, &alone)?;
    let want: Vec<Frame> = (0..60)
        .map(|n| buf_frame(&mut a, &alone, n, 1.0))
        .collect::<Result<_>>()?;
    if want[40] == want[41] {
        bail!("the comp's buffer doesn't change from frame to frame");
    }
    // 1. A comp shot half a second in: comp frame n + 15, held at 59 after the comp ends.
    let shot_dir = dir.join("shot");
    files(&shot_dir)?;
    let film = movie(
        &shot_dir,
        "shotglass = \"0.1.10\"\n[comps]\nc = \"c.toml\"\n\
         [[shot]]\nid=\"s\"\ncomp=\"c\"\ntime_offset=0.5\nduration=2.5\n",
    )?;
    let comp_frame = |n: u64| (n + 15).min(59) as usize;
    let mut r = renderer(gl, &film)?;
    let mut fwd = vec![];
    for n in 0..75 {
        let f = buf_frame(&mut r, &film, n, 1.0)?;
        expect_fold(
            gl,
            &format!(
                "comp shot, film frame {n} vs the comp as a film, frame {}",
                comp_frame(n)
            ),
            &f,
            &want[comp_frame(n)],
            0,
            0.0,
        )?;
        fwd.push(f);
    }
    if r.blocking_catchups > 1 {
        // The first frame starts the comp's fold at comp frame 15 (one catch-up).
        bail!("forward rendering caught up {} times", r.blocking_catchups);
    }
    let (cks, _) = r.checkpoint_stats();
    if cks == 0 {
        bail!("75 frames forward took no checkpoint of the comp's buffer");
    }
    let before = r.buffer_restores;
    for n in [40u64, 3, 70, 16, 14, 44, 0] {
        expect_fold(
            gl,
            &format!("comp shot, film frame {n} after seeking"),
            &buf_frame(&mut r, &film, n, 1.0)?,
            &fwd[n as usize],
            0,
            0.0,
        )?;
    }
    if r.buffer_restores == before {
        bail!("seeking back in the comp didn't restore a checkpoint");
    }
    for n in [44u64, 10, 74] {
        let mut c = renderer(gl, &film)?;
        expect_fold(
            gl,
            &format!("comp shot, cold still of film frame {n}"),
            &buf_frame(&mut c, &film, n, 1.0)?,
            &fwd[n as usize],
            0,
            0.0,
        )?;
    }
    // Playing (not blocking) from the start shows the same frames and never blocks.
    let mut p = renderer(gl, &film)?;
    for n in 0..30u64 {
        let (f, _) = buf_play(&mut p, &film, n)?;
        expect_fold(
            gl,
            &format!("comp shot played, frame {n}"),
            &f,
            &fwd[n as usize],
            0,
            0.0,
        )?;
    }
    if p.blocking_catchups != 0 {
        bail!("playback caught up blocking {} times", p.blocking_catchups);
    }
    // 2. A loop comp wraps: its frame 0 is a reset, so period k equals period 0.
    let loop_dir = dir.join("loop");
    files(&loop_dir)?;
    let lfilm = movie(
        &loop_dir,
        "shotglass = \"0.1.10\"\n[comps]\nc = { file = \"c.toml\", loop = true }\n\
         [[shot]]\nid=\"s\"\ncomp=\"c\"\nduration=3\n",
    )?;
    let mut l = renderer(gl, &lfilm)?;
    for n in [5u64, 59, 60, 65, 89, 31] {
        expect_fold(
            gl,
            &format!("loop comp, film frame {n} vs comp frame {}", n % 60),
            &buf_frame(&mut l, &lfilm, n, 1.0)?,
            &want[(n % 60) as usize],
            0,
            0.0,
        )?;
    }
    // 3. A film buffer reads the comp (nested folds): every path agrees.
    let read_dir = dir.join("read");
    files(&read_dir)?;
    let rfilm = movie(
        &read_dir,
        "shotglass = \"0.1.10\"\n[comps]\nc = \"c.toml\"\n\
         [[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\n\
         [[buffer]]\nid=\"fb\"\nscene=\"acc.glsl\"\nsize=[32, 20]\nformat=\"rgba32f\"\n\
         channels=[\"@prev\", \"@c\", \"t.png\"]\n\
         [[layer]]\nid=\"show\"\nscene=\"showacc.glsl\"\nchannels=[\"@fb\"]\n",
    )?;
    let mut f = renderer(gl, &rfilm)?;
    let rfwd: Vec<Frame> = (0..50)
        .map(|n| buf_frame(&mut f, &rfilm, n, 1.0))
        .collect::<Result<_>>()?;
    if rfwd[20] == rfwd[21] {
        bail!("the film buffer reading the comp doesn't change");
    }
    for n in [45u64, 12, 31, 49, 0] {
        expect_fold(
            gl,
            &format!("film buffer reading the comp, frame {n} after seeking"),
            &buf_frame(&mut f, &rfilm, n, 1.0)?,
            &rfwd[n as usize],
            0,
            0.0,
        )?;
    }
    let mut c = renderer(gl, &rfilm)?;
    c.set_buffer_checkpoints(false);
    for n in [37u64, 8] {
        c.forget_buffer_state();
        expect_fold(
            gl,
            &format!("film buffer reading the comp, frame {n} folded from the resets"),
            &buf_frame(&mut c, &rfilm, n, 1.0)?,
            &rfwd[n as usize],
            0,
            0.0,
        )?;
    }
    // 4. Editing the comp's shot (which its buffer reads as @picture) invalidates the
    // comp's fold and the film buffer's that reads the comp.
    write(
        &read_dir,
        &[(
            "pat.glsl",
            &scene("fragColor = vec4(uv.y, fract(uv.x - iTime * 0.6), 0.3, 1.0);"),
        )],
    )?;
    let edited = Movie::load(&read_dir)?;
    f.load_all(&edited)
        .map_err(|e| anyhow!("comp edit: {}", crate::gfx::join_errors(&e)))?;
    let got = buf_frame(&mut f, &edited, 40, 1.0)?;
    let mut fresh = renderer(gl, &edited)?;
    expect_fold(
        gl,
        "frame 40 after editing the comp's shot",
        &got,
        &buf_frame(&mut fresh, &edited, 40, 1.0)?,
        0,
        0.0,
    )?;
    if got == rfwd[40] {
        bail!("editing the comp's shot didn't change the film buffer reading the comp");
    }
    // 5. A sliced catch-up inside a comp (the player while paused): every held render
    // leaves the default framebuffer bound (the UI draws next), and when the playhead
    // leaves the comp mid catch-up, its progress isn't reported any more.
    let mid_dir = dir.join("mid");
    files(&mid_dir)?;
    let mid = movie(
        &mid_dir,
        "shotglass = \"0.1.10\"\n[comps]\nc = \"c.toml\"\n\
         [[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\n\
         [[shot]]\nid=\"s\"\ncomp=\"c\"\nduration=2\n",
    )?;
    let mut sl = renderer(gl, &mid)?;
    sl.set_catchup_slices(true);
    let far = FrameInfo::frame(&mid, 85);
    let mut held = 0;
    while sl.render(&mid, W, H, &far, true).is_none() {
        let fbo = unsafe { gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING) };
        if fbo != 0 {
            bail!("a held comp catch-up render left framebuffer {fbo} bound");
        }
        if sl.catchup_progress().is_none() {
            bail!("a held comp catch-up reported no progress");
        }
        held += 1;
        if held == 2 {
            break;
        }
    }
    if held == 0 {
        bail!("a 55-frame comp catch-up wasn't sliced");
    }
    buf_frame(&mut sl, &mid, 10, 1.0)?;
    if let Some(p) = sl.catchup_progress() {
        bail!("frame 10 (no comp) drew, but the comp's catch-up is still reported ({p:?})");
    }
    // 6. A film buffer reading a comp without `size` whose buffer is a `scale` of the
    // output: a resize changes the comp's state size, so the film fold starts over.
    let size_dir = dir.join("resize");
    files(&size_dir)?;
    write(
        &size_dir,
        &[(
            "c.toml",
            &COMP_BUF_BODY.replace("size=[40, 24]", "scale=0.25"),
        )],
    )?;
    let sfilm = movie(
        &size_dir,
        "shotglass = \"0.1.10\"\n[comps]\nc = \"c.toml\"\n\
         [[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\n\
         [[buffer]]\nid=\"fb\"\nscene=\"acc.glsl\"\nsize=[32, 20]\nformat=\"rgba32f\"\n\
         channels=[\"@prev\", \"@c\", \"t.png\"]\n\
         [[layer]]\nid=\"show\"\nscene=\"showacc.glsl\"\nchannels=[\"@fb\"]\n",
    )?;
    let at = |r: &mut Renderer, n: u64, (w, h): (i32, i32)| -> Result<Frame> {
        r.render(&sfilm, w, h, &FrameInfo::frame(&sfilm, n), true)
            .ok_or_else(|| anyhow!("frame {n}: nothing rendered"))?;
        r.read_output().ok_or_else(|| anyhow!("no output"))
    };
    let mut rs = renderer(gl, &sfilm)?;
    for n in 0..20 {
        at(&mut rs, n, (W, H))?;
    }
    let big = (W * 2, H * 2);
    let got = at(&mut rs, 20, big)?;
    let mut fresh = renderer(gl, &sfilm)?;
    expect_fold(
        gl,
        "frame 20 after a resize (the comp's buffer follows the output)",
        &got,
        &at(&mut fresh, 20, big)?,
        0,
        0.0,
    )?;
    // 7. Removing every buffer from the comp frees its states (no checkpoints left).
    let (before, _) = r.checkpoint_stats();
    if before == 0 {
        bail!("the comp shot film took no checkpoints to free");
    }
    write(
        &shot_dir,
        &[(
            "c.toml",
            "[[shot]]\nid=\"p\"\nscene=\"pat.glsl\"\nduration=2\n",
        )],
    )?;
    let plain = Movie::load(&shot_dir)?;
    r.load_all(&plain)
        .map_err(|e| anyhow!("comp without buffers: {}", crate::gfx::join_errors(&e)))?;
    buf_frame(&mut r, &plain, 10, 1.0)?;
    if r.checkpoint_stats().0 != 0 {
        bail!(
            "{} checkpoints kept after the comp's buffers were removed",
            r.checkpoint_stats().0
        );
    }
    Ok(format!(
        "75 comp-shot frames = the comp as a film; {cks} checkpoints; loop comp wraps; nested folds \
         agree; a comp edit, a resize and removing its buffers invalidate"
    ))
}

pub(super) fn buffers_agree(gl: &Gl, dir: &Path) -> Result<String> {
    let m = buffer_film(dir, HIST3)?;
    let mut r = renderer(gl, &m)?;
    let mut fwd = vec![];
    for n in 0..=55 {
        fwd.push(buf_frame(&mut r, &m, n, 1.0)?);
    }
    if r.blocking_catchups != 0 {
        bail!("forward rendering caught up {} times", r.blocking_catchups);
    }
    if fwd[20] == fwd[21] {
        bail!("the fixture's buffers don't change from frame to frame");
    }
    let (cks, _) = r.checkpoint_stats();
    if cks == 0 {
        bail!("55 frames forward took no checkpoint");
    }
    // Back to 45 (the cursor is past it): from the checkpoint at 30.
    let before = r.buffer_restores;
    expect_fold(
        gl,
        "frame 45 from a checkpoint",
        &buf_frame(&mut r, &m, 45, 1.0)?,
        &fwd[45],
        0,
        0.0,
    )?;
    if r.buffer_restores == before {
        bail!("seeking back to frame 45 didn't restore a checkpoint");
    }
    for n in [10, 52, 29, 30, 31, 3, 55, 54] {
        expect_fold(
            gl,
            &format!("frame {n} after seeking"),
            &buf_frame(&mut r, &m, n, 1.0)?,
            &fwd[n as usize],
            0,
            0.0,
        )?;
    }
    for n in [0, 29, 30, 44] {
        let mut c = renderer(gl, &m)?;
        expect_fold(
            gl,
            &format!("cold still of frame {n}"),
            &buf_frame(&mut c, &m, n, 1.0)?,
            &fwd[n as usize],
            0,
            0.0,
        )?;
    }
    let mut c = renderer(gl, &m)?;
    c.set_buffer_checkpoints(false);
    for n in [50, 12, 33] {
        c.forget_buffer_state();
        expect_fold(
            gl,
            &format!("frame {n} folded from the resets"),
            &buf_frame(&mut c, &m, n, 1.0)?,
            &fwd[n as usize],
            0,
            0.0,
        )?;
    }
    // A deep pure history (40) starts before acc's reset at the cut: stepping it alone
    // through frame 0 mustn't checkpoint acc (no state there yet), and later seeks
    // restore only true states.
    let ddir = dir.join("deep-hist");
    buffer_film(&ddir, HIST3)?;
    let text = std::fs::read_to_string(ddir.join("movie.toml"))?;
    std::fs::write(
        ddir.join("movie.toml"),
        text.replace("history=4", "history=40"),
    )?;
    let deep = Movie::load(&ddir)?;
    let mut d = renderer(gl, &deep)?;
    let mut dfwd = vec![];
    for n in 0..=45 {
        dfwd.push(buf_frame(&mut d, &deep, n, 1.0)?);
    }
    drop(d);
    let mut d = renderer(gl, &deep)?;
    for n in [45, 15, 44, 31, 0] {
        expect_fold(
            gl,
            &format!("deep history: frame {n} after a cold seek"),
            &buf_frame(&mut d, &deep, n, 1.0)?,
            &dfwd[n as usize],
            0,
            0.0,
        )?;
    }
    // Only non-pure buffers: a seek back onto a checkpoint's own frame steps nothing.
    let adir = dir.join("acc-only");
    let only = buffer_film(&adir, "\"@acc\"")?;
    let text = std::fs::read_to_string(adir.join("movie.toml"))?;
    let cut = text.find("[[buffer]]\nid=\"hist\"").unwrap();
    let end = cut + text[cut..].find("[[layer]]").unwrap();
    std::fs::write(
        adir.join("movie.toml"),
        format!("{}{}", &text[..cut], &text[end..]),
    )?;
    let only2 = Movie::load(&adir)?;
    drop(only);
    let mut o = renderer(gl, &only2)?;
    let mut ofwd = vec![];
    for n in 0..=40 {
        ofwd.push(buf_frame(&mut o, &only2, n, 1.0)?);
    }
    let steps = o.buffer_steps;
    expect_fold(
        gl,
        "frame 30 restored from its own checkpoint",
        &buf_frame(&mut o, &only2, 30, 1.0)?,
        &ofwd[30],
        0,
        0.0,
    )?;
    if o.buffer_steps != steps {
        bail!(
            "restoring the checkpoint at frame 30 stepped {} times",
            o.buffer_steps - steps
        );
    }
    expect_fold(
        gl,
        "frame 31 after it",
        &buf_frame(&mut o, &only2, 31, 1.0)?,
        &ofwd[31],
        0,
        0.0,
    )?;
    Ok(format!("{cks} checkpoints"))
}

pub(super) fn buffers_stalls(gl: &Gl, dir: &Path) -> Result<String> {
    let m = buffer_film(dir, HIST3)?;
    let mut f = renderer(gl, &m)?;
    let mut fwd = vec![];
    for n in 0..=24 {
        fwd.push(buf_frame(&mut f, &m, n, 1.0)?);
    }
    let mut r = renderer(gl, &m)?;
    for n in 0..=20 {
        buf_play(&mut r, &m, n)?;
    }
    // Frames 21 and 22 are missed: playback goes from 20 to 23.
    let steps = r.buffer_steps;
    let (got, _) = buf_play(&mut r, &m, 23)?;
    expect_fold(gl, "frame 23 after a stall", &got, &fwd[23], 0, 0.0)?;
    let stepped = r.buffer_steps - steps;
    if stepped != 6 {
        bail!("a 3-frame jump stepped {stepped} times, wanted 6 (3 frames × 2 buffers)");
    }
    let (got, _) = buf_play(&mut r, &m, 24)?;
    expect_fold(gl, "frame 24 after it", &got, &fwd[24], 0, 0.0)?;
    Ok(String::new())
}

pub(super) fn buffers_playback(gl: &Gl, dir: &Path) -> Result<String> {
    let m = buffer_film(dir, HIST3)?;
    let mut f = renderer(gl, &m)?;
    let want = buf_frame(&mut f, &m, 57, 1.0)?;
    let mut r = renderer(gl, &m)?;
    for n in 0..=10 {
        buf_play(&mut r, &m, n)?;
    }
    // A far seek while playing: caught up a budget of steps per render, never blocking.
    let (got, held) = buf_play(&mut r, &m, 57)?;
    expect_fold(gl, "frame 57 after a far seek", &got, &want, 0, 0.0)?;
    if held == 0 {
        bail!("a 47-frame catch-up finished in one non-blocking render (not sliced)");
    }
    if r.blocking_catchups != 0 {
        bail!("playback did {} blocking catch-ups", r.blocking_catchups);
    }
    if r.catchup_progress().is_some() {
        bail!("catch-up progress still shown after the frame drew");
    }
    // The player while paused: blocking renders, but sliced.
    let mut p = renderer(gl, &m)?;
    p.set_catchup_slices(true);
    let mut tries = 0;
    let i = FrameInfo::frame(&m, 57);
    while p.render(&m, W, H, &i, true).is_none() {
        // A held render leaves the default framebuffer bound (the UI draws next).
        let fbo = unsafe { gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING) };
        if fbo != 0 {
            bail!("a held catch-up render left framebuffer {fbo} bound");
        }
        tries += 1;
        if p.catchup_progress().is_none() || tries > 100 {
            bail!("a paused catch-up returned nothing without progress");
        }
    }
    if tries == 0 {
        bail!("a paused 57-frame catch-up wasn't sliced");
    }
    expect_fold(
        gl,
        "frame 57 caught up while paused",
        &p.read_output().unwrap(),
        &want,
        0,
        0.0,
    )?;
    Ok(format!("{held} renders held"))
}

pub(super) fn buffers_scale(gl: &Gl, dir: &Path) -> Result<String> {
    let m = buffer_film(dir, HIST3)?;
    let mut a = renderer(gl, &m)?;
    let mut b = renderer(gl, &m)?;
    for n in 0..=20 {
        let fa = buf_frame(&mut a, &m, n, 1.0)?;
        // The auto scale moving around: the picture track at 40–100 %.
        let fb = buf_frame(&mut b, &m, n, [0.4, 0.7, 1.0][n as usize % 3])?;
        expect_fold(
            gl,
            &format!("frame {n} at a lower render scale"),
            &fb,
            &fa,
            0,
            0.0,
        )?;
    }
    Ok(String::new())
}

pub(super) fn buffers_resets(gl: &Gl, dir: &Path) -> Result<String> {
    // The two shots are identical from their starts and acc resets at the cut (frame
    // 30), so frame 30 + k of acc is frame k: forward through the reset = a fresh start.
    let m = buffer_film(dir, HIST3)?;
    let mut r = renderer(gl, &m)?;
    let mut fwd = vec![];
    for n in 0..=36 {
        fwd.push(buf_frame(&mut r, &m, n, 1.0)?);
    }
    for k in 0..=6 {
        expect_fold(
            gl,
            &format!("acc at frame {} (reset at 30) vs frame {k}", 30 + k),
            &half(&fwd[30 + k], true),
            &half(&fwd[k], true),
            0,
            0.0,
        )?;
    }
    if half(&fwd[29], true) == half(&fwd[30], true) {
        bail!("acc didn't change at its reset");
    }
    // The init pass is the state before the reset: frame 0 isn't the black fold.
    write(dir, &[("seed.glsl", &scene("fragColor = vec4(0.0);"))])?;
    let black = Movie::load(dir)?;
    let mut b = renderer(gl, &black)?;
    if half(&buf_frame(&mut b, &black, 0, 1.0)?, true) == half(&fwd[0], true) {
        bail!("the init pass made no difference at the reset");
    }
    Ok(String::new())
}

pub(super) fn buffers_history(gl: &Gl, dir: &Path) -> Result<String> {
    let a = buffer_film(dir, HIST3)?;
    let bdir = dir.join("now");
    let b = buffer_film(&bdir, "\"@hist\"")?;
    let mut ra = renderer(gl, &a)?;
    let mut rb = renderer(gl, &b)?;
    let mut fa = vec![];
    let mut fb = vec![];
    for n in 0..=40 {
        fa.push(buf_frame(&mut ra, &a, n, 1.0)?);
        fb.push(buf_frame(&mut rb, &b, n, 1.0)?);
    }
    for n in [3usize, 10, 31, 32, 40] {
        expect_fold(
            gl,
            &format!("hist frame = -3 at {n} vs hist at {}", n - 3),
            &half(&fa[n], false),
            &half(&fb[n - 3], false),
            0,
            0.0,
        )?;
    }
    // Before the reset (frame 0), frame = -3 reads the init state.
    expect_px(
        "frame = -3 at frame 1 (the init state)",
        &fa[1],
        120,
        45,
        [51, 153, 102],
        1,
    )?;
    // A pure history buffer rebuilt after a seek equals one played into.
    let mut c = renderer(gl, &a)?;
    expect_fold(
        gl,
        "hist after a seek to 40",
        &buf_frame(&mut c, &a, 40, 1.0)?,
        &fa[40],
        0,
        0.0,
    )?;
    // An init pass reading its own history ({ source = "@sim", frame = -2 }) reads black,
    // never the slot it's drawing into, also when that slot holds an old fold's state.
    let idir = dir.join("init-self");
    write(
        &idir,
        &[
            ("s.glsl", &scene("fragColor = vec4(uv, 0.0, 1.0);")),
            (
                "step.glsl",
                &scene("fragColor = texture(iChannel0, uv) + vec4(0.1);"),
            ),
            (
                "ini.glsl",
                &scene("fragColor = vec4(0.5) + texture(iChannel0, uv);"),
            ),
            (
                "show.glsl",
                &scene("fragColor = vec4(texture(iChannel0, uv).rgb, 1.0);"),
            ),
        ],
    )?;
    let im = movie(
        &idir,
        "shotglass = \"0.1.9\"
[[shot]]
id=\"a\"
scene=\"s.glsl\"
duration=1
         [[buffer]]
id=\"sim\"
scene=\"step.glsl\"
init=\"ini.glsl\"
size=[16, 16]
         history=2
channels=[{ source = \"@sim\", frame = -2 }]
         [[layer]]
id=\"l\"
scene=\"show.glsl\"
channels=[\"@sim\"]
",
    )?;
    let mut ri = renderer(gl, &im)?;
    for n in [0, 1, 2, 3, 4, 5, 6, 0] {
        let f = buf_frame(&mut ri, &im, n, 1.0)?;
        if n == 0 {
            // Frame 0 reads frame -2, clamped to the init state: 0.5 + 0.1.
            expect_px(
                "init reading its own history (frame 0)",
                &f,
                10,
                10,
                [153, 153, 153],
                1,
            )?;
        }
    }
    // A film with only a pure history buffer takes no checkpoints.
    let pdir = dir.join("pure");
    write(
        &pdir,
        &[
            ("s.glsl", &scene("fragColor = vec4(uv, fract(iTime), 1.0);")),
            ("copy.glsl", &scene("fragColor = texture(iChannel0, uv);")),
        ],
    )?;
    let p = movie(
        &pdir,
        "shotglass = \"0.1.9\"\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\n\
         [[buffer]]\nid=\"h\"\nscene=\"copy.glsl\"\nhistory=2\nchannels=[\"@picture\"]\n\
         [[layer]]\nid=\"l\"\nscene=\"copy.glsl\"\nchannels=[{ source = \"@h\", frame = -2 }]\n",
    )?;
    let mut rp = renderer(gl, &p)?;
    for n in 0..=45 {
        buf_frame(&mut rp, &p, n, 1.0)?;
    }
    if rp.checkpoint_stats().0 != 0 {
        bail!("a pure history buffer took checkpoints");
    }
    let steps = rp.buffer_steps;
    buf_frame(&mut rp, &p, 20, 1.0)?;
    if rp.buffer_steps - steps != 3 {
        bail!(
            "a seek back rebuilt a 2-deep pure history in {} steps, wanted 3",
            rp.buffer_steps - steps
        );
    }
    Ok(String::new())
}

pub(super) fn buffers_edits(gl: &Gl, dir: &Path) -> Result<String> {
    let m = buffer_film(dir, HIST3)?;
    let mut r = renderer(gl, &m)?;
    let mut before = None;
    for n in 0..=40 {
        before = Some(buf_frame(&mut r, &m, n, 1.0)?);
    }
    let before = before.unwrap();
    let mut last = before;
    type Edit<'a> = (&'static str, Box<dyn Fn() -> Result<()> + 'a>);
    let edits: [Edit; 3] = [
        (
            "the buffer's scene",
            Box::new(|| {
                let s = std::fs::read_to_string(dir.join("acc.glsl"))?;
                std::fs::write(dir.join("acc.glsl"), s.replace("1.37", "1.38"))?;
                Ok(())
            }),
        ),
        (
            "a texture it reads",
            Box::new(|| png(dir, "t.png", 4, 4, |x, _| [(x * 60) as u8, 200, 30, 255])),
        ),
        (
            "the shot it reads (@picture)",
            Box::new(|| {
                write(
                    dir,
                    &[(
                        "s.glsl",
                        &scene("fragColor = vec4(uv.y, fract(iTime * 0.9), uv.x, 1.0);"),
                    )],
                )
            }),
        ),
    ];
    for (what, edit) in edits {
        edit()?;
        let m2 = Movie::load(dir)?;
        r.load_all(&m2)
            .map_err(|e| anyhow!("{what}: {}", crate::gfx::join_errors(&e)))?;
        let got = buf_frame(&mut r, &m2, 40, 1.0)?;
        let mut fresh = renderer(gl, &m2)?;
        expect_fold(
            gl,
            &format!("after editing {what}"),
            &got,
            &buf_frame(&mut fresh, &m2, 40, 1.0)?,
            0,
            0.0,
        )?;
        if half(&got, true) == half(&last, true) {
            bail!("editing {what} didn't change the buffer");
        }
        last = got;
    }
    // A broken edit keeps the last working version, its fold included (no re-fold).
    let good = std::fs::read_to_string(dir.join("acc.glsl"))?;
    write(dir, &[("acc.glsl", &scene("fragColor = vec4(oops);"))])?;
    let broken = Movie::load(dir)?;
    r.set_movie(&broken)
        .map_err(|_| anyhow!("set_movie refused a buffer compile error (should defer it)"))?;
    let steps = r.buffer_steps;
    let got = buf_frame(&mut r, &broken, 40, 1.0)?;
    expect_fold(gl, "a broken buffer edit", &got, &last, 0, 0.0)?;
    if r.buffer_steps != steps {
        bail!(
            "a broken edit re-folded the buffers ({} steps)",
            r.buffer_steps - steps
        );
    }
    std::fs::write(dir.join("acc.glsl"), good)?;
    Ok(String::new())
}

/// Film frame `n` rendered as the player does while paused (blocking, sliced): retried
/// until the catch-up reaches it.
pub(super) fn buf_sliced(r: &mut Renderer, m: &Movie, n: u64) -> Result<Frame> {
    let i = FrameInfo::frame(m, n);
    for _ in 0..2000 {
        if r.render(m, W, H, &i, true).is_some() {
            return r.read_output().ok_or_else(|| anyhow!("no output"));
        }
    }
    bail!("frame {n} never caught up")
}

/// `buffer_film`'s text with `edit` applied, loaded from `sub`.
pub(super) fn buffer_film_with(
    dir: &Path,
    sub: &str,
    edit: impl Fn(String) -> String,
) -> Result<Movie> {
    let d = dir.join(sub);
    buffer_film(&d, HIST3)?;
    let text = std::fs::read_to_string(d.join("movie.toml"))?;
    std::fs::write(d.join("movie.toml"), edit(text))?;
    Movie::load(&d)
}

pub(super) fn buffers_scrub(gl: &Gl, dir: &Path) -> Result<String> {
    let m = buffer_film(dir, HIST3)?;
    let mut c = renderer(gl, &m)?;
    let canon = buf_frame(&mut c, &m, 50, 1.0)?;
    let mut r = renderer(gl, &m)?;
    let mut at31 = None;
    for n in 0..=31 {
        at31 = Some(buf_frame(&mut r, &m, n, 1.0)?);
    }
    let at31 = at31.unwrap();
    // Dragging to 50: drawn at once from the state at 31, marked approximate.
    r.set_catchup_slices(true);
    r.set_scrub_preview(true);
    let steps = r.buffer_steps;
    r.render(&m, W, H, &FrameInfo::frame(&m, 50), true)
        .ok_or_else(|| anyhow!("the scrubbed frame wasn't drawn at once"))?;
    if !r.buffer_approximate() {
        bail!("a scrubbed frame far from any state wasn't marked approximate");
    }
    if r.buffer_steps != steps {
        bail!("scrubbing stepped {} times", r.buffer_steps - steps);
    }
    let approx = r.read_output().unwrap();
    if approx == canon {
        bail!("the approximate frame equals the exact one: the fixture can't tell them apart");
    }
    // The left panel shows only @acc: there, acc's state at 31.
    expect_fold(
        gl,
        "@acc on the approximate frame 50 vs frame 31",
        &half(&approx, true),
        &half(&at31, true),
        0,
        0.0,
    )?;
    // The drag stops: the same frame refines to the canonical state.
    r.set_scrub_preview(false);
    let got = buf_sliced(&mut r, &m, 50)?;
    if r.buffer_approximate() {
        bail!("the refined frame is still approximate");
    }
    expect_fold(gl, "frame 50 refined after a scrub", &got, &canon, 0, 0.0)?;
    // Headless renders never preview: a far still is exact.
    let mut h = renderer(gl, &m)?;
    buf_frame(&mut h, &m, 31, 1.0)?;
    let got = buf_frame(&mut h, &m, 50, 1.0)?;
    if h.buffer_approximate() {
        bail!("a headless render was approximate");
    }
    expect_fold(gl, "a still of frame 50", &got, &canon, 0, 0.0)?;
    // preview_lookback: a far scrub steps only the last 0.2 s from black, and its
    // states are never checkpointed (dense would take one every frame).
    let lm = buffer_film_with(dir, "lookback", |t| {
        t.replace(
            "format=\"rgba32f\"",
            "format=\"rgba32f\"\npreview_lookback=0.2\ncache=\"dense\"",
        )
    })?;
    let mut l = renderer(gl, &lm)?;
    l.set_scrub_preview(true);
    let steps = l.buffer_steps;
    l.render(&lm, W, H, &FrameInfo::frame(&lm, 25), true)
        .ok_or_else(|| anyhow!("the lookback frame wasn't drawn"))?;
    let walked = l.buffer_steps - steps;
    if l.checkpoint_stats().0 != 0 {
        bail!(
            "a lookback walk took {} checkpoints",
            l.checkpoint_stats().0
        );
    }
    if !l.buffer_approximate() || l.lookback_walks != 1 || walked > 2 * 8 {
        bail!(
            "a far scrub with preview_lookback: approximate {}, {} walks, {walked} steps",
            l.buffer_approximate(),
            l.lookback_walks
        );
    }
    let approx = l.read_output().unwrap();
    l.set_scrub_preview(false);
    let got = buf_frame(&mut l, &lm, 25, 1.0)?;
    let mut lc = renderer(gl, &lm)?;
    let canon = buf_frame(&mut lc, &lm, 25, 1.0)?;
    if approx == canon {
        bail!("the lookback frame equals the exact one: the fixture can't tell them apart");
    }
    expect_fold(
        gl,
        "frame 25 refined after a lookback scrub",
        &got,
        &canon,
        0,
        0.0,
    )?;
    Ok(format!("lookback walked {walked} steps"))
}

pub(super) fn buffers_fill(gl: &Gl, dir: &Path) -> Result<String> {
    // No cut resets: frames 5 → 30 → … are one long fold.
    let m = buffer_film_with(dir, "nocuts", |t| t.replace("reset=\"cuts\"\n", ""))?;
    let mut c = renderer(gl, &m)?;
    let canon45 = buf_frame(&mut c, &m, 45, 1.0)?;
    let mut r = renderer(gl, &m)?;
    r.set_catchup_slices(true);
    let at5 = buf_sliced(&mut r, &m, 5)?;
    let (before, _) = r.checkpoint_stats();
    // As the player does while paused and idle: pump, then fill, every redraw.
    let (mut calls, mut idle) = (0, 0);
    let t5 = FrameInfo::frame(&m, 5).t;
    while idle < 20 {
        r.pump(&m, t5, false);
        let steps = r.buffer_steps;
        if r.fill_checkpoints(&m, W, H) {
            idle = 0;
            calls += 1;
        } else {
            idle += 1;
        }
        // At most a step budget of frames per call (two buffers each).
        if r.buffer_steps - steps > 2 * 8 {
            bail!("one fill call stepped {} times", r.buffer_steps - steps);
        }
        if calls > 500 {
            bail!("the background fill never finished");
        }
    }
    let (after, _) = r.checkpoint_stats();
    if after <= before || r.fill_steps == 0 {
        bail!("the background fill took no checkpoint ({before} → {after})");
    }
    if r.blocking_catchups != 0 {
        bail!("the background fill caught up blocking");
    }
    if r.catchup_progress().is_some() {
        bail!("the background fill shows as a catch-up");
    }
    expect_fold(
        gl,
        "frame 5 after the fill",
        &buf_sliced(&mut r, &m, 5)?,
        &at5,
        0,
        0.0,
    )?;
    let restores = r.buffer_restores;
    let steps = r.buffer_steps;
    expect_fold(
        gl,
        "frame 45 after the fill",
        &buf_sliced(&mut r, &m, 45)?,
        &canon45,
        0,
        0.0,
    )?;
    if r.buffer_restores == restores || r.buffer_steps - steps > 40 {
        bail!(
            "frame 45 didn't start from a filled checkpoint ({} steps)",
            r.buffer_steps - steps
        );
    }
    // cache = "dense": a checkpoint every frame.
    let dm = buffer_film_with(dir, "dense", |t| {
        t.replace("format=\"rgba32f\"", "format=\"rgba32f\"\ncache=\"dense\"")
    })?;
    let mut d = renderer(gl, &dm)?;
    for n in 0..=10 {
        buf_frame(&mut d, &dm, n, 1.0)?;
    }
    let (dense, _) = d.checkpoint_stats();
    if dense < 10 {
        bail!("cache = \"dense\" took {dense} checkpoints in 11 frames");
    }
    Ok(format!("{} checkpoints in {calls} calls", after - before))
}

pub(super) fn buffers_invalidation(gl: &Gl, dir: &Path) -> Result<String> {
    // Three shots, no cut resets; the last uses its own scene.
    let edit = |t: String| {
        let t = t.replace("reset=\"cuts\"\n", "");
        t.replacen(
            "[[buffer]]",
            "[[shot]]\nid=\"c\"\nscene=\"s2.glsl\"\nduration=1\n[[buffer]]",
            1,
        )
    };
    let d = dir.join("third");
    buffer_film(&d, HIST3)?;
    std::fs::copy(d.join("s.glsl"), d.join("s2.glsl"))?;
    let text = std::fs::read_to_string(d.join("movie.toml"))?;
    std::fs::write(d.join("movie.toml"), edit(text))?;
    let m = Movie::load(&d)?;
    let mut r = renderer(gl, &m)?;
    let mut fwd = vec![];
    for n in 0..=70 {
        fwd.push(buf_frame(&mut r, &m, n, 1.0)?);
    }
    // Edit the last shot (read from frame 60): states before it stay.
    write(
        &d,
        &[(
            "s2.glsl",
            &scene("fragColor = vec4(uv.y, fract(iTime * 0.7), uv.x, 1.0);"),
        )],
    )?;
    let m2 = Movie::load(&d)?;
    r.load_all(&m2)
        .map_err(|e| anyhow!("{}", crate::gfx::join_errors(&e)))?;
    let (restores, steps) = (r.buffer_restores, r.buffer_steps);
    let got = buf_frame(&mut r, &m2, 45, 1.0)?;
    if r.buffer_restores == restores || r.buffer_steps - steps > 40 {
        bail!(
            "after an edit at frame 60, frame 45 folded {} steps (its checkpoint at 30 was dropped)",
            r.buffer_steps - steps
        );
    }
    expect_fold(gl, "frame 45 after an edit at 60", &got, &fwd[45], 0, 0.0)?;
    let mut fresh = renderer(gl, &m2)?;
    for n in [65, 61, 70] {
        let got = buf_frame(&mut r, &m2, n, 1.0)?;
        expect_fold(
            gl,
            &format!("frame {n} after the edit vs a cold fold"),
            &got,
            &buf_frame(&mut fresh, &m2, n, 1.0)?,
            0,
            0.0,
        )?;
        if n == 65 && half(&got, true) == half(&fwd[65], true) {
            bail!("editing the last shot didn't change the buffer at 65");
        }
    }
    Ok(String::new())
}

/// `buffers_after_layer`'s fixture (#138), on `buffer_film`'s files: "stk" (rgba32f,
/// declared first) steps after layer "grade" and folds its previous state with the
/// composite so far (`@input`), layer "lo" (`@lo`) and "pre"'s state this frame; its
/// init pass reads `@input` too. "pre" is an ordinary buffer (before every layer) that
/// "lo" reads, so "stk" must step after it whatever the file order. "grade" starts at
/// 1.2 s (before that the anchor is inactive); "above" is over the anchor. "show"
/// (opaque, on top) shows stk (left) and the picture below it (right).
pub(super) const STACK_BODY: &str = "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\n\
     [[shot]]\nid=\"b\"\nscene=\"s.glsl\"\nduration=1\n\
     [[buffer]]\nid=\"stk\"\nscene=\"stk.glsl\"\ninit=\"sinit.glsl\"\nafter=\"grade\"\n\
     size=[40, 24]\nformat=\"rgba32f\"\nchannels=[\"@prev\", \"@input\", \"@lo\", \"@pre\"]\n\
     [[buffer]]\nid=\"pre\"\nscene=\"acc.glsl\"\ninit=\"seed.glsl\"\nsize=[40, 24]\n\
     format=\"rgba32f\"\nchannels=[\"@prev\", \"@picture\", \"t.png\"]\n\
     [[layer]]\nid=\"lo\"\nscene=\"lo.glsl\"\nchannels=[\"@pre\"]\n\
     [[layer]]\nid=\"grade\"\nscene=\"grade.glsl\"\nfrom=1.2\nchannels=[\"@input\"]\n\
     [[layer]]\nid=\"above\"\nscene=\"above.glsl\"\nfrom=0.25\nchannels=[\"@input\"]\n";

/// The fixture's layer and buffer shaders.
pub(super) fn stack_files(d: &Path) -> Result<()> {
    buffer_film(d, HIST3)?;
    write(
        d,
        &[
            (
                "lo.glsl",
                &scene(
                    "vec4 p = texture(iChannel0, uv);\n  \
                     fragColor = vec4(p.bgr, step(0.5, fract(uv.x * 3.0 + iTime)) * 0.8);",
                ),
            ),
            (
                "grade.glsl",
                &scene("fragColor = vec4(texture(iChannel0, uv).rgb * vec3(1.2, 0.9, 0.7), 1.0);"),
            ),
            (
                "above.glsl",
                &scene("fragColor = vec4(1.0 - texture(iChannel0, uv).rgb, 0.5);"),
            ),
            (
                "stk.glsl",
                &scene(
                    "vec4 p = texture(iChannel0, uv);\n  vec4 i = texture(iChannel1, uv);\n  \
                     vec4 l = texture(iChannel2, uv);\n  vec4 q = texture(iChannel3, uv);\n  \
                     fragColor = vec4(fract(p.rgb * 1.37 + i.rgb * 0.53 + l.rgb * 0.29 + \
                     q.rgb * 0.17 + float(iFrame) * 0.011), 1.0);",
                ),
            ),
            (
                "sinit.glsl",
                &scene("fragColor = vec4(texture(iChannel1, uv).rgb * 0.5, 1.0);"),
            ),
            (
                "show.glsl",
                &scene(
                    "fragColor = uv.x < 0.5 ? texture(iChannel0, vec2(uv.x * 2.0, uv.y)) \
                     : texture(iChannel1, vec2(uv.x * 2.0 - 1.0, uv.y));\n  fragColor.a = 1.0;",
                ),
            ),
            (
                "view.glsl",
                &scene("fragColor = vec4(texture(iChannel0, uv).rgb, 1.0);"),
            ),
        ],
    )
}

/// `buffers_after_layer`'s copy film, written to `dir/copy`.
pub(super) fn with_cp_movie(dir: &Path, body: &str) -> Result<Movie> {
    let d = dir.join("copy");
    stack_files(&d)?;
    movie(&d, &format!("shotglass = \"0.1.11\"\n{body}"))
}

/// `buffers_after_layer`'s warm-loop part: a stack that doesn't read film time (shot
/// time repeats, buffer iFrame counts on), so a warm loop's frame n is the unlooped
/// period-twice film's frame p + n.
pub(super) const WARM_STACK: &str = "[[buffer]]\nid=\"stk\"\nscene=\"stk.glsl\"\ninit=\"sinit.glsl\"\nafter=\"grade\"\n\
     size=[40, 24]\nformat=\"rgba32f\"\nchannels=[\"@prev\", \"@input\", \"@lo\", \"@pre\"]\n\
     [[buffer]]\nid=\"pre\"\nscene=\"acc.glsl\"\ninit=\"seed.glsl\"\nsize=[40, 24]\n\
     format=\"rgba32f\"\nchannels=[\"@prev\", \"@picture\", \"t.png\"]\n\
     [[layer]]\nid=\"lo\"\nscene=\"lo2.glsl\"\nchannels=[\"@pre\"]\n\
     [[layer]]\nid=\"grade\"\nscene=\"grade.glsl\"\nchannels=[\"@input\"]\n\
     [[layer]]\nid=\"show\"\nscene=\"show.glsl\"\nscale=1\nchannels=[\"@stk\", \"@input\"]\n";

pub(super) fn buffers_after_layer(gl: &Gl, dir: &Path) -> Result<String> {
    let film = |sub: &str, top: &str| -> Result<Movie> {
        let d = dir.join(sub);
        stack_files(&d)?;
        movie(&d, &format!("shotglass = \"0.1.11\"\n{STACK_BODY}{top}"))
    };
    let show =
        "[[layer]]\nid=\"show\"\nscene=\"show.glsl\"\nscale=1\nchannels=[\"@stk\", \"@input\"]\n";
    let m = film("main", show)?;
    if m.buffers[0].is_pure() {
        bail!("stk reads a layer that reads buffer state: it can't be pure");
    }
    // 1. Every path agrees: forward, checkpoint, seeks, cold stills, folds from resets,
    //    non-blocking playback.
    let mut r = renderer(gl, &m)?;
    let mut fwd = vec![];
    for n in 0..=55 {
        fwd.push(buf_frame(&mut r, &m, n, 1.0)?);
    }
    if r.blocking_catchups != 0 {
        bail!("forward rendering caught up {} times", r.blocking_catchups);
    }
    if half(&fwd[20], true) == half(&fwd[21], true) {
        bail!("the after buffer doesn't change from frame to frame");
    }
    let before = r.buffer_restores;
    expect_fold(
        gl,
        "frame 45 from a checkpoint",
        &buf_frame(&mut r, &m, 45, 1.0)?,
        &fwd[45],
        0,
        0.0,
    )?;
    if r.buffer_restores == before {
        bail!("seeking back to frame 45 didn't restore a checkpoint");
    }
    for n in [10, 52, 35, 36, 37, 3, 55] {
        expect_fold(
            gl,
            &format!("frame {n} after seeking"),
            &buf_frame(&mut r, &m, n, 1.0)?,
            &fwd[n as usize],
            0,
            0.0,
        )?;
    }
    for n in [0, 1, 29, 36, 44] {
        let mut c = renderer(gl, &m)?;
        expect_fold(
            gl,
            &format!("cold still of frame {n}"),
            &buf_frame(&mut c, &m, n, 1.0)?,
            &fwd[n as usize],
            0,
            0.0,
        )?;
    }
    let mut c = renderer(gl, &m)?;
    c.set_buffer_checkpoints(false);
    for n in [50, 12, 40] {
        c.forget_buffer_state();
        expect_fold(
            gl,
            &format!("frame {n} folded from the reset"),
            &buf_frame(&mut c, &m, n, 1.0)?,
            &fwd[n as usize],
            0,
            0.0,
        )?;
    }
    let mut p = renderer(gl, &m)?;
    for n in 0..=8 {
        let (got, _) = buf_play(&mut p, &m, n)?;
        expect_fold(
            gl,
            &format!("frame {n} played"),
            &got,
            &fwd[n as usize],
            0,
            0.0,
        )?;
    }
    let (got, held) = buf_play(&mut p, &m, 50)?;
    expect_fold(
        gl,
        "frame 50 played after a far seek",
        &got,
        &fwd[50],
        0,
        0.0,
    )?;
    if held == 0 || p.blocking_catchups != 0 {
        bail!(
            "a far seek while playing: {held} renders held, {} blocking catch-ups",
            p.blocking_catchups
        );
    }
    // 2. Hidden layers are preview only: the state is the same with "lo" and "grade"
    //    hidden from the start (the picture below "show" isn't).
    let mut h = renderer(gl, &m)?;
    h.set_hidden_layers(["lo".to_string(), "grade".to_string()].into());
    for n in 0..=40 {
        let f = buf_frame(&mut h, &m, n, 1.0)?;
        expect_fold(
            gl,
            &format!("frame {n} with layers below the anchor hidden"),
            &half(&f, true),
            &half(&fwd[n as usize], true),
            0,
            0.0,
        )?;
        if n == 40 && half(&f, false) == half(&fwd[40], false) {
            bail!("hiding \"lo\" and \"grade\" didn't change the picture");
        }
    }
    // 3. The composite a step reads is the frame drawn up to its layer: a copy buffer
    //    after "grade" at the output size, shown alone 3 frames back, equals the film
    //    without it 3 frames earlier (both without "above"). cp is declared before
    //    "pre", which "lo" reads, and it isn't a pure history: its states 3 back from a
    //    checkpoint need pre's states then.
    let base = &STACK_BODY[..STACK_BODY.find("[[layer]]\nid=\"above\"").unwrap()];
    let cp = "[[buffer]]\nid=\"cp\"\nscene=\"copy.glsl\"\nafter=\"grade\"\nformat=\"rgba8\"\n\
              history=3\nchannels=[\"@input\"]\n";
    let view = "[[layer]]\nid=\"view\"\nscene=\"view.glsl\"\nscale=1\n\
                channels=[{ source = \"@cp\", frame = -3 }]\n";
    let with_cp = format!(
        "{}{view}",
        base.replacen("[[buffer]]", &format!("{cp}[[buffer]]"), 1)
    );
    let dp = dir.join("plain");
    stack_files(&dp)?;
    let with = with_cp_movie(dir, &with_cp)?;
    let plain = movie(&dp, &format!("shotglass = \"0.1.11\"\n{base}"))?;
    if with.buffers[0].is_pure() {
        bail!("cp's composite reads buffer state: it can't be pure");
    }
    let (mut a, mut b) = (renderer(gl, &with)?, renderer(gl, &plain)?);
    for n in [3, 20, 40, 31] {
        expect_fold(
            gl,
            &format!(
                "frame {n}: the composite after \"grade\" 3 frames back vs frame {} drawn \
                 without the buffer",
                n - 3
            ),
            &buf_frame(&mut a, &with, n, 1.0)?,
            &buf_frame(&mut b, &plain, n - 3, 1.0)?,
            1,
            0.0,
        )?;
    }
    // `@<layer>`: a copy of "lo" taken right after it equals a top layer reading "@lo"
    // (a layer only a buffer reads is held in the step's composite).
    let cl = "[[buffer]]\nid=\"cl\"\nscene=\"copy.glsl\"\nafter=\"lo\"\nformat=\"rgba8\"\n\
              channels=[\"@lo\"]\n";
    let top = |c: &str| {
        format!("[[layer]]\nid=\"view\"\nscene=\"view.glsl\"\nscale=1\nchannels=[\"{c}\"]\n")
    };
    let (dw, dp) = (dir.join("copy-layer"), dir.join("read-layer"));
    stack_files(&dw)?;
    stack_files(&dp)?;
    let with = movie(
        &dw,
        &format!("shotglass = \"0.1.11\"\n{base}{cl}{}", top("@cl")),
    )?;
    let plain = movie(
        &dp,
        &format!("shotglass = \"0.1.11\"\n{base}{}", top("@lo")),
    )?;
    let (mut a, mut b) = (renderer(gl, &with)?, renderer(gl, &plain)?);
    for n in [0, 20, 40] {
        expect_fold(
            gl,
            &format!("frame {n}: a copy of @lo after \"lo\" vs a layer reading @lo"),
            &buf_frame(&mut a, &with, n, 1.0)?,
            &buf_frame(&mut b, &plain, n, 1.0)?,
            1,
            0.0,
        )?;
    }
    // The scratch sets steps draw into follow the sizes in use: rendering at another
    // output size (cp is `scale` 1) replaces cp's composite set, it doesn't add one.
    let zm = with_cp_movie(dir, &with_cp)?;
    let mut z = renderer(gl, &zm)?;
    let sets = |z: &mut Renderer, w: i32, h: i32| -> Result<usize> {
        z.render(&zm, w, h, &FrameInfo::frame(&zm, 20), true)
            .ok_or_else(|| anyhow!("{w}x{h}: nothing rendered"))?;
        Ok(z.step_scratch_sets())
    };
    let first = sets(&mut z, W, H)?;
    for (w, h) in [(W / 2, H / 2), (W - 8, H), (W, H)] {
        let now = sets(&mut z, w, h)?;
        if now != first {
            bail!("rendering at {w}x{h} left {now} step scratch sets (at {W}x{H}: {first})");
        }
    }
    // An inactive anchor: `@<anchor>` is unbound (resolution 0) until "grade" starts at
    // 1.2 s, then bound; the composite below it is checked above (frames 3 and 20).
    let seen = "[[buffer]]\nid=\"seen\"\nscene=\"res.glsl\"\nafter=\"grade\"\nformat=\"rgba8\"\n\
                size=[8, 8]\nchannels=[\"@grade\"]\n";
    let ds = dir.join("anchor-off");
    stack_files(&ds)?;
    write(
        &ds,
        &[(
            "res.glsl",
            &scene("fragColor = vec4(iChannelResolution[0].x > 0.0 ? 1.0 : 0.0, 0.0, 0.0, 1.0);"),
        )],
    )?;
    let sm = movie(
        &ds,
        &format!("shotglass = \"0.1.11\"\n{base}{seen}{}", top("@seen")),
    )?;
    let mut sr = renderer(gl, &sm)?;
    for (n, bound) in [
        (20u64, false),
        (35, false),
        (36, true),
        (40, true),
        (33, false),
    ] {
        let f = buf_frame(&mut sr, &sm, n, 1.0)?;
        let red = f[(W as usize / 2, H as usize / 2)][0];
        if (red > 128) != bound {
            bail!(
                "frame {n}: @grade reads as {} (want {})",
                if red > 128 { "bound" } else { "unbound" },
                if bound { "bound" } else { "unbound" }
            );
        }
    }
    // In a comp (its layers, comp-local frames): a comp shot equals the comp as a film.
    let dc = dir.join("comp");
    stack_files(&dc)?;
    write(&dc, &[("c.toml", &format!("{STACK_BODY}{show}"))])?;
    let cm = movie(
        &dc,
        "shotglass = \"0.1.11\"\n[comps]\nc = \"c.toml\"\n\
         [[shot]]\nid=\"s\"\ncomp=\"c\"\nduration=2\n",
    )?;
    let mut cr = renderer(gl, &cm)?;
    for n in [0, 1, 2, 40, 41, 12, 55] {
        expect_fold(
            gl,
            &format!("comp shot, frame {n} vs the comp as a film"),
            &buf_frame(&mut cr, &cm, n, 1.0)?,
            &fwd[n as usize],
            0,
            0.0,
        )?;
    }
    // A warm loop: the composite's inputs repeat per period (real frames) while the
    // states it reads count virtual frames.
    const P: u64 = 30;
    let warm_film = |sub: &str, head: &str, shots: &str| -> Result<Movie> {
        let d = dir.join(sub);
        stack_files(&d)?;
        write(
            &d,
            &[(
                "lo2.glsl",
                &scene(
                    "vec4 p = texture(iChannel0, uv);\n  \
                     fragColor = vec4(p.bgr, step(0.5, fract(uv.x * 3.0)) * 0.8);",
                ),
            )],
        )?;
        movie(
            &d,
            &format!("shotglass = \"0.1.11\"\n{head}\n{shots}{WARM_STACK}"),
        )
    };
    let warm = warm_film(
        "warm",
        "loop = \"warm\"",
        "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\n",
    )?;
    let twice = warm_film(
        "twice",
        "",
        "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\n\
         [[shot]]\nid=\"b\"\nscene=\"s.glsl\"\nduration=1\n",
    )?;
    let (mut wr, mut tr) = (renderer(gl, &warm)?, renderer(gl, &twice)?);
    for n in [0u64, 1, 2, 15, 29, 0, 7] {
        expect_fold(
            gl,
            &format!(
                "warm frame {n} vs the unlooped second period's frame {}",
                P + n
            ),
            &buf_frame(&mut wr, &warm, n, 1.0)?,
            &buf_frame(&mut tr, &twice, P + n, 1.0)?,
            0,
            0.0,
        )?;
    }
    // 4. Edits: a layer below the anchor ("grade", from 1.2 s = frame 36) invalidates from
    //    its start; a layer above it ("above") doesn't touch the fold.
    let d = dir.join("main");
    write(
        &d,
        &[(
            "above.glsl",
            &scene("fragColor = vec4(texture(iChannel0, uv).gbr, 0.6);"),
        )],
    )?;
    let m2 = Movie::load(&d)?;
    r.load_all(&m2)
        .map_err(|e| anyhow!("{}", crate::gfx::join_errors(&e)))?;
    let (restores, steps) = (r.buffer_restores, r.buffer_steps);
    let got = buf_frame(&mut r, &m2, 55, 1.0)?;
    if r.buffer_restores != restores || r.buffer_steps != steps {
        bail!(
            "editing a layer above the anchor moved the fold ({} restores, {} steps)",
            r.buffer_restores - restores,
            r.buffer_steps - steps
        );
    }
    expect_fold(
        gl,
        "frame 55's state after editing a layer above the anchor",
        &half(&got, true),
        &half(&fwd[55], true),
        0,
        0.0,
    )?;
    write(
        &d,
        &[(
            "grade.glsl",
            &scene("fragColor = vec4(texture(iChannel0, uv).rgb * vec3(0.7, 1.1, 1.3), 1.0);"),
        )],
    )?;
    let m3 = Movie::load(&d)?;
    r.load_all(&m3)
        .map_err(|e| anyhow!("{}", crate::gfx::join_errors(&e)))?;
    let mut fresh = renderer(gl, &m3)?;
    // The cursor is at 55, after the edited layer starts: it mustn't be kept.
    let steps = r.buffer_steps;
    let got = buf_frame(&mut r, &m3, 55, 1.0)?;
    if r.buffer_steps == steps {
        bail!("editing a layer below the anchor kept the state at the cursor (frame 55)");
    }
    expect_fold(
        gl,
        "frame 55 after editing a layer below the anchor vs a cold fold",
        &got,
        &buf_frame(&mut fresh, &m3, 55, 1.0)?,
        0,
        0.0,
    )?;
    let (restores, steps) = (r.buffer_restores, r.buffer_steps);
    let got = buf_frame(&mut r, &m3, 34, 1.0)?;
    if r.buffer_restores == restores || r.buffer_steps - steps > 34 {
        bail!(
            "after editing a layer from frame 36, frame 34 folded {} steps (its checkpoint at 30 was dropped)",
            r.buffer_steps - steps
        );
    }
    expect_fold(
        gl,
        "frame 34 after editing a layer from 36",
        &half(&got, true),
        &half(&fwd[34], true),
        0,
        0.0,
    )?;
    for n in [37, 50] {
        let got = buf_frame(&mut r, &m3, n, 1.0)?;
        expect_fold(
            gl,
            &format!("frame {n} after the edit vs a cold fold"),
            &got,
            &buf_frame(&mut fresh, &m3, n, 1.0)?,
            0,
            0.0,
        )?;
        if half(&got, true) == half(&fwd[n as usize], true) {
            bail!("editing \"grade\" didn't change the after buffer at frame {n}");
        }
    }
    // Declarations in movie.toml: "above"'s opacity doesn't touch the fold, "grade"'s
    // does. The cursor is at 50.
    let toml = d.join("movie.toml");
    let text = std::fs::read_to_string(&toml)?;
    for (layer, moves) in [("above", false), ("grade", true)] {
        let from = format!("scene=\"{layer}.glsl\"\n");
        let text = text.replacen(&from, &format!("{from}opacity=0.7\n"), 1);
        std::fs::write(&toml, &text)?;
        let m4 = Movie::load(&d)?;
        r.load_all(&m4)
            .map_err(|e| anyhow!("{}", crate::gfx::join_errors(&e)))?;
        let steps = r.buffer_steps;
        let got = buf_frame(&mut r, &m4, 50, 1.0)?;
        if (r.buffer_steps != steps) != moves {
            bail!(
                "changing \"{layer}\"'s opacity: {} steps at the cursor",
                r.buffer_steps - steps
            );
        }
        let mut cold = renderer(gl, &m4)?;
        expect_fold(
            gl,
            &format!("frame 50 after changing \"{layer}\"'s opacity vs a cold fold"),
            &got,
            &buf_frame(&mut cold, &m4, 50, 1.0)?,
            0,
            0.0,
        )?;
    }
    Ok(String::new())
}
