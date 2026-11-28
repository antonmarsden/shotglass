//! Loop films (`loop = true`): picture and sound across the seam.

use super::*;

pub(super) const LOOP_PICTURE_MOVIE: &str = r#"
loop = true
fps = 30

[[shot]]
id = "a"
scene = "p.glsl"
duration = 1.0
uniforms.uK = { keys = [[0, 0.0], [1, 1.0]] }

[[shot]]
id = "b"
scene = "q.glsl"
duration = 1.0
transition = { type = "fx/crossfade.glsl", duration = 0.4 }
"#;

/// A loop film (48 frames) whose every frame differs: frame n of loop k must be frame n
/// of loop 0, by frame number exactly and by time (f32 times, wrapped in f64).
pub(super) fn loop_picture(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            (
                "p.glsl",
                &scene(
                    "fragColor = vec4(fract(iTime * 0.37 + uv.x + uK), fract(float(iFrame) / 7.0 + uv.y), fract(iMovieTime * 0.53 + iShotTime * 0.11), 1.0);",
                ),
            ),
            (
                "q.glsl",
                &scene(
                    "fragColor = vec4(fract(iMovieTime * 0.71 + uv.y), fract(iTime * 1.3 + uv.x * uv.x), fract(float(iFrame) * 0.13 + iShotProgress), 1.0);",
                ),
            ),
        ],
    )?;
    let m = movie(dir, LOOP_PICTURE_MOVIE)?;
    if m.loop_frames != Some(48) {
        bail!(
            "the fixture should loop every 48 frames, not {:?}",
            m.loop_frames
        );
    }
    let mut r = renderer(gl, &m)?;
    r.set_readback(true);
    let mut grab = |info: FrameInfo| -> Result<Frame> {
        r.render(&m, W, H, &info, true)
            .ok_or_else(|| anyhow!("nothing rendered"))?;
        r.read_output().ok_or_else(|| anyhow!("no output"))
    };
    // 0, a shot, the transition, the last frame (just before the seam).
    let probes = [0u64, 7, 24, 47];
    let mut base = vec![];
    for &n in &probes {
        base.push(grab(FrameInfo::frame(&m, n))?);
    }
    if diff(&base[0], &base[3], 0).0 == 0 {
        bail!("the fixture's first and last frames are the same; it can't show a wrap");
    }
    for (i, &n) in probes.iter().enumerate() {
        for k in [1u64, 5, 1_000_000] {
            let f = grab(FrameInfo::frame(&m, n + k * 48))?;
            if diff(&f, &base[i], 0).0 != 0 {
                bail!(
                    "frame {} (loop {k}, frame {n}) differs from frame {n}",
                    n + k * 48
                );
            }
        }
        // By time, as `still --at` and the player pass it: f32 seconds, a few loops in.
        for k in [1u64, 2, 3] {
            let t = m.frame_time(n + k * 48);
            let f = grab(FrameInfo::at(&m, t))?;
            let (max, frac) = diff(&f, &base[i], 1);
            if max > 1 || frac > 0.0 {
                bail!(
                    "t = {t}s (loop {k}, frame {n}) differs from frame {n} by up to {max} \
                     ({:.2}% of pixels)",
                    frac * 100.0
                );
            }
        }
    }
    // The frame after the last is frame 0.
    let f = grab(FrameInfo::at(&m, m.frame_time(48)))?;
    if diff(&f, &base[0], 1).0 > 1 {
        bail!("the frame after the last isn't frame 0");
    }
    // The same film without `loop` doesn't wrap: t + period is the end of the film.
    let once = movie(dir, &LOOP_PICTURE_MOVIE.replace("loop = true", ""))?;
    let mut r = renderer(gl, &once)?;
    r.set_readback(true);
    r.render(
        &once,
        W,
        H,
        &FrameInfo::at(&once, once.frame_time(7 + 48)),
        true,
    )
    .ok_or_else(|| anyhow!("nothing rendered"))?;
    let f = r.read_output().ok_or_else(|| anyhow!("no output"))?;
    if diff(&f, &base[1], 1).0 <= 1 {
        bail!("without loop = true, t + period still looks like t: the check proves nothing");
    }
    Ok(format!(
        "{} frames × loops 1, 5, 10⁶ by frame number, 1–3 by time",
        probes.len()
    ))
}

/// Shots for the loop sound fixture: shot "a" (hard cut in), then "b" crossfading in.
pub(super) fn loop_sound_shots(suffix: &str) -> String {
    format!(
        r#"
[[shot]]
id = "a{suffix}"
scene = "s.glsl"
duration = 0.5
sound = "tone.glsl"
uniforms.uPitch = 1.0

[[shot]]
id = "b{suffix}"
scene = "s.glsl"
duration = 0.5
sound = "tone.glsl"
uniforms.uPitch = 1.5
transition = {{ type = "fx/crossfade.glsl", duration = 0.2 }}
"#
    )
}

pub(super) const LOOP_SOUND_HEADER: &str = r#"
fps = 30
[sound]
music = "music.glsl"
mix = "mix.glsl"
mix_lookback = 0.5
"#;

/// Sound across the seam: a 0.8 s loop film whose mix echoes music and shot sounds
/// by 300 ms (so the end of each loop rings into the next) and ducks with a look-ahead.
/// One looped period must (1) be the steady state of a long render of the same
/// material played three times without `loop` (so: no click at the wrap, and the echo
/// carries over), (2) step no more at the seam than anywhere else, (3) be the same
/// whatever the chunk size, and (4) stream (starting just before the seam, as the
/// player does when it plays across it) exactly as it renders blocking.
pub(super) fn loop_sound(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.0, 0.0, 0.0, 1.0);")),
            // Exactly periodic in samples: 192 samples = 250 Hz, 200 cycles per loop.
            (
                "music.glsl",
                "vec2 mainSound(int samp, float time) {\n  float s = 0.2 * sin(6.2831853 * float(samp % 192) / 192.0);\n  return vec2(s, s * 0.9);\n}\n",
            ),
            (
                "tone.glsl",
                "vec2 mainSound(int samp, float time) {\n  float s = 0.2 * sin(6.2831853 * 337.0 * uPitch * time + 0.7);\n  return vec2(s * 0.8, s);\n}\n",
            ),
            (
                "mix.glsl",
                "vec2 mainMix(int samp, float time) {\n  int ms = int(iSampleRate * 0.001);\n  vec2 echo = (music(samp - 300 * ms) + sfx(samp - 300 * ms)) * 0.4;\n  float duck = 1.0 - 0.3 * smoothstep(0.05, 0.3, sfxLevel(samp));\n  return (music(samp) + echo) * duck + sfx(samp);\n}\n",
            ),
        ],
    )?;
    let looped_toml = format!("loop = true\n{LOOP_SOUND_HEADER}{}", loop_sound_shots(""));
    let m = movie(dir, &looped_toml)?;
    let p = m.sound_samples(48000);
    if p != 38400 {
        bail!("the fixture's period should be 38400 samples, not {p}");
    }
    let looped = render_sound(gl, &m, crate::gfx::AUDIO_BLOCK)?; // one chunk
    if looped.len() != p * 2 {
        bail!("one period has {} samples, not {p}", looped.len() / 2);
    }
    if looped.iter().all(|x| x.abs() < 1e-4) {
        bail!("the fixture rendered silence");
    }
    let mut notes = vec![];

    // (1) The steady state of a long, unlooped render: its second period.
    let long_toml = format!(
        "{LOOP_SOUND_HEADER}{}{}{}",
        loop_sound_shots("1"),
        loop_sound_shots("2"),
        loop_sound_shots("3")
    );
    let long_dir = dir.join("long");
    std::fs::create_dir_all(&long_dir)?;
    for f in ["s.glsl", "music.glsl", "tone.glsl", "mix.glsl"] {
        std::fs::copy(dir.join(f), long_dir.join(f))?;
    }
    let long_m = movie(&long_dir, &long_toml)?;
    let long = render_sound(gl, &long_m, crate::gfx::AUDIO_BLOCK)?;
    if long.len() < 3 * p * 2 {
        bail!("the long render is too short ({} samples)", long.len() / 2);
    }
    let steady = &long[p * 2..p * 4];
    let (i, d) = looped
        .iter()
        .zip(steady)
        .map(|(a, b)| (a - b).abs())
        .enumerate()
        .fold(
            (0, 0f32),
            |acc, (i, d)| if d > acc.1 { (i, d) } else { acc },
        );
    // Shot sounds see f32 times a period later in the long film: tiny phase drift.
    if d > 2e-3 {
        bail!(
            "the looped period differs from the long render's steady state by {d} at {:.4}s \
             (the mix should read across the seam)",
            (i / 2) as f32 / 48000.0
        );
    }

    // (2) No click: the seam steps no more than the largest step inside the period,
    // and just as the long render steps there.
    let step = |a: &[f32], b: &[f32]| (a[0] - b[0]).abs().max((a[1] - b[1]).abs());
    let largest = looped
        .as_chunks::<2>()
        .0
        .windows(2)
        .map(|w| step(&w[0], &w[1]))
        .fold(0f32, f32::max);
    let seam = step(&looped[0..2], &looped[p * 2 - 2..p * 2]);
    let long_seam = step(&long[p * 2..p * 2 + 2], &long[p * 2 - 2..p * 2]);
    if seam > largest + 1e-4 || (seam - long_seam).abs() > 2e-3 {
        bail!(
            "the sound clicks at the loop seam: it steps by {seam} there (largest step elsewhere \
             {largest}, the long render steps by {long_seam})"
        );
    }

    // (3) Whatever the chunk size (one that doesn't divide the period, too).
    for chunk in [32768, 7919] {
        let parts = render_sound(gl, &m, chunk)?;
        let d = parts
            .iter()
            .zip(&looped)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        if parts.len() != looped.len() || d > 1e-5 {
            sound_mismatch(
                gl,
                &mut notes,
                format!("chunk {chunk}: {} samples, differs by {d}", parts.len() / 2),
            )?;
        }
    }

    // (4) Streamed, from the last chunk across the seam, as the player asks for it.
    let chunk = 7919;
    let blocking = render_sound(gl, &m, chunk)?;
    let mut r = Renderer::new(gl.clone())?;
    let mut st = Soundtrack::with_chunk(chunk);
    st.set_movie(&mut r, &m, 48000)
        .map_err(|e| anyhow!("{e:?}"))?;
    let n = st.total_chunks();
    let mut got: Vec<Option<Arc<Vec<f32>>>> = vec![None; n];
    let start = Instant::now();
    while got.iter().any(|c| c.is_none()) {
        if start.elapsed().as_secs() > 60 {
            bail!("streaming didn't finish in 60 s");
        }
        let wanted: Vec<usize> = (0..n)
            .map(|k| (n - 1 + k) % n)
            .filter(|&c| got[c].is_none())
            .collect();
        let deadline = Instant::now() + std::time::Duration::from_millis(4);
        for (c, d) in st
            .stream(&mut r, &wanted, 2, deadline)
            .map_err(|e| anyhow!(e))?
        {
            got[c] = Some(d);
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    if r.blocking_audio_reads != 0 {
        bail!(
            "streaming waited for the GPU {} time(s)",
            r.blocking_audio_reads
        );
    }
    let streamed: Vec<f32> = got.into_iter().flat_map(|c| c.unwrap().to_vec()).collect();
    if streamed.len() != blocking.len()
        || streamed
            .iter()
            .zip(&blocking)
            .any(|(a, b)| a.to_bits() != b.to_bits())
    {
        sound_mismatch(
            gl,
            &mut notes,
            "streamed loop sound differs from the blocking render".into(),
        )?;
    }
    notes.insert(
        0,
        format!("long render within {d:.1e}; seam step {seam:.4} (largest {largest:.4})"),
    );
    Ok(notes.join("; "))
}
