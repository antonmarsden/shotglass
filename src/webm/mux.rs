//! A small WebM / Matroska writer: one AV1 video track and an optional Opus track.
//!
//! Layout: EBML header, then a Segment holding a SeekHead (Info, Tracks, Cues), Info
//! (duration back-patched), Tracks (av1C `CodecPrivate`, BT.709 Colour; OpusHead,
//! CodecDelay, SeekPreRoll), Clusters (one per video keyframe; SimpleBlocks, audio
//! interleaved by time), and Cues (one per cluster) at the end. Clusters are built in
//! memory and written whole, so the only seeks are the final back-patches. The bytes
//! depend only on the calls made: no clocks, no randomness (track UIDs are fixed).

use std::collections::VecDeque;
use std::io::{self, Seek, SeekFrom, Write};

pub mod id {
    pub const EBML: u32 = 0x1A45DFA3;
    pub const EBML_VERSION: u32 = 0x4286;
    pub const EBML_READ_VERSION: u32 = 0x42F7;
    pub const EBML_MAX_ID_LENGTH: u32 = 0x42F2;
    pub const EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
    pub const DOC_TYPE: u32 = 0x4282;
    pub const DOC_TYPE_VERSION: u32 = 0x4287;
    pub const DOC_TYPE_READ_VERSION: u32 = 0x4285;
    pub const SEGMENT: u32 = 0x18538067;
    pub const SEEK_HEAD: u32 = 0x114D9B74;
    pub const SEEK: u32 = 0x4DBB;
    pub const SEEK_ID: u32 = 0x53AB;
    pub const SEEK_POSITION: u32 = 0x53AC;
    pub const INFO: u32 = 0x1549A966;
    pub const TIMESTAMP_SCALE: u32 = 0x2AD7B1;
    pub const DURATION: u32 = 0x4489;
    pub const MUXING_APP: u32 = 0x4D80;
    pub const WRITING_APP: u32 = 0x5741;
    pub const TRACKS: u32 = 0x1654AE6B;
    pub const TRACK_ENTRY: u32 = 0xAE;
    pub const TRACK_NUMBER: u32 = 0xD7;
    pub const TRACK_UID: u32 = 0x73C5;
    pub const TRACK_TYPE: u32 = 0x83;
    pub const FLAG_LACING: u32 = 0x9C;
    pub const CODEC_ID: u32 = 0x86;
    pub const CODEC_PRIVATE: u32 = 0x63A2;
    pub const CODEC_DELAY: u32 = 0x56AA;
    pub const SEEK_PRE_ROLL: u32 = 0x56BB;
    pub const DEFAULT_DURATION: u32 = 0x23E383;
    pub const VIDEO: u32 = 0xE0;
    pub const PIXEL_WIDTH: u32 = 0xB0;
    pub const PIXEL_HEIGHT: u32 = 0xBA;
    pub const COLOUR: u32 = 0x55B0;
    pub const MATRIX_COEFFICIENTS: u32 = 0x55B1;
    pub const BITS_PER_CHANNEL: u32 = 0x55B2;
    pub const CHROMA_SUBSAMPLING_HORZ: u32 = 0x55B3;
    pub const CHROMA_SUBSAMPLING_VERT: u32 = 0x55B4;
    pub const CHROMA_SITING_HORZ: u32 = 0x55B7;
    pub const CHROMA_SITING_VERT: u32 = 0x55B8;
    pub const RANGE: u32 = 0x55B9;
    pub const TRANSFER_CHARACTERISTICS: u32 = 0x55BA;
    pub const PRIMARIES: u32 = 0x55BB;
    pub const AUDIO: u32 = 0xE1;
    pub const SAMPLING_FREQUENCY: u32 = 0xB5;
    pub const CHANNELS: u32 = 0x9F;
    pub const BIT_DEPTH: u32 = 0x6264;
    pub const CLUSTER: u32 = 0x1F43B675;
    pub const TIMESTAMP: u32 = 0xE7;
    pub const SIMPLE_BLOCK: u32 = 0xA3;
    pub const BLOCK_GROUP: u32 = 0xA0;
    pub const BLOCK: u32 = 0xA1;
    pub const DISCARD_PADDING: u32 = 0x75A2;
    pub const CUES: u32 = 0x1C53BB6B;
    pub const CUE_POINT: u32 = 0xBB;
    pub const CUE_TIME: u32 = 0xB3;
    pub const CUE_TRACK_POSITIONS: u32 = 0xB7;
    pub const CUE_TRACK: u32 = 0xF7;
    pub const CUE_CLUSTER_POSITION: u32 = 0xF1;
    pub const VOID: u32 = 0xEC;
}

/// Nanoseconds per Matroska tick: 1 ms, as WebM recommends.
pub const TIMESTAMP_SCALE: u64 = 1_000_000;
const VIDEO_TRACK: u64 = 1;
const AUDIO_TRACK: u64 = 2;
/// Relative block timestamps are i16 ticks; start a new cluster well before that.
const MAX_CLUSTER_TICKS: u64 = 30_000;

/// Element building, all in memory.
pub mod ebml {
    pub fn id(out: &mut Vec<u8>, id: u32) {
        let b = id.to_be_bytes();
        let skip = b.iter().take_while(|&&x| x == 0).count();
        out.extend_from_slice(&b[skip..]);
    }

    /// A size as the shortest EBML variable-length integer.
    pub fn size(out: &mut Vec<u8>, v: u64) {
        let mut len = 1;
        while len < 8 && v >= (1u64 << (7 * len)) - 1 {
            len += 1;
        }
        sized(out, v, len);
    }

    /// A size as an EBML variable-length integer of exactly `len` bytes.
    pub fn sized(out: &mut Vec<u8>, v: u64, len: usize) {
        assert!((1..=8).contains(&len) && v < (1u64 << (7 * len)) - 1);
        let marked = v | (1u64 << (7 * len));
        out.extend_from_slice(&marked.to_be_bytes()[8 - len..]);
    }

    pub fn bytes(out: &mut Vec<u8>, id_: u32, data: &[u8]) {
        id(out, id_);
        size(out, data.len() as u64);
        out.extend_from_slice(data);
    }

    pub fn uint(out: &mut Vec<u8>, id_: u32, v: u64) {
        let b = v.to_be_bytes();
        let skip = b.iter().take_while(|&&x| x == 0).count().min(7);
        bytes(out, id_, &b[skip..]);
    }

    /// An unsigned integer stored in exactly 8 bytes (for back-patching).
    pub fn uint8(out: &mut Vec<u8>, id_: u32, v: u64) {
        bytes(out, id_, &v.to_be_bytes());
    }

    pub fn int(out: &mut Vec<u8>, id_: u32, v: i64) {
        let b = v.to_be_bytes();
        let mut skip = 0;
        while skip < 7 {
            let (x, next) = (b[skip], b[skip + 1]);
            if (x == 0 && next & 0x80 == 0) || (x == 0xff && next & 0x80 != 0) {
                skip += 1;
            } else {
                break;
            }
        }
        bytes(out, id_, &b[skip..]);
    }

    pub fn float(out: &mut Vec<u8>, id_: u32, v: f64) {
        bytes(out, id_, &v.to_be_bytes());
    }

    pub fn string(out: &mut Vec<u8>, id_: u32, s: &str) {
        bytes(out, id_, s.as_bytes());
    }

    /// A master element around whatever `f` writes.
    pub fn master(out: &mut Vec<u8>, id_: u32, f: impl FnOnce(&mut Vec<u8>)) {
        let mut body = vec![];
        f(&mut body);
        bytes(out, id_, &body);
    }
}

/// The video track.
#[derive(Clone, Debug)]
pub struct Video {
    pub width: u32,
    pub height: u32,
    /// `V_AV1`'s `av1C` record.
    pub codec_private: Vec<u8>,
    pub bit_depth: u8,
    /// Frame rate as a fraction.
    pub fps: (u32, u32),
}

/// The Opus track.
#[derive(Clone, Debug)]
pub struct Audio {
    pub channels: u8,
    /// The encoder's lookahead in 48 kHz samples (OpusHead pre-skip, CodecDelay).
    pub pre_skip: u16,
}

impl Audio {
    /// RFC 7845's identification header, Matroska's `A_OPUS` CodecPrivate.
    pub fn opus_head(&self) -> Vec<u8> {
        let mut v = b"OpusHead".to_vec();
        v.push(1);
        v.push(self.channels);
        v.extend_from_slice(&self.pre_skip.to_le_bytes());
        v.extend_from_slice(&48_000u32.to_le_bytes());
        v.extend_from_slice(&0i16.to_le_bytes());
        v.push(0); // mapping family 0: mono or stereo
        v
    }

    pub fn codec_delay_ns(&self) -> u64 {
        u64::from(self.pre_skip) * 1_000_000_000 / 48_000
    }
}

/// An audio packet waiting to be interleaved.
#[derive(Clone, Debug)]
pub struct AudioPacket {
    /// Presentation time in ns (before CodecDelay is subtracted: the first packet is 0).
    pub pts_ns: u64,
    pub data: Vec<u8>,
    /// Samples to drop from the end of this packet, in ns (the last one only).
    pub discard_padding_ns: i64,
}

struct Cue {
    ticks: u64,
    track: u64,
    /// Cluster offset from the start of the Segment's data.
    cluster: u64,
}

pub struct Writer<W: Write + Seek> {
    w: W,
    segment_data: u64,
    segment_size_at: u64,
    seek_head_at: u64,
    duration_at: u64,
    tracks_offset: u64,
    info_offset: u64,
    has_video: bool,
    audio: VecDeque<AudioPacket>,
    cluster: Vec<u8>,
    cluster_ticks: Option<u64>,
    cues: Vec<Cue>,
}

fn ticks(ns: u64) -> u64 {
    (ns + TIMESTAMP_SCALE / 2) / TIMESTAMP_SCALE
}

/// Fixed-size SeekHead: three Seek entries, positions in 8 bytes, patched at the end.
fn seek_head(positions: [(u32, u64); 3]) -> Vec<u8> {
    let mut out = vec![];
    ebml::master(&mut out, id::SEEK_HEAD, |o| {
        for (target, pos) in positions {
            ebml::master(o, id::SEEK, |s| {
                let mut tid = vec![];
                ebml::id(&mut tid, target);
                ebml::bytes(s, id::SEEK_ID, &tid);
                ebml::uint8(s, id::SEEK_POSITION, pos);
            });
        }
    });
    out
}

impl<W: Write + Seek> Writer<W> {
    /// Start a file. `doc_type` is "webm" or "matroska". With `audio`, its packets
    /// are interleaved with the video as it's written.
    pub fn new(
        mut w: W,
        doc_type: &str,
        video: &Video,
        audio: Option<(&Audio, Vec<AudioPacket>)>,
    ) -> io::Result<Self> {
        let start = w.stream_position()?;
        let mut out = vec![];
        ebml::master(&mut out, id::EBML, |o| {
            ebml::uint(o, id::EBML_VERSION, 1);
            ebml::uint(o, id::EBML_READ_VERSION, 1);
            ebml::uint(o, id::EBML_MAX_ID_LENGTH, 4);
            ebml::uint(o, id::EBML_MAX_SIZE_LENGTH, 8);
            ebml::string(o, id::DOC_TYPE, doc_type);
            ebml::uint(o, id::DOC_TYPE_VERSION, 4);
            ebml::uint(o, id::DOC_TYPE_READ_VERSION, 2);
        });
        ebml::id(&mut out, id::SEGMENT);
        let segment_size_at = start + out.len() as u64;
        ebml::sized(&mut out, 0, 8); // patched in `finish`
        let segment_data = start + out.len() as u64;

        let seek_head_at = out.len() as u64 + start;
        out.extend(seek_head([(id::INFO, 0), (id::TRACKS, 0), (id::CUES, 0)]));

        let info_offset = start + out.len() as u64 - segment_data;
        let mut info = vec![];
        ebml::uint(&mut info, id::TIMESTAMP_SCALE, TIMESTAMP_SCALE);
        ebml::string(&mut info, id::MUXING_APP, "shotglass");
        ebml::string(&mut info, id::WRITING_APP, "shotglass");
        ebml::id(&mut info, id::DURATION);
        ebml::size(&mut info, 8);
        let duration_in_info = info.len();
        info.extend_from_slice(&0f64.to_be_bytes()); // patched in `finish`
        ebml::id(&mut out, id::INFO);
        ebml::size(&mut out, info.len() as u64);
        let duration_at = start + (out.len() + duration_in_info) as u64;
        out.extend(info);

        let tracks_offset = start + out.len() as u64 - segment_data;
        ebml::master(&mut out, id::TRACKS, |o| {
            ebml::master(o, id::TRACK_ENTRY, |t| {
                ebml::uint(t, id::TRACK_NUMBER, VIDEO_TRACK);
                ebml::uint(t, id::TRACK_UID, VIDEO_TRACK);
                ebml::uint(t, id::TRACK_TYPE, 1);
                ebml::uint(t, id::FLAG_LACING, 0);
                ebml::string(t, id::CODEC_ID, "V_AV1");
                ebml::bytes(t, id::CODEC_PRIVATE, &video.codec_private);
                ebml::uint(
                    t,
                    id::DEFAULT_DURATION,
                    1_000_000_000 * u64::from(video.fps.1) / u64::from(video.fps.0),
                );
                ebml::master(t, id::VIDEO, |v| {
                    ebml::uint(v, id::PIXEL_WIDTH, video.width.into());
                    ebml::uint(v, id::PIXEL_HEIGHT, video.height.into());
                    ebml::master(v, id::COLOUR, |c| {
                        ebml::uint(c, id::MATRIX_COEFFICIENTS, 1); // BT.709
                        ebml::uint(c, id::BITS_PER_CHANNEL, video.bit_depth.into());
                        ebml::uint(c, id::CHROMA_SUBSAMPLING_HORZ, 1);
                        ebml::uint(c, id::CHROMA_SUBSAMPLING_VERT, 1);
                        ebml::uint(c, id::CHROMA_SITING_HORZ, 1); // left (co-sited)
                        ebml::uint(c, id::CHROMA_SITING_VERT, 2); // half
                        ebml::uint(c, id::RANGE, 1); // broadcast (limited)
                        ebml::uint(c, id::TRANSFER_CHARACTERISTICS, 1);
                        ebml::uint(c, id::PRIMARIES, 1);
                    });
                });
            });
            if let Some((a, _)) = &audio {
                ebml::master(o, id::TRACK_ENTRY, |t| {
                    ebml::uint(t, id::TRACK_NUMBER, AUDIO_TRACK);
                    ebml::uint(t, id::TRACK_UID, AUDIO_TRACK);
                    ebml::uint(t, id::TRACK_TYPE, 2);
                    ebml::uint(t, id::FLAG_LACING, 0);
                    ebml::string(t, id::CODEC_ID, "A_OPUS");
                    ebml::bytes(t, id::CODEC_PRIVATE, &a.opus_head());
                    ebml::uint(t, id::CODEC_DELAY, a.codec_delay_ns());
                    ebml::uint(t, id::SEEK_PRE_ROLL, 80_000_000);
                    ebml::master(t, id::AUDIO, |s| {
                        ebml::float(s, id::SAMPLING_FREQUENCY, 48_000.0);
                        ebml::uint(s, id::CHANNELS, a.channels.into());
                        ebml::uint(s, id::BIT_DEPTH, 32);
                    });
                });
            }
        });
        w.write_all(&out)?;
        Ok(Writer {
            w,
            segment_data,
            segment_size_at,
            seek_head_at,
            duration_at,
            tracks_offset,
            info_offset,
            has_video: false,
            audio: audio.map(|(_, p)| p.into()).unwrap_or_default(),
            cluster: vec![],
            cluster_ticks: None,
            cues: vec![],
        })
    }

    fn pos(&mut self) -> io::Result<u64> {
        Ok(self.w.stream_position()? - self.segment_data)
    }

    fn flush_cluster(&mut self) -> io::Result<()> {
        if self.cluster_ticks.take().is_some() {
            let mut out = vec![];
            ebml::bytes(&mut out, id::CLUSTER, &self.cluster);
            self.w.write_all(&out)?;
            self.cluster.clear();
        }
        Ok(())
    }

    fn open_cluster(&mut self, ticks: u64, cue_track: Option<u64>) -> io::Result<()> {
        self.flush_cluster()?;
        if let Some(track) = cue_track {
            let cluster = self.pos()?;
            self.cues.push(Cue {
                ticks,
                track,
                cluster,
            });
        }
        ebml::uint(&mut self.cluster, id::TIMESTAMP, ticks);
        self.cluster_ticks = Some(ticks);
        Ok(())
    }

    fn block(&mut self, track: u64, ticks: u64, key: bool, data: &[u8], discard_ns: i64) {
        let base = self.cluster_ticks.expect("block outside a cluster");
        let rel = i16::try_from(ticks as i64 - base as i64).expect("cluster too long");
        let mut head = vec![];
        ebml::size(&mut head, track);
        head.extend_from_slice(&rel.to_be_bytes());
        if discard_ns != 0 {
            head.push(0);
            let mut block = head;
            block.extend_from_slice(data);
            ebml::master(&mut self.cluster, id::BLOCK_GROUP, |g| {
                ebml::bytes(g, id::BLOCK, &block);
                ebml::int(g, id::DISCARD_PADDING, discard_ns);
            });
        } else {
            head.push(if key { 0x80 } else { 0 });
            ebml::id(&mut self.cluster, id::SIMPLE_BLOCK);
            ebml::size(&mut self.cluster, (head.len() + data.len()) as u64);
            self.cluster.extend_from_slice(&head);
            self.cluster.extend_from_slice(data);
        }
    }

    /// Write queued audio packets before `until` ticks (all of them for None).
    fn audio_until(&mut self, until: Option<u64>) -> io::Result<()> {
        while let Some(p) = self.audio.front() {
            let t = ticks(p.pts_ns);
            if until.is_some_and(|u| t >= u) {
                break;
            }
            let p = self.audio.pop_front().unwrap();
            match self.cluster_ticks {
                Some(c) if t >= c && t - c < MAX_CLUSTER_TICKS => {}
                // Before any video (or past a long gap): a cluster of its own, cued
                // only when there's no video to seek by.
                _ => self.open_cluster(t, None)?,
            }
            self.block(AUDIO_TRACK, t, true, &p.data, p.discard_padding_ns);
        }
        Ok(())
    }

    /// One AV1 temporal unit (temporal delimiters already stripped) at `pts_ns`.
    /// Units must come in presentation order; keyframes start a cluster and a cue.
    pub fn video(&mut self, pts_ns: u64, keyframe: bool, data: &[u8]) -> io::Result<()> {
        let t = ticks(pts_ns);
        self.audio_until(Some(t))?;
        let first = !self.has_video;
        self.has_video = true;
        let long = self
            .cluster_ticks
            .is_none_or(|c| t < c || t - c >= MAX_CLUSTER_TICKS);
        if keyframe || first || long {
            self.open_cluster(t, keyframe.then_some(VIDEO_TRACK))?;
        }
        self.block(VIDEO_TRACK, t, keyframe, data, 0);
        Ok(())
    }

    /// Write what's left (audio after the last frame, cues) and patch the header.
    /// `duration_ns` is the presentation length (the last frame's end).
    pub fn finish(mut self, duration_ns: u64) -> io::Result<W> {
        self.audio_until(None)?;
        self.flush_cluster()?;
        let cues_offset = self.pos()?;
        let mut out = vec![];
        if !self.cues.is_empty() {
            ebml::master(&mut out, id::CUES, |o| {
                for c in &self.cues {
                    ebml::master(o, id::CUE_POINT, |p| {
                        ebml::uint(p, id::CUE_TIME, c.ticks);
                        ebml::master(p, id::CUE_TRACK_POSITIONS, |t| {
                            ebml::uint(t, id::CUE_TRACK, c.track);
                            ebml::uint(t, id::CUE_CLUSTER_POSITION, c.cluster);
                        });
                    });
                }
            });
        }
        self.w.write_all(&out)?;
        let end = self.w.stream_position()?;
        let duration = duration_ns as f64 / TIMESTAMP_SCALE as f64;

        self.w.seek(SeekFrom::Start(self.segment_size_at))?;
        let mut size = vec![];
        ebml::sized(&mut size, end - self.segment_data, 8);
        self.w.write_all(&size)?;

        self.w.seek(SeekFrom::Start(self.seek_head_at))?;
        let head = if self.cues.is_empty() {
            // No cues: point the third entry at Tracks again rather than at nothing.
            seek_head([
                (id::INFO, self.info_offset),
                (id::TRACKS, self.tracks_offset),
                (id::TRACKS, self.tracks_offset),
            ])
        } else {
            seek_head([
                (id::INFO, self.info_offset),
                (id::TRACKS, self.tracks_offset),
                (id::CUES, cues_offset),
            ])
        };
        self.w.write_all(&head)?;

        self.w.seek(SeekFrom::Start(self.duration_at))?;
        self.w.write_all(&duration.to_be_bytes())?;
        self.w.seek(SeekFrom::Start(end))?;
        self.w.flush()?;
        Ok(self.w)
    }
}

/// Reading EBML back, for tests (ours and the export's).
pub mod read {
    use anyhow::{Result, bail};

    #[derive(Debug, Clone)]
    pub struct Element<'a> {
        pub id: u32,
        /// Offset of the element's first byte in the buffer it was read from.
        pub offset: usize,
        pub data: &'a [u8],
    }

    pub fn vint(data: &[u8], is_id: bool) -> Result<(u64, usize)> {
        let Some(&first) = data.first() else {
            bail!("truncated vint");
        };
        let len = first.leading_zeros() as usize + 1;
        if len > 8 || data.len() < len {
            bail!("bad vint");
        }
        let mut v = if is_id {
            u64::from(first)
        } else {
            u64::from(first) & ((1 << (8 - len)) - 1)
        };
        for &b in &data[1..len] {
            v = (v << 8) | u64::from(b);
        }
        Ok((v, len))
    }

    /// The elements directly inside `data` (which starts at `base` in its buffer).
    pub fn children(data: &[u8], base: usize) -> Result<Vec<Element<'_>>> {
        let mut out = vec![];
        let mut pos = 0;
        while pos < data.len() {
            let (id, a) = vint(&data[pos..], true)?;
            let (size, b) = vint(&data[pos + a..], false)?;
            let start = pos + a + b;
            let end = start + size as usize;
            if end > data.len() {
                bail!("element {id:#x} at {} runs past its parent", base + pos);
            }
            out.push(Element {
                id: id as u32,
                offset: base + pos,
                data: &data[start..end],
            });
            pos = end;
        }
        Ok(out)
    }

    pub fn uint(data: &[u8]) -> u64 {
        data.iter().fold(0, |v, &b| (v << 8) | u64::from(b))
    }

    pub fn find<'a>(els: &[Element<'a>], id: u32) -> Option<Element<'a>> {
        els.iter().find(|e| e.id == id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::read::{children, find, uint};
    use super::*;
    use std::io::Cursor;

    fn video() -> Video {
        Video {
            width: 64,
            height: 48,
            codec_private: vec![0x81, 0, 0x4c, 0, 0x0a, 0],
            bit_depth: 10,
            fps: (24, 1),
        }
    }

    #[test]
    fn vints_and_ints() {
        let s = |v| {
            let mut o = vec![];
            ebml::size(&mut o, v);
            o
        };
        assert_eq!(s(0), [0x80]);
        assert_eq!(s(126), [0xfe]);
        assert_eq!(s(127), [0x40, 0x7f]); // all ones is reserved (unknown size)
        assert_eq!(s(300), [0x41, 0x2c]);
        let i = |v| {
            let mut o = vec![];
            ebml::int(&mut o, 0x75A2, v);
            o[3..].to_vec()
        };
        assert_eq!(i(0), [0]);
        assert_eq!(i(-1), [0xff]);
        assert_eq!(i(128), [0, 0x80]);
        assert_eq!(i(-129), [0xff, 0x7f]);
        let mut o = vec![];
        ebml::id(&mut o, id::SEGMENT);
        ebml::id(&mut o, id::VOID);
        assert_eq!(o, [0x18, 0x53, 0x80, 0x67, 0xec]);
        assert_eq!(read::vint(&[0x41, 0x2c], false).unwrap(), (300, 2));
    }

    /// A file with two keyframe clusters and interleaved audio, read back.
    fn sample() -> Vec<u8> {
        let a = Audio {
            channels: 2,
            pre_skip: 312,
        };
        // 20 ms packets over 1.1 s; the last one drops 5 ms.
        let packets: Vec<AudioPacket> = (0..55)
            .map(|k| AudioPacket {
                pts_ns: k * 20_000_000,
                data: vec![k as u8; 3],
                discard_padding_ns: if k == 54 { 5_000_000 } else { 0 },
            })
            .collect();
        let mut w =
            Writer::new(Cursor::new(vec![]), "webm", &video(), Some((&a, packets))).unwrap();
        for n in 0..24u64 {
            let ns = n * 1_000_000_000 / 24;
            w.video(ns, n % 12 == 0, &[n as u8; 5]).unwrap();
        }
        w.finish(1_000_000_000).unwrap().into_inner()
    }

    #[test]
    fn writes_a_readable_webm() {
        let file = sample();
        let top = children(&file, 0).unwrap();
        assert_eq!(top.len(), 2);
        let head = children(top[0].data, 0).unwrap();
        assert_eq!(find(&head, id::DOC_TYPE).unwrap().data, b"webm");
        assert_eq!(top[1].id, id::SEGMENT);
        let seg = top[1].data;
        let seg_base = file.len() - seg.len();
        let els = children(seg, 0).unwrap();
        let ids: Vec<u32> = els.iter().map(|e| e.id).collect();
        assert_eq!(
            ids,
            [
                id::SEEK_HEAD,
                id::INFO,
                id::TRACKS,
                id::CLUSTER,
                id::CLUSTER,
                id::CUES
            ]
        );
        // The segment size was patched to cover exactly the rest of the file.
        assert_eq!(seg_base + seg.len(), file.len());

        // SeekHead positions point at the elements they name.
        for s in children(els[0].data, 0).unwrap() {
            let kids = children(s.data, 0).unwrap();
            let target = read::vint(find(&kids, id::SEEK_ID).unwrap().data, true)
                .unwrap()
                .0 as u32;
            let pos = uint(find(&kids, id::SEEK_POSITION).unwrap().data) as usize;
            let at = els.iter().find(|e| e.offset == pos).expect("seek target");
            assert_eq!(at.id, target);
        }

        // Duration (ms) patched.
        let info = children(els[1].data, 0).unwrap();
        let d = find(&info, id::DURATION).unwrap().data;
        assert_eq!(f64::from_be_bytes(d.try_into().unwrap()), 1000.0);

        // Tracks: AV1 with BT.709 limited range, Opus with its delay.
        let tracks = children(els[2].data, 0).unwrap();
        assert_eq!(tracks.len(), 2);
        let v = children(tracks[0].data, 0).unwrap();
        assert_eq!(find(&v, id::CODEC_ID).unwrap().data, b"V_AV1");
        assert_eq!(
            find(&v, id::CODEC_PRIVATE).unwrap().data[..4],
            [0x81, 0, 0x4c, 0]
        );
        let vid = children(find(&v, id::VIDEO).unwrap().data, 0).unwrap();
        assert_eq!(uint(find(&vid, id::PIXEL_WIDTH).unwrap().data), 64);
        let col = children(find(&vid, id::COLOUR).unwrap().data, 0).unwrap();
        for (i, want) in [
            (id::MATRIX_COEFFICIENTS, 1),
            (id::PRIMARIES, 1),
            (id::TRANSFER_CHARACTERISTICS, 1),
            (id::RANGE, 1),
            (id::BITS_PER_CHANNEL, 10),
            (id::CHROMA_SITING_HORZ, 1),
            (id::CHROMA_SITING_VERT, 2),
        ] {
            assert_eq!(uint(find(&col, i).unwrap().data), want, "{i:#x}");
        }
        let a = children(tracks[1].data, 0).unwrap();
        assert_eq!(find(&a, id::CODEC_ID).unwrap().data, b"A_OPUS");
        assert_eq!(uint(find(&a, id::CODEC_DELAY).unwrap().data), 6_500_000);
        let head = find(&a, id::CODEC_PRIVATE).unwrap().data;
        assert_eq!(&head[..8], b"OpusHead");
        assert_eq!(u16::from_le_bytes([head[10], head[11]]), 312);

        // Clusters start at keyframes (0 and 500 ms), video blocks are keyed right,
        // audio is interleaved by time, and the last audio packet carries its padding.
        let mut cluster_times = vec![];
        let mut last_audio = 0;
        let mut video_frames = 0;
        let mut audio_packets = 0;
        let mut padding = None;
        for c in els.iter().filter(|e| e.id == id::CLUSTER) {
            let kids = children(c.data, 0).unwrap();
            let base = uint(kids[0].data);
            assert_eq!(kids[0].id, id::TIMESTAMP);
            cluster_times.push(base);
            let mut prev = 0;
            for b in &kids[1..] {
                let (block, discard) = if b.id == id::BLOCK_GROUP {
                    let g = children(b.data, 0).unwrap();
                    let d = find(&g, id::DISCARD_PADDING).unwrap().data;
                    (find(&g, id::BLOCK).unwrap().data, Some(uint(d)))
                } else {
                    assert_eq!(b.id, id::SIMPLE_BLOCK);
                    (b.data, None)
                };
                let track = block[0] & 0x7f;
                let t = base as i64 + i16::from_be_bytes([block[1], block[2]]) as i64;
                assert!(t >= prev, "blocks in time order");
                prev = t;
                if track == 1 {
                    let n = block[4] as i64;
                    assert_eq!(t, (n * 1000 + 12) / 24, "frame {n} at its time");
                    assert_eq!(block[3] & 0x80 != 0, n % 12 == 0);
                    video_frames += 1;
                } else {
                    assert_eq!(block[3] & 0x80 != 0, discard.is_none());
                    last_audio = t;
                    audio_packets += 1;
                    padding = discard.or(padding);
                }
            }
        }
        assert_eq!(cluster_times, [0, 500]);
        assert_eq!((video_frames, audio_packets), (24, 55));
        assert_eq!(last_audio, 1080);
        assert_eq!(padding, Some(5_000_000));

        // Cues point at the keyframe clusters.
        let cues = children(els[5].data, 0).unwrap();
        let clusters: Vec<usize> = els
            .iter()
            .filter(|e| e.id == id::CLUSTER)
            .map(|e| e.offset)
            .collect();
        for (cue, (want_t, at)) in cues.iter().zip([(0, clusters[0]), (500, clusters[1])]) {
            let p = children(cue.data, 0).unwrap();
            assert_eq!(uint(find(&p, id::CUE_TIME).unwrap().data), want_t);
            let tp = children(find(&p, id::CUE_TRACK_POSITIONS).unwrap().data, 0).unwrap();
            assert_eq!(uint(find(&tp, id::CUE_TRACK).unwrap().data), 1);
            assert_eq!(
                uint(find(&tp, id::CUE_CLUSTER_POSITION).unwrap().data) as usize,
                at
            );
        }
    }

    #[test]
    fn is_deterministic() {
        assert_eq!(sample(), sample());
    }

    #[test]
    fn long_gops_still_get_clusters() {
        let mut w = Writer::new(Cursor::new(vec![]), "matroska", &video(), None).unwrap();
        for n in 0..(24 * 70u64) {
            w.video(n * 1_000_000_000 / 24, n == 0, &[1]).unwrap();
        }
        let file = w.finish(70_000_000_000).unwrap().into_inner();
        let top = children(&file, 0).unwrap();
        let els = children(top[1].data, 0).unwrap();
        let clusters = els.iter().filter(|e| e.id == id::CLUSTER).count();
        assert_eq!(clusters, 3, "a new cluster every 30 s");
        assert_eq!(els.iter().filter(|e| e.id == id::CUES).count(), 1);
    }

    #[test]
    fn opus_head_layout() {
        let h = Audio {
            channels: 2,
            pre_skip: 312,
        }
        .opus_head();
        assert_eq!(
            h,
            [
                b'O', b'p', b'u', b's', b'H', b'e', b'a', b'd', 1, 2, 0x38, 0x01, 0x80, 0xbb, 0, 0,
                0, 0, 0
            ]
        );
    }

    /// The fixture AV1 stream in our WebM, checked by ffprobe where it's installed.
    #[test]
    fn ffprobe_reads_the_fixture() {
        use crate::webm::av1;
        let data: &[u8] = include_bytes!("../../tests/fixtures/av1/testsrc-64x48-10bit.obu");
        let (seq, obu) = av1::find_sequence_header(data).unwrap();
        let v = Video {
            width: 64,
            height: 48,
            codec_private: av1::av1c(&seq, &obu),
            bit_depth: 10,
            fps: (24, 1),
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.webm");
        let f = std::fs::File::create(&path).unwrap();
        let mut w = Writer::new(f, "webm", &v, None).unwrap();
        let tus = av1::temporal_units(data).unwrap();
        for (n, tu) in tus.iter().enumerate() {
            let tu = av1::strip_temporal_delimiters(tu).unwrap();
            w.video(n as u64 * 1_000_000_000 / 24, n % 4 == 0, &tu)
                .unwrap();
        }
        w.finish(tus.len() as u64 * 1_000_000_000 / 24).unwrap();
        let out = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-count_frames", "-select_streams", "v:0"])
            .args(["-show_entries", "stream=nb_read_frames,codec_name,pix_fmt,color_space,color_primaries,color_transfer,color_range"])
            .args(["-of", "default=nw=1"])
            .arg(&path)
            .output();
        let Ok(out) = out else {
            eprintln!("ffprobe not found; skipped");
            return;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && err.trim().is_empty(), "{err}");
        for want in [
            "codec_name=av1",
            "pix_fmt=yuv420p10le",
            "color_range=tv",
            "color_space=bt709",
            "color_transfer=bt709",
            "color_primaries=bt709",
            "nb_read_frames=6",
        ] {
            assert!(
                text.contains(want),
                "{want} in:
{text}"
            );
        }
    }
}
