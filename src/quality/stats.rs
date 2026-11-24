//! Reduced values per frame and over frames, percentiles from histograms, limits,
//! and the report (`quality.json`, heatmaps).

use super::config::{BINS, Bound, Metric, SoundConfig, SoundStat, Stat};
use super::loudness::Loudness;
use crate::gfx::Frame;
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

/// One metric on one frame, reduced on the GPU.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Reduced {
    /// Mean of the finite measures (None if there are none).
    pub mean: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Share of finite pixels above the metric's threshold (None without one).
    pub over: Option<f64>,
    /// Pixels with a finite measure, and with NaN/Inf (`invalid`).
    pub pixels: u64,
    pub invalid: u64,
}

impl Reduced {
    /// From the reduction's last texels: (sum, max, finite, non-finite) and
    /// (over, min, -, -).
    pub fn from_texels(a: [f32; 4], b: [f32; 4], threshold: bool) -> Reduced {
        let n = a[2].max(0.0) as u64;
        let some = |v: f64| (n > 0).then_some(v);
        Reduced {
            mean: some(a[0] as f64 / n.max(1) as f64),
            min: some(b[1] as f64),
            max: some(a[1] as f64),
            over: if threshold {
                some(b[0].max(0.0) as f64 / n.max(1) as f64)
            } else {
                None
            },
            pixels: n,
            invalid: a[3].max(0.0) as u64,
        }
    }
}

/// A metric over every sample frame.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Totals {
    pub mean: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub over: Option<f64>,
    /// "p50", "p95", "p99" and any percentile a limit names, from the histogram
    /// (exact to a bin: range width / 256), within [min, max].
    pub percentiles: BTreeMap<String, f64>,
    pub pixels: u64,
    pub invalid: u64,
    /// The histogram's range and its counts of finite measures (`BINS` bins; values
    /// outside the range are counted in the end bins).
    pub range: [f64; 2],
    pub histogram: Vec<u64>,
}

/// The value below which `p` percent of the counts lie, interpolated within its bin.
pub fn percentile(hist: &[u64], range: (f64, f64), p: f64) -> Option<f64> {
    let n: u64 = hist.iter().sum();
    if n == 0 {
        return None;
    }
    let target = p / 100.0 * n as f64;
    let w = (range.1 - range.0) / hist.len() as f64;
    let mut acc = 0.0;
    for (i, &c) in hist.iter().enumerate() {
        if c > 0 && acc + c as f64 >= target {
            let frac = ((target - acc) / c as f64).clamp(0.0, 1.0);
            return Some(range.0 + (i as f64 + frac) * w);
        }
        acc += c as f64;
    }
    Some(range.1)
}

impl Totals {
    /// Over frames: pixel-weighted mean and over, extreme min/max, summed counts and
    /// histograms.
    pub fn combine(metric: &Metric, xs: &[Reduced], hists: &[Vec<u64>]) -> Totals {
        let pixels: u64 = xs.iter().map(|r| r.pixels).sum();
        let weighted = |f: fn(&Reduced) -> Option<f64>| -> Option<f64> {
            let mut any = false;
            let s: f64 = xs
                .iter()
                .filter_map(|r| {
                    let v = f(r)?;
                    any = true;
                    Some(v * r.pixels as f64)
                })
                .sum();
            (any && pixels > 0).then(|| s / pixels as f64)
        };
        let mut histogram = vec![0u64; BINS];
        for h in hists {
            for (t, c) in histogram.iter_mut().zip(h) {
                *t += c;
            }
        }
        let min = xs.iter().filter_map(|r| r.min).reduce(f64::min);
        let max = xs.iter().filter_map(|r| r.max).reduce(f64::max);
        let percentiles = metric
            .percentiles()
            .into_iter()
            .filter_map(|p| {
                let v = percentile(&histogram, metric.range, p as f64)?;
                let v = v.clamp(min.unwrap_or(v), max.unwrap_or(v));
                Some((format!("p{p}"), v))
            })
            .collect();
        Totals {
            mean: weighted(|r| r.mean),
            min,
            max,
            over: weighted(|r| r.over),
            percentiles,
            pixels,
            invalid: xs.iter().map(|r| r.invalid).sum(),
            range: [metric.range.0, metric.range.1],
            histogram,
        }
    }

    pub fn get(&self, s: Stat) -> Option<f64> {
        match s {
            Stat::Mean => self.mean,
            Stat::Min => self.min,
            Stat::Max => self.max,
            Stat::Over => self.over,
            Stat::P(p) => self.percentiles.get(&format!("p{p}")).copied(),
        }
    }
}

/// Gate a metric's totals: `invalid` always, then its limits.
pub fn judge(metric: &Metric, total: &Totals) -> Vec<String> {
    let mut f = vec![];
    if total.invalid > 0 {
        f.push(format!(
            "invalid: {} pixel(s) with a NaN or Inf measure",
            total.invalid
        ));
    }
    for (s, b) in &metric.limits {
        f.extend(b.violation(&s.to_string(), total.get(*s), "no finite pixels to measure"));
    }
    f
}

#[derive(Debug, Clone, Serialize)]
pub struct FrameReport {
    pub index: usize,
    pub time: f32,
    pub frame: i32,
    /// The film frames `@prev` and `@next` showed, when a metric read them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<u64>,
    /// GPU time of the 1× render (`cost`); None without a GPU timer.
    pub cost_ms: Option<f32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricReport {
    pub name: String,
    #[serde(rename = "use")]
    pub use_: String,
    pub library_version: Option<String>,
    /// The render variants it read: "frame", "reference", "prev", "next".
    pub variants: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
    /// Per frame, in `frames` order.
    pub frames: Vec<Reduced>,
    pub total: Totals,
    /// Limits by stat name.
    pub limits: BTreeMap<String, Bound>,
    pub failures: Vec<String>,
    pub pass: bool,
    #[serde(skip)]
    pub heatmap: Option<Frame>,
}

/// `[sound]`: the soundtrack's loudness and peaks, judged against its limits.
#[derive(Debug, Clone, Serialize)]
pub struct SoundReport {
    /// None: the film has no sound.
    pub measured: Option<Loudness>,
    /// Limits by name (`integrated`, `true_peak`, …).
    pub limits: BTreeMap<String, Bound>,
    pub failures: Vec<String>,
    pub pass: bool,
}

impl SoundReport {
    pub fn judge(cfg: &SoundConfig, measured: Option<Loudness>) -> SoundReport {
        let mut failures = vec![];
        for (stat, b) in &cfg.limits {
            let v = measured.as_ref().and_then(|l| l.get(*stat));
            let missing = "not measured (no sound, silence, or too short)";
            failures.extend(b.violation(stat.name(), v, missing));
        }
        SoundReport {
            measured,
            limits: cfg
                .limits
                .iter()
                .map(|(s, b)| (s.name().to_string(), *b))
                .collect(),
            pass: failures.is_empty(),
            failures,
        }
    }

    pub fn value(&self, s: SoundStat) -> Option<f64> {
        self.measured.as_ref()?.get(s)
    }

    fn print(&self) {
        let Some(l) = &self.measured else {
            println!(
                "{:<5} sound            (the film has no sound)",
                if self.pass { "ok" } else { "FAIL" }
            );
            return self.failures.iter().for_each(|f| println!("      {f}"));
        };
        let db = |v: Option<f64>| v.map_or("-".into(), |v| format!("{v:.2}"));
        println!(
            "{:<5} sound            integrated {} LUFS  momentary max {}  short-term max {}  \
             sample peak {} dBFS  true peak {} dBTP   ({:.2} s)",
            if self.pass { "ok" } else { "FAIL" },
            db(l.integrated),
            db(l.momentary_max),
            db(l.short_term_max),
            db(l.sample_peak),
            db(l.true_peak),
            l.seconds
        );
        for f in &self.failures {
            println!("      {f}");
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema: u32,
    pub engine: &'static str,
    pub film: String,
    pub size: [u32; 2],
    pub reference: u32,
    pub frames: Vec<FrameReport>,
    pub metrics: Vec<MetricReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sound: Option<SoundReport>,
    pub pass: bool,
}

fn fmt_opt(v: Option<f64>) -> String {
    v.map_or("-".into(), |v| format!("{v:.6}"))
}

impl Report {
    pub fn print(&self) {
        for f in &self.frames {
            let cost = f.cost_ms.map_or("-".into(), |c| format!("{c:.2} ms"));
            println!(
                "#{:<3} {:>7.2}s  frame {:<6} cost {cost}",
                f.index, f.time, f.frame
            );
        }
        for m in &self.metrics {
            let v = m
                .library_version
                .as_ref()
                .map_or(String::new(), |v| format!(" {v}"));
            let t = &m.total;
            let over = t
                .over
                .map_or(String::new(), |o| format!("  over {:.4}%", o * 100.0));
            println!(
                "{:<5} {:<16} mean {:>10}  p95 {:>10}  max {:>10}{over}  invalid {}   ({}{v})",
                if m.pass { "ok" } else { "FAIL" },
                m.name,
                fmt_opt(t.mean),
                fmt_opt(t.get(Stat::P(95))),
                fmt_opt(t.max),
                t.invalid,
                m.use_
            );
            for f in &m.failures {
                println!("      {f}");
            }
        }
        if let Some(s) = &self.sound {
            s.print();
        }
    }

    /// `quality.json` plus one heatmap contact sheet per metric (`<name>.png`).
    pub fn write(&self, out: &Path) -> Result<()> {
        std::fs::create_dir_all(out)?;
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(out.join("quality.json"), json + "\n")?;
        for m in &self.metrics {
            if let Some(h) = &m.heatmap {
                crate::save_png(&out.join(format!("{}.png", m.name)), h)?;
            }
        }
        Ok(())
    }

    /// The names of the metrics (and `sound`) that failed.
    pub fn failed(&self) -> Vec<&str> {
        let metrics = self
            .metrics
            .iter()
            .filter(|x| !x.pass)
            .map(|x| x.name.as_str());
        let sound = self.sound.as_ref().filter(|s| !s.pass).map(|_| "sound");
        metrics.chain(sound).collect()
    }

    /// Metric `i`'s value of `s`, for `--tighten`.
    pub fn value(&self, i: usize, s: Stat) -> Option<f64> {
        self.metrics.get(i)?.total.get(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric(limits: Vec<(Stat, Bound)>) -> Metric {
        Metric {
            name: "x".into(),
            use_: "q/x".into(),
            defines: vec![],
            range: (0.0, 1.0),
            threshold: Some(0.5),
            limits,
        }
    }

    fn r(mean: f64, min: f64, max: f64, over: f64, pixels: u64, invalid: u64) -> Reduced {
        Reduced {
            mean: Some(mean),
            min: Some(min),
            max: Some(max),
            over: Some(over),
            pixels,
            invalid,
        }
    }

    #[test]
    fn texels_to_reduced() {
        let x = Reduced::from_texels([6.0, 3.0, 4.0, 2.0], [1.0, -1.0, 0.0, 0.0], true);
        assert_eq!(x, r(1.5, -1.0, 3.0, 0.25, 4, 2));
        let none = Reduced::from_texels([0.0, -3.0e38, 0.0, 5.0], [0.0, 3.0e38, 0.0, 0.0], true);
        assert_eq!((none.max, none.min, none.over), (None, None, None));
        assert_eq!(
            Reduced::from_texels([6.0, 3.0, 4.0, 2.0], [1.0, -1.0, 0.0, 0.0], false).over,
            None
        );
    }

    #[test]
    fn percentiles_interpolate_within_a_bin() {
        // 100 values: 50 in bin 0, 50 in bin 3, of 4 bins over [0, 4).
        let h = [50, 0, 0, 50];
        assert_eq!(percentile(&h, (0.0, 4.0), 25.0), Some(0.5));
        assert_eq!(percentile(&h, (0.0, 4.0), 50.0), Some(1.0));
        assert_eq!(percentile(&h, (0.0, 4.0), 75.0), Some(3.5));
        assert_eq!(percentile(&h, (0.0, 4.0), 100.0), Some(4.0));
        assert_eq!(percentile(&[0, 0], (0.0, 1.0), 50.0), None);
    }

    #[test]
    fn combines_frames_and_judges() {
        let m = metric(vec![
            (
                Stat::Mean,
                Bound {
                    min: None,
                    max: Some(3.0),
                },
            ),
            (
                Stat::Max,
                Bound {
                    min: Some(1.0),
                    max: None,
                },
            ),
            (
                Stat::P(90),
                Bound {
                    min: None,
                    max: Some(0.5),
                },
            ),
        ]);
        let a = r(1.0, 0.0, 2.0, 0.1, 10, 0);
        let b = r(4.0, -1.0, 5.0, 0.5, 30, 1);
        let mut h1 = vec![0; BINS];
        h1[0] = 10;
        let mut h2 = vec![0; BINS];
        h2[255] = 30;
        let t = Totals::combine(&m, &[a, b], &[h1, h2]);
        assert_eq!(t.mean, Some(3.25));
        assert_eq!((t.min, t.max), (Some(-1.0), Some(5.0)));
        assert_eq!(t.over, Some(0.4));
        assert_eq!((t.pixels, t.invalid), (40, 1));
        assert_eq!(t.histogram.iter().sum::<u64>(), 40);
        assert_eq!(
            t.percentiles.keys().collect::<Vec<_>>(),
            ["p50", "p90", "p95", "p99"]
        );
        // p90: 36 of 40, in the top bin: 255/256 + (26/30)/256.
        let p90 = t.get(Stat::P(90)).unwrap();
        assert!((p90 - (255.0 + 26.0 / 30.0) / 256.0).abs() < 1e-12, "{p90}");
        let f = judge(&m, &t);
        assert_eq!(f.len(), 3, "{f:?}");
        assert!(f[0].starts_with("invalid: 1"));
        assert!(f[1].starts_with("mean 3.25"));
        assert!(f[2].starts_with("p90 0.99"));
        // A set bound with nothing finite to measure fails.
        let empty = Totals::combine(&m, &[], &[]);
        assert!(!judge(&m, &empty).is_empty());
    }

    #[test]
    fn percentiles_stay_within_min_and_max() {
        let m = metric(vec![]);
        let mut h = vec![0; BINS];
        h[128] = 4; // all at 0.5..0.504; measured exactly 0.5
        let t = Totals::combine(&m, &[r(0.5, 0.5, 0.5, 0.0, 4, 0)], &[h]);
        assert_eq!(t.get(Stat::P(99)), Some(0.5));
    }
}
