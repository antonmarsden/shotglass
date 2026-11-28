//! `shotglass conform` (#73): convert a clip once into the footage shotglass reads,
//! AV1 in WebM at a constant frame rate, written next to the source (a file the user
//! keeps, commits or deletes; nothing is cached). ffmpeg only decodes: it delivers raw
//! Y'CbCr at a constant rate (autorotate applied) through a pipe, and our own encoder
//! (SVT-AV1) and muxer write the file. The source's matrix and range are kept when
//! SVT-AV1 can tag them (no trip through RGB); otherwise ffmpeg converts to BT.709
//! limited range. Conforming re-encodes, so pixels change slightly, and a variable-rate
//! source gets frames repeated or dropped onto the constant-rate grid.

use crate::export::ffmpeg_bin;
use crate::webm::ColourTags;
use crate::webm::encode::{Av1Settings, Rate};
use anyhow::{Context, Result, anyhow, bail};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct Options {
    pub out: Option<PathBuf>,
    /// Frames between keyframes; None = about two seconds.
    pub keyint: Option<u32>,
    pub crf: u8,
    /// 8 or 10; None = the source's depth, at most 10.
    pub depth: Option<u8>,
    /// The output rate; for an image sequence, its rate (required).
    pub fps: Option<(u32, u32)>,
    pub speed: u8,
}

/// The source as ffprobe sees it (the first video stream).
#[derive(Debug, Default)]
struct Probe {
    width: u32,
    height: u32,
    rate: Option<(u32, u32)>,
    avg_rate: Option<(u32, u32)>,
    pix_fmt: String,
    matrix: Option<u8>,
    primaries: Option<u8>,
    transfer: Option<u8>,
    full_range: Option<bool>,
    rotation: i64,
}

fn ffprobe_bin() -> PathBuf {
    if let Some(p) = std::env::var_os("SHOTGLASS_FFPROBE") {
        return p.into();
    }
    let ff = ffmpeg_bin();
    let name = if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    };
    match ff.parent() {
        Some(d) if !d.as_os_str().is_empty() && d.join(name).is_file() => d.join(name),
        _ => "ffprobe".into(),
    }
}

fn ratio(s: &str) -> Option<(u32, u32)> {
    let (n, d) = s.split_once('/')?;
    let (n, d): (u32, u32) = (n.parse().ok()?, d.parse().ok()?);
    (n > 0 && d > 0).then_some((n, d))
}

/// ISO/IEC 23091-4 code points from ffmpeg's names.
fn code(kind: &str, name: &str) -> Option<u8> {
    Some(match (kind, name) {
        ("matrix", "bt709") | ("primaries", "bt709") | ("transfer", "bt709") => 1,
        ("matrix", "bt470bg") | ("primaries", "bt470bg") | ("transfer", "bt470bg") => 5,
        ("matrix", "smpte170m") | ("primaries", "smpte170m") | ("transfer", "smpte170m") => 6,
        ("matrix", "smpte240m") | ("primaries", "smpte240m") | ("transfer", "smpte240m") => 7,
        ("matrix", "bt2020nc") | ("primaries", "bt2020") => 9,
        ("transfer", "iec61966-2-1") => 13,
        ("transfer", "bt2020-10") => 14,
        ("transfer", "bt2020-12") => 15,
        ("transfer", "smpte2084") => 16,
        ("transfer", "arib-std-b67") => 18,
        _ => return None,
    })
}

fn parse_probe(json: &str) -> Result<Probe> {
    let v: serde_json::Value = serde_json::from_str(json).context("ffprobe's output")?;
    let s = v["streams"]
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| anyhow!("no video stream"))?;
    let st = |k: &str| s[k].as_str().unwrap_or("").to_string();
    let rotation = s["side_data_list"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|d| d["rotation"].as_i64())
        .unwrap_or(0);
    Ok(Probe {
        width: s["width"].as_u64().unwrap_or(0) as u32,
        height: s["height"].as_u64().unwrap_or(0) as u32,
        rate: ratio(&st("r_frame_rate")),
        avg_rate: ratio(&st("avg_frame_rate")),
        pix_fmt: st("pix_fmt"),
        matrix: code("matrix", &st("color_space")),
        primaries: code("primaries", &st("color_primaries")),
        transfer: code("transfer", &st("color_transfer")),
        full_range: match st("color_range").as_str() {
            "pc" => Some(true),
            "tv" => Some(false),
            _ => None,
        },
        rotation,
    })
}

/// "frames/plate_####.jpg" → "frames/plate_%04d.jpg" (ffmpeg's image2 pattern).
fn ffmpeg_pattern(input: &str) -> Option<String> {
    let start = input
        .rfind('#')
        .map(|e| input[..=e].trim_end_matches('#').len())?;
    let digits = input[start..].chars().take_while(|c| *c == '#').count();
    Some(format!(
        "{}%0{digits}d{}",
        &input[..start],
        &input[start + digits..]
    ))
}

/// Run it.
pub fn conform(input: &str, o: &Options) -> Result<PathBuf> {
    let pattern = ffmpeg_pattern(input);
    let path = Path::new(input);
    let out = o.out.clone().unwrap_or_else(|| {
        let stem = match &pattern {
            Some(_) => path
                .file_stem()
                .map(|n| n.to_string_lossy().replace('#', ""))
                .unwrap_or_default()
                .trim_end_matches(['_', '-', '.'])
                .to_string(),
            None => path
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        let stem = stem.split('.').next().unwrap_or("clip").to_string();
        let base = path.parent().unwrap_or(Path::new(""));
        let mut out = base.join(format!("{stem}.webm"));
        if out == path {
            out = base.join(format!("{stem}.conformed.webm"));
        }
        out
    });
    if pattern.is_none() && !path.is_file() {
        bail!("{input}: not found");
    }
    let untouched = o.keyint.is_none() && o.depth.is_none() && o.fps.is_none();
    if pattern.is_none() && untouched {
        let ok = crate::video::Source::open(path, &Default::default());
        if ok.is_ok() {
            if out != path && o.out.is_some() {
                std::fs::copy(path, &out)
                    .with_context(|| format!("copying {input} to {}", out.display()))?;
                println!(
                    "{input} is already AV1 in WebM at a constant frame rate: copied to {}",
                    out.display()
                );
                return Ok(out);
            }
            println!("{input} is already AV1 in WebM at a constant frame rate: nothing to do");
            return Ok(path.to_path_buf());
        }
    }
    // An image sequence's first frame number: ffmpeg only looks for one in 0..=4.
    let start = match &pattern {
        Some(_) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let pat = crate::footage::Pattern::parse(&name).map_err(|e| anyhow!("{e}"))?;
            let dir = match path.parent() {
                Some(d) if !d.as_os_str().is_empty() => d,
                _ => Path::new("."),
            };
            let scan = crate::footage::scan(dir, input, &pat, None).map_err(|e| anyhow!("{e}"))?;
            scan.first.to_string()
        }
        None => String::new(),
    };
    let mut input_args: Vec<String> = vec!["-nostdin".into(), "-hide_banner".into()];
    if let Some(p) = &pattern {
        let fps = o.fps.ok_or_else(|| {
            anyhow!("{input} is an image sequence: give its rate with --fps (24, \"30000/1001\")")
        })?;
        input_args.extend([
            "-framerate".into(),
            format!("{}/{}", fps.0, fps.1),
            "-start_number".into(),
            start.clone(),
            "-i".into(),
            p.clone(),
        ]);
    } else {
        input_args.extend(["-i".into(), input.into()]);
    }
    let probe_out = Command::new(ffprobe_bin())
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_streams",
            "-of",
            "json",
        ])
        .args(match &pattern {
            Some(p) => vec![
                "-framerate".to_string(),
                "1".into(),
                "-start_number".into(),
                start.clone(),
                "-i".into(),
                p.clone(),
            ],
            None => vec![input.to_string()],
        })
        .stdin(Stdio::null())
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => anyhow!(
                "ffprobe not found (looked for \"{}\"). conform needs ffmpeg and ffprobe: \
                 winget install Gyan.FFmpeg / brew install ffmpeg / apt install ffmpeg \
                 (or set SHOTGLASS_FFMPEG and SHOTGLASS_FFPROBE)",
                ffprobe_bin().display()
            ),
            _ => anyhow!("couldn't run ffprobe: {e}"),
        })?;
    if !probe_out.status.success() {
        bail!(
            "{input}: ffprobe can't read it: {}",
            String::from_utf8_lossy(&probe_out.stderr).trim()
        );
    }
    let p = parse_probe(&String::from_utf8_lossy(&probe_out.stdout))
        .with_context(|| input.to_string())?;
    if matches!(p.transfer, Some(16 | 18)) {
        bail!("{input}: HDR isn't supported yet; convert to SDR BT.709 first");
    }
    // The output rate: given, else the stream's when constant, else its average.
    let fps = match (o.fps, p.rate, p.avg_rate) {
        (Some(f), _, _) => f,
        (None, Some(r), Some(a)) if r == a => r,
        (None, _, Some(a)) => {
            let s = crate::webm::demux::snap_fps(1e9 * a.1 as f64 / a.0 as f64, 2e5);
            println!(
                "{input}: variable frame rate; conforming to {} (frames repeat or drop onto that grid)",
                crate::webm::demux::show_fps(s)
            );
            s
        }
        (None, Some(r), None) => r,
        _ => bail!("{input}: no frame rate; give one with --fps"),
    };
    let src_depth =
        if p.pix_fmt.contains("10") || p.pix_fmt.contains("12") || p.pix_fmt.contains("16") {
            10
        } else {
            8
        };
    let depth = o.depth.unwrap_or(src_depth);
    let (mut w, mut h) = (p.width, p.height);
    if p.rotation.rem_euclid(180) == 90 {
        std::mem::swap(&mut w, &mut h);
    }
    let (w, h) = ((w / 2 * 2) as usize, (h / 2 * 2) as usize);
    if w == 0 || h == 0 {
        bail!("{input}: no picture size");
    }
    // Keep the source's colour when it's YUV and SVT-AV1 can tag it; else BT.709.
    let rgb_source = p.pix_fmt.starts_with("rgb")
        || p.pix_fmt.starts_with("bgr")
        || p.pix_fmt.starts_with("gbr")
        || p.pix_fmt.starts_with("pal")
        || p.pix_fmt.starts_with("gray")
        || p.pix_fmt.starts_with("ya")
        || p.pix_fmt.contains("rgba");
    let kept = ColourTags {
        primaries: p.primaries.unwrap_or(1),
        transfer: p.transfer.unwrap_or(1),
        matrix: p.matrix.unwrap_or(if p.height >= 720 { 1 } else { 6 }),
        full_range: p.full_range.unwrap_or(p.pix_fmt.starts_with("yuvj")),
    };
    let keep = !rgb_source && kept.svt_names().is_some();
    let colour = if keep { kept } else { ColourTags::BT709 };
    let fmt = if depth > 8 { "yuv420p10le" } else { "yuv420p" };
    let mut vf = format!("crop={w}:{h}:0:0");
    if !keep {
        vf += ",scale=out_color_matrix=bt709:out_range=tv:flags=accurate_rnd+full_chroma_int";
    } else if kept.full_range {
        vf += ",scale=out_range=pc";
    }
    let set = Av1Settings {
        width: w,
        height: h,
        fps,
        depth,
        rate: Rate::Quality(o.crf),
        speed: o.speed,
        keyint: o.keyint.unwrap_or_else(|| crate::webm::export::keyint(fps)),
        grain: 0,
        colour,
    };
    let mut child = Command::new(ffmpeg_bin())
        .args(&input_args)
        .args(["-map", "0:v:0", "-an", "-sn", "-dn"])
        .args([
            "-vf",
            &vf,
            "-fps_mode",
            "cfr",
            "-r",
            &format!("{}/{}", fps.0, fps.1),
        ])
        .args(["-pix_fmt", fmt, "-f", "rawvideo", "-loglevel", "error", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                anyhow!(
                    "ffmpeg not found (looked for \"{}\")",
                    ffmpeg_bin().display()
                )
            }
            _ => anyhow!("couldn't run ffmpeg: {e}"),
        })?;
    let mut stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let errs = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::BufReader::new(stderr).read_to_string(&mut s);
        s
    });
    let frame = crate::yuv::frame_bytes(w, h, depth);
    let sample = if depth > 8 { 2 } else { 1 };
    let mut count = 0u64;
    let mut failed = None;
    let frames = std::iter::from_fn(|| {
        let mut planar = vec![0u8; frame];
        match stdout.read_exact(&mut planar) {
            Ok(()) => {
                count += 1;
                Some(interleave_chroma_rows(&planar, w, h, sample))
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => None,
            Err(e) => {
                failed = Some(e);
                None
            }
        }
    });
    let tmp = out.with_extension("webm.part");
    let written = crate::webm::export::write_clip(&tmp, &set, frames);
    // Close our end first: if the encoder stopped early, ffmpeg may be blocked writing.
    drop(stdout);
    let status = child.wait()?;
    let log = errs.join().unwrap_or_default();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.context(format!("{input}: {}", log.trim())));
    }
    if let Some(e) = failed {
        let _ = std::fs::remove_file(&tmp);
        bail!("{input}: reading ffmpeg's frames: {e}");
    }
    if !status.success() || count == 0 {
        let _ = std::fs::remove_file(&tmp);
        bail!("{input}: ffmpeg failed: {}", log.trim());
    }
    std::fs::rename(&tmp, &out)?;
    println!(
        "{} → {}: {count} frames, {w}×{h} at {}, AV1 {depth}-bit, crf {}, keyframes every {} frames, {} {}{}",
        input,
        out.display(),
        crate::webm::demux::show_fps(fps),
        o.crf,
        set.keyint,
        if keep { "colour kept:" } else { "converted to" },
        match colour.matrix {
            1 => "BT.709",
            5 | 6 => "BT.601",
            7 => "SMPTE 240M",
            9 => "BT.2020",
            _ => "its matrix",
        },
        if colour.full_range {
            ", full range"
        } else {
            ", limited range"
        },
    );
    println!("(re-encoded: pixels differ slightly from the source)");
    Ok(out)
}

/// ffmpeg's planar I420 (Y, all of U, all of V) → our layout (Y, then each U row
/// followed by the matching V row).
fn interleave_chroma_rows(planar: &[u8], w: usize, h: usize, sample: usize) -> Vec<u8> {
    let y = w * h * sample;
    let row = w / 2 * sample;
    let rows = h / 2;
    let (u, v) = planar[y..].split_at(row * rows);
    let mut out = Vec::with_capacity(planar.len());
    out.extend_from_slice(&planar[..y]);
    for r in 0..rows {
        out.extend_from_slice(&u[r * row..(r + 1) * row]);
        out.extend_from_slice(&v[r * row..(r + 1) * row]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        assert_eq!(
            ffmpeg_pattern("frames/plate_####.jpg").unwrap(),
            "frames/plate_%04d.jpg"
        );
        assert_eq!(ffmpeg_pattern("a/m_##.png").unwrap(), "a/m_%02d.png");
        assert!(ffmpeg_pattern("clip.mp4").is_none());
    }

    #[test]
    fn probes() {
        let p = parse_probe(
            r#"{"streams":[{"codec_name":"h264","width":1920,"height":1080,
            "r_frame_rate":"30000/1001","avg_frame_rate":"30000/1001","pix_fmt":"yuv420p",
            "color_space":"bt709","color_range":"tv","color_transfer":"bt709",
            "color_primaries":"bt709"}]}"#,
        )
        .unwrap();
        assert_eq!((p.width, p.height), (1920, 1080));
        assert_eq!(p.rate, Some((30000, 1001)));
        assert_eq!((p.matrix, p.full_range), (Some(1), Some(false)));
        let phone = parse_probe(
            r#"{"streams":[{"codec_name":"hevc","width":1920,"height":1080,
            "r_frame_rate":"30/1","avg_frame_rate":"28743/959","pix_fmt":"yuv420p10le",
            "color_transfer":"arib-std-b67","side_data_list":[{"rotation":-90}]}]}"#,
        )
        .unwrap();
        assert_ne!(phone.rate, phone.avg_rate);
        assert_eq!(phone.transfer, Some(18));
        assert_eq!(phone.rotation, -90);
        assert!(parse_probe(r#"{"streams":[]}"#).is_err());
    }

    #[test]
    fn chroma_rows() {
        // 4×2, 8-bit: Y 8 bytes, U 2, V 2.
        let planar = [0, 1, 2, 3, 4, 5, 6, 7, 10, 11, 20, 21];
        assert_eq!(
            interleave_chroma_rows(&planar, 4, 2, 1),
            [0, 1, 2, 3, 4, 5, 6, 7, 10, 11, 20, 21]
        );
        // 4×4: U rows [10,11],[12,13]; V rows [20,21],[22,23].
        let mut planar: Vec<u8> = (0..16).collect();
        planar.extend([10, 11, 12, 13, 20, 21, 22, 23]);
        let out = interleave_chroma_rows(&planar, 4, 4, 1);
        assert_eq!(&out[16..], &[10, 11, 20, 21, 12, 13, 22, 23]);
    }
}
