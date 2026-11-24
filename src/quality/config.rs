//! `quality.toml`: metrics, their params and limits, and the ratchet (`--tighten`).

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::movie::Movie;
use crate::shader::define_value;

/// The config's file name, next to `movie.toml`.
pub const QUALITY_FILE: &str = "quality.toml";

/// The largest supersampling factor for `@reference`.
pub const MAX_REFERENCE: u32 = 8;

/// Histogram bins: percentiles are exact to (range width) / BINS.
pub const BINS: usize = 256;

/// A reduced value a limit can name: `mean`, `min`, `max`, `over` (the share of
/// pixels above the metric's `threshold`) or a percentile `p1`..`p99`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stat {
    Mean,
    Min,
    Max,
    Over,
    P(u8),
}

impl Stat {
    pub fn parse(key: &str) -> Option<Stat> {
        Some(match key {
            "mean" => Stat::Mean,
            "min" => Stat::Min,
            "max" => Stat::Max,
            "over" => Stat::Over,
            _ => {
                let p: u8 = key.strip_prefix('p')?.parse().ok()?;
                if !(1..=99).contains(&p) || key.len() > 3 || key.starts_with("p0") {
                    return None;
                }
                Stat::P(p)
            }
        })
    }
}

impl std::fmt::Display for Stat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stat::Mean => write!(f, "mean"),
            Stat::Min => write!(f, "min"),
            Stat::Max => write!(f, "max"),
            Stat::Over => write!(f, "over"),
            Stat::P(p) => write!(f, "p{p}"),
        }
    }
}

/// Limits on a reduced value; either side optional.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bound {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

impl Bound {
    pub fn is_set(&self) -> bool {
        self.min.is_some() || self.max.is_some()
    }

    /// Why `v` is outside, if it is. No value at all fails a set bound, saying
    /// `missing` (why there was nothing to measure).
    pub fn violation(&self, what: &str, v: Option<f64>, missing: &str) -> Option<String> {
        if !self.is_set() {
            return None;
        }
        let Some(v) = v else {
            return Some(format!("{what}: {missing}"));
        };
        if let Some(lo) = self.min.filter(|lo| v < *lo) {
            return Some(format!("{what} {v:.6} < min {lo}"));
        }
        if let Some(hi) = self.max.filter(|hi| v > *hi) {
            return Some(format!("{what} {v:.6} > max {hi}"));
        }
        None
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    count: Option<usize>,
    at: Option<Vec<toml::Value>>,
    size: Option<String>,
    reference: Option<u32>,
    #[serde(default)]
    metric: Vec<toml::Table>,
    sound: Option<toml::Table>,
}

/// A sound measurement the engine makes (`loudness.rs`), named in `[sound]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SoundStat {
    Integrated,
    MomentaryMax,
    ShortTermMax,
    SamplePeak,
    TruePeak,
}

impl SoundStat {
    pub const ALL: [SoundStat; 5] = [
        SoundStat::Integrated,
        SoundStat::MomentaryMax,
        SoundStat::ShortTermMax,
        SoundStat::SamplePeak,
        SoundStat::TruePeak,
    ];

    pub fn name(self) -> &'static str {
        match self {
            SoundStat::Integrated => "integrated",
            SoundStat::MomentaryMax => "momentary_max",
            SoundStat::ShortTermMax => "short_term_max",
            SoundStat::SamplePeak => "sample_peak",
            SoundStat::TruePeak => "true_peak",
        }
    }

    pub fn parse(key: &str) -> Option<SoundStat> {
        SoundStat::ALL.into_iter().find(|s| s.name() == key)
    }
}

/// `[sound]`: the soundtrack is measured (loudness, peaks) and these limits apply.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SoundConfig {
    pub limits: Vec<(SoundStat, Bound)>,
    /// `[[sound.metric]]`: GLSL metrics over the samples (`mainSoundMetric`).
    pub metrics: Vec<Metric>,
}

/// A `{ min, max }` limit; `ctx` names it in errors (`metric "x": mean`).
fn parse_bound(v: &toml::Value, ctx: &str) -> Result<Bound> {
    let b: Bound = v.clone().try_into().context(ctx.to_string())?;
    if let (Some(lo), Some(hi)) = (b.min, b.max)
        && lo > hi
    {
        bail!("{ctx} has min {lo} > max {hi}");
    }
    Ok(b)
}

/// `[sound]`; `taken` are the picture metrics (names are unique across both).
fn parse_sound(t: &toml::Table, taken: &[Metric]) -> Result<SoundConfig> {
    let mut limits = vec![];
    // Picture metrics, then sound ones: names are checked against all of them.
    let mut all = taken.to_vec();
    for (k, v) in t {
        if k == "metric" {
            let tables = v
                .as_array()
                .filter(|a| a.iter().all(|x| x.is_table()))
                .ok_or_else(|| anyhow!("[sound]: `metric` must be [[sound.metric]] tables"))?;
            for x in tables {
                let m = parse_metric(x.as_table().unwrap(), &all)
                    .map_err(|e| anyhow!("[[sound.metric]]: {e}"))?;
                all.push(m);
            }
            continue;
        }
        let stat = SoundStat::parse(k).ok_or_else(|| {
            anyhow!(
                "[sound]: unknown field `{k}` (expected {}, or [[sound.metric]])",
                SoundStat::ALL.map(|s| s.name()).join(", ")
            )
        })?;
        let b = parse_bound(v, &format!("[sound]: {k}"))?;
        limits.push((stat, b));
    }
    limits.sort_by_key(|(s, _)| *s);
    let metrics = all.split_off(taken.len());
    Ok(SoundConfig { limits, metrics })
}

/// `quality.toml`, resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub count: usize,
    /// `at = [...]`: explicit sample times (as `--at` spells them: seconds, `shot`,
    /// `shot+offset`), instead of `count` evenly spaced frames.
    pub at: Option<Vec<String>>,
    pub size: (i32, i32),
    /// Supersampling factor N of `@reference` (N×N samples per pixel).
    pub reference: u32,
    pub metrics: Vec<Metric>,
    /// `[sound]`, if present: measure the soundtrack.
    pub sound: Option<SoundConfig>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Metric {
    pub name: String,
    /// `<library>/<name>` or a film-relative `.glsl` path.
    pub use_: String,
    /// `#define` lines' (name, value) in GLSL.
    pub defines: Vec<(String, String)>,
    /// The histogram's range (percentiles); values outside land in the end bins.
    pub range: (f64, f64),
    /// `over` counts pixels whose measure is above this.
    pub threshold: Option<f64>,
    /// Limits, by stat (mean, min, max, over, then percentiles).
    pub limits: Vec<(Stat, Bound)>,
}

impl Metric {
    /// The percentiles the report shows: p50, p95, p99 and any a limit names.
    pub fn percentiles(&self) -> Vec<u8> {
        let mut ps = vec![50, 95, 99];
        for (s, _) in &self.limits {
            if let Stat::P(p) = s {
                ps.push(*p);
            }
        }
        ps.sort_unstable();
        ps.dedup();
        ps
    }
}

fn is_ident(s: &str) -> bool {
    let mut c = s.chars();
    c.next()
        .is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
        && c.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `#rrggbb` → `vec3(r, g, b)` in 0..1 (as written: sRGB-encoded, like the frame).
fn hex_colour(s: &str) -> Option<String> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let ch = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap() as f32 / 255.0;
    Some(define_value(&[ch(0), ch(2), ch(4)]))
}

fn number(v: &toml::Value) -> Option<f64> {
    match v {
        toml::Value::Float(f) if f.is_finite() => Some(*f),
        toml::Value::Integer(i) => Some(*i as f64),
        _ => None,
    }
}

/// A param as `#define`s: a number → float, 2–4 numbers → vecN, `"#rrggbb"` → vec3,
/// a list of colours → `vec3[n](…)` plus `<NAME>_COUNT`.
fn param_defines(metric: &str, name: &str, v: &toml::Value) -> Result<Vec<(String, String)>> {
    if !is_ident(name) || name.starts_with("sg_") || name.starts_with("gl_") {
        bail!("metric \"{metric}\": param \"{name}\" isn't a usable GLSL name");
    }
    let bad = || {
        anyhow!(
            "metric \"{metric}\": param \"{name}\" must be a number, 2-4 numbers, \
             a colour \"#rrggbb\" or a list of colours"
        )
    };
    if let Some(x) = number(v) {
        return Ok(vec![(name.into(), define_value(&[x as f32]))]);
    }
    match v {
        toml::Value::String(s) => Ok(vec![(name.into(), hex_colour(s).ok_or_else(bad)?)]),
        toml::Value::Array(a) if a.iter().all(|x| number(x).is_some()) => {
            if !(2..=4).contains(&a.len()) {
                return Err(bad());
            }
            let xs: Vec<f32> = a.iter().filter_map(number).map(|x| x as f32).collect();
            Ok(vec![(name.into(), define_value(&xs))])
        }
        toml::Value::Array(a) if !a.is_empty() => {
            let cs = a
                .iter()
                .map(|x| x.as_str().and_then(hex_colour).ok_or_else(bad))
                .collect::<Result<Vec<_>>>()?;
            Ok(vec![
                (
                    name.into(),
                    format!("vec3[{}]({})", cs.len(), cs.join(", ")),
                ),
                (format!("{name}_COUNT"), cs.len().to_string()),
            ])
        }
        _ => Err(bad()),
    }
}

/// One `[[metric]]` table: the known keys, then limits (`mean`, `min`, `max`, `over`,
/// `pNN`); anything else is an error, as `deny_unknown_fields` would say.
pub fn parse_metric(t: &toml::Table, taken: &[Metric]) -> Result<Metric> {
    let str_of = |k: &str| -> Result<Option<String>> {
        match t.get(k) {
            None => Ok(None),
            Some(toml::Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => bail!("a [[metric]]'s {k} must be a string"),
        }
    };
    let use_ = str_of("use")?.ok_or_else(|| anyhow!("a [[metric]] has no use = \"…\""))?;
    if use_.trim().is_empty() {
        bail!("a [[metric]] has an empty use");
    }
    let name = match str_of("name")? {
        Some(n) => n,
        None => use_
            .trim_end_matches(".glsl")
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string(),
    };
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!(
            "metric name \"{name}\" must be letters, digits, '-' and '_' (it names the heatmap file)"
        );
    }
    if taken.iter().any(|x| x.name == name) {
        bail!("two metrics are named \"{name}\" (give one a name = \"…\")");
    }
    if !use_.ends_with(".glsl") && !use_.contains('/') {
        bail!(
            "metric \"{name}\": use = \"{use_}\" needs a library (\"quality/{use_}\") or a .glsl file"
        );
    }
    let mut defines = vec![];
    if let Some(p) = t.get("params") {
        let p = p
            .as_table()
            .ok_or_else(|| anyhow!("metric \"{name}\": params must be a table"))?;
        for (k, v) in p {
            defines.extend(param_defines(&name, k, v)?);
        }
    }
    let range = match t.get("range") {
        None => (0.0, 1.0),
        Some(toml::Value::Array(a)) if a.len() == 2 && a.iter().all(|x| number(x).is_some()) => {
            let (lo, hi) = (number(&a[0]).unwrap(), number(&a[1]).unwrap());
            if lo >= hi {
                bail!("metric \"{name}\": range [{lo}, {hi}] must go up");
            }
            (lo, hi)
        }
        Some(_) => bail!("metric \"{name}\": range must be [low, high]"),
    };
    let threshold = match t.get("threshold") {
        None => None,
        Some(v) => Some(
            number(v).ok_or_else(|| anyhow!("metric \"{name}\": threshold must be a number"))?,
        ),
    };
    let mut limits = vec![];
    for (k, v) in t {
        if ["use", "name", "params", "range", "threshold"].contains(&k.as_str()) {
            continue;
        }
        let stat = Stat::parse(k).ok_or_else(|| {
            anyhow!(
                "metric \"{name}\": unknown field `{k}` (expected use, name, params, range, \
                 threshold, or a limit: mean, min, max, over, p1..p99)"
            )
        })?;
        let b = parse_bound(v, &format!("metric \"{name}\": {k}"))?;
        if stat == Stat::Over && threshold.is_none() {
            bail!("metric \"{name}\": an over limit needs threshold = <measure>");
        }
        limits.push((stat, b));
    }
    limits.sort_by_key(|(s, _)| *s);
    Ok(Metric {
        name,
        use_,
        defines,
        range,
        threshold,
        limits,
    })
}

impl Config {
    pub fn parse(text: &str) -> Result<Config> {
        let raw: RawConfig = toml::from_str(text)?;
        let count = raw.count.unwrap_or(8);
        if !(1..=64).contains(&count) {
            bail!("count must be 1 to 64, not {count}");
        }
        let size = crate::parse_size(raw.size.as_deref().unwrap_or("384x216"))?;
        let reference = raw.reference.unwrap_or(4);
        if !(2..=MAX_REFERENCE).contains(&reference) {
            bail!(
                "reference must be 2 to {MAX_REFERENCE} (samples per pixel along each axis), not {reference}"
            );
        }
        let at = match raw.at {
            None => None,
            Some(_) if raw.count.is_some() => bail!("set count or at, not both"),
            Some(xs) if xs.is_empty() => bail!("at = [] has no times"),
            Some(xs) => Some(
                xs.iter()
                    .map(|v| match v {
                        toml::Value::String(s) if !s.contains(',') && !s.trim().is_empty() => {
                            Ok(s.trim().to_string())
                        }
                        v => number(v).map(|x| x.to_string()).ok_or_else(|| {
                            anyhow!("at: {v} isn't a time (seconds, \"shot\" or \"shot+offset\")")
                        }),
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
        };
        if raw.metric.is_empty() && raw.sound.is_none() {
            bail!("nothing to measure: no [[metric]] entries and no [sound] table");
        }
        let mut metrics: Vec<Metric> = vec![];
        for t in &raw.metric {
            let m = parse_metric(t, &metrics)?;
            metrics.push(m);
        }
        let sound = raw
            .sound
            .as_ref()
            .map(|t| parse_sound(t, &metrics))
            .transpose()?;
        Ok(Config {
            count,
            at,
            size,
            reference,
            metrics,
            sound,
        })
    }
}

impl Config {
    /// Every metric, picture ones first, with its kind.
    pub fn all_metrics(&self) -> impl Iterator<Item = (Kind, &Metric)> {
        let sound = self.sound.iter().flat_map(|s| &s.metrics);
        self.metrics
            .iter()
            .map(|x| (Kind::Picture, x))
            .chain(sound.map(|x| (Kind::Sound, x)))
    }

    /// The film's `quality.toml`, if it has one.
    pub fn of_film(m: &Movie) -> Result<Option<Config>> {
        let path = m.root.join(QUALITY_FILE);
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Config::parse(&text)
            .with_context(|| format!("{}", path.display()))
            .map(Some)
    }
}

// ------------------------------------------------------------------ ratchet

/// The smallest change `--tighten` makes to a limit, whatever the margin.
const ABS_MARGIN: f64 = 1e-6;

/// `x` rounded to 3 significant digits, `up` or down, as a tidy decimal.
fn round_sig(x: f64, up: bool) -> f64 {
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    let mag = x.abs().log10().floor() as i32;
    let decimals = (2 - mag).max(0) as usize;
    let step = 10f64.powi(mag - 2);
    let r = if up {
        (x / step - 1e-9).ceil() * step
    } else {
        (x / step + 1e-9).floor() * step
    };
    format!("{r:.decimals$}").parse().unwrap_or(r)
}

/// `--tighten`'s margin for `[sound]` limits, in dB (LU): loudness meters agree to
/// a few tenths, and synthesis differs slightly between drivers.
pub const SOUND_MARGIN_DB: f64 = 0.5;

/// `x` rounded outward to tenths (dB values: LUFS, dBTP), as a tidy decimal.
fn round_tenth(x: f64, up: bool) -> f64 {
    let r = if up {
        (x * 10.0 - 1e-9).ceil() / 10.0
    } else {
        (x * 10.0 + 1e-9).floor() / 10.0
    };
    format!("{r:.1}").parse().unwrap_or(r)
}

/// Move the `min`/`max` of one limit `bound` towards the measured `v` padded by `pad`
/// (`round` outward), only if that's stricter. One line per change.
fn tighten_bound(
    bound: &mut dyn toml_edit::TableLike,
    what: &str,
    v: f64,
    pad: f64,
    round: fn(f64, bool) -> f64,
    changes: &mut Vec<String>,
) {
    for (side, up) in [("max", true), ("min", false)] {
        let old = bound
            .get(side)
            .and_then(|x| x.as_float().or_else(|| x.as_integer().map(|i| i as f64)));
        let Some(old) = old else { continue };
        let new = round(if up { v + pad } else { v - pad }, up);
        let stricter = if up { new < old } else { new > old };
        if stricter {
            bound.insert(side, toml_edit::value(new));
            changes.push(format!("{what} {side} {old} → {new} (measured {v:.6})"));
        }
    }
}

/// `--tighten`: every limit in `text` moves to the measured value plus a margin,
/// rounded outward, but only towards stricter: a max goes down, a min goes up, never
/// back. Metric limits get `margin` (a fraction of the value, at least `ABS_MARGIN`)
/// and 3 significant digits; `[sound]` limits (dB) get `sound_margin` dB and tenths.
/// `metric(i, stat)` is metric i's value, `sound(stat)` the soundtrack's. Comments and
/// layout are kept. Returns the new text and one line per change.
pub fn tighten(
    text: &str,
    margin: f64,
    sound_margin: f64,
    metric: impl Fn(Kind, usize, Stat) -> Option<f64>,
    sound: impl Fn(SoundStat) -> Option<f64>,
) -> Result<(String, Vec<String>)> {
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    let mut changes = vec![];
    if let Some(t) = doc.get_mut("sound").and_then(|t| t.as_table_like_mut()) {
        let keys: Vec<String> = t.iter().map(|(k, _)| k.to_string()).collect();
        for key in keys {
            let (Some(stat), Some(bound)) = (
                SoundStat::parse(&key),
                t.get_mut(&key).and_then(|b| b.as_table_like_mut()),
            ) else {
                continue;
            };
            if let Some(v) = sound(stat) {
                let what = format!("sound: {key}");
                tighten_bound(bound, &what, v, sound_margin, round_tenth, &mut changes);
            }
        }
    }
    if let Some(a) = doc
        .get_mut("sound")
        .and_then(|t| t.get_mut("metric"))
        .and_then(|m| m.as_array_of_tables_mut())
    {
        tighten_metrics(a, margin, |i, s| metric(Kind::Sound, i, s), &mut changes);
    }
    if let Some(a) = doc
        .get_mut("metric")
        .and_then(|m| m.as_array_of_tables_mut())
    {
        tighten_metrics(a, margin, |i, s| metric(Kind::Picture, i, s), &mut changes);
    }
    Ok((doc.to_string(), changes))
}

/// Whether a metric measures frames (`[[metric]]`) or samples (`[[sound.metric]]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Picture,
    Sound,
}

fn tighten_metrics(
    metrics: &mut toml_edit::ArrayOfTables,
    margin: f64,
    metric: impl Fn(usize, Stat) -> Option<f64>,
    changes: &mut Vec<String>,
) {
    for (i, t) in metrics.iter_mut().enumerate() {
        let name = t
            .get("name")
            .and_then(|n| n.as_str())
            .map(String::from)
            .unwrap_or_else(|| format!("metric {}", i + 1));
        let keys: Vec<String> = t.iter().map(|(k, _)| k.to_string()).collect();
        for key in keys {
            let Some(stat) = Stat::parse(&key) else {
                continue;
            };
            let Some(v) = metric(i, stat) else { continue };
            let Some(bound) = t.get_mut(&key).and_then(|b| b.as_table_like_mut()) else {
                continue;
            };
            let pad = (v.abs() * margin).max(ABS_MARGIN);
            let what = format!("{name}: {key}");
            tighten_bound(bound, &what, v, pad, round_sig, changes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_config() {
        let c = Config::parse(
            r##"
count = 4
size = "320x180"
reference = 3

[[metric]]
use = "quality/alias"
mean = { max = 0.02 }
max = { max = 0.5 }
p95 = { max = 0.1 }

[[metric]]
use = "fx/palette.glsl"
name = "pal"
params = { tol = 2, dir = [1, 0.5], ink = "#ff8000", colours = ["#000000", "#ffffff"] }
range = [0, 4]
threshold = 0.5
mean = { min = 0.9 }
over = { max = 0.01 }
"##,
        )
        .unwrap();
        assert_eq!(c.count, 4);
        assert_eq!(c.size, (320, 180));
        assert_eq!(c.reference, 3);
        let a = &c.metrics[0];
        assert_eq!(a.name, "alias");
        assert_eq!((a.range, a.threshold), ((0.0, 1.0), None));
        assert_eq!(
            a.limits,
            vec![
                (
                    Stat::Mean,
                    Bound {
                        min: None,
                        max: Some(0.02)
                    }
                ),
                (
                    Stat::Max,
                    Bound {
                        min: None,
                        max: Some(0.5)
                    }
                ),
                (
                    Stat::P(95),
                    Bound {
                        min: None,
                        max: Some(0.1)
                    }
                ),
            ]
        );
        assert_eq!(a.percentiles(), vec![50, 95, 99]);
        let p = &c.metrics[1];
        assert_eq!(p.name, "pal");
        assert_eq!((p.range, p.threshold), ((0.0, 4.0), Some(0.5)));
        assert_eq!(p.limits[1].0, Stat::Over);
        let d: std::collections::HashMap<_, _> = p.defines.iter().cloned().collect();
        assert_eq!(d["tol"], "2.0");
        assert_eq!(d["dir"], "vec2(1.0, 0.5)");
        assert_eq!(d["ink"], "vec3(1.0, 0.5019608, 0.0)");
        assert_eq!(
            d["colours"],
            "vec3[2](vec3(0.0, 0.0, 0.0), vec3(1.0, 1.0, 1.0))"
        );
        assert_eq!(d["colours_COUNT"], "2");
    }

    #[test]
    fn defaults() {
        let c = Config::parse("[[metric]]\nuse = \"q/m\"\n").unwrap();
        assert_eq!((c.count, c.size, c.reference), (8, (384, 216), 4));
        assert!(c.metrics[0].limits.is_empty());
    }

    #[test]
    fn at_lists_sample_times() {
        let c = Config::parse(
            "at = [\"intro+0.5\", 1.25, 2, \"outro\"]
[[metric]]
use = \"q/m\"
",
        )
        .unwrap();
        assert_eq!(c.at.unwrap(), ["intro+0.5", "1.25", "2", "outro"]);
        for bad in [
            "at = []",
            "at = [true]",
            "at = [\"a,b\"]",
            "count = 3
at = [1]",
        ] {
            let text = format!(
                "{bad}
[[metric]]
use = \"q/m\"
"
            );
            assert!(Config::parse(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn stats_parse() {
        for (k, s) in [
            ("mean", Some(Stat::Mean)),
            ("over", Some(Stat::Over)),
            ("p1", Some(Stat::P(1))),
            ("p99", Some(Stat::P(99))),
            ("p0", None),
            ("p100", None),
            ("p05", None),
            ("p", None),
            ("median", None),
        ] {
            assert_eq!(Stat::parse(k), s, "{k}");
            if let Some(s) = s {
                assert_eq!(s.to_string(), k);
            }
        }
    }

    #[test]
    fn refuses_bad_configs() {
        let bad = [
            ("", "nothing to measure"),
            (
                "[sound]\nloudness = { max = 1 }\n",
                "unknown field `loudness`",
            ),
            (
                "[sound]\ntrue_peak = { min = 0, max = -1 }\n",
                "min 0 > max -1",
            ),
            ("[[metric]]\nname = \"x\"\n", "no use"),
            ("[[metric]]\nuse = \"alias\"\n", "needs a library"),
            (
                "[[metric]]\nuse = \"q/m\"\nbogus = 1\n",
                "unknown field `bogus`",
            ),
            (
                "[[metric]]\nuse = \"q/m\"\np100 = { max = 1 }\n",
                "unknown field `p100`",
            ),
            (
                "reference = 1\n[[metric]]\nuse = \"q/m\"\n",
                "reference must be",
            ),
            (
                "[[metric]]\nuse = \"q/m\"\n[[metric]]\nuse = \"r/m\"\n",
                "two metrics",
            ),
            (
                "[[metric]]\nuse = \"q/m\"\nparams = { c = \"#ff\" }\n",
                "colour",
            ),
            (
                "[[metric]]\nuse = \"q/m\"\nparams = { v = [1, 2, 3, 4, 5] }\n",
                "2-4 numbers",
            ),
            (
                "[[metric]]\nuse = \"q/m\"\nparams = { sg_x = 1 }\n",
                "GLSL name",
            ),
            (
                "[[metric]]\nuse = \"q/m\"\nmean = { min = 2, max = 1 }\n",
                "min 2 > max 1",
            ),
            (
                "[[metric]]\nuse = \"q/m\"\nmax = { below = 1 }\n",
                "unknown field",
            ),
            (
                "[[metric]]\nuse = \"q/m\"\nover = { max = 0.1 }\n",
                "needs threshold",
            ),
            ("[[metric]]\nuse = \"q/m\"\nrange = [1, 1]\n", "must go up"),
            ("[[metric]]\nuse = \"q/m\"\nrange = [0]\n", "[low, high]"),
            (
                "size = \"0x10\"\n[[metric]]\nuse = \"q/m\"\n",
                "out of range",
            ),
        ];
        for (text, want) in bad {
            let e = format!("{:#}", Config::parse(text).unwrap_err());
            assert!(e.contains(want), "{text:?}: {e}");
        }
    }

    #[test]
    fn parses_sound_limits() {
        let c = Config::parse(
            "[sound]\ntrue_peak = { max = -1 }\nintegrated = { min = -18, max = -14 }\n",
        )
        .unwrap();
        assert!(c.metrics.is_empty());
        let limits = c.sound.unwrap().limits;
        assert_eq!(
            limits,
            vec![
                (
                    SoundStat::Integrated,
                    Bound {
                        min: Some(-18.0),
                        max: Some(-14.0)
                    }
                ),
                (
                    SoundStat::TruePeak,
                    Bound {
                        min: None,
                        max: Some(-1.0)
                    }
                ),
            ]
        );
        // An empty [sound] measures and reports, with no limits.
        assert_eq!(
            Config::parse("[sound]\n").unwrap().sound,
            Some(SoundConfig::default())
        );
    }

    #[test]
    fn parses_sound_metrics() {
        let c = Config::parse(
            "[[metric]]
use = \"a.glsl\"
[sound]
true_peak = { max = -1 }
             [[sound.metric]]
use = \"q/clicks.glsl\"
threshold = 8
over = { max = 0 }
",
        )
        .unwrap();
        let s = c.sound.unwrap();
        assert_eq!(s.limits.len(), 1);
        assert_eq!(s.metrics.len(), 1);
        assert_eq!(s.metrics[0].name, "clicks");
        assert_eq!(s.metrics[0].threshold, Some(8.0));
        // Names are unique across picture and sound metrics.
        let e = Config::parse(
            "[[metric]]
use = \"x/clicks.glsl\"
[[sound.metric]]
use = \"q/clicks.glsl\"
",
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("[[sound.metric]]"), "{e:#}");
    }

    #[test]
    fn tighten_sound_metrics() {
        let text = "[sound]
[[sound.metric]]
use = \"c.glsl\"
max = { max = 1 }
";
        let m = |k: Kind, _, s| (k == Kind::Sound && s == Stat::Max).then_some(0.2);
        let (out, changes) = tighten(text, 0.25, 0.5, m, |_| None).unwrap();
        assert!(out.contains("max = { max = 0.25 }"), "{out}");
        assert_eq!(changes.len(), 1);
    }

    #[test]
    fn tighten_sound_limits_in_tenths_of_a_db() {
        let text = "[sound]\nintegrated = { min = -20, max = -10 }\ntrue_peak = { max = 0 }\n";
        let sound = |s: SoundStat| match s {
            SoundStat::Integrated => Some(-16.04),
            SoundStat::TruePeak => Some(-1.23),
            _ => None,
        };
        let (out, changes) = tighten(text, 0.25, 0.5, |_, _, _| None, sound).unwrap();
        assert_eq!(
            out,
            "[sound]\nintegrated = { min = -16.6, max = -15.5 }\ntrue_peak = { max = -0.7 }\n"
        );
        assert_eq!(changes.len(), 3, "{changes:?}");
        assert!(changes[0].starts_with("sound: integrated max -10 → -15.5"));
    }

    #[test]
    fn rounds_to_three_significant_digits() {
        assert_eq!(round_sig(0.0064505, true), 0.00646);
        assert_eq!(round_sig(0.0064505, false), 0.00645);
        assert_eq!(round_sig(123.4, true), 124.0);
        assert_eq!(round_sig(-0.01234, true), -0.0123);
        assert_eq!(round_sig(0.5, true), 0.5);
    }

    #[test]
    fn tighten_only_ever_tightens_and_keeps_comments() {
        let text = "\
# film limits
[[metric]]
use = \"q/alias\"
name = \"alias\"
mean = { max = 0.01 }    # loose for now
p95 = { max = 1 }
min = { min = -5, max = 2 }

[[metric]]
use = \"q/clip\"
mean = { max = 0.0001 }
";
        let measured = |i: usize, s: Stat| match (i, s) {
            (0, Stat::Mean) => Some(0.0059),
            (0, Stat::P(95)) => Some(0.2),
            (0, Stat::Min) => Some(0.0),
            (1, Stat::Mean) => Some(0.000095), // + 10% is above the limit: kept
            _ => None,
        };
        let (out, changes) = tighten(text, 0.1, 0.5, |_, i, s| measured(i, s), |_| None).unwrap();
        assert_eq!(
            out,
            "\
# film limits
[[metric]]
use = \"q/alias\"
name = \"alias\"
mean = { max = 0.00649 }    # loose for now
p95 = { max = 0.22 }
min = { min = -0.000001, max = 0.000001 }

[[metric]]
use = \"q/clip\"
mean = { max = 0.0001 }
"
        );
        assert_eq!(changes.len(), 4, "{changes:?}");
        assert!(changes[0].starts_with("alias: mean max 0.01 → 0.00649"));
        // A second run changes nothing.
        let (again, none) = tighten(&out, 0.1, 0.5, |_, i, s| measured(i, s), |_| None).unwrap();
        assert_eq!(again, out);
        assert!(none.is_empty(), "{none:?}");
    }
}
