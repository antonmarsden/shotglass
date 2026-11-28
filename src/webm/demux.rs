//! Reading WebM / Matroska video for footage (#73): the first video track's settings
//! and an index of every frame (file offset, size, keyframe, timestamp), built once
//! by walking the Clusters' block headers (payloads are skipped). With the index,
//! frame `k` is one read, and the keyframe at or before it is a lookup, so Cues
//! aren't needed. Only constant-rate clips are accepted: frame `k`'s timestamp must
//! be within a quarter of a frame of `k · den / num` seconds after the first.

use super::mux::{id, read};
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// Colour as Matroska's Colour element records it (ISO/IEC 23091-4 code points;
/// None = not written).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Colour {
    pub matrix: Option<u64>,
    /// 1 = limited (broadcast), 2 = full.
    pub range: Option<u64>,
    pub transfer: Option<u64>,
    pub primaries: Option<u64>,
    pub bits: Option<u64>,
    /// 1 = left (co-sited), 2 = half.
    pub siting_horz: Option<u64>,
    pub siting_vert: Option<u64>,
}

/// The video track.
#[derive(Clone, Debug)]
pub struct Track {
    pub number: u64,
    /// `V_AV1`, `V_VP9`, …
    pub codec: String,
    pub codec_private: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub colour: Colour,
}

/// One frame (a block of the video track) in presentation order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Offset of the frame's data in the file.
    pub offset: u64,
    pub size: u32,
    pub key: bool,
    /// Presentation time in ns from the first frame.
    pub pts_ns: u64,
}

/// A clip: its track, rate and frame index.
#[derive(Clone, Debug)]
pub struct Clip {
    pub track: Track,
    /// Frames per second as a fraction (reduced).
    pub fps: (u32, u32),
    pub frames: Vec<Frame>,
}

impl Clip {
    /// The keyframe at or before frame `k` (frame 0 is always one).
    pub fn keyframe_at_or_before(&self, k: usize) -> usize {
        (0..=k.min(self.frames.len().saturating_sub(1)))
            .rev()
            .find(|&i| self.frames[i].key)
            .unwrap_or(0)
    }

    /// The longest distance between keyframes, in frames (the end counts as one).
    pub fn max_gop(&self) -> usize {
        let mut last = 0;
        let mut most = 0;
        for (i, f) in self.frames.iter().enumerate().skip(1) {
            if f.key {
                most = most.max(i - last);
                last = i;
            }
        }
        most.max(self.frames.len() - last)
    }

    /// Frame `k`'s data, read from `file`.
    pub fn read_frame<R: Read + Seek>(&self, file: &mut R, k: usize) -> Result<Vec<u8>> {
        let f = self.frames[k];
        file.seek(SeekFrom::Start(f.offset))?;
        let mut buf = vec![0; f.size as usize];
        file.read_exact(&mut buf)
            .with_context(|| format!("frame {k} is cut short (the file was truncated?)"))?;
        Ok(buf)
    }
}

/// Read a clip's headers and index its frames.
pub fn open(path: &Path) -> Result<Clip> {
    let file = File::open(path)?;
    parse(BufReader::new(file))
}

/// An element header: id, size (None = unknown), and where its data starts.
struct Header {
    id: u32,
    size: Option<u64>,
    data: u64,
}

fn read_vint<R: Read>(r: &mut R, is_id: bool) -> Result<(u64, usize, bool)> {
    let mut first = [0u8];
    r.read_exact(&mut first)?;
    let len = first[0].leading_zeros() as usize + 1;
    if len > 8 {
        bail!("bad EBML number");
    }
    let mut v = if is_id {
        u64::from(first[0])
    } else {
        u64::from(first[0]) & ((1 << (8 - len)) - 1)
    };
    // A size whose value bits are all ones is "unknown".
    let mut all_ones = v == (1 << (8 - len)) - 1;
    let mut rest = [0u8; 7];
    r.read_exact(&mut rest[..len - 1])?;
    for &b in &rest[..len - 1] {
        v = (v << 8) | u64::from(b);
        all_ones &= b == 0xff;
    }
    Ok((v, len, !is_id && all_ones))
}

fn header<R: Read + Seek>(r: &mut R) -> Result<Header> {
    let (id, _, _) = read_vint(r, true)?;
    let (size, _, unknown) = read_vint(r, false)?;
    let data = r.stream_position()?;
    Ok(Header {
        id: id as u32,
        size: (!unknown).then_some(size),
        data,
    })
}

fn body<R: Read>(r: &mut R, size: u64, what: &str) -> Result<Vec<u8>> {
    if size > 64 << 20 {
        bail!("{what} is {size} bytes (too big to be real)");
    }
    let mut v = vec![0; size as usize];
    r.read_exact(&mut v)
        .with_context(|| format!("{what} is cut short (the file was truncated?)"))?;
    Ok(v)
}

fn sized(h: &Header, what: &str) -> Result<u64> {
    h.size.ok_or_else(|| {
        anyhow::anyhow!(
            "{what} has an unknown size (a live stream?); shotglass needs a finished file"
        )
    })
}

/// A Block's header: track, relative timestamp, flags; returns the header length.
fn block_header(b: &[u8]) -> Result<(u64, i16, u8, usize)> {
    let (track, n) = read::vint(b, false)?;
    if b.len() < n + 3 {
        bail!("a block is cut short");
    }
    let rel = i16::from_be_bytes([b[n], b[n + 1]]);
    Ok((track, rel, b[n + 2], n + 3))
}

fn parse<R: Read + Seek>(mut r: R) -> Result<Clip> {
    let end = r.seek(SeekFrom::End(0))?;
    r.seek(SeekFrom::Start(0))?;
    let ebml = header(&mut r).context("not a WebM file")?;
    if ebml.id != id::EBML {
        bail!("not a WebM file (no EBML header)");
    }
    let head = body(&mut r, sized(&ebml, "the EBML header")?, "the EBML header")?;
    let kids = read::children(&head, 0)?;
    let doc = read::find(&kids, id::DOC_TYPE)
        .map(|e| {
            String::from_utf8_lossy(e.data)
                .trim_end_matches('\0')
                .to_string()
        })
        .unwrap_or_default();
    if doc != "webm" && doc != "matroska" {
        bail!("not a WebM file (document type \"{doc}\")");
    }
    let seg = header(&mut r)?;
    if seg.id != id::SEGMENT {
        bail!("not a WebM file (no Segment)");
    }
    let seg_end = seg.size.map_or(end, |s| (seg.data + s).min(end));

    let mut scale = 1_000_000u64;
    let mut track: Option<Track> = None;
    let mut default_duration = None;
    // (cluster timestamp + block's, offset, size, key) in file order.
    let mut blocks: Vec<(i64, u64, u32, bool)> = vec![];
    let mut pos = seg.data;
    while pos < seg_end {
        r.seek(SeekFrom::Start(pos))?;
        let Ok(h) = header(&mut r) else { break };
        match h.id {
            id::INFO => {
                let b = body(&mut r, sized(&h, "Info")?, "Info")?;
                if let Some(e) = read::find(&read::children(&b, 0)?, id::TIMESTAMP_SCALE) {
                    scale = read::uint(e.data).max(1);
                }
            }
            id::TRACKS => {
                let b = body(&mut r, sized(&h, "Tracks")?, "Tracks")?;
                for t in read::children(&b, 0)? {
                    if t.id != id::TRACK_ENTRY || track.is_some() {
                        continue;
                    }
                    let k = read::children(t.data, 0)?;
                    let uint = |i| read::find(&k, i).map(|e| read::uint(e.data));
                    if uint(id::TRACK_TYPE) != Some(1) {
                        continue;
                    }
                    let video = read::find(&k, id::VIDEO)
                        .map(|v| read::children(v.data, 0))
                        .transpose()?
                        .unwrap_or_default();
                    let vuint = |i| read::find(&video, i).map(|e| read::uint(e.data));
                    let colour = read::find(&video, id::COLOUR)
                        .map(|c| read::children(c.data, 0))
                        .transpose()?
                        .unwrap_or_default();
                    let cuint = |i| read::find(&colour, i).map(|e| read::uint(e.data));
                    default_duration = uint(id::DEFAULT_DURATION);
                    track = Some(Track {
                        number: uint(id::TRACK_NUMBER).unwrap_or(1),
                        codec: read::find(&k, id::CODEC_ID)
                            .map(|e| String::from_utf8_lossy(e.data).to_string())
                            .unwrap_or_default(),
                        codec_private: read::find(&k, id::CODEC_PRIVATE)
                            .map(|e| e.data.to_vec())
                            .unwrap_or_default(),
                        width: vuint(id::PIXEL_WIDTH).unwrap_or(0) as u32,
                        height: vuint(id::PIXEL_HEIGHT).unwrap_or(0) as u32,
                        colour: Colour {
                            matrix: cuint(id::MATRIX_COEFFICIENTS),
                            range: cuint(id::RANGE),
                            transfer: cuint(id::TRANSFER_CHARACTERISTICS),
                            primaries: cuint(id::PRIMARIES),
                            bits: cuint(id::BITS_PER_CHANNEL),
                            siting_horz: cuint(id::CHROMA_SITING_HORZ),
                            siting_vert: cuint(id::CHROMA_SITING_VERT),
                        },
                    });
                }
            }
            id::CLUSTER => {
                let Some(t) = &track else {
                    bail!("a Cluster comes before the Tracks");
                };
                let size = sized(&h, "a Cluster")?;
                let cend = (h.data + size).min(seg_end);
                let mut cts = 0i64;
                let mut p = h.data;
                while p < cend {
                    r.seek(SeekFrom::Start(p))?;
                    let e = header(&mut r)?;
                    let esize = sized(&e, "a Cluster element")?;
                    match e.id {
                        id::TIMESTAMP => {
                            cts = read::uint(&body(&mut r, esize, "a Timestamp")?) as i64
                        }
                        id::SIMPLE_BLOCK => {
                            let b = body(&mut r, esize.min(16), "a SimpleBlock")?;
                            let (tn, rel, flags, n) = block_header(&b)?;
                            if tn == t.number {
                                if flags & 0x06 != 0 {
                                    bail!("laced video blocks aren't supported");
                                }
                                blocks.push((
                                    cts + i64::from(rel),
                                    e.data + n as u64,
                                    (esize - n as u64) as u32,
                                    flags & 0x80 != 0,
                                ));
                            }
                        }
                        id::BLOCK_GROUP => {
                            // Read the group's children's headers, not the payload.
                            let mut q = e.data;
                            let gend = e.data + esize;
                            let mut found = None;
                            let mut referenced = false;
                            while q < gend {
                                r.seek(SeekFrom::Start(q))?;
                                let g = header(&mut r)?;
                                let gsize = sized(&g, "a BlockGroup element")?;
                                match g.id {
                                    id::BLOCK => {
                                        let b = body(&mut r, gsize.min(16), "a Block")?;
                                        let (tn, rel, flags, n) = block_header(&b)?;
                                        if flags & 0x06 != 0 && tn == t.number {
                                            bail!("laced video blocks aren't supported");
                                        }
                                        found = (tn == t.number).then_some((
                                            cts + i64::from(rel),
                                            g.data + n as u64,
                                            (gsize - n as u64) as u32,
                                        ));
                                    }
                                    REFERENCE_BLOCK => referenced = true,
                                    _ => {}
                                }
                                q = g.data + gsize;
                            }
                            if let Some((ts, off, sz)) = found {
                                blocks.push((ts, off, sz, !referenced));
                            }
                        }
                        _ => {}
                    }
                    p = e.data + esize;
                }
            }
            _ => {}
        }
        match h.size {
            Some(s) => pos = h.data + s,
            None => bail!("element {:#x} has an unknown size", h.id),
        }
    }
    let Some(track) = track else {
        bail!("no video track");
    };
    if blocks.is_empty() {
        bail!("the video track has no frames");
    }
    if !blocks[0].3 {
        bail!("the first frame isn't a keyframe");
    }
    for w in blocks.windows(2) {
        if w[1].0 <= w[0].0 {
            bail!(
                "frames are out of order or repeated at {:.3} s (shotglass needs frames stored in presentation order)",
                w[1].0 as f64 * scale as f64 / 1e9
            );
        }
    }
    let t0 = blocks[0].0;
    let ns = |ts: i64| ((ts - t0) as u128 * scale as u128) as u64;
    // DefaultDuration is whole ns; a mean over the clip is good to half a tick over
    // the frame count.
    let (period, tolerance) = match default_duration {
        Some(d) if d > 0 => (d as f64, 1_000.0),
        _ if blocks.len() > 1 => {
            let n = (blocks.len() - 1) as f64;
            let mean = ns(blocks[blocks.len() - 1].0) as f64 / n;
            (mean, (scale as f64 / 2.0 / n).max(1_000.0))
        }
        _ => bail!("a one-frame clip with no frame rate (DefaultDuration)"),
    };
    let fps = snap_fps(period, tolerance);
    let frames: Vec<Frame> = blocks
        .iter()
        .map(|&(ts, offset, size, key)| Frame {
            offset,
            size,
            key,
            pts_ns: ns(ts),
        })
        .collect();
    check_cfr(&frames, fps)?;
    Ok(Clip { track, fps, frames })
}

const REFERENCE_BLOCK: u32 = 0xFB;

/// The fraction for a frame period in ns: whole rates, then NTSC-style n·1000/1001,
/// then thousandths.
pub fn snap_fps(period_ns: f64, tolerance_ns: f64) -> (u32, u32) {
    let rate = 1e9 / period_ns;
    let whole = rate.round().max(1.0);
    if (1e9 / whole - period_ns).abs() <= tolerance_ns {
        return (whole as u32, 1);
    }
    let ntsc = ((rate * 1.001).round() * 1000.0).max(1000.0);
    if (1e9 * 1001.0 / ntsc - period_ns).abs() <= tolerance_ns {
        return (ntsc as u32, 1001);
    }
    let num = (rate * 1000.0).round().max(1.0) as u64;
    let g = gcd(num, 1000);
    ((num / g) as u32, (1000 / g) as u32)
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Frame `k` must be within a quarter frame of `k · den / num` s.
fn check_cfr(frames: &[Frame], (num, den): (u32, u32)) -> Result<()> {
    let period = 1e9 * f64::from(den) / f64::from(num);
    for (k, f) in frames.iter().enumerate() {
        let want = k as f64 * period;
        if (f.pts_ns as f64 - want).abs() > period / 4.0 {
            bail!(
                "variable frame rate (frame {k} at {:.3} s, expected {:.3} s at {})",
                f.pts_ns as f64 / 1e9,
                want / 1e9,
                show_fps((num, den))
            );
        }
    }
    Ok(())
}

pub fn show_fps((num, den): (u32, u32)) -> String {
    if den == 1 {
        format!("{num} fps")
    } else {
        format!("{num}/{den} fps")
    }
}

#[cfg(test)]
mod tests {
    use super::super::mux::{Video, Writer};
    use super::*;
    use std::io::Cursor;

    fn video(fps: (u32, u32)) -> Video {
        Video {
            width: 64,
            height: 36,
            codec_private: vec![0x81, 0, 0x0c, 0],
            bit_depth: 8,
            fps,
            colour: crate::webm::ColourTags::BT709,
        }
    }

    /// A file of `n` frames, keyframe every `keyint`, payload = [k as u8; 3 + k % 5].
    fn file(fps: (u32, u32), n: u64, keyint: u64, step: impl Fn(u64) -> u64) -> Vec<u8> {
        let mut w = Writer::new(Cursor::new(vec![]), "webm", &video(fps), None).unwrap();
        for k in 0..n {
            w.video(
                step(k),
                k % keyint == 0,
                &vec![k as u8; 3 + (k % 5) as usize],
            )
            .unwrap();
        }
        w.finish(step(n)).unwrap().into_inner()
    }

    fn cfr(fps: (u32, u32)) -> impl Fn(u64) -> u64 {
        move |k| k * 1_000_000_000 * u64::from(fps.1) / u64::from(fps.0)
    }

    #[test]
    fn indexes_frames_and_keyframes() {
        let f = file((24, 1), 30, 8, cfr((24, 1)));
        let clip = parse(Cursor::new(f.clone())).unwrap();
        assert_eq!(clip.track.codec, "V_AV1");
        assert_eq!((clip.track.width, clip.track.height), (64, 36));
        assert_eq!(clip.track.colour.matrix, Some(1));
        assert_eq!(clip.track.colour.range, Some(1));
        assert_eq!(clip.fps, (24, 1));
        assert_eq!(clip.frames.len(), 30);
        assert_eq!(clip.keyframe_at_or_before(0), 0);
        assert_eq!(clip.keyframe_at_or_before(15), 8);
        assert_eq!(clip.keyframe_at_or_before(16), 16);
        assert_eq!(clip.keyframe_at_or_before(99), 24);
        assert_eq!(clip.max_gop(), 8);
        let mut c = Cursor::new(f);
        for k in 0..30 {
            assert_eq!(
                clip.read_frame(&mut c, k).unwrap(),
                vec![k as u8; 3 + k % 5],
                "frame {k}"
            );
        }
    }

    #[test]
    fn ntsc_rates_with_millisecond_timestamps() {
        for fps in [
            (30000, 1001),
            (24000, 1001),
            (60000, 1001),
            (25, 1),
            (60, 1),
        ] {
            let f = file(fps, 200, 60, cfr(fps));
            let clip = parse(Cursor::new(f)).unwrap();
            assert_eq!(clip.fps, fps);
            assert_eq!(clip.frames.len(), 200);
        }
    }

    #[test]
    fn variable_rate_is_refused() {
        let base = cfr((30, 1));
        let f = file((30, 1), 40, 10, |k| {
            if k >= 20 {
                base(k) + 20_000_000
            } else {
                base(k)
            }
        });
        let e = parse(Cursor::new(f)).unwrap_err().to_string();
        assert!(e.contains("variable frame rate (frame 20"), "{e}");
    }

    #[test]
    fn not_webm() {
        let e = parse(Cursor::new(b"\x89PNG\r\n\x1a\n........".to_vec())).unwrap_err();
        assert!(e.to_string().contains("not a WebM file"), "{e}");
    }

    #[test]
    fn snapping() {
        assert_eq!(snap_fps(33_366_666.0, 1000.0), (30000, 1001));
        assert_eq!(snap_fps(41_666_666.0, 1000.0), (24, 1));
        assert_eq!(snap_fps(40_000_000.0, 1000.0), (25, 1));
        assert_eq!(snap_fps(1e9 / 12.5, 1000.0), (25, 2));
    }
}
