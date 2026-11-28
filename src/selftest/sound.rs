//! Soundtrack: chunk and band equivalence, streaming, clicks, sound edits.

use super::*;

pub(super) const SOUND_MOVIE: &str = r#"
[sound]
music = "music.glsl"
mix = "mix.glsl"
mix_lookback = 0.5

[[shot]]
id = "a"
scene = "s.glsl"
duration = 1.03
uniforms.uPitch = 1.0
sound = "tone.glsl"
sound_tail = 0.2

[[shot]]
id = "b"
scene = "s.glsl"
duration = 0.937
sound = "tone.glsl"
sound_lead = 0.1
uniforms.uPitch = 1.5
transition = { type = "fx/crossfade.glsl", duration = 0.4 }

[[shot]]
id = "c"
scene = "s.glsl"
duration = 0.8
sound = "tone.glsl"
uniforms.uPitch = 2.0
"#;

pub(super) fn sound_fixture(dir: &Path) -> Result<Movie> {
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.0, 0.0, 0.0, 1.0);")),
            (
                "music.glsl",
                "vec2 mainSound(int samp, float time) {\n  float s = 0.2 * sin(6.2831853 * 220.0 * time + 2.0 * sin(6.2831853 * 3.0 * time));\n  return vec2(s, s * 0.9);\n}\n",
            ),
            (
                "tone.glsl",
                "vec2 mainSound(int samp, float time) {\n  float s = 0.2 * sin(6.2831853 * 337.0 * uPitch * time + 0.7);\n  return vec2(s * 0.8, s);\n}\n",
            ),
            (
                "mix.glsl",
                "vec2 mainMix(int samp, float time) {\n  int ms = int(iSampleRate * 0.001);\n  vec2 echo = music(samp - 300 * ms) * 0.3;\n  float duck = 1.0 - 0.3 * smoothstep(0.05, 0.3, sfxLevel(samp));\n  return (music(samp) + echo) * duck + sfx(samp);\n}\n",
            ),
        ],
    )?;
    movie(dir, SOUND_MOVIE)
}

pub(super) fn render_sound(gl: &Gl, m: &Movie, chunk: usize) -> Result<Vec<f32>> {
    strict(m)?;
    let mut r = Renderer::new(gl.clone())?;
    let mut st = Soundtrack::with_chunk(chunk);
    st.set_movie(&mut r, m, 48000)
        .map_err(|e| anyhow!("{}", e.iter().map(|e| e.to_string()).collect::<String>()))?;
    let mut out = vec![];
    st.render_all(&mut r, |d| out.extend_from_slice(d))
        .map_err(|e| anyhow!(e))?;
    Ok(out)
}

pub(super) fn sound_chunk_equivalence(gl: &Gl, dir: &Path) -> Result<String> {
    let m = sound_fixture(dir)?;
    let whole = render_sound(gl, &m, crate::gfx::AUDIO_BLOCK)?; // the 2 s film fits one chunk
    let mut notes = vec![];
    let mut skipped = vec![];
    for chunk in [32768, 7919] {
        let parts = render_sound(gl, &m, chunk)?;
        if parts.len() != whole.len() {
            bail!(
                "chunk {chunk}: {} samples, expected {}",
                parts.len(),
                whole.len()
            );
        }
        let (i, d) = parts
            .iter()
            .zip(&whole)
            .map(|(a, b)| (a - b).abs())
            .enumerate()
            .fold(
                (0, 0f32),
                |acc, (i, d)| if d > acc.1 { (i, d) } else { acc },
            );
        notes.push(format!("{chunk}"));
        if d > 1e-5 {
            let msg = format!(
                "chunk {chunk}: differs by {d} at {:.4}s",
                (i / 2) as f32 / 48000.0
            );
            sound_mismatch(gl, &mut skipped, msg)?;
        }
    }
    if whole.iter().all(|x| x.abs() < 1e-4) {
        bail!("the fixture rendered silence");
    }
    skipped.insert(0, format!("chunks of {} samples", notes.join(" and ")));
    Ok(skipped.join("; "))
}

pub(super) fn sound_banding(gl: &Gl, dir: &Path) -> Result<String> {
    let m = sound_fixture(dir)?;
    let render = |banded: bool| -> Result<Vec<f32>> {
        let mut r = Renderer::new(gl.clone())?;
        let mut st = Soundtrack::with_chunk(32768);
        st.set_movie(&mut r, &m, 48000)
            .map_err(|e| anyhow!("{e:?}"))?;
        let mut out = vec![];
        r.set_audio_banded(banded);
        let n = st.total_chunks();
        for c in 0..n {
            r.set_audio_banded(banded); // the renderer adapts after each block; pin it
            out.extend_from_slice(&st.chunk(&mut r, c).map_err(|e| anyhow!(e))?);
        }
        Ok(out)
    };
    let plain = render(false)?;
    let banded = render(true)?;
    let d = plain
        .iter()
        .zip(&banded)
        .map(|(a, b)| (a - b).abs())
        .fold(0f32, f32::max);
    if plain.len() != banded.len() {
        bail!(
            "banded synthesis gave {} samples, not {}",
            banded.len(),
            plain.len()
        );
    }
    let mut notes = vec![];
    if d > 1e-6 {
        sound_mismatch(gl, &mut notes, format!("banded synthesis differs by {d}"))?;
    }
    Ok(notes.join("; "))
}

/// Playback-style synthesis: `Soundtrack::stream` once per "frame", as the player
/// does, must never wait on a readback, and must produce the blocking render exactly.
pub(super) fn sound_streaming(gl: &Gl, dir: &Path) -> Result<String> {
    let m = sound_fixture(dir)?;
    let mut notes = vec![];
    for (chunk, persistent) in [(32768, true), (7919, true), (32768, false), (7919, false)] {
        let blocking = render_sound(gl, &m, chunk)?;
        let mut r = Renderer::new(gl.clone())?;
        if r.set_persistent_buffers(persistent) != persistent {
            notes.push("no persistent buffers on this driver".into());
            continue;
        }
        let mut st = Soundtrack::with_chunk(chunk);
        st.set_movie(&mut r, &m, 48000)
            .map_err(|e| anyhow!("{e:?}"))?;
        let n = st.total_chunks();
        let mut got: Vec<Option<Arc<Vec<f32>>>> = vec![None; n];
        let mut frames = 0;
        let mut max_in_flight = 0;
        let mut cancelled = false;
        let start = Instant::now();
        while got.iter().any(|c| c.is_none()) {
            if start.elapsed().as_secs() > 60 {
                bail!("chunk {chunk}: streaming didn't finish in 60 s ({frames} frames)");
            }
            let wanted: Vec<usize> = (0..n).filter(|&c| got[c].is_none()).collect();
            let deadline = Instant::now() + std::time::Duration::from_millis(4);
            for (c, d) in st
                .stream(&mut r, &wanted, 2, deadline)
                .map_err(|e| anyhow!(e))?
            {
                got[c] = Some(d);
            }
            max_in_flight = max_in_flight.max(r.audio_in_flight());
            if !cancelled && r.audio_in_flight() > 0 {
                st.cancel_all(&mut r);
                if st.in_flight() != 0 || r.audio_in_flight() != 0 {
                    bail!("chunk {chunk}: cancellation left blocks in flight");
                }
                cancelled = true;
            }
            frames += 1;
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        if r.blocking_audio_reads != 0 {
            bail!(
                "chunk {chunk}: streaming waited for the GPU {} time(s)",
                r.blocking_audio_reads
            );
        }
        if st.in_flight() != 0 || r.audio_in_flight() != 0 {
            bail!("chunk {chunk}: blocks left in flight after the last chunk");
        }
        if max_in_flight == 0 {
            bail!("chunk {chunk}: nothing was ever queued asynchronously");
        }
        let streamed: Vec<f32> = got.into_iter().flat_map(|c| c.unwrap().to_vec()).collect();
        if streamed.len() != blocking.len() {
            bail!(
                "chunk {chunk}: {} samples streamed, {} blocking",
                streamed.len(),
                blocking.len()
            );
        }
        if let Some(i) = streamed
            .iter()
            .zip(&blocking)
            .position(|(a, b)| a.to_bits() != b.to_bits())
        {
            let msg = format!(
                "chunk {chunk}: streamed sound differs at {:.4}s ({} vs {})",
                (i / 2) as f32 / 48000.0,
                streamed[i],
                blocking[i]
            );
            sound_mismatch(gl, &mut notes, msg)?;
        }
        notes.push(format!(
            "{chunk}{}: {frames} frames, ≤{max_in_flight} queued",
            if persistent { " persistent" } else { "" }
        ));
    }
    // The blocking path does count its waits, so the zero above means something.
    let mut r = Renderer::new(gl.clone())?;
    let mut st = Soundtrack::new();
    st.set_movie(&mut r, &m, 48000)
        .map_err(|e| anyhow!("{e:?}"))?;
    st.chunk(&mut r, 0).map_err(|e| anyhow!(e))?;
    if r.blocking_audio_reads == 0 {
        bail!("a blocking chunk wasn't counted as a blocking readback");
    }
    Ok(notes.join("; "))
}

pub(super) fn sound_no_clicks(gl: &Gl, dir: &Path) -> Result<String> {
    let m = sound_fixture(dir)?;
    let s = render_sound(gl, &m, 7919)?;
    // Every source is a ≤0.25-amplitude sine below 700 Hz (phases chosen so no
    // window edge falls on a zero crossing): the steepest legal step is
    // about 2π·700·0.45/48000 ≈ 0.04 per sample. A click is a much bigger jump.
    let mut worst = (0f32, 0usize);
    for ch in 0..2 {
        for i in 1..s.len() / 2 {
            let d = (s[i * 2 + ch] - s[(i - 1) * 2 + ch]).abs();
            if d > worst.0 {
                worst = (d, i);
            }
        }
    }
    if worst.0 > 0.06 {
        bail!(
            "a jump of {:.3} at {:.4}s",
            worst.0,
            worst.1 as f32 / 48000.0
        );
    }
    Ok(format!("largest step {:.3}", worst.0))
}

pub(super) fn sound_errors(gl: &Gl, dir: &Path) -> Result<String> {
    let m = sound_fixture(dir)?;
    let mut r = Renderer::new(gl.clone())?;
    let mut st = Soundtrack::new();
    st.set_movie(&mut r, &m, 48000)
        .map_err(|e| anyhow!("{e:?}"))?;
    let version = st.version();
    let last = st.total_chunks() - 1;
    let samples = st.chunk(&mut r, last).map_err(|e| anyhow!(e))?;
    let music = dir.join("music.glsl");
    let working = std::fs::read(&music)?;
    std::fs::write(
        &music,
        "// music\n\nvec2 mainSound(int samp, float time) {\n  return vec2(nope);\n}\n",
    )?;
    match st.set_movie(&mut r, &m, 48000) {
        Ok(()) => bail!("a broken music shader was accepted"),
        Err(e) => {
            let text: String = e.iter().map(|e| e.to_string()).collect();
            if !text.contains("music.glsl:4:") {
                bail!("expected an error at music.glsl:4, got:\n{text}");
            }
        }
    }
    if st.version() != version
        || !std::sync::Arc::ptr_eq(&samples, &st.chunk(&mut r, last).map_err(|e| anyhow!(e))?)
    {
        bail!("a broken shader discarded timing-compatible fallback audio");
    }
    let changed = movie(
        dir,
        &SOUND_MOVIE.replace("duration = 1.03", "duration = 2.03"),
    )?;
    if changed.sound_samples(48000) <= m.sound_samples(48000) {
        bail!("the reload fixture did not extend the soundtrack");
    }
    if st.set_movie(&mut r, &changed, 48000).is_ok()
        || st.version().is_some()
        || st.total_chunks() != 0
        || st.in_flight() != 0
    {
        bail!("a broken reload with a longer film kept stale soundtrack timing");
    }
    std::fs::write(music, working)?;
    st.set_movie(&mut r, &changed, 48000)
        .map_err(|e| anyhow!("{e:?}"))?;
    if st.version().is_none() || st.total_samples() != changed.sound_samples(48000) {
        bail!("fixing the shader did not restore audio at the new film length");
    }
    Ok(String::new())
}
