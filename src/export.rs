//! `shotglass export`: render a movie offline to a video file (or a PNG sequence),
//! frame- and sample-accurate.
//!
//! Frame n is rendered at exactly `movie.frame_time(n)` at full resolution with
//! blocking compiles, so it is the same frame `still` renders at that time. Raw RGBA
//! frames are piped to an external `ffmpeg`; the soundtrack is written first as the
//! same 16-bit WAV `shotglass wav` writes, and ffmpeg muxes it: the user's ffmpeg
//! brings the codecs. WebM (and AV1 in Matroska) are the exception: with the `webm`
//! feature they're encoded and muxed in-process (`webm::export`, #116).

use crate::audio;
use crate::gfx::{Frame, FrameInfo, Renderer};
use crate::movie::Movie;
use crate::sound::Soundtrack;
use anyhow::{Context, Result, anyhow, bail};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

/// What to write.
#[derive(Clone, Debug, PartialEq)]
pub enum Format {
    /// AV1 (SVT-AV1, 10-bit) + Opus in WebM, built in; without the `webm` feature,
    /// VP9 + Opus through ffmpeg.
    Webm,
    /// AV1 + Opus (WebM or MKV, by the file extension): built in like `Webm`, or
    /// through ffmpeg without the `webm` feature.
    Av1,
    /// FFV1 (lossless RGB) + FLAC in Matroska: archival.
    Ffv1,
    /// A folder of PNGs plus `sound.wav`: lossless, for editors. Needs no ffmpeg.
    Png,
    /// Your own ffmpeg output arguments (codecs, quality, pixel format, …).
    Ffmpeg(Vec<String>),
}

impl Format {
    pub const NAMES: &str = "webm, av1, ffv1, png, ffmpeg";

    /// Encoded in-process (SVT-AV1 + libopus + our WebM writer), no ffmpeg.
    pub fn builtin(&self) -> bool {
        cfg!(feature = "webm") && matches!(self, Format::Webm | Format::Av1)
    }

    /// Runs the user's ffmpeg.
    pub fn needs_ffmpeg(&self) -> bool {
        !self.builtin() && *self != Format::Png
    }

    /// The Matroska DocType the built-in encoder writes to `out`.
    #[cfg_attr(not(feature = "webm"), allow(dead_code))]
    pub fn doc_type(&self, out: &Path) -> &'static str {
        let mkv = out
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("mkv"));
        if *self == Format::Av1 && mkv {
            "matroska"
        } else {
            "webm"
        }
    }

    /// From `--format`, or from the output's extension when there's no `--format`.
    pub fn choose(name: Option<&str>, out: &Path, ffmpeg_args: Option<&str>) -> Result<Format> {
        let args = || -> Result<Vec<String>> {
            let a = ffmpeg_args.ok_or_else(|| {
                anyhow!("--format ffmpeg needs --ffmpeg-args \"-c:v ... -c:a ...\"")
            })?;
            Ok(a.split_whitespace().map(String::from).collect())
        };
        if ffmpeg_args.is_some() && name.is_some_and(|n| n != "ffmpeg") {
            bail!("--ffmpeg-args only goes with --format ffmpeg");
        }
        if let Some(n) = name {
            return Ok(match n {
                "webm" | "vp9" => Format::Webm,
                "av1" => Format::Av1,
                "ffv1" | "mkv" => Format::Ffv1,
                "png" => Format::Png,
                "ffmpeg" => Format::Ffmpeg(args()?),
                _ => bail!("unknown --format \"{n}\" (one of: {})", Format::NAMES),
            });
        }
        if ffmpeg_args.is_some() {
            return Ok(Format::Ffmpeg(args()?));
        }
        let ext = out.extension().map(|e| e.to_string_lossy().to_lowercase());
        match ext.as_deref() {
            Some("webm") => Ok(Format::Webm),
            Some("mkv") => Ok(Format::Ffv1),
            None => Ok(Format::Png),
            Some(e) => bail!(
                "can't tell the format from \".{e}\": use .webm (AV1 + Opus), .mkv (FFV1 + FLAC), \
                 a folder name (PNG sequence + WAV), or --format ({})",
                Format::NAMES
            ),
        }
    }

    /// 4:2:0 YUV formats store colour at half resolution: the size must be even.
    pub fn check_size(&self, w: usize, h: usize) -> Result<()> {
        if matches!(self, Format::Webm | Format::Av1) && (w % 2 == 1 || h % 2 == 1) {
            bail!(
                "{w}x{h}: this format stores colour at half resolution, so width and height must be even"
            );
        }
        Ok(())
    }
}

/// The built-in AV1 encoder's settings (`--quality`, `--bitrate`, `--speed`,
/// `--depth`, `--grain`).
#[derive(Clone, Debug, PartialEq)]
pub struct Av1Options {
    /// SVT-AV1 CRF, 0..=63 (lower is better); ignored with `bitrate`.
    pub quality: u8,
    /// Target bitrate in kbit/s instead of constant quality.
    pub bitrate: Option<u32>,
    /// SVT-AV1 preset, 0..=13 (higher is faster).
    pub speed: u8,
    /// 8 or 10 bits per sample.
    pub depth: u8,
    /// Film-grain denoising and synthesis, 0 (off) ..= 50.
    pub grain: u8,
}

impl Default for Av1Options {
    fn default() -> Self {
        Av1Options {
            quality: 30,
            bitrate: None,
            speed: 6,
            depth: 10,
            grain: 0,
        }
    }
}

pub struct Options {
    pub out: PathBuf,
    pub w: usize,
    pub h: usize,
    pub format: Format,
    /// Audio sample rate.
    pub rate: u32,
    /// Periods of a `loop = true` film to write, one after another (1 otherwise).
    pub loops: u64,
    /// For the built-in encoder.
    #[cfg_attr(not(feature = "webm"), allow(dead_code))]
    pub av1: Av1Options,
}

/// What was written.
#[derive(Debug)]
pub struct Summary {
    pub frames: u64,
    /// Stereo sample frames (0 without sound).
    pub audio_frames: usize,
}

/// The ffmpeg to run: `SHOTGLASS_FFMPEG` or `ffmpeg` on the PATH.
pub fn ffmpeg_bin() -> PathBuf {
    std::env::var_os("SHOTGLASS_FFMPEG")
        .map(PathBuf::from)
        .unwrap_or_else(|| "ffmpeg".into())
}

/// ffmpeg's encoder list, or a helpful error if it isn't installed.
pub fn ffmpeg_encoders() -> Result<String> {
    let bin = ffmpeg_bin();
    let out = Command::new(&bin)
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => bail!("{} -encoders failed ({})", bin.display(), o.status),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
            "ffmpeg not found (looked for \"{}\").\n\
             Install it and make sure it's on your PATH, or set SHOTGLASS_FFMPEG to its full path:\n  \
             Windows: winget install Gyan.FFmpeg\n  \
             macOS:   brew install ffmpeg\n  \
             Linux:   sudo apt install ffmpeg (or your distribution's package)\n\
             A PNG sequence + WAV needs no ffmpeg: --format png",
            bin.display()
        ),
        Err(e) => Err(e).with_context(|| format!("couldn't run {}", bin.display())),
    }
}

fn has_encoder(list: &str, name: &str) -> bool {
    list.lines()
        .any(|l| l.split_whitespace().nth(1) == Some(name))
}

/// BT.709 limited range: convert with the 709 matrix (swscale defaults to 601) and
/// tag the stream so players don't shift colours.
const YUV709: &[&str] = &[
    "-vf",
    "scale=out_color_matrix=bt709:out_range=tv:flags=accurate_rnd+full_chroma_int,format=yuv420p",
    "-colorspace",
    "bt709",
    "-color_primaries",
    "bt709",
    "-color_trc",
    "bt709",
    "-color_range",
    "tv",
];

/// ffmpeg output arguments (codecs and colour tags) for a format.
fn codec_args(format: &Format, encoders: &str, sound: bool) -> Result<Vec<String>> {
    let s = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let need = |name: &str| -> Result<()> {
        if has_encoder(encoders, name) {
            Ok(())
        } else {
            bail!(
                "this ffmpeg has no {name} encoder; install a full build, or pick another --format"
            )
        }
    };
    let mut v = vec![];
    match format {
        Format::Webm => {
            need("libvpx-vp9")?;
            v.extend(s(&[
                "-c:v",
                "libvpx-vp9",
                "-crf",
                "20",
                "-b:v",
                "0",
                "-row-mt",
                "1",
                "-deadline",
                "good",
                "-cpu-used",
                "2",
            ]));
            v.extend(s(YUV709));
        }
        Format::Av1 => {
            if has_encoder(encoders, "libsvtav1") {
                v.extend(s(&["-c:v", "libsvtav1", "-crf", "30", "-preset", "6"]));
            } else {
                need("libaom-av1")?;
                v.extend(s(&[
                    "-c:v",
                    "libaom-av1",
                    "-crf",
                    "30",
                    "-b:v",
                    "0",
                    "-cpu-used",
                    "6",
                    "-row-mt",
                    "1",
                ]));
            }
            v.extend(s(YUV709));
        }
        Format::Ffv1 => {
            need("ffv1")?;
            // Lossless RGB: full range, no matrix (tagged as RGB), BT.709 primaries.
            v.extend(s(&[
                "-c:v",
                "ffv1",
                "-level",
                "3",
                "-g",
                "1",
                "-slicecrc",
                "1",
                "-pix_fmt",
                "bgr0",
                "-colorspace",
                "rgb",
                "-color_primaries",
                "bt709",
                "-color_trc",
                "bt709",
                "-color_range",
                "pc",
            ]));
        }
        Format::Ffmpeg(args) => return Ok(args.clone()),
        Format::Png => unreachable!("PNG sequences don't use ffmpeg"),
    }
    if sound {
        match format {
            Format::Ffv1 => v.extend(s(&["-c:a", "flac"])),
            _ => {
                need("libopus")?;
                // The muxer records Opus's encoder delay (pre-skip / CodecDelay), so
                // players drop it and sync stays exact.
                v.extend(s(&["-c:a", "libopus", "-b:a", "192k"]));
            }
        }
    }
    Ok(v)
}

/// Synthesise the soundtrack into a 16-bit WAV, exactly as `shotglass wav` does.
/// Returns the number of stereo frames: `ceil(duration × rate)` from sample 0, or for
/// a loop film `loops` periods (`Movie::sound_samples` each). A loop film's mix reads
/// across the seam, so the file itself loops seamlessly.
/// `m`'s soundtrack, ready to synthesise at `rate`.
pub fn soundtrack(r: &mut Renderer, m: &Movie, rate: u32) -> Result<Soundtrack> {
    let mut st = Soundtrack::new();
    st.set_movie(r, m, rate)
        .map_err(|errors| anyhow!("{}", crate::gfx::join_errors(&errors)))?;
    Ok(st)
}

pub fn write_wav(r: &mut Renderer, m: &Movie, rate: u32, out: &Path, loops: u64) -> Result<usize> {
    let mut st = soundtrack(r, m, rate)?;
    let loops = loops.max(1) as usize;
    let frames = m.sound_samples(rate) * loops;
    let mut w = audio::WavWriter::create(out, frames, rate)
        .with_context(|| format!("couldn't create {}", out.display()))?;
    let mut err = None;
    // Every period is the same; synthesising it again keeps memory bounded.
    for _ in 0..loops {
        st.render_all(r, |d| {
            if let Err(e) = w.write(d) {
                err.get_or_insert(e);
            }
        })
        .map_err(|e| anyhow!(e))?;
    }
    if let Some(e) = err {
        return Err(e.into());
    }
    w.finish()?;
    Ok(frames)
}

/// Where finished frames go, on their own thread so encoding overlaps rendering.
enum Sink {
    Ffmpeg(std::process::ChildStdin),
    Png(PathBuf),
}

impl Sink {
    fn write(&mut self, n: u64, frame: &Frame) -> Result<()> {
        match self {
            Sink::Ffmpeg(stdin) => stdin
                .write_all(frame.data().as_flattened())
                .context("ffmpeg stopped reading frames"),
            Sink::Png(dir) => crate::save_png(&dir.join(format!("frame_{n:06}.png")), frame),
        }
    }
}

/// Render `m` to `opts.out`. `progress(done, total)` is called after each frame;
/// a cancelled video export discards its staged files.
pub fn export(
    r: &mut Renderer,
    m: &Movie,
    opts: &Options,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<Summary> {
    let (w, h) = (opts.w, opts.h);
    opts.format.check_size(w, h)?;
    if opts.loops > 1 && m.loop_frames.is_none() {
        bail!("--loops needs a film with loop = true");
    }
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    #[cfg(feature = "webm")]
    if opts.format.builtin() {
        let doc = opts.format.doc_type(&opts.out);
        return crate::webm::export::export(r, m, opts, &opts.av1, doc, cancel, progress);
    }
    let png = opts.format == Format::Png;
    let encoders = if png {
        String::new()
    } else {
        ffmpeg_encoders()?
    };
    let sound = m.has_sound();
    let args = if png {
        vec![]
    } else {
        codec_args(&opts.format, &encoders, sound)?
    };
    let staged = if png {
        std::fs::create_dir_all(&opts.out)
            .with_context(|| format!("couldn't create {}", opts.out.display()))?;
        None
    } else {
        let dir = opts
            .out
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let name = opts
            .out
            .file_name()
            .context("expected an output file name")?;
        std::fs::create_dir_all(dir)?;
        // Keep the output name for FFmpeg's muxer selection and stage beside the
        // destination so publishing only needs a rename.
        let temp = tempfile::Builder::new()
            .prefix(".shotglass-export-")
            .tempdir_in(dir)?;
        let video_dir = temp.path().join("video");
        std::fs::create_dir(&video_dir)?;
        Some((temp, video_dir.join(name)))
    };

    // Sound first: it's quick, and ffmpeg needs the whole file as its second input.
    let mut audio_frames = 0;
    let wav = staged
        .as_ref()
        .map_or(opts.out.as_path(), |(temp, _)| temp.path())
        .join("sound.wav");
    if sound {
        audio_frames = write_wav(r, m, opts.rate, &wav, opts.loops)?;
    }
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    let total = m.frame_count() * opts.loops.max(1);
    let mut child_guard = None;
    let sink = if let Some((_, video)) = &staged {
        let mut cmd = Command::new(ffmpeg_bin());
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
        cmd.args(["-f", "rawvideo", "-pix_fmt", "rgba"]);
        cmd.args(["-video_size", &format!("{w}x{h}")]);
        cmd.args(["-framerate", &format!("{}/{}", m.fps_num, m.fps_den)]);
        cmd.args(["-i", "pipe:0"]);
        if sound {
            cmd.arg("-i").arg(&wav);
            cmd.args(["-map", "0:v:0", "-map", "1:a:0"]);
        }
        cmd.args(&args);
        cmd.arg(video);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // Ctrl-C is ours to handle: keep the terminal's interrupt away from ffmpeg so
        // it doesn't quit half-way while we're still writing.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0000_0200); // CREATE_NEW_PROCESS_GROUP
        let mut child = cmd
            .spawn()
            .with_context(|| format!("couldn't start {}", ffmpeg_bin().display()))?;
        let stdin = child.stdin.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let log = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = std::io::BufReader::new(stderr).read_to_string(&mut s);
            s
        });
        child_guard = Some(KillOnDrop(Some(child), Some(log)));
        Sink::Ffmpeg(stdin)
    } else {
        Sink::Png(opts.out.clone())
    };

    // Two frames in flight: one being read back while the writer encodes the other.
    // The buffers go round and round; nothing is allocated per frame.
    let (to_writer, from_main) = mpsc::sync_channel::<(u64, Frame)>(1);
    let (to_main, spare) = mpsc::channel::<Frame>();
    let writer = std::thread::spawn(move || -> Result<()> {
        let mut sink = sink;
        for (n, frame) in from_main {
            sink.write(n, &frame)?;
            if to_main.send(frame).is_err() {
                break;
            }
        }
        Ok(())
    });
    let mut allocated = 0;
    let mut failed = None;
    for n in 0..total {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let t = m.frame_time(n);
        if r.render(m, w as i32, h as i32, &FrameInfo::frame(m, n), true)
            .is_none()
        {
            failed = Some(anyhow!("frame {n} ({t:.3}s) didn't render"));
            break;
        }
        if let Some(e) = r.footage_failure() {
            failed = Some(anyhow!("frame {n} ({t:.3}s): {e}"));
            break;
        }
        // Export reads every frame back anyway, so a once-a-second check costs nothing.
        if r.context_lost(true, false) {
            failed = Some(anyhow!(
                "the GPU driver reset at frame {n} ({t:.3}s); a shader probably ran too long"
            ));
            break;
        }
        let mut buf = match spare.try_recv() {
            Ok(b) => b,
            Err(_) if allocated < 2 => {
                allocated += 1;
                Frame::init(w, h, [0; 4])
            }
            Err(_) => match spare.recv() {
                Ok(b) => b,
                Err(_) => break, // the writer failed; its error is reported below
            },
        };
        r.read_output_into(&mut buf, (0, 0));
        if to_writer.send((n, buf)).is_err() {
            break;
        }
        progress(n + 1, total);
    }
    drop(to_writer);
    if (cancel.load(Ordering::Relaxed) || failed.is_some())
        && let Some(KillOnDrop(Some(child), _)) = &mut child_guard
    {
        // Unblock the writer if FFmpeg is still processing queued frames.
        let _ = child.kill();
    }
    let written = writer
        .join()
        .map_err(|_| anyhow!("the frame writer panicked"))?;

    if let Some(mut g) = child_guard {
        let (status, log) = g.wait(cancel, failed.is_some())?;
        if !cancel.load(Ordering::Relaxed) && failed.is_none() && !status.success() {
            bail!(
                "ffmpeg failed ({status}):\n{}",
                log.trim_end()
                    .lines()
                    .rev()
                    .take(20)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    if let Some(e) = failed {
        return Err(e);
    }
    written?;
    if let Some((_temp, video)) = staged {
        std::fs::rename(&video, &opts.out)
            .with_context(|| format!("couldn't publish {}", opts.out.display()))?;
    }
    Ok(Summary {
        frames: total,
        audio_frames,
    })
}

/// Render frames `frames` (frame n at `frame_time(n)`, as `still`) through the YUV
/// pass, pipelined: frame n+1 renders while earlier frames are read back through the
/// reader's ring, collected oldest first. `sink` gets every frame in order. The only
/// waits on the GPU are back-pressure (the ring is full) and the final drain.
pub fn render_yuv(
    r: &mut Renderer,
    m: &Movie,
    (w, h): (usize, usize),
    frames: std::ops::Range<u64>,
    reader: &mut crate::yuv::YuvReader,
    cancel: &AtomicBool,
    mut sink: impl FnMut(&mut crate::yuv::YuvReader, crate::yuv::YuvFrame) -> Result<()>,
) -> Result<()> {
    for n in frames {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let t = m.frame_time(n);
        let Some(tex) = r.render(m, w as i32, h as i32, &FrameInfo::frame(m, n), true) else {
            bail!("frame {n} ({t:.3}s) didn't render");
        };
        if let Some(e) = r.footage_failure() {
            bail!("frame {n} ({t:.3}s): {e}");
        }
        if r.context_lost(true, false) {
            bail!("the GPU driver reset at frame {n} ({t:.3}s); a shader probably ran too long");
        }
        while !reader.has_room() {
            let f = reader.collect(true).expect("a full ring has a frame");
            sink(reader, f)?;
        }
        reader.submit(r, tex, n)?;
        while let Some(f) = reader.collect(false) {
            sink(reader, f)?;
        }
    }
    while let Some(f) = reader.collect(true) {
        sink(reader, f)?;
    }
    Ok(())
}

/// The ffmpeg process and its stderr reader; killed if the export bails early.
struct KillOnDrop(Option<Child>, Option<std::thread::JoinHandle<String>>);

impl KillOnDrop {
    /// Wait for ffmpeg with stdin closed, checking for cancellation during muxing.
    fn wait(
        &mut self,
        cancel: &AtomicBool,
        abort: bool,
    ) -> Result<(std::process::ExitStatus, String)> {
        let child = self.0.as_mut().unwrap();
        let status = loop {
            if abort || cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                break child.wait()?;
            }
            if let Some(status) = child.try_wait()? {
                break status;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        self.0.take();
        let log = self
            .1
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        Ok((status, log))
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_from_flag_or_extension() {
        let f = |name: Option<&str>, out: &str| Format::choose(name, Path::new(out), None);
        assert_eq!(f(None, "film.webm").unwrap(), Format::Webm);
        assert_eq!(f(None, "film.MKV").unwrap(), Format::Ffv1);
        assert_eq!(f(None, "frames").unwrap(), Format::Png);
        assert_eq!(f(Some("av1"), "film.webm").unwrap(), Format::Av1);
        assert_eq!(f(Some("png"), "x.webm").unwrap(), Format::Png);
        assert!(f(None, "film.mp4").is_err());
        assert!(f(Some("h264"), "film.mp4").is_err());
        assert!(f(Some("ffmpeg"), "film.mp4").is_err()); // needs --ffmpeg-args
        assert_eq!(
            Format::choose(
                None,
                Path::new("f.mov"),
                Some(" -c:v prores  -c:a pcm_s16le ")
            )
            .unwrap(),
            Format::Ffmpeg(vec![
                "-c:v".into(),
                "prores".into(),
                "-c:a".into(),
                "pcm_s16le".into()
            ])
        );
        assert!(Format::choose(Some("webm"), Path::new("f.webm"), Some("-c:v x")).is_err());
    }

    #[test]
    fn builtin_formats_and_doc_types() {
        let built = cfg!(feature = "webm");
        assert_eq!(Format::Webm.builtin(), built);
        assert_eq!(Format::Av1.builtin(), built);
        assert!(!Format::Ffv1.builtin() && Format::Ffv1.needs_ffmpeg());
        assert!(!Format::Png.needs_ffmpeg());
        assert_eq!(Format::Webm.needs_ffmpeg(), !built);
        assert_eq!(Format::Av1.doc_type(Path::new("a.MKV")), "matroska");
        assert_eq!(Format::Av1.doc_type(Path::new("a.webm")), "webm");
        assert_eq!(Format::Webm.doc_type(Path::new("a.mkv")), "webm");
    }

    #[test]
    fn codec_args_need_the_encoders() {
        let list = " V....D libvpx-vp9  libvpx VP9\n A....D libopus  libopus Opus\n V.S..D ffv1 FFmpeg video codec #1\n A....D flac FLAC\n";
        // Without the webm feature, WebM falls back to ffmpeg's VP9.
        let webm = codec_args(&Format::Webm, list, true).unwrap().join(" ");
        assert!(webm.contains("libvpx-vp9") && webm.contains("libopus"));
        assert!(webm.contains("out_color_matrix=bt709") && webm.contains("-color_range tv"));
        assert!(
            !codec_args(&Format::Webm, list, false)
                .unwrap()
                .join(" ")
                .contains("-c:a")
        );
        let ffv1 = codec_args(&Format::Ffv1, list, true).unwrap().join(" ");
        assert!(ffv1.contains("ffv1") && ffv1.contains("-c:a flac"));
        let err = codec_args(&Format::Av1, list, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("libaom-av1"), "{err}");
        assert!(!has_encoder(list, "vp9"));
    }
}
