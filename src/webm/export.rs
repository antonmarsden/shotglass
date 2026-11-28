//! `export --format webm` (and `av1`) without ffmpeg: the soundtrack synthesised at
//! 48 kHz and encoded to Opus first, then frames rendered, converted to YUV on the
//! GPU and read back pipelined (`export::render_yuv`), encoded by SVT-AV1 on a
//! thread of their own (a bounded channel caps the frames in flight) and muxed by
//! `mux::Writer` as packets arrive.

use super::encode::{Av1Encoder, Av1Settings, OPUS_FRAME, OpusPackets, Rate};
use super::{av1, mux};
use crate::export::{Av1Options, Options, Summary};
use crate::gfx::Renderer;
use crate::movie::Movie;
use crate::yuv::YuvReader;
use anyhow::{Context, Result, anyhow, bail};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

/// Opus bitrate (stereo).
pub const OPUS_KBPS: u32 = 160;
/// Frames in flight between the GPU and the encoder: the readback ring and the channel.
const RING: usize = 4;
const QUEUE: usize = 4;

/// Frame `n`'s presentation time in ns, exactly (rational frame rate).
fn frame_ns((num, den): (u32, u32), n: u64) -> u64 {
    (u128::from(n) * 1_000_000_000 * u128::from(den) / u128::from(num)) as u64
}

/// Encode I420 frames (as `yuv::frame_bytes` lays them out) into a video-only WebM:
/// test fixtures for footage (#73), and what `shotglass conform` writes.
#[cfg_attr(not(test), allow(dead_code))] // until `shotglass conform`
pub fn write_clip(
    path: &Path,
    set: &Av1Settings,
    frames: impl IntoIterator<Item = Vec<u8>>,
) -> Result<()> {
    let mut enc = Av1Encoder::new(set)?;
    let (seq, seq_obu) = av1::find_sequence_header(enc.header())?;
    let video = mux::Video {
        width: set.width as u32,
        height: set.height as u32,
        codec_private: av1::av1c(&seq, &seq_obu),
        bit_depth: set.depth,
        fps: set.fps,
        colour: set.colour,
    };
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut mux = mux::Writer::new(file, "webm", &video, None)?;
    let mut next = 0i64;
    let mut write = |mux: &mut mux::Writer<_>, p: super::encode::Packet| -> Result<()> {
        if p.pts != next {
            bail!(
                "the encoder returned frame {} when frame {next} was due",
                p.pts
            );
        }
        next += 1;
        let tu = av1::strip_temporal_delimiters(&p.data)?;
        mux.video(frame_ns(set.fps, p.pts as u64), p.keyframe, &tu)?;
        Ok(())
    };
    for (n, f) in frames.into_iter().enumerate() {
        enc.send(&f, n as i64)?;
        while let Some(p) = enc.receive()? {
            write(&mut mux, p)?;
        }
    }
    enc.finish()?;
    while let Some(p) = enc.receive()? {
        write(&mut mux, p)?;
    }
    mux.finish(frame_ns(set.fps, next as u64))?;
    Ok(())
}

/// Frames between keyframes: about two seconds, so browsers seek quickly.
pub fn keyint((num, den): (u32, u32)) -> u32 {
    ((2.0 * f64::from(num) / f64::from(den)).round() as u32).max(1)
}

pub fn settings(m: &Movie, w: usize, h: usize, o: &Av1Options) -> Av1Settings {
    Av1Settings {
        width: w,
        height: h,
        fps: (m.fps_num, m.fps_den),
        depth: o.depth,
        rate: match o.bitrate {
            Some(k) => Rate::Bitrate(k),
            None => Rate::Quality(o.quality),
        },
        speed: o.speed,
        keyint: o.keyint.unwrap_or_else(|| keyint((m.fps_num, m.fps_den))),
        grain: o.grain,
        colour: super::ColourTags::BT709,
    }
}

/// The soundtrack as Opus: `loops` periods (one for a plain film) at 48 kHz.
fn encode_sound(r: &mut Renderer, m: &Movie, loops: u64) -> Result<OpusPackets> {
    let mut st = crate::export::soundtrack(r, m, 48_000)?;
    let per = m.sound_samples(48_000);
    let mut opus = OpusPackets::new(OPUS_KBPS)?;
    let mut err = None;
    for _ in 0..loops.max(1) {
        let mut left = per;
        st.render_all(r, |d| {
            let k = (d.len() / 2).min(left);
            left -= k;
            if k > 0
                && err.is_none()
                && let Err(e) = opus.push(&d[..k * 2])
            {
                err = Some(e);
            }
        })
        .map_err(|e| anyhow!(e))?;
        if left > 0 {
            opus.push(&vec![0.0; left * 2])?;
        }
    }
    if let Some(e) = err {
        return Err(e);
    }
    opus.finish()?;
    Ok(opus)
}

/// What the encoder thread is sent.
struct Job {
    n: u64,
    data: Vec<u8>,
}

/// Render `m` to `opts.out` (staged beside it, renamed at the end).
pub fn export(
    r: &mut Renderer,
    m: &Movie,
    opts: &Options,
    av1o: &Av1Options,
    doc_type: &'static str,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<Summary> {
    let (w, h) = (opts.w, opts.h);
    let loops = opts.loops.max(1);
    let dir = opts
        .out
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut staged = tempfile::Builder::new()
        .prefix(".shotglass-export-")
        .suffix(".part")
        .tempfile_in(dir)?;

    let sound = if m.has_sound() {
        Some(encode_sound(r, m, loops)?)
    } else {
        None
    };
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }

    let set = settings(m, w, h, av1o);
    let enc = Av1Encoder::new(&set)?;
    let (seq, seq_obu) = av1::find_sequence_header(enc.header())?;
    let video = mux::Video {
        width: w as u32,
        height: h as u32,
        codec_private: av1::av1c(&seq, &seq_obu),
        bit_depth: av1o.depth,
        fps: (m.fps_num, m.fps_den),
        colour: set.colour,
    };
    let audio_frames = sound.as_ref().map_or(0, |s| s.samples);
    let audio = sound.map(|s| {
        let track = mux::Audio {
            channels: 2,
            pre_skip: s.pre_skip,
        };
        let last = s.packets.len().saturating_sub(1);
        // Rounded to the nearest ns, so demuxers converting back to samples get exactly
        // `padding` whichever way they round.
        let pad_ns = ((s.padding as u64 * 1_000_000_000 + 24_000) / 48_000) as i64;
        let packets = s
            .packets
            .into_iter()
            .enumerate()
            .map(|(k, data)| mux::AudioPacket {
                pts_ns: k as u64 * OPUS_FRAME as u64 * 1_000_000_000 / 48_000,
                data,
                discard_padding_ns: if k == last { pad_ns } else { 0 },
            })
            .collect::<Vec<_>>();
        (track, packets)
    });

    let total = m.frame_count() * loops;
    let duration_ns = frame_ns((m.fps_num, m.fps_den), total);
    let file = staged.as_file().try_clone()?;
    let fps = (m.fps_num, m.fps_den);
    // Set when rendering stops early (cancelled or failed): the encoder thread then
    // drops what it has instead of flushing SVT-AV1 and muxing a file nobody keeps.
    let abandon = Arc::new(AtomicBool::new(false));
    let abandoned = abandon.clone();
    let (to_encoder, jobs) = mpsc::sync_channel::<Job>(QUEUE);
    let (to_main, spare) = mpsc::channel::<Vec<u8>>();
    let encoder = std::thread::Builder::new()
        .name("av1 encoder".into())
        .spawn(move || -> Result<()> {
            let mut enc = enc;
            let (track, packets) = audio.unzip();
            let mut mux = mux::Writer::new(
                std::io::BufWriter::new(file),
                doc_type,
                &video,
                track.as_ref().zip(packets),
            )?;
            let ns = |n: i64| frame_ns(fps, n.max(0) as u64);
            let mut next = 0i64;
            let mut write = |mux: &mut mux::Writer<_>, p: super::encode::Packet| -> Result<()> {
                if p.pts != next {
                    bail!(
                        "the encoder returned frame {} when frame {next} was due",
                        p.pts
                    );
                }
                next += 1;
                let tu = av1::strip_temporal_delimiters(&p.data)?;
                mux.video(ns(p.pts), p.keyframe, &tu)?;
                Ok(())
            };
            for job in jobs {
                if abandoned.load(Ordering::Relaxed) {
                    return Ok(());
                }
                enc.send(&job.data, job.n as i64)?;
                let _ = to_main.send(job.data);
                while let Some(p) = enc.receive()? {
                    write(&mut mux, p)?;
                }
            }
            if abandoned.load(Ordering::Relaxed) {
                return Ok(());
            }
            enc.finish()?;
            while let Some(p) = enc.receive()? {
                write(&mut mux, p)?;
            }
            let mut out = mux.finish(duration_ns)?;
            std::io::Write::flush(&mut out)?;
            Ok(())
        })?;

    let mut reader = YuvReader::new(r, w, h, av1o.depth, RING)?;
    let mut sent_all = true;
    let rendered =
        crate::export::render_yuv(r, m, (w, h), 0..total, &mut reader, cancel, |rd, f| {
            while let Ok(b) = spare.try_recv() {
                rd.recycle(b);
            }
            let n = f.n;
            if to_encoder.send(Job { n, data: f.data }).is_err() {
                sent_all = false;
                bail!("the encoder stopped");
            }
            progress(n + 1, total);
            Ok(())
        });
    if rendered.is_err() {
        abandon.store(true, Ordering::Relaxed);
    }
    drop(to_encoder);
    let encoded = encoder
        .join()
        .map_err(|_| anyhow!("the encoder thread panicked"))?;
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    // The encoder's own error explains a refused send better than "stopped".
    if !sent_all {
        encoded?;
        rendered?;
    } else {
        rendered?;
        encoded?;
    }
    staged.as_file_mut().sync_all().ok();
    staged
        .persist(&opts.out)
        .map_err(|e| e.error)
        .with_context(|| format!("couldn't publish {}", opts.out.display()))?;
    Ok(Summary {
        frames: total,
        audio_frames,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframes_about_every_two_seconds() {
        assert_eq!(keyint((30000, 1001)), 60);
        assert_eq!(keyint((24, 1)), 48);
        assert_eq!(keyint((1, 10)), 1);
        assert_eq!(frame_ns((30000, 1001), 30000), 1001 * 1_000_000_000);
        assert_eq!(frame_ns((24, 1), 3), 125_000_000);
    }
}
