//! Video footage (#73), CPU side: decoded Y'CbCr planes → RGBA8, and the colour
//! settings that choose the conversion (matrix, range, chroma siting) from the
//! file's tags or the channel's overrides.
//!
//! The conversion is shotglass's own, so a frame's texels depend only on the file,
//! the frame number and the options: AV1 decoding is bit-exact by specification, and
//! this is plain IEEE f32 arithmetic (no FMA contraction in Rust, no transcendental
//! functions). Chroma is upsampled bilinearly at the stream's siting; output is
//! gamma-encoded RGB as the file's transfer leaves it (like a JPEG photo), alpha 255.

pub mod decode;

use crate::webm::demux::{self, Clip, Colour};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Readers kept per clip: enough for a few separate playheads on one clip (two
/// shots of it crossing in a transition, a comp channel's offset, speed > 2).
const MAX_READERS: usize = 4;

/// One clip as footage: its index, how its frames convert, and a small pool of
/// readers that decode them on the footage decode pool.
pub struct Source {
    pub path: PathBuf,
    pub clip: Arc<Clip>,
    pub conversion: Conversion,
    readers: Mutex<Vec<Slot>>,
    tick: AtomicU64,
}

struct Slot {
    reader: Arc<Mutex<decode::Reader>>,
    /// The frame it would serve next without seeking (`usize::MAX`: not started).
    pos: Arc<AtomicUsize>,
    used: u64,
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("video::Source")
            .field("path", &self.path)
            .field("frames", &self.clip.frames.len())
            .field("conversion", &self.conversion)
            .finish()
    }
}

impl PartialEq for Source {
    fn eq(&self, o: &Source) -> bool {
        self.path == o.path
            && self.conversion == o.conversion
            && self.clip.frames == o.clip.frames
            && self.clip.fps == o.clip.fps
    }
}

impl Source {
    /// Read a clip's index and settings (no decoding), refusing what we can't show.
    pub fn open(path: &Path, opts: &crate::movie::VideoOptions) -> Result<Source> {
        let clip = demux::open(path)?;
        if clip.track.codec != "V_AV1" {
            bail!(
                "{} video; shotglass decodes AV1 only. Convert it once: shotglass conform {}",
                decode::codec_name(&clip.track.codec),
                path.display()
            );
        }
        if !decode::AVAILABLE {
            bail!("{}", decode::UNAVAILABLE);
        }
        let seq = crate::webm::av1::find_sequence_header_in_av1c(&clip.track.codec_private)
            .map(|(s, _)| s)
            .ok();
        let transfer = clip
            .track
            .colour
            .transfer
            .or_else(|| seq.as_ref().and_then(|s| s.color).map(|c| u64::from(c.1)));
        match transfer {
            Some(16) => {
                bail!("HDR (PQ, SMPTE ST 2084) isn't supported yet; convert to SDR BT.709 first")
            }
            Some(18) => bail!("HDR (HLG) isn't supported yet; convert to SDR BT.709 first"),
            _ => {}
        }
        let mut conversion =
            Conversion::from_tags(&clip.track.colour, seq.as_ref(), clip.track.height);
        if let Some(m) = opts.matrix {
            conversion.matrix = m;
        }
        if let Some(f) = opts.full_range {
            conversion.full_range = f;
        }
        Ok(Source {
            path: path.to_path_buf(),
            clip: Arc::new(clip),
            conversion,
            readers: Mutex::new(vec![]),
            tick: AtomicU64::new(0),
        })
    }

    /// Decoded planes of frame `k`, from the reader that reaches it soonest: one that
    /// decodes on to it (waiting for it if another worker is using it, which is
    /// cheaper than seeking), else a new reader, else the least recently used one.
    pub fn planes(&self, k: usize) -> Result<Planes> {
        let key = self.clip.keyframe_at_or_before(k);
        let (reader, pos) = {
            let mut slots = self.readers.lock().unwrap();
            let used = self.tick.fetch_add(1, Ordering::Relaxed);
            let near = (0..slots.len())
                .filter(|&i| {
                    let p = slots[i].pos.load(Ordering::Relaxed);
                    p != usize::MAX && p <= k && p >= key
                })
                .max_by_key(|&i| slots[i].pos.load(Ordering::Relaxed));
            let i = match near {
                Some(i) => i,
                None if slots.len() < MAX_READERS => {
                    let reader = decode::Reader::open(&self.path, self.clip.clone())?;
                    slots.push(Slot {
                        reader: Arc::new(Mutex::new(reader)),
                        pos: Arc::new(AtomicUsize::new(usize::MAX)),
                        used,
                    });
                    slots.len() - 1
                }
                None => (0..slots.len()).min_by_key(|&i| slots[i].used).unwrap(),
            };
            let slot = &mut slots[i];
            slot.used = used;
            // Claimed: later requests see where it will be once this one is done.
            slot.pos.store(k + 1, Ordering::Relaxed);
            (slot.reader.clone(), slot.pos.clone())
        };
        let mut r = reader.lock().unwrap();
        let out = r.frame(k);
        pos.store(r.position().unwrap_or(usize::MAX), Ordering::Relaxed);
        out
    }

    /// `check`: decode frame 0, or with `deep` every frame in order, and compare a
    /// sample of them against seeks (a file whose frames differ by route would make
    /// play, `still` and `export` disagree). Problems as messages.
    pub fn check(&self, deep: bool) -> Vec<String> {
        let mut r = match decode::Reader::open(&self.path, self.clip.clone()) {
            Ok(r) => r,
            Err(e) => return vec![e.to_string()],
        };
        let n = self.clip.frames.len();
        if !deep {
            return r
                .frame(0)
                .err()
                .map(|e| e.to_string())
                .into_iter()
                .collect();
        }
        let step = (n / 16).max(1);
        let mut samples = vec![];
        for k in 0..n {
            match r.frame(k) {
                Ok(p) if k % step == 0 || k == n - 1 => samples.push((k, p)),
                Ok(_) => {}
                Err(e) => return vec![e.to_string()],
            }
        }
        let mut problems = vec![];
        for (k, p) in samples.iter().rev() {
            let mut fresh = match decode::Reader::open(&self.path, self.clip.clone()) {
                Ok(r) => r,
                Err(e) => return vec![e.to_string()],
            };
            match fresh.frame(*k) {
                Ok(q) if q == *p => {}
                Ok(_) => problems.push(format!(
                    "frame {k} decodes differently after a seek than read in order"
                )),
                Err(e) => problems.push(format!("frame {k} after a seek: {e}")),
            }
        }
        problems
    }

    /// Frame `k` as RGBA8, ready to upload.
    pub fn decode(
        &self,
        k: u64,
        label: &str,
        vflip: bool,
    ) -> std::result::Result<crate::texture::DecodedImage, String> {
        let p = self
            .planes(k as usize)
            .map_err(|e| format!("{label}: {e}"))?;
        let f = to_rgba(&p, &self.conversion, vflip);
        Ok(crate::texture::DecodedImage {
            width: p.width,
            height: p.height,
            rgba: toodee::TooDeeOps::rows(&f)
                .flat_map(|r| r.iter().flatten().copied())
                .collect(),
            warnings: vec![],
            meta: [0.0; 4],
        })
    }
}

/// Y'CbCr → R'G'B' matrices (ITU-R BT.601, BT.709, BT.2020 non-constant luminance).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Matrix {
    Bt601,
    Bt709,
    Bt2020,
}

impl Matrix {
    pub fn parse(s: &str) -> Option<Matrix> {
        match s {
            "bt601" => Some(Matrix::Bt601),
            "bt709" => Some(Matrix::Bt709),
            "bt2020" => Some(Matrix::Bt2020),
            _ => None,
        }
    }

    /// (Kr, Kb).
    fn k(self) -> (f32, f32) {
        match self {
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt2020 => (0.2627, 0.0593),
        }
    }

    /// From an ISO/IEC 23091-4 MatrixCoefficients code point (Matroska Colour, AV1
    /// sequence header); None = unspecified or not one we convert.
    pub fn from_code(c: u64) -> Option<Matrix> {
        match c {
            1 => Some(Matrix::Bt709),
            5 | 6 => Some(Matrix::Bt601),
            9 => Some(Matrix::Bt2020),
            _ => None,
        }
    }
}

/// Where chroma samples sit along one axis of a subsampled plane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Siting {
    /// On the first luma sample (horizontal "left", vertical "top").
    CoSited,
    /// Halfway between two luma samples.
    Half,
}

/// How to convert one clip's frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Conversion {
    pub matrix: Matrix,
    pub full_range: bool,
    /// (horizontal, vertical).
    pub siting: (Siting, Siting),
}

impl Conversion {
    /// From the file's tags (the Matroska Colour element first, then the AV1 sequence
    /// header's `color` and `chroma_sample_position`), with ffmpeg's rule for
    /// untagged files: height ≥ 720 → BT.709, else BT.601; limited range; 4:2:0
    /// chroma sited left (H.264/HEVC/AV1's usual), i.e. co-sited horizontally and
    /// halfway vertically.
    pub fn from_tags(
        colour: &Colour,
        seq: Option<&crate::webm::av1::SequenceHeader>,
        height: u32,
    ) -> Conversion {
        let matrix = colour
            .matrix
            .and_then(Matrix::from_code)
            .or_else(|| {
                seq.and_then(|s| s.color)
                    .and_then(|c| Matrix::from_code(c.2.into()))
            })
            .unwrap_or(if height >= 720 {
                Matrix::Bt709
            } else {
                Matrix::Bt601
            });
        let full_range = match colour.range {
            Some(2) => true,
            Some(1) => false,
            _ => seq.is_some_and(|s| s.full_range),
        };
        let axis = |v: Option<u64>| match v {
            Some(1) => Some(Siting::CoSited),
            Some(2) => Some(Siting::Half),
            _ => None,
        };
        // AV1: 1 = vertical (left), 2 = colocated (top-left), 0 = unknown.
        let (sh, sv) = match seq.map(|s| s.chroma_sample_position) {
            Some(2) => (Siting::CoSited, Siting::CoSited),
            _ => (Siting::CoSited, Siting::Half),
        };
        Conversion {
            matrix,
            full_range,
            siting: (
                axis(colour.siting_horz).unwrap_or(sh),
                axis(colour.siting_vert).unwrap_or(sv),
            ),
        }
    }
}

/// A decoded frame's planes, every sample as u16 (8-bit values stay 0..=255).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planes {
    pub width: usize,
    pub height: usize,
    pub bits: u8,
    /// log2 of the chroma subsampling (x, y): (1, 1) = 4:2:0, (1, 0) = 4:2:2,
    /// (0, 0) = 4:4:4.
    pub sub: (u8, u8),
    pub y: Vec<u16>,
    /// Empty for monochrome.
    pub u: Vec<u16>,
    pub v: Vec<u16>,
}

impl Planes {
    pub fn chroma_size(&self) -> (usize, usize) {
        (
            self.width.div_ceil(1 << self.sub.0),
            self.height.div_ceil(1 << self.sub.1),
        )
    }
}

/// Positions along one axis: for each output sample, (index, next index, weight of
/// the next out of 4).
fn taps(n: usize, sub: u8, siting: Siting, cn: usize) -> Vec<(usize, usize, u32)> {
    (0..n)
        .map(|i| {
            if sub == 0 {
                return (i, i, 0);
            }
            // Chroma coordinate of output sample i, in quarters.
            let q = match siting {
                Siting::CoSited => 2 * i as i64,
                Siting::Half => 2 * i as i64 - 1,
            };
            let i0 = q.div_euclid(4);
            let f = q.rem_euclid(4) as u32;
            let c = |j: i64| j.clamp(0, cn as i64 - 1) as usize;
            (c(i0), c(i0 + 1), f)
        })
        .collect()
}

/// Convert to RGBA8, top row first (`vflip` = bottom row first).
pub fn to_rgba(p: &Planes, c: &Conversion, vflip: bool) -> crate::gfx::Frame {
    let (w, h) = (p.width, p.height);
    let mut out = crate::gfx::Frame::init(w, h, [0, 0, 0, 255]);
    let max = ((1u32 << p.bits) - 1) as f32;
    let s = (1u32 << (p.bits - 8)) as f32;
    let (y_off, y_rng, c_rng) = if c.full_range {
        (0.0, max, max)
    } else {
        (16.0 * s, 219.0 * s, 224.0 * s)
    };
    let c_mid = (1u32 << (p.bits - 1)) as f32;
    let (kr, kb) = c.matrix.k();
    let kg = 1.0 - kr - kb;
    let (cw, ch) = p.chroma_size();
    let mono = p.u.is_empty();
    let tx = taps(w, p.sub.0, c.siting.0, cw);
    let ty = taps(h, p.sub.1, c.siting.1, ch);
    let q8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    let mut cb_row = vec![0f32; w];
    let mut cr_row = vec![0f32; w];
    for yy in 0..h {
        if !mono {
            let (r0, r1, fy) = ty[yy];
            let up = |plane: &[u16], x: usize| -> f32 {
                let (c0, c1, fx) = tx[x];
                let a = |r: usize, cx: usize| u32::from(plane[r * cw + cx]);
                let top = a(r0, c0) * (4 - fx) + a(r0, c1) * fx;
                let bot = a(r1, c0) * (4 - fx) + a(r1, c1) * fx;
                (top * (4 - fy) + bot * fy) as f32 / 16.0
            };
            for x in 0..w {
                cb_row[x] = (up(&p.u, x) - c_mid) / c_rng;
                cr_row[x] = (up(&p.v, x) - c_mid) / c_rng;
            }
        }
        let row = if vflip { h - 1 - yy } else { yy };
        for x in 0..w {
            let l = (f32::from(p.y[yy * w + x]) - y_off) / y_rng;
            let px = if mono {
                let g = q8(l);
                [g, g, g, 255]
            } else {
                let r = l + 2.0 * (1.0 - kr) * cr_row[x];
                let b = l + 2.0 * (1.0 - kb) * cb_row[x];
                let g = (l - kr * r - kb * b) / kg;
                [q8(r), q8(g), q8(b), 255]
            };
            out[(x, row)] = px;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(matrix: Matrix, full_range: bool) -> Conversion {
        Conversion {
            matrix,
            full_range,
            siting: (Siting::CoSited, Siting::Half),
        }
    }

    /// A flat 4:2:0 frame of one colour.
    fn flat(bits: u8, y: u16, u: u16, v: u16) -> Planes {
        Planes {
            width: 5,
            height: 3,
            bits,
            sub: (1, 1),
            y: vec![y; 15],
            u: vec![u; 6],
            v: vec![v; 6],
        }
    }

    fn px(p: &Planes, c: &Conversion) -> [u8; 4] {
        to_rgba(p, c, false)[(4, 2)]
    }

    #[test]
    fn known_colours() {
        let c709 = conv(Matrix::Bt709, false);
        // Limited-range black, white and mid grey.
        assert_eq!(px(&flat(8, 16, 128, 128), &c709), [0, 0, 0, 255]);
        assert_eq!(px(&flat(8, 235, 128, 128), &c709), [255, 255, 255, 255]);
        assert_eq!(px(&flat(10, 940, 512, 512), &c709), [255, 255, 255, 255]);
        assert_eq!(px(&flat(8, 126, 128, 128), &c709), [128, 128, 128, 255]);
        // BT.709 limited bars: red (63, 102, 240), 75% red (51, 109, 212).
        let red = px(&flat(8, 63, 102, 240), &c709);
        assert!(red[0] >= 254 && red[1] <= 1 && red[2] <= 1, "{red:?}");
        let red = px(&flat(8, 51, 109, 212), &c709);
        assert!(
            red[0].abs_diff(191) <= 1 && red[1] <= 1 && red[2] <= 1,
            "{red:?}"
        );
        // BT.601 limited red (81, 90, 240) → (255, 0, 0).
        let red = px(&flat(8, 81, 90, 240), &conv(Matrix::Bt601, false));
        assert!(red[0] >= 254 && red[1] <= 1 && red[2] <= 1, "{red:?}");
        // Full range: Y = 255 is white, Y = 0 black.
        let full = conv(Matrix::Bt709, true);
        assert_eq!(px(&flat(8, 255, 128, 128), &full), [255, 255, 255, 255]);
        assert_eq!(px(&flat(8, 0, 128, 128), &full), [0, 0, 0, 255]);
        // BT.2020 blue (10-bit limited): Y = 64 + 876·0.0593, Cb max.
        let y = (64.0 + 876.0 * 0.0593f32).round() as u16;
        let blue = px(
            &flat(10, y, 960, 512 - (448.0 * 0.0593 / (1.0 - 0.2627)) as u16),
            &conv(Matrix::Bt2020, false),
        );
        assert!(blue[2] >= 253 && blue[0] <= 3 && blue[1] <= 3, "{blue:?}");
    }

    #[test]
    fn chroma_siting_and_flip() {
        // 4×2 luma grey, chroma: left column Cr high, right column neutral.
        let p = Planes {
            width: 4,
            height: 2,
            bits: 8,
            sub: (1, 1),
            y: vec![126; 8],
            u: vec![128, 128],
            v: vec![200, 128],
        };
        let c = |h| Conversion {
            matrix: Matrix::Bt709,
            full_range: false,
            siting: (h, Siting::Half),
        };
        let co = to_rgba(&p, &c(Siting::CoSited), false);
        // Co-sited: luma 0 is exactly chroma 0, luma 1 is halfway, 2 is chroma 1.
        assert!(co[(0, 0)][0] > co[(1, 0)][0] && co[(1, 0)][0] > co[(2, 0)][0]);
        assert_eq!(co[(2, 0)], co[(3, 0)]);
        let half = to_rgba(&p, &c(Siting::Half), false);
        // Centred: luma 0 and 1 are a quarter either side of chroma 0, both clamped
        // to it on the left.
        assert_eq!(half[(0, 0)], co[(0, 0)]);
        assert!(half[(1, 0)][0] > co[(1, 0)][0]);
        // Rows are equal here, so a flip changes nothing; check it swaps rows.
        let mut q = p.clone();
        q.y = vec![16, 16, 16, 16, 235, 235, 235, 235];
        let f = to_rgba(&q, &c(Siting::CoSited), true);
        assert_eq!(f[(3, 0)][1], 255);
        assert_eq!(f[(3, 1)][1], 0);
    }

    #[test]
    fn odd_sizes_and_monochrome() {
        let mut p = flat(8, 235, 128, 128);
        p.width = 5;
        p.height = 3;
        assert_eq!(p.chroma_size(), (3, 2));
        assert_eq!(
            to_rgba(&p, &conv(Matrix::Bt709, false), false)[(4, 2)],
            [255; 4]
        );
        p.u.clear();
        p.v.clear();
        assert_eq!(
            to_rgba(&p, &conv(Matrix::Bt709, false), false)[(0, 0)],
            [255; 4]
        );
    }

    #[test]
    fn tags() {
        let none = Colour::default();
        assert_eq!(
            Conversion::from_tags(&none, None, 1080).matrix,
            Matrix::Bt709
        );
        assert_eq!(
            Conversion::from_tags(&none, None, 480).matrix,
            Matrix::Bt601
        );
        assert!(!Conversion::from_tags(&none, None, 1080).full_range);
        let tagged = Colour {
            matrix: Some(9),
            range: Some(2),
            siting_horz: Some(2),
            siting_vert: Some(1),
            ..Colour::default()
        };
        let c = Conversion::from_tags(&tagged, None, 480);
        assert_eq!(c.matrix, Matrix::Bt2020);
        assert!(c.full_range);
        assert_eq!(c.siting, (Siting::Half, Siting::CoSited));
    }
}
