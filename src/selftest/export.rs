//! Offline export: lossless and built-in WebM.

use super::*;

pub(super) const EXPORT_MOVIE: &str = r#"
fps = "30000/1001"
[sound]
music = "music.glsl"

[[shot]]
id = "a"
scene = "a.glsl"
duration = 0.8

[[shot]]
id = "b"
scene = "b.glsl"
duration = 0.9
transition = { type = "fx/crossfade.glsl", duration = 0.4 }
"#;

/// Decode a video's frames as RGBA and its audio as 16-bit PCM bytes, with ffmpeg.
pub(super) fn decode(path: &Path, what: &[&str]) -> Result<Vec<u8>> {
    let out = std::process::Command::new(crate::export::ffmpeg_bin())
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(what)
        .arg("-")
        .stdin(std::process::Stdio::null())
        .output()?;
    if !out.status.success() {
        bail!("decoding failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(out.stdout)
}

pub(super) fn export_lossless(gl: &Gl, dir: &Path) -> Result<String> {
    use crate::export::{self, Format, Options};
    use std::sync::atomic::{AtomicBool, Ordering};
    // Every frame differs (iTime and iFrame), and top differs from bottom.
    write(
        dir,
        &[
            (
                "a.glsl",
                &scene(
                    "fragColor = vec4(fract(iTime * 1.7 + uv.x), uv.y, fract(float(iFrame) / 7.0), 1.0);",
                ),
            ),
            (
                "b.glsl",
                &scene(
                    "fragColor = vec4(uv.y * uv.y, fract(iTime * 0.9), fract(float(iFrame) * 0.37 + uv.x), 1.0);",
                ),
            ),
            (
                "music.glsl",
                "vec2 mainSound(int samp, float time) {\n  float s = 0.3 * sin(6.2831853 * 330.0 * time);\n  return vec2(s, -0.5 * s);\n}\n",
            ),
        ],
    )?;
    let m = movie(dir, EXPORT_MOVIE)?;
    let total = m.frame_count();
    let (w, h) = (W as usize, H as usize);

    // The references: what `still` and `wav` produce.
    let mut reference = renderer(gl, &m)?;
    let wav = dir.join("ref.wav");
    let samples = export::write_wav(&mut reference, &m, 48000, &wav, 1)?;
    let pcm = std::fs::read(&wav)?[44..].to_vec();
    let mut still = |n: u64| -> Result<Frame> {
        reference
            .render(&m, W, H, &FrameInfo::at(&m, m.frame_time(n)), true)
            .ok_or_else(|| anyhow!("nothing rendered"))?;
        Ok(reference.read_output().unwrap())
    };
    let probes = [0, 1, total / 2, total - 1];
    let no_cancel = AtomicBool::new(false);
    let run_with = |format: Format,
                    out: PathBuf,
                    cancel: &AtomicBool,
                    stop_at: u64,
                    av1: export::Av1Options| {
        let mut r = renderer(gl, &m)?;
        r.set_readback(true);
        let opts = Options {
            out,
            w,
            h,
            format,
            rate: 48000,
            loops: 1,
            av1,
        };
        export::export(&mut r, &m, &opts, cancel, |done, _| {
            if done == stop_at {
                cancel.store(true, Ordering::Relaxed);
            }
        })
    };
    let run = |format: Format, out: PathBuf, cancel: &AtomicBool, stop_at: u64| {
        run_with(format, out, cancel, stop_at, export::Av1Options::default())
    };

    // PNG sequence + WAV: needs no ffmpeg, so it always runs.
    let png_dir = dir.join("png");
    let s = run(Format::Png, png_dir.clone(), &no_cancel, u64::MAX)?;
    let pngs = std::fs::read_dir(&png_dir)?
        .filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.path().extension().is_some_and(|x| x == "png"))
        })
        .count() as u64;
    if s.frames != total || pngs != total {
        bail!(
            "png: {pngs} files ({} reported), expected {total}",
            s.frames
        );
    }
    let mut notes = vec![];
    if s.audio_frames != samples {
        bail!(
            "png: sound.wav has {} samples, not {samples}",
            s.audio_frames
        );
    }
    if std::fs::read(png_dir.join("sound.wav"))?[44..] != pcm[..] {
        sound_mismatch(gl, &mut notes, "png: sound.wav differs from `wav`".into())?;
    }
    for n in probes {
        let img = image::open(png_dir.join(format!("frame_{n:06}.png")))?.into_rgba8();
        let got = Frame::from_vec(w, h, img.pixels().map(|p| p.0).collect());
        expect_same(&format!("png frame {n}"), &got, &still(n)?, 0, 0.0)?;
    }

    #[cfg(feature = "webm")]
    notes.push(export_webm(
        &m,
        &mut still,
        &pcm,
        samples,
        dir,
        &|f, out, c, stop, av1| run_with(f, out, c, stop, av1),
    )?);

    if let Err(e) = export::ffmpeg_encoders() {
        let first = e.to_string().lines().next().unwrap_or_default().to_string();
        notes.insert(0, format!("{total} frames as PNG; video skipped: {first}"));
        return Ok(notes.join("; "));
    }
    // FFV1 + FLAC: lossless, so frames and samples must come back exactly.
    let mkv = dir.join("film.mkv");
    let previous = b"previous export";
    std::fs::write(&mkv, previous)?;
    let old_wav_path = dir.join("film.mkv.shotglass-sound.wav");
    std::fs::write(&old_wav_path, b"unrelated WAV")?;
    let snapshot = || -> Result<Vec<_>> {
        let mut names = std::fs::read_dir(dir)?
            .map(|entry| entry.map(|e| e.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    };
    let before = snapshot()?;
    let bad = Format::Ffmpeg(vec!["-c:v".into(), "shotglass_no_such_encoder".into()]);
    match run(bad, mkv.clone(), &no_cancel, u64::MAX) {
        Err(e) if e.to_string().starts_with("ffmpeg failed") => {}
        Err(e) => bail!("failed video export reported: {e:#}"),
        Ok(_) => bail!("export accepted an invalid encoder"),
    }
    if std::fs::read(&mkv)? != previous || snapshot()? != before {
        bail!("failed export changed the existing output or left staged files");
    }
    let s = run(Format::Ffv1, mkv.clone(), &no_cancel, u64::MAX)?;
    if snapshot()? != before || std::fs::read(&old_wav_path)? != b"unrelated WAV" {
        bail!("video export changed an unrelated file or left staged files");
    }
    let video = decode(
        &mkv,
        &["-map", "0:v:0", "-f", "rawvideo", "-pix_fmt", "rgba"],
    )?;
    let frame_bytes = w * h * 4;
    if s.frames != total || video.len() != total as usize * frame_bytes {
        bail!(
            "mkv: {} frames decoded ({} reported), expected {total}",
            video.len() as f32 / frame_bytes as f32,
            s.frames
        );
    }
    for n in probes {
        let bytes = &video[n as usize * frame_bytes..][..frame_bytes];
        let got = Frame::from_vec(w, h, bytes.as_chunks::<4>().0.to_vec());
        expect_same(&format!("mkv frame {n}"), &got, &still(n)?, 0, 0.0)?;
    }
    let audio = decode(&mkv, &["-map", "0:a:0", "-f", "s16le"])?;
    if audio.len() == samples * 4 && audio != pcm {
        sound_mismatch(gl, &mut notes, "mkv: audio differs from `wav`".into())?;
    } else if audio.len() != samples * 4 {
        bail!(
            "mkv: audio is {} stereo samples{}, expected {samples} identical to `wav`",
            audio.len() / 4,
            if audio.len() == samples * 4 {
                " but differs"
            } else {
                ""
            }
        );
    }

    // Cancelling stops early and leaves no half-written file.
    let cancelled = dir.join("cancelled.mkv");
    let stop = AtomicBool::new(false);
    match run(Format::Ffv1, cancelled.clone(), &stop, 3) {
        Err(e) if e.to_string() == "cancelled" => {}
        Err(e) => bail!("cancelled export failed oddly: {e:#}"),
        Ok(_) => bail!("cancelling didn't stop the export"),
    }
    if snapshot()? != before {
        bail!("a cancelled export left files behind");
    }
    // A cancellation reported by the final progress callback must also abort.
    let video_file = std::fs::read(&mkv)?;
    let stop = AtomicBool::new(false);
    match run(Format::Ffv1, mkv.clone(), &stop, total) {
        Err(e) if e.to_string() == "cancelled" => {}
        Err(e) => bail!("last-frame cancellation failed oddly: {e:#}"),
        Ok(_) => bail!("last-frame cancellation published a video"),
    }
    if std::fs::read(&mkv)? != video_file || snapshot()? != before {
        bail!("last-frame cancellation changed the existing video or left staged files");
    }
    notes.insert(
        0,
        format!("{total} frames, {samples} samples, as PNG + WAV and FFV1 + FLAC"),
    );
    Ok(notes.join("; "))
}

/// The built-in WebM export (#116): its structure read back with our own EBML reader
/// (one block per frame within half a millisecond of its time, keyframes cued, Opus
/// with its delay), and a cancelled export leaves nothing. Where ffmpeg is installed,
/// the decoded planes are close to `yuv::reference` of `still`'s frames (much closer
/// than to the neighbouring frame), and the decoded sound has exactly `wav`'s length
/// and is close to it.
#[cfg(feature = "webm")]
#[allow(clippy::type_complexity)]
pub(super) fn export_webm(
    m: &Movie,
    still: &mut dyn FnMut(u64) -> Result<Frame>,
    pcm: &[u8],
    samples: usize,
    dir: &Path,
    run: &dyn Fn(
        crate::export::Format,
        PathBuf,
        &std::sync::atomic::AtomicBool,
        u64,
        crate::export::Av1Options,
    ) -> Result<crate::export::Summary>,
) -> Result<String> {
    use crate::export::Format;
    use crate::webm::mux::{id, read};
    use std::sync::atomic::AtomicBool;
    let total = m.frame_count();
    let (w, h) = (W as usize, H as usize);
    let out = dir.join("film.webm");
    use crate::export::Av1Options;
    // A short keyframe interval, so the 1.3 s film has several (the default is ~2 s).
    const KEYINT: u64 = 8;
    let short_gops = Av1Options {
        keyint: Some(KEYINT as u32),
        ..Default::default()
    };
    let s = run(
        Format::Webm,
        out.clone(),
        &AtomicBool::new(false),
        u64::MAX,
        short_gops,
    )?;
    if s.frames != total || s.audio_frames != samples {
        bail!(
            "webm: {} frames, {} samples reported",
            s.frames,
            s.audio_frames
        );
    }
    let file = std::fs::read(&out)?;
    let top = read::children(&file, 0)?;
    let seg = read::children(top[1].data, 0)?;
    let mut video_blocks = vec![];
    let mut audio_blocks = 0;
    // The first video block of each cluster that has one (by index in video_blocks).
    let mut cluster_firsts = vec![];
    for c in seg.iter().filter(|e| e.id == id::CLUSTER) {
        let kids = read::children(c.data, 0)?;
        let base = read::uint(kids[0].data) as i64;
        let mut first = true;
        for b in &kids[1..] {
            let data = if b.id == id::BLOCK_GROUP {
                read::find(&read::children(b.data, 0)?, id::BLOCK)
                    .unwrap()
                    .data
            } else {
                b.data
            };
            let t = base + i16::from_be_bytes([data[1], data[2]]) as i64;
            if data[0] & 0x7f == 1 {
                if first {
                    cluster_firsts.push(video_blocks.len() as u64);
                    first = false;
                }
                video_blocks.push((t, data[3] & 0x80 != 0));
            } else {
                audio_blocks += 1;
            }
        }
    }
    if video_blocks.len() as u64 != total {
        bail!("webm: {} video blocks, want {total}", video_blocks.len());
    }
    let fps_num = i128::from(m.fps_num);
    let fps_den = i128::from(m.fps_den);
    for (n, &(ms, _)) in (0..total).zip(&video_blocks) {
        // Compare in units of 1/fps_num milliseconds to avoid rounding the reference.
        let expected = i128::from(n) * fps_den * 1_000;
        let error = (i128::from(ms) * fps_num - expected).abs();
        if 2 * error > fps_num {
            bail!("webm: frame {n} at {ms} ms exceeds the 0.5 ms tolerance");
        }
    }
    // Keyframes exactly every KEYINT frames (closed GOPs), and clusters start exactly
    // at the keyframes (every cluster here is far shorter than the 30 s cap).
    let keys: Vec<u64> = (0..video_blocks.len())
        .filter(|&n| video_blocks[n].1)
        .map(|n| n as u64)
        .collect();
    let want_keys: Vec<u64> = (0..total).filter(|n| n % KEYINT == 0).collect();
    if want_keys.len() < 3 {
        bail!(
            "webm: {total} frames give only {} keyframes to check",
            want_keys.len()
        );
    }
    if keys != want_keys {
        bail!("webm: keyframes at {keys:?}, want {want_keys:?}");
    }
    if cluster_firsts != want_keys {
        bail!(
            "webm: clusters start at frames {cluster_firsts:?}, want the keyframes {want_keys:?}"
        );
    }
    // libopus's lookahead, as the export's encoder reports it (version-dependent).
    let pre_skip = usize::from(
        crate::webm::encode::OpusPackets::new(crate::webm::export::OPUS_KBPS)?.pre_skip,
    );
    if audio_blocks != (samples + pre_skip).div_ceil(crate::webm::encode::OPUS_FRAME) {
        bail!("webm: {audio_blocks} Opus packets for {samples} samples");
    }
    if read::find(&seg, id::CUES).is_none() {
        bail!("webm: no cues");
    }

    // Cancelling leaves no file and no staged part.
    let cancelled = dir.join("cancelled.webm");
    let names = || -> Result<usize> { Ok(std::fs::read_dir(dir)?.count()) };
    let before = names()?;
    match run(
        Format::Webm,
        cancelled.clone(),
        &AtomicBool::new(false),
        3,
        Av1Options::default(),
    ) {
        Err(e) if e.to_string() == "cancelled" => {}
        Err(e) => bail!("cancelled webm export failed oddly: {e:#}"),
        Ok(_) => bail!("cancelling didn't stop the webm export"),
    }
    if cancelled.exists() || names()? != before {
        bail!("a cancelled webm export left files behind");
    }

    if crate::export::ffmpeg_encoders().is_err() {
        return Ok(format!("webm: {total} frames (not decoded: no ffmpeg)"));
    }
    // Our interleaved-row I420 as planar (ffmpeg's yuv420p / yuv420p10le).
    let planar = |i420: Vec<u16>| -> Vec<u16> {
        let (cw, ch) = (w / 2, h / 2);
        let mut v = i420[..w * h].to_vec();
        for plane in 0..2 {
            for row in 0..ch {
                let at = w * h + row * w + plane * cw;
                v.extend_from_slice(&i420[at..at + cw]);
            }
        }
        v
    };
    let mean_err = |a: &[u16], b: &[u16]| -> f64 {
        a.iter()
            .zip(b)
            .map(|(x, y)| x.abs_diff(*y) as f64)
            .sum::<f64>()
            / a.len() as f64
    };
    // Errors in 10-bit codes at both depths (8-bit errors × 4).
    let mut worst = [0f64; 2];
    for (k, depth) in [10u8, 8].into_iter().enumerate() {
        let path = if depth == 10 {
            out.clone()
        } else {
            let p = dir.join("film-8bit.webm");
            run(
                Format::Webm,
                p.clone(),
                &AtomicBool::new(false),
                u64::MAX,
                Av1Options {
                    depth: 8,
                    ..Default::default()
                },
            )?;
            p
        };
        let pix_fmt = if depth == 10 {
            "yuv420p10le"
        } else {
            "yuv420p"
        };
        let planes = decode(
            &path,
            &["-map", "0:v:0", "-f", "rawvideo", "-pix_fmt", pix_fmt],
        )?;
        let per = crate::yuv::frame_bytes(w, h, depth);
        if planes.len() != per * total as usize {
            bail!(
                "webm {depth}-bit: decoded {} bytes of video, want {}",
                planes.len(),
                per * total as usize
            );
        }
        let unit = f64::from(1u32 << (10 - depth));
        for n in [0, 1, total / 2, total - 1] {
            let got = crate::yuv::samples(&planes[n as usize * per..][..per], depth);
            let want = planar(crate::yuv::reference(&still(n)?, depth));
            let err = mean_err(&got, &want) * unit;
            let other = if n == 0 { 1 } else { n - 1 };
            let neighbour =
                mean_err(&got, &planar(crate::yuv::reference(&still(other)?, depth))) * unit;
            if err > 6.0 || err * 2.0 > neighbour {
                bail!(
                    "webm {depth}-bit frame {n}: mean error {err:.2} (10-bit codes), {neighbour:.2} against frame {other}"
                );
            }
            worst[k] = worst[k].max(err);
        }
    }
    let audio = decode(&out, &["-map", "0:a:0", "-f", "s16le", "-ar", "48000"])?;
    if audio.len() != samples * 4 {
        bail!(
            "webm: decoded {} stereo samples, want {samples}",
            audio.len() / 4
        );
    }
    let pcm16 = |b: &[u8]| -> Vec<f64> {
        b.as_chunks::<2>()
            .0
            .iter()
            .map(|s| i16::from_le_bytes(*s) as f64)
            .collect()
    };
    let (a, b) = (pcm16(&audio), pcm16(pcm));
    let noise = a.iter().zip(&b).map(|(x, y)| (x - y).powi(2)).sum::<f64>();
    let signal = b.iter().map(|y| y * y).sum::<f64>();
    let snr = 10.0 * (signal / noise.max(1.0)).log10();
    if snr < 20.0 {
        bail!("webm: the decoded sound is {snr:.1} dB from `wav`");
    }
    Ok(format!(
        "webm: {total} frames, mean error ≤ {:.2} at 10 bits, ≤ {:.2} at 8 (10-bit codes), sound {snr:.0} dB SNR",
        worst[0], worst[1]
    ))
}
