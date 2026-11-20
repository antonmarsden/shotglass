//! Golden references. Pictures (`strip --compare`): a rendered contact sheet against a
//! stored reference, with a per-pixel tolerance and a budget of pixels allowed to
//! differ beyond it (software OpenGL and real GPUs round differently, and hash noise
//! built on `sin` differs between drivers, so exact equality is too strict).
//! Sound (`wav --compare`): a soundtrack against a stored WAV, by the RMS and peak of
//! the difference in short windows, reporting the first time that differs.

use crate::gfx::Frame;
use anyhow::{Context, Result, bail};
use std::path::Path;
use toodee::{TooDeeOps, TooDeeOpsMut};

/// How far a picture may drift from its reference.
#[derive(Clone, Copy, Debug)]
pub struct Tolerance {
    /// A pixel differs when any of R, G, B is off by more than this (out of 255).
    pub per_pixel: u8,
    /// Fraction of pixels (0..1) allowed to differ.
    pub budget: f32,
    /// Largest allowed average difference over all pixels (out of 255): catches a
    /// small shift everywhere (a colour or gamma tweak) that stays under `per_pixel`.
    pub mean: f32,
}

/// Measured on the demo film and the template (320x180): llvmpipe and an NVIDIA GPU
/// differ by at most 3/255 per channel and 0.08/255 on average, while a one-line
/// shader change (a darker window light, a gamma tweak) moves hundreds of pixels by
/// more than 8 or the average by 0.3 or more.
impl Default for Tolerance {
    fn default() -> Self {
        Tolerance {
            per_pixel: 8,
            budget: 0.001,
            mean: 0.25,
        }
    }
}

#[derive(Debug)]
pub struct Comparison {
    /// Pixels differing by more than the tolerance.
    pub differing: usize,
    pub total: usize,
    /// Largest channel difference anywhere.
    pub max: u8,
    /// Average over all pixels of the largest channel difference.
    pub mean: f32,
    /// The actual picture dimmed to grey, with differing pixels in magenta.
    pub diff: Frame,
}

impl Comparison {
    pub fn fraction(&self) -> f32 {
        self.differing as f32 / self.total.max(1) as f32
    }

    pub fn passes(&self, tol: Tolerance) -> bool {
        self.fraction() <= tol.budget && self.mean <= tol.mean
    }

    pub fn describe(&self, tol: Tolerance) -> String {
        format!(
            "{:.3}% of pixels ({} of {}) differ by more than {}/255 (budget {:.3}%), \
             mean difference {:.3}/255 (limit {:.3}); largest {}",
            self.fraction() * 100.0,
            self.differing,
            self.total,
            tol.per_pixel,
            tol.budget * 100.0,
            self.mean,
            tol.mean,
            self.max
        )
    }
}

/// Compare RGB (alpha is ignored); the pictures must be the same size.
pub fn compare(actual: &Frame, reference: &Frame, per_pixel: u8) -> Result<Comparison> {
    if actual.size() != reference.size() {
        bail!(
            "the picture is {}x{} but the reference is {}x{} (different --size, --count or --cols?)",
            actual.num_cols(),
            actual.num_rows(),
            reference.num_cols(),
            reference.num_rows()
        );
    }
    let mut diff = Frame::init(actual.num_cols(), actual.num_rows(), [0, 0, 0, 255]);
    let (mut differing, mut max, mut sum) = (0, 0u8, 0u64);
    for ((a, r), d) in actual.cells().zip(reference.cells()).zip(diff.cells_mut()) {
        let delta = (0..3).map(|i| a[i].abs_diff(r[i])).max().unwrap();
        max = max.max(delta);
        sum += delta as u64;
        *d = if delta > per_pixel {
            differing += 1;
            [255, 0, 255, 255]
        } else {
            let grey = ((a[0] as u32 * 2 + a[1] as u32 * 5 + a[2] as u32) / 8 / 3) as u8;
            [grey, grey, grey, 255]
        };
    }
    let total = actual.num_cols() * actual.num_rows();
    Ok(Comparison {
        differing,
        total,
        max,
        mean: sum as f32 / total.max(1) as f32,
        diff,
    })
}

pub fn load_png(path: &Path) -> Result<Frame> {
    let img = image::open(path)
        .with_context(|| format!("reading {}", path.display()))?
        .to_rgba8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    Ok(Frame::from_vec(w, h, img.pixels().map(|p| p.0).collect()))
}

/// Parse "1%", "0.5%" or a plain fraction like "0.01".
pub fn parse_budget(s: &str) -> Result<f32> {
    let s = s.trim();
    let v = match s.strip_suffix('%') {
        Some(p) => p.trim().parse::<f32>().map(|v| v / 100.0),
        None => s.parse::<f32>(),
    }
    .map_err(|_| anyhow::anyhow!("--budget must look like 1% or 0.01"))?;
    if !(0.0..=1.0).contains(&v) {
        bail!("--budget must be between 0% and 100%");
    }
    Ok(v)
}

/// How far a soundtrack may drift from its reference. Both limits apply to the
/// difference signal (actual - reference, full scale = 1.0) in each window of `window`
/// seconds, over both channels.
#[derive(Clone, Copy, Debug)]
pub struct SoundTolerance {
    /// Largest allowed RMS of the difference in any window.
    pub rms: f32,
    /// Largest allowed single-sample difference.
    pub peak: f32,
    /// Window length in seconds.
    pub window: f32,
}

/// Measured on the demo film at 8 kHz: llvmpipe and an NVIDIA GPU differ by at most
/// 3.9e-4 RMS in a window and 1.7e-3 in a sample (phase rounding in high partials,
/// growing with time), while changing one gain in its music.glsl by 10% (four tried)
/// moves the worst window's RMS to 3.8e-3 or more. (The 16-bit WAV rounds to 3e-5.)
impl Default for SoundTolerance {
    fn default() -> Self {
        SoundTolerance {
            rms: 1.5e-3,
            peak: 1e-2,
            window: 0.05,
        }
    }
}

/// One window of the difference signal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SoundWindow {
    /// Start time in seconds.
    pub t: f32,
    pub rms: f32,
    pub peak: f32,
}

impl SoundWindow {
    fn fails(&self, tol: SoundTolerance) -> bool {
        self.rms > tol.rms || self.peak > tol.peak
    }

    fn describe(&self) -> String {
        format!(
            "at {:.2}s: rms {:.2e}, peak {:.2e}",
            self.t, self.rms, self.peak
        )
    }
}

#[derive(Debug)]
pub struct SoundComparison {
    pub windows: usize,
    /// The first window beyond the tolerance.
    pub first: Option<SoundWindow>,
    /// Windows beyond the tolerance.
    pub failing: usize,
    /// The window with the largest RMS difference.
    pub worst: SoundWindow,
    /// actual - reference, interleaved stereo (for `<out>.diff.wav`).
    pub diff: Vec<f32>,
}

impl SoundComparison {
    pub fn passes(&self) -> bool {
        self.failing == 0
    }

    pub fn describe(&self, tol: SoundTolerance) -> String {
        let limits = format!(
            "limits per {:.0} ms window: rms {:.1e}, peak {:.1e}",
            tol.window * 1000.0,
            tol.rms,
            tol.peak
        );
        match &self.first {
            None => format!("largest difference {} ({limits})", self.worst.describe()),
            Some(first) => format!(
                "first differs {}; {} of {} windows differ, worst {} ({limits})",
                first.describe(),
                self.failing,
                self.windows,
                self.worst.describe()
            ),
        }
    }
}

/// Compare two interleaved stereo soundtracks at the same rate and length.
pub fn compare_sound(
    actual: &[f32],
    reference: &[f32],
    rate: u32,
    tol: SoundTolerance,
) -> Result<SoundComparison> {
    if actual.len() != reference.len() {
        bail!(
            "the soundtrack is {:.3}s but the reference is {:.3}s (different duration or --rate?)",
            actual.len() as f32 / 2.0 / rate as f32,
            reference.len() as f32 / 2.0 / rate as f32
        );
    }
    let diff: Vec<f32> = actual.iter().zip(reference).map(|(a, r)| a - r).collect();
    let frames = ((tol.window * rate as f32).round() as usize).max(1);
    let (mut windows, mut failing, mut first) = (0, 0, None);
    let mut worst = SoundWindow {
        t: 0.0,
        rms: 0.0,
        peak: 0.0,
    };
    for (i, chunk) in diff.chunks(frames * 2).enumerate() {
        let sum: f64 = chunk.iter().map(|d| (*d as f64) * (*d as f64)).sum();
        let w = SoundWindow {
            t: (i * frames) as f32 / rate as f32,
            rms: (sum / chunk.len() as f64).sqrt() as f32,
            peak: chunk.iter().fold(0.0f32, |m, d| m.max(d.abs())),
        };
        windows += 1;
        if w.rms > worst.rms {
            worst = w;
        }
        if w.fails(tol) {
            failing += 1;
            first.get_or_insert(w);
        }
    }
    Ok(SoundComparison {
        windows,
        first,
        failing,
        worst,
        diff,
    })
}

/// Read a 16-bit PCM stereo WAV (what `shotglass wav` writes): (rate, interleaved samples).
pub fn load_wav(path: &Path) -> Result<(u32, Vec<f32>)> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    parse_wav(&bytes).with_context(|| format!("reading {}", path.display()))
}

fn parse_wav(b: &[u8]) -> Result<(u32, Vec<f32>)> {
    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        bail!("not a WAV file");
    }
    let (mut rate, mut pos) = (None, 12);
    while pos + 8 <= b.len() {
        let (id, len) = (&b[pos..pos + 4], u32_at(pos + 4) as usize);
        let body = pos + 8;
        let end = body
            .checked_add(len)
            .filter(|e| *e <= b.len())
            .ok_or_else(|| anyhow::anyhow!("truncated {} chunk", String::from_utf8_lossy(id)))?;
        match id {
            b"fmt " => {
                if len < 16 {
                    bail!("truncated fmt chunk");
                }
                let (format, channels, bits) = (u16_at(body), u16_at(body + 2), u16_at(body + 14));
                if (format, channels, bits) != (1, 2, 16) {
                    bail!(
                        "expected 16-bit PCM stereo (as `shotglass wav` writes), got format \
                         {format}, {channels} channels, {bits} bits"
                    );
                }
                rate = Some(u32_at(body + 4));
            }
            b"data" => {
                let rate = rate.ok_or_else(|| anyhow::anyhow!("data before the fmt chunk"))?;
                let samples = b[body..end]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|s| i16::from_le_bytes(*s) as f32 / 32767.0)
                    .collect();
                return Ok((rate, samples));
            }
            _ => {}
        }
        pos = end + (len & 1); // chunks are padded to even lengths
    }
    bail!("no data chunk")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sound_tol() -> SoundTolerance {
        SoundTolerance {
            rms: 0.01,
            peak: 0.05,
            window: 0.01,
        }
    }

    #[test]
    fn identical_sound_passes() {
        let a: Vec<f32> = (0..2000).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
        let c = compare_sound(&a, &a, 1000, sound_tol()).unwrap();
        assert!(c.passes());
        assert_eq!((c.windows, c.failing, c.first), (100, 0, None));
        assert_eq!(c.worst.rms, 0.0);
    }

    #[test]
    fn sound_reports_the_first_difference() {
        let rate = 1000; // 10-frame (20-sample) windows
        let a = vec![0.25f32; 2000];
        let mut b = a.clone();
        b[2 * 345] += 0.001; // tiny: within both limits
        for s in &mut b[2 * 500..2 * 520] {
            *s += 0.02; // a quiet shift over two windows: rms only
        }
        b[2 * 801 + 1] -= 0.3; // one click: peak (and rms) in the window at 0.80 s
        let c = compare_sound(&b, &a, rate, sound_tol()).unwrap();
        assert_eq!(c.failing, 3);
        let first = c.first.unwrap();
        assert!((first.t - 0.5).abs() < 1e-6, "{first:?}");
        assert!((first.rms - 0.02).abs() < 1e-4 && (first.peak - 0.02).abs() < 1e-4);
        assert!((c.worst.t - 0.8).abs() < 1e-6 && (c.worst.peak - 0.3).abs() < 1e-4);
        assert!((c.diff[2 * 801 + 1] + 0.3).abs() < 1e-6);
        let text = c.describe(sound_tol());
        assert!(
            text.contains("at 0.50s") && text.contains("3 of 100"),
            "{text}"
        );
        // a click alone fails on the peak even when its window's rms is small
        let mut d = a.clone();
        d[7] += 0.06;
        let tol = SoundTolerance {
            window: 0.1,
            ..sound_tol()
        };
        let c = compare_sound(&d, &a, rate, tol).unwrap();
        assert!(c.first.unwrap().rms < tol.rms && !c.passes());
    }

    #[test]
    fn sound_length_mismatch_is_an_error() {
        let e = compare_sound(&[0.0; 200], &[0.0; 100], 100, sound_tol()).unwrap_err();
        assert!(e.to_string().contains("1.000s") && e.to_string().contains("0.500s"));
    }

    #[test]
    fn wav_round_trip() {
        let dir = std::env::temp_dir().join(format!("shotglass-golden-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.wav");
        let samples = [0.0, 0.5, -0.5, 1.0, -1.0, 0.25];
        let mut w = crate::audio::WavWriter::create(&p, samples.len() / 2, 22050).unwrap();
        w.write(&samples).unwrap();
        w.finish().unwrap();
        let (rate, back) = load_wav(&p).unwrap();
        assert_eq!((rate, back.len()), (22050, samples.len()));
        for (a, b) in samples.iter().zip(&back) {
            assert!((a - b).abs() <= 1.0 / 32767.0, "{a} vs {b}");
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(parse_wav(b"RIFF\0\0\0\0WAVE").is_err());
        assert!(parse_wav(b"not a wav at all").is_err());
    }

    fn frame(w: usize, h: usize, c: [u8; 4]) -> Frame {
        Frame::init(w, h, c)
    }

    #[test]
    fn identical_pictures_pass() {
        let a = frame(8, 4, [10, 20, 30, 255]);
        let c = compare(&a, &a.clone(), 0).unwrap();
        assert_eq!((c.differing, c.max, c.total), (0, 0, 32));
        assert_eq!(c.mean, 0.0);
        assert!(c.passes(Tolerance {
            per_pixel: 0,
            budget: 0.0,
            mean: 0.0
        }));
    }

    #[test]
    fn tolerance_and_budget() {
        let a = frame(10, 10, [100, 100, 100, 255]);
        let mut b = a.clone();
        b[(0, 0)] = [110, 100, 100, 255]; // off by 10: within 24
        b[(1, 0)] = [100, 200, 100, 255]; // off by 100
        b[(2, 0)] = [100, 100, 100, 0]; // alpha only: ignored
        let c = compare(&a, &b, 24).unwrap();
        assert_eq!((c.differing, c.max), (1, 100));
        assert_eq!(c.mean, 1.1); // (10 + 100) / 100 pixels
        let tol = Tolerance {
            per_pixel: 24,
            budget: 0.01,
            mean: 2.0,
        };
        assert!(c.passes(tol));
        assert!(!c.passes(Tolerance { budget: 0.0, ..tol }));
        assert!(!c.passes(Tolerance { mean: 1.0, ..tol }));
        assert_eq!(c.diff[(1, 0)], [255, 0, 255, 255]);
        assert_ne!(c.diff[(0, 0)], [255, 0, 255, 255]);
        assert!(compare(&a, &b, 9).unwrap().differing == 2);
    }

    #[test]
    fn a_small_shift_everywhere_fails_on_the_mean() {
        let a = frame(10, 10, [100, 100, 100, 255]);
        let b = frame(10, 10, [103, 100, 100, 255]);
        let c = compare(&a, &b, 24).unwrap();
        assert_eq!((c.differing, c.mean), (0, 3.0));
        assert!(!c.passes(Tolerance::default()));
    }

    #[test]
    fn size_mismatch_is_an_error() {
        let e = compare(&frame(4, 4, [0; 4]), &frame(4, 5, [0; 4]), 0).unwrap_err();
        assert!(e.to_string().contains("4x4") && e.to_string().contains("4x5"));
    }

    #[test]
    fn budgets() {
        assert_eq!(parse_budget("1%").unwrap(), 0.01);
        assert_eq!(parse_budget(" 0.5 % ").unwrap(), 0.005);
        assert_eq!(parse_budget("0.02").unwrap(), 0.02);
        assert!(parse_budget("150%").is_err());
        assert!(parse_budget("lots").is_err());
    }

    #[test]
    fn png_round_trip() {
        let dir = std::env::temp_dir().join(format!("shotglass-golden-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.png");
        let mut a = frame(3, 2, [1, 2, 3, 255]);
        a[(2, 1)] = [200, 100, 50, 255];
        crate::save_png(&p, &a).unwrap();
        assert_eq!(load_png(&p).unwrap(), a);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
