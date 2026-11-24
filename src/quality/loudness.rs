//! Sound measurements built into the engine (#96 phase 3): loudness and peaks as
//! ITU-R BS.1770-4 defines them. Unlike the picture metrics these aren't a matter of
//! taste, so they aren't pluggable GLSL: they're computed exactly, in Rust, on the
//! CPU, from the soundtrack as `wav` renders it (48 kHz, stereo, before 16-bit).
//!
//! - **Integrated loudness** (LUFS): K-weighted (a high-shelf and a high-pass biquad),
//!   mean square per channel over 400 ms blocks every 100 ms, gated twice: blocks below
//!   −70 LUFS are dropped, then blocks more than 10 LU below the mean of the rest.
//! - **Momentary** (400 ms) and **short-term** (3 s) loudness: the loudest window.
//! - **Sample peak** and **true peak** (dBFS / dBTP): the largest |sample|, and the
//!   largest after 4× oversampling (a windowed-sinc interpolator, as BS.1770's Annex 2
//!   allows), so peaks between samples count.
//!
//! [`Meter`] takes the soundtrack in chunks of interleaved stereo samples, so a long
//! film never needs all of it in memory.

use serde::Serialize;

/// The rate the meter's filter coefficients are for; `quality` renders sound at it.
pub const RATE: u32 = 48_000;

/// BS.1770-4 K-weighting at 48 kHz: stage 1 (high shelf, +4 dB above ~1.5 kHz) and
/// stage 2 (RLB high-pass, ~38 Hz). (b0, b1, b2, a1, a2), a0 = 1.
const SHELF: [f64; 5] = [
    1.535_124_859_586_97,
    -2.691_696_189_406_38,
    1.198_392_810_852_85,
    -1.690_659_293_182_41,
    0.732_480_774_215_85,
];
const HIGH_PASS: [f64; 5] = [1.0, -2.0, 1.0, -1.990_047_454_833_98, 0.990_072_250_366_21];

/// Oversampling for the true peak, and the interpolator's taps per phase.
const OVERSAMPLE: usize = 4;
const TAPS: usize = 16;

/// A biquad in direct form II transposed.
#[derive(Clone, Copy, Default)]
struct Biquad {
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn run(&mut self, c: &[f64; 5], x: f64) -> f64 {
        let y = c[0] * x + self.z1;
        self.z1 = c[1] * x - c[3] * y + self.z2;
        self.z2 = c[2] * x - c[4] * y;
        y
    }
}

/// The interpolator for phase p of `OVERSAMPLE` (p ≥ 1): a Blackman-windowed sinc,
/// taps for samples n − TAPS/2 + 1 ..= n + TAPS/2 around the point n + p/OVERSAMPLE,
/// normalised to unity gain at DC.
fn phase_taps(p: usize) -> [f64; TAPS] {
    let frac = p as f64 / OVERSAMPLE as f64;
    let half = (TAPS / 2) as f64;
    let mut h = [0.0; TAPS];
    for (k, v) in h.iter_mut().enumerate() {
        // Distance from the interpolated point to tap k's sample.
        let t = (k as f64 - (half - 1.0)) - frac;
        let sinc = if t == 0.0 {
            1.0
        } else {
            (std::f64::consts::PI * t).sin() / (std::f64::consts::PI * t)
        };
        // Blackman window over [-half, half].
        let u = (t + half) / (2.0 * half);
        let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * u).cos()
            + 0.08 * (4.0 * std::f64::consts::PI * u).cos();
        *v = sinc * w;
    }
    let sum: f64 = h.iter().sum();
    h.map(|v| v / sum)
}

/// What [`Meter::finish`] reports. `None` = nothing to measure (silence or too short:
/// the momentary needs 400 ms, the short-term 3 s).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Loudness {
    /// Integrated loudness, LUFS.
    pub integrated: Option<f64>,
    /// The loudest 400 ms window, LUFS.
    pub momentary_max: Option<f64>,
    /// The loudest 3 s window, LUFS.
    pub short_term_max: Option<f64>,
    /// The largest |sample|, dBFS.
    pub sample_peak: Option<f64>,
    /// The largest |sample| after 4× oversampling, dBTP.
    pub true_peak: Option<f64>,
    /// Seconds measured.
    pub seconds: f64,
}

/// Streaming BS.1770 meter for stereo at [`RATE`].
pub struct Meter {
    k: [[Biquad; 2]; 2],
    /// Sum of squares of the K-weighted signal (both channels) in the current 100 ms.
    acc: f64,
    in_block: usize,
    /// Completed 100 ms sub-blocks' sums of squares.
    sub: Vec<f64>,
    /// The last `TAPS` samples per channel, oldest first, for the interpolator.
    hist: [[f64; TAPS]; 2],
    phases: Vec<[f64; TAPS]>,
    sample_peak: f64,
    true_peak: f64,
    frames: u64,
}

impl Default for Meter {
    fn default() -> Self {
        Meter::new()
    }
}

impl Meter {
    pub fn new() -> Meter {
        Meter {
            k: [[Biquad::default(); 2]; 2],
            acc: 0.0,
            in_block: 0,
            sub: vec![],
            hist: [[0.0; TAPS]; 2],
            phases: (1..OVERSAMPLE).map(phase_taps).collect(),
            sample_peak: 0.0,
            true_peak: 0.0,
            frames: 0,
        }
    }

    const SUB: usize = (RATE / 10) as usize;

    /// Interleaved stereo samples (L, R, L, R, …).
    pub fn push(&mut self, samples: &[f32]) {
        for lr in samples.as_chunks::<2>().0 {
            for (c, &x) in lr.iter().enumerate() {
                let x = x as f64;
                let [shelf, hp] = &mut self.k[c];
                let y = hp.run(&HIGH_PASS, shelf.run(&SHELF, x));
                self.acc += y * y;
                // Peaks: the sample itself, and the points between the middle two
                // samples of the history window.
                let h = &mut self.hist[c];
                h.copy_within(1.., 0);
                h[TAPS - 1] = x;
                self.sample_peak = self.sample_peak.max(x.abs());
                self.true_peak = self.true_peak.max(x.abs());
                for taps in &self.phases {
                    let v: f64 = taps.iter().zip(h.iter()).map(|(a, b)| a * b).sum();
                    self.true_peak = self.true_peak.max(v.abs());
                }
            }
            self.frames += 1;
            self.in_block += 1;
            if self.in_block == Self::SUB {
                self.sub.push(self.acc);
                self.acc = 0.0;
                self.in_block = 0;
            }
        }
    }

    pub fn finish(&self) -> Loudness {
        let lufs = |ms: f64| (ms > 0.0).then(|| -0.691 + 10.0 * ms.log10());
        // Mean square (both channels summed) of each window of `n` sub-blocks, every
        // 100 ms.
        let windows = |n: usize| -> Vec<f64> {
            self.sub
                .windows(n)
                .map(|w| w.iter().sum::<f64>() / (n * Self::SUB) as f64)
                .collect()
        };
        let blocks = windows(4);
        let loudest = |ws: &[f64]| ws.iter().copied().reduce(f64::max).and_then(lufs);
        // Gating: absolute at −70 LUFS, then relative at 10 LU below the mean of the
        // blocks that passed.
        let abs: Vec<f64> = blocks
            .iter()
            .copied()
            .filter(|z| lufs(*z).is_some_and(|l| l > -70.0))
            .collect();
        let integrated = if abs.is_empty() {
            None
        } else {
            let mean = abs.iter().sum::<f64>() / abs.len() as f64;
            let gate = lufs(mean).unwrap() - 10.0;
            let rel: Vec<f64> = abs
                .iter()
                .copied()
                .filter(|z| lufs(*z).is_some_and(|l| l > gate))
                .collect();
            lufs(rel.iter().sum::<f64>() / rel.len().max(1) as f64)
        };
        let db = |p: f64| (p > 0.0).then(|| 20.0 * p.log10());
        Loudness {
            integrated,
            momentary_max: loudest(&blocks),
            short_term_max: loudest(&windows(30)),
            sample_peak: db(self.sample_peak),
            true_peak: db(self.true_peak),
            seconds: self.frames as f64 / RATE as f64,
        }
    }
}

impl Loudness {
    /// The value `[sound]` names `s`; None when nothing was measured (silence, or
    /// too short for a 400 ms / 3 s window).
    pub fn get(&self, s: super::config::SoundStat) -> Option<f64> {
        use super::config::SoundStat::*;
        match s {
            Integrated => self.integrated,
            MomentaryMax => self.momentary_max,
            ShortTermMax => self.short_term_max,
            SamplePeak => self.sample_peak,
            TruePeak => self.true_peak,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `secs` of a stereo sine: (frequency, amplitude, phase), the same in both channels.
    fn sine(secs: f64, f: f64, a: f64, phase: f64) -> Vec<f32> {
        let n = (secs * RATE as f64) as usize;
        (0..n)
            .flat_map(|i| {
                let v = (a
                    * (2.0 * std::f64::consts::PI * f * i as f64 / RATE as f64 + phase).sin())
                    as f32;
                [v, v]
            })
            .collect()
    }

    fn measure(samples: &[f32]) -> Loudness {
        let mut m = Meter::new();
        // Odd chunk sizes: the meter streams.
        for c in samples.chunks(2 * 1001) {
            m.push(c);
        }
        m.finish()
    }

    fn near(what: &str, got: Option<f64>, want: f64, tol: f64) {
        let g = got.unwrap_or_else(|| panic!("{what}: nothing measured"));
        assert!((g - want).abs() <= tol, "{what}: {g}, want {want} ± {tol}");
    }

    #[test]
    fn a_1khz_sine_reads_its_level() {
        // BS.1770: a 0 dBFS 997 Hz sine in one channel reads −3.01 LKFS, so in both
        // channels 0; at −20 dBFS, −20.
        let l = measure(&sine(5.0, 997.0, 0.1, 0.0));
        near("integrated", l.integrated, -20.0, 0.05);
        near("momentary", l.momentary_max, -20.0, 0.05);
        near("short-term", l.short_term_max, -20.0, 0.05);
        near("sample peak", l.sample_peak, -20.0, 0.01);
        near("true peak", l.true_peak, -20.0, 0.01);
        assert_eq!(l.seconds, 5.0);
    }

    #[test]
    fn k_weighting_cuts_low_and_lifts_high() {
        // The RLB high-pass takes several dB off 30 Hz; the shelf adds ~+4.04 dB at
        // 10 kHz, of which the −0.691 offset already cancels 1 kHz's +0.69: 3.35 dB up.
        let low = measure(&sine(5.0, 30.0, 0.1, 0.0)).integrated.unwrap();
        let high = measure(&sine(5.0, 10_000.0, 0.1, 0.0)).integrated.unwrap();
        assert!(low < -22.0, "30 Hz: {low}");
        near("10 kHz", Some(high), -16.65, 0.1);
    }

    #[test]
    fn gating_drops_silence_and_quiet_passages() {
        let loud = sine(20.0, 997.0, 0.1, 0.0);
        // Silence (absolute gate) doesn't lower the integrated loudness, apart from the
        // three 400 ms blocks that straddle the edge (partly loud: they pass both gates).
        let mut with_silence = loud.clone();
        with_silence.extend(std::iter::repeat_n(0.0f32, 2 * 4 * RATE as usize));
        near(
            "with silence",
            measure(&with_silence).integrated,
            -20.0,
            0.05,
        );
        // …nor does a passage 30 dB down (relative gate, 10 LU below).
        let mut with_quiet = loud.clone();
        with_quiet.extend(sine(20.0, 997.0, 0.1 / 31.62, 0.0));
        near("with quiet", measure(&with_quiet).integrated, -20.0, 0.1);
        // A passage 6 dB down is above the gate: it counts.
        let mut with_soft = loud;
        with_soft.extend(sine(20.0, 997.0, 0.05, 0.0));
        let both = measure(&with_soft).integrated.unwrap();
        assert!((-23.0..-20.5).contains(&both), "{both}");
    }

    #[test]
    fn true_peak_finds_peaks_between_samples() {
        // fs/4 at 45°: every sample is ±0.707 A, the waveform peaks at A between them.
        // A 20 ms fade-in: a sine that starts abruptly really does overshoot when
        // band-limited (Gibbs), and a true-peak meter rightly reads that.
        let mut s = sine(1.0, RATE as f64 / 4.0, 0.5, std::f64::consts::FRAC_PI_4);
        let fade = RATE as usize / 50;
        for (i, lr) in s.as_chunks_mut::<2>().0.iter_mut().take(fade).enumerate() {
            let g = (0.5 - 0.5 * (std::f64::consts::PI * i as f64 / fade as f64).cos()) as f32;
            lr.iter_mut().for_each(|v| *v *= g);
        }
        let l = measure(&s);
        near(
            "sample peak",
            l.sample_peak,
            20.0 * (0.5f64 / 2f64.sqrt()).log10(),
            0.01,
        );
        near("true peak", l.true_peak, 20.0 * 0.5f64.log10(), 0.05);
    }

    #[test]
    fn silence_and_short_sounds_measure_nothing() {
        let l = measure(&vec![0.0; 2 * RATE as usize]);
        assert_eq!(
            (l.integrated, l.sample_peak, l.true_peak),
            (None, None, None)
        );
        let short = measure(&sine(1.0, 997.0, 0.1, 0.0));
        assert_eq!(short.short_term_max, None); // under 3 s
        assert!(short.integrated.is_some());
    }

    #[test]
    fn interpolator_taps_sum_to_one() {
        for p in 1..OVERSAMPLE {
            let s: f64 = phase_taps(p).iter().sum();
            assert!((s - 1.0).abs() < 1e-12);
        }
    }
}
