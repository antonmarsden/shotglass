//! `shotglass quality` (#96): pluggable quality metrics.
//!
//! The engine provides the measurement contract, not the metrics: a metric is GLSL
//! from a library (`use = "quality/alias"`) or the film (`use = "fx/m.glsl"`) with the
//! entry point `void mainMetric(out vec4 value, in vec2 fragCoord)`. It reads render
//! variants of the sample frame through the metric header: `@frame` (`getFrame(uv)`,
//! the frame at 1×) and `@reference` (`getReference(uv)`, the frame rendered at N×N
//! and box-filtered to 1×, only rendered when a metric calls it). `value.rgb` is the
//! heatmap colour, `value.a` the per-pixel measure.
//!
//! Measures are reduced on the GPU (sum, max, finite count, non-finite count, 4×4
//! blocks per pass), so only one texel per metric and frame comes back for the
//! numbers; heatmaps are read back for their PNGs. Two checks are fixed in the engine:
//! `invalid` (non-finite measures: always fails, and excluded from every reduction)
//! and `cost` (GPU time per frame: reported, never gated).
//!
//! The config is `quality.toml` next to `movie.toml`.

use crate::gfx::{Frame, FrameInfo, Renderer};
use crate::movie::{Movie, TransitionKind};
use crate::shader::{Assembled, Builder, Dialect, define_value, mentions_ident};
use crate::validate::{Source, Stage};
use anyhow::{Context, Result, anyhow, bail};
use eframe::glow::{self, HasContext};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use toodee::TooDeeOps;

pub const QUALITY_FILE: &str = "quality.toml";

/// The largest supersampling factor for `@reference`.
pub const MAX_REFERENCE: u32 = 8;

const METRIC_HEADER: &str = "\
uniform vec3 iResolution;
uniform float iTime;
uniform int iFrame;
uniform sampler2D sg_frame;
uniform sampler2D sg_reference;
out vec4 shotglass_out;
vec4 getFrame(vec2 uv) { return texture(sg_frame, uv); }
vec4 getReference(vec2 uv) { return texture(sg_reference, uv); }
";

const METRIC_FOOTER: &str = "void main(){\n  vec4 v = vec4(0.0);\n  mainMetric(v, gl_FragCoord.xy);\n  shotglass_out = v;\n}";

/// One reduction pass: each output texel sums a 4×4 block. The first pass reads the
/// measure (`.a` of the metric map); later ones read (sum, max, finite, non-finite).
/// Non-finite is tested on the bits (exponent all ones), which no optimiser drops.
const REDUCE: &str = "
uniform sampler2D sg_src;
uniform ivec2 sg_size;
uniform int sg_first;
out vec4 o;
void main() {
  ivec2 base = ivec2(gl_FragCoord.xy) * 4;
  float sum = 0.0, mx = -3.0e38, n = 0.0, bad = 0.0;
  for (int y = 0; y < 4; y++) {
    for (int x = 0; x < 4; x++) {
      ivec2 p = base + ivec2(x, y);
      if (p.x >= sg_size.x || p.y >= sg_size.y) continue;
      vec4 v = texelFetch(sg_src, p, 0);
      if (sg_first == 1) {
        if ((floatBitsToUint(v.a) & 0x7f800000u) == 0x7f800000u) {
          bad += 1.0;
        } else {
          sum += v.a;
          mx = max(mx, v.a);
          n += 1.0;
        }
      } else {
        sum += v.r;
        mx = max(mx, v.g);
        n += v.b;
        bad += v.a;
      }
    }
  }
  o = vec4(sum, mx, n, bad);
}
";

// ------------------------------------------------------------------ config

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    count: Option<usize>,
    size: Option<String>,
    reference: Option<u32>,
    #[serde(default)]
    metric: Vec<RawMetric>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMetric {
    #[serde(rename = "use")]
    use_: String,
    name: Option<String>,
    #[serde(default)]
    params: toml::Table,
    mean: Option<Bound>,
    max: Option<Bound>,
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
    fn is_set(&self) -> bool {
        self.min.is_some() || self.max.is_some()
    }

    /// Why `v` is outside, if it is (no value at all fails a set bound).
    fn violation(&self, what: &str, v: Option<f64>) -> Option<String> {
        if !self.is_set() {
            return None;
        }
        let Some(v) = v else {
            return Some(format!("{what}: no finite pixels to measure"));
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

/// `quality.toml`, resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub count: usize,
    pub size: (i32, i32),
    /// Supersampling factor N of `@reference` (N×N samples per pixel).
    pub reference: u32,
    pub metrics: Vec<Metric>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Metric {
    pub name: String,
    /// `<library>/<name>` or a film-relative `.glsl` path.
    pub use_: String,
    /// `#define` lines' (name, value) in GLSL.
    pub defines: Vec<(String, String)>,
    pub mean: Bound,
    pub max: Bound,
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

fn number(v: &toml::Value) -> Option<f32> {
    match v {
        toml::Value::Float(f) if f.is_finite() => Some(*f as f32),
        toml::Value::Integer(i) => Some(*i as f32),
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
        return Ok(vec![(name.into(), define_value(&[x]))]);
    }
    match v {
        toml::Value::String(s) => Ok(vec![(name.into(), hex_colour(s).ok_or_else(bad)?)]),
        toml::Value::Array(a) if a.iter().all(|x| number(x).is_some()) => {
            if !(2..=4).contains(&a.len()) {
                return Err(bad());
            }
            let xs: Vec<f32> = a.iter().filter_map(number).collect();
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
        if raw.metric.is_empty() {
            bail!("no [[metric]] entries");
        }
        let mut metrics: Vec<Metric> = vec![];
        for m in raw.metric {
            if m.use_.trim().is_empty() {
                bail!("a [[metric]] has an empty use");
            }
            let name = match m.name {
                Some(n) => n,
                None => m
                    .use_
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
            if metrics.iter().any(|x| x.name == name) {
                bail!("two metrics are named \"{name}\" (give one a name = \"…\")");
            }
            if !m.use_.ends_with(".glsl") && !m.use_.contains('/') {
                bail!(
                    "metric \"{name}\": use = \"{}\" needs a library (\"quality/{}\") or a .glsl file",
                    m.use_,
                    m.use_
                );
            }
            let mut defines = vec![];
            for (k, v) in &m.params {
                defines.extend(param_defines(&name, k, v)?);
            }
            for (what, b) in [("mean", m.mean), ("max", m.max)] {
                if let Some(Bound {
                    min: Some(lo),
                    max: Some(hi),
                }) = b
                    && lo > hi
                {
                    bail!("metric \"{name}\": {what} has min {lo} > max {hi}");
                }
            }
            metrics.push(Metric {
                name,
                use_: m.use_,
                defines,
                mean: m.mean.unwrap_or_default(),
                max: m.max.unwrap_or_default(),
            });
        }
        Ok(Config {
            count,
            size,
            reference,
            metrics,
        })
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

// ------------------------------------------------------------------ shaders

/// The fragment shader for a metric: the metric header, its params as `#define`s,
/// the metric (a library's or a film file), the footer that calls `mainMetric`.
pub fn metric_source(d: Dialect, metric: &Metric, m: &Movie) -> Result<Assembled> {
    let mut header = METRIC_HEADER.to_string();
    for (k, v) in &metric.defines {
        header += &format!("#define {k} {v}\n");
    }
    let b = Builder::new(d, &header, &m.root).libraries(&m.libraries);
    let b = b.pass(&m.root, "metric", &metric.use_, |n| {
        anyhow!("metric \"{n}\" needs a library or a .glsl file")
    })?;
    Ok(b.footer(METRIC_FOOTER).build())
}

/// Whether a metric reads `@reference` (calls `getReference` outside the header).
pub fn reads_reference(src: &Assembled) -> bool {
    mentions_ident(&src.text.replacen(METRIC_HEADER, "", 1), "getReference")
}

/// The film's metric shaders for the strict check (`check`, `check --web`).
pub fn sources(m: &Movie, d: Dialect) -> Result<Vec<Source>> {
    let Some(cfg) = Config::of_film(m)? else {
        return Ok(vec![]);
    };
    Ok(cfg
        .metrics
        .iter()
        .map(|x| Source {
            what: format!("metric \"{}\" ({})", x.name, x.use_),
            stage: Stage::Fragment,
            src: metric_source(d, x, m),
        })
        .collect())
}

/// The version of the library a metric comes from, for the report.
fn library_version(m: &Movie, use_: &str) -> Option<String> {
    match crate::movie::transition_kind(use_) {
        TransitionKind::Library(lib, _) => m
            .libraries
            .iter()
            .find(|l| l.name == lib)
            .map(|l| l.version.to_string()),
        _ => None,
    }
}

// ------------------------------------------------------------------ report

/// One metric on one frame, reduced.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Reduced {
    /// Mean of the finite measures (None if there are none).
    pub mean: Option<f64>,
    pub max: Option<f64>,
    /// Pixels with a finite measure, and with NaN/Inf (`invalid`).
    pub pixels: u64,
    pub invalid: u64,
}

impl Reduced {
    fn from_texel(t: [f32; 4]) -> Reduced {
        let n = t[2].max(0.0) as u64;
        Reduced {
            mean: (n > 0).then(|| t[0] as f64 / n as f64),
            max: (n > 0).then_some(t[1] as f64),
            pixels: n,
            invalid: t[3].max(0.0) as u64,
        }
    }

    /// Over frames: pixel-weighted mean, max of maxima, summed counts.
    pub fn combine(xs: &[Reduced]) -> Reduced {
        let pixels: u64 = xs.iter().map(|r| r.pixels).sum();
        let sum: f64 = xs
            .iter()
            .filter_map(|r| r.mean.map(|m| m * r.pixels as f64))
            .sum();
        Reduced {
            mean: (pixels > 0).then(|| sum / pixels as f64),
            max: xs.iter().filter_map(|r| r.max).reduce(f64::max),
            pixels,
            invalid: xs.iter().map(|r| r.invalid).sum(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FrameReport {
    pub index: usize,
    pub time: f32,
    pub frame: i32,
    /// GPU time of the 1× render (`cost`); None without a GPU timer.
    pub cost_ms: Option<f32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricReport {
    pub name: String,
    #[serde(rename = "use")]
    pub use_: String,
    pub library_version: Option<String>,
    pub reference: bool,
    /// Per frame, in `frames` order.
    pub frames: Vec<Reduced>,
    pub total: Reduced,
    pub limits: Limits,
    pub failures: Vec<String>,
    pub pass: bool,
    #[serde(skip)]
    pub heatmap: Option<Frame>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Limits {
    pub mean: Bound,
    pub max: Bound,
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
    pub pass: bool,
}

/// Gate a metric's totals: `invalid` always, then its limits.
pub fn judge(metric: &Metric, total: &Reduced) -> Vec<String> {
    let mut f = vec![];
    if total.invalid > 0 {
        f.push(format!(
            "invalid: {} pixel(s) with a NaN or Inf measure",
            total.invalid
        ));
    }
    f.extend(metric.mean.violation("mean", total.mean));
    f.extend(metric.max.violation("max", total.max));
    f
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
            println!(
                "{:<5} {:<16} mean {:>10}  max {:>10}  invalid {}   ({}{v})",
                if m.pass { "ok" } else { "FAIL" },
                m.name,
                fmt_opt(m.total.mean),
                fmt_opt(m.total.max),
                m.total.invalid,
                m.use_
            );
            for f in &m.failures {
                println!("      {f}");
            }
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
}

// ------------------------------------------------------------------ GPU

/// An RGBA32F render target.
struct Target {
    tex: glow::Texture,
    fbo: glow::Framebuffer,
    size: (i32, i32),
}

unsafe fn float_target(gl: &glow::Context, w: i32, h: i32) -> Result<Target> {
    unsafe {
        let tex = gl.create_texture().map_err(|e| anyhow!(e))?;
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA32F as i32,
            w,
            h,
            0,
            glow::RGBA,
            glow::FLOAT,
            glow::PixelUnpackData::Slice(None),
        );
        nearest(gl);
        let fbo = gl.create_framebuffer().map_err(|e| anyhow!(e))?;
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(tex),
            0,
        );
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        gl.bind_texture(glow::TEXTURE_2D, None);
        if status != glow::FRAMEBUFFER_COMPLETE {
            gl.delete_framebuffer(fbo);
            gl.delete_texture(tex);
            bail!("this driver can't render to RGBA32F targets (framebuffer status {status:#x})");
        }
        Ok(Target {
            tex,
            fbo,
            size: (w, h),
        })
    }
}

unsafe fn nearest(gl: &glow::Context) {
    unsafe {
        for (p, v) in [
            (glow::TEXTURE_MIN_FILTER, glow::NEAREST),
            (glow::TEXTURE_MAG_FILTER, glow::NEAREST),
            (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
            (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
        ] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, p, v as i32);
        }
    }
}

unsafe fn delete_target(gl: &glow::Context, t: &Target) {
    unsafe {
        gl.delete_framebuffer(t.fbo);
        gl.delete_texture(t.tex);
    }
}

/// Upload texels (bottom row first, as GL wants) as an RGBA32F texture.
unsafe fn upload_float(
    gl: &glow::Context,
    w: usize,
    h: usize,
    px: &[[f32; 4]],
) -> Result<glow::Texture> {
    unsafe {
        let tex = gl.create_texture().map_err(|e| anyhow!(e))?;
        gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
        gl.pixel_store_i32(glow::UNPACK_SKIP_PIXELS, 0);
        gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 0);
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        let bytes: Vec<u8> = px
            .iter()
            .flat_map(|p| p.iter().flat_map(|c| c.to_ne_bytes()))
            .collect();
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA32F as i32,
            w as i32,
            h as i32,
            0,
            glow::RGBA,
            glow::FLOAT,
            glow::PixelUnpackData::Slice(Some(&bytes)),
        );
        nearest(gl);
        gl.bind_texture(glow::TEXTURE_2D, None);
        Ok(tex)
    }
}

unsafe fn read_float(gl: &glow::Context, t: &Target) -> Vec<[f32; 4]> {
    let (w, h) = t.size;
    let mut bytes = vec![0u8; (w * h * 16) as usize];
    unsafe {
        gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(t.fbo));
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 4);
        gl.read_pixels(
            0,
            0,
            w,
            h,
            glow::RGBA,
            glow::FLOAT,
            glow::PixelPackData::Slice(Some(&mut bytes)),
        );
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
    }
    bytes
        .as_chunks::<16>()
        .0
        .iter()
        .map(|c| std::array::from_fn(|i| f32::from_ne_bytes(*c[i * 4..].first_chunk().unwrap())))
        .collect()
}

/// A frame (top row first) as GL texels in 0..1, bottom row first.
fn frame_texels(f: &Frame) -> Vec<[f32; 4]> {
    let mut out = Vec::with_capacity(f.num_cols() * f.num_rows());
    for y in (0..f.num_rows()).rev() {
        out.extend(f[y].iter().map(|p| p.map(|c| c as f32 / 255.0)));
    }
    out
}

/// The N×-sized frame `f` box-filtered down by `n` (averages of 8-bit values, as the
/// 1× frame is), as GL texels, bottom row first.
pub fn box_filter(f: &Frame, n: usize) -> Vec<[f32; 4]> {
    let (w, h) = (f.num_cols() / n, f.num_rows() / n);
    let k = (n * n) as f32;
    let mut out = Vec::with_capacity(w * h);
    for y in (0..h).rev() {
        for x in 0..w {
            let mut s = [0u32; 4];
            for dy in 0..n {
                for p in &f[y * n + dy][x * n..x * n + n] {
                    for c in 0..4 {
                        s[c] += p[c] as u32;
                    }
                }
            }
            out.push(s.map(|c| c as f32 / (k * 255.0)));
        }
    }
    out
}

/// Render `times` of `m` and measure them with `cfg`'s metrics. `heatmaps` keeps a
/// contact sheet per metric (`cols` wide, as `strip` lays frames out).
pub fn run(
    gl: Arc<glow::Context>,
    m: &Movie,
    cfg: &Config,
    times: &[f32],
    heatmaps: bool,
) -> Result<Report> {
    let mut r = Renderer::new(gl.clone())?;
    r.set_readback(true);
    if let Err(errors) = r.load_all(m) {
        bail!(
            "shader errors:\n{}",
            errors.iter().map(|e| e.to_string()).collect::<String>()
        );
    }
    let dialect = r.dialect_for(m).map_err(|e| anyhow!(e))?;
    let (w, h) = (cfg.size.0 as usize, cfg.size.1 as usize);
    let n = cfg.reference as usize;

    // Compile the metrics.
    let mut progs = vec![];
    let mut needs_ref = vec![];
    for x in &cfg.metrics {
        let src = metric_source(dialect, x, m).with_context(|| format!("metric \"{}\"", x.name))?;
        needs_ref.push(reads_reference(&src));
        match unsafe { crate::glsl_test::compile(&gl, &src) } {
            Ok(p) => progs.push(p),
            Err(e) => {
                for p in progs {
                    unsafe { gl.delete_program(p) };
                }
                bail!(
                    "metric \"{}\" ({}) failed to compile:\n  {}",
                    x.name,
                    x.use_,
                    src.explain(e).join("\n  ")
                );
            }
        }
    }
    let reduce = unsafe {
        crate::glsl_test::compile(
            &gl,
            &Assembled {
                text: format!("{}{REDUCE}", dialect.version_line()),
                dialect,
                files: vec!["<shotglass reduction>".into()],
                warnings: vec![],
                clashes: vec![],
            },
        )
    }
    .map_err(|e| anyhow!("the reduction pass failed to compile: {}", e.join("; ")))?;
    let any_ref = needs_ref.iter().any(|b| *b);
    if any_ref {
        let max = unsafe { gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) } as usize;
        if w * n > max || h * n > max {
            bail!(
                "@reference at {}x{} ({w}x{h} × {n}) is larger than this GPU's limit of {max}; \
                 lower reference or size in {QUALITY_FILE}",
                w * n,
                h * n
            );
        }
    }

    let layout = crate::Sheet::new(times.len(), 4, w, h);
    let mut sheets: Vec<Option<Frame>> = cfg
        .metrics
        .iter()
        .map(|_| heatmaps.then(|| layout.blank()))
        .collect();
    let mut per_metric: Vec<Vec<Reduced>> = vec![vec![]; cfg.metrics.len()];
    let mut frames = vec![];

    let result = (|| -> Result<()> {
        let vao = unsafe { gl.create_vertex_array().map_err(|e| anyhow!(e))? };
        let map = unsafe { float_target(&gl, w as i32, h as i32)? };
        let mut levels = vec![];
        let (mut lw, mut lh) = (w as i32, h as i32);
        while lw > 1 || lh > 1 {
            lw = (lw + 3) / 4;
            lh = (lh + 3) / 4;
            levels.push(unsafe { float_target(&gl, lw, lh)? });
        }
        let cleanup = |gl: &glow::Context, levels: &[Target]| unsafe {
            gl.delete_vertex_array(vao);
            delete_target(gl, &map);
            for t in levels {
                delete_target(gl, t);
            }
        };
        let out = (|| -> Result<()> {
            for (i, &t) in times.iter().enumerate() {
                let info = FrameInfo::at(m, t);
                r.gpu_ms(); // drop older samples
                r.render(m, w as i32, h as i32, &info, true)
                    .ok_or_else(|| anyhow!("{t:.3}s: nothing rendered"))?;
                if let Some(e) = r.footage_failure() {
                    bail!("{t:.3}s: {e}");
                }
                let frame = r.read_output().ok_or_else(|| anyhow!("nothing rendered"))?;
                r.finish();
                let cost_ms = r.gpu_ms();
                let reference = if any_ref {
                    r.render(m, (w * n) as i32, (h * n) as i32, &info, true)
                        .ok_or_else(|| anyhow!("{t:.3}s: nothing rendered at {n}×"))?;
                    let big = r.read_output().ok_or_else(|| anyhow!("nothing rendered"))?;
                    Some(box_filter(&big, n))
                } else {
                    None
                };
                let frame_tex = unsafe { upload_float(&gl, w, h, &frame_texels(&frame))? };
                let ref_tex = match &reference {
                    Some(px) => Some(unsafe { upload_float(&gl, w, h, px)? }),
                    None => None,
                };
                for (k, prog) in progs.iter().enumerate() {
                    let texel = unsafe {
                        draw_metric(&gl, *prog, vao, &map, frame_tex, ref_tex, &info, (w, h));
                        reduce_map(&gl, reduce, vao, &map, &levels)
                    };
                    per_metric[k].push(Reduced::from_texel(texel));
                    if let Some(sheet) = &mut sheets[k] {
                        let px = unsafe { read_float(&gl, &map) };
                        let (x0, y0) = layout.origin(i);
                        for y in 0..h {
                            let row = &px[(h - 1 - y) * w..(h - y) * w];
                            for (x, v) in row.iter().enumerate() {
                                let c = |f: f32| (f.clamp(0.0, 1.0) * 255.0).round() as u8;
                                sheet[y0 + y][x0 + x] = [c(v[0]), c(v[1]), c(v[2]), 255];
                            }
                        }
                    }
                }
                unsafe {
                    gl.delete_texture(frame_tex);
                    if let Some(t) = ref_tex {
                        gl.delete_texture(t);
                    }
                }
                frames.push(FrameReport {
                    index: i,
                    time: t,
                    frame: info.frame,
                    cost_ms,
                });
            }
            Ok(())
        })();
        cleanup(&gl, &levels);
        out
    })();
    unsafe {
        for p in &progs {
            gl.delete_program(*p);
        }
        gl.delete_program(reduce);
    }
    result?;

    let metrics: Vec<MetricReport> = cfg
        .metrics
        .iter()
        .zip(per_metric)
        .zip(sheets)
        .zip(needs_ref)
        .map(|(((x, frames), heatmap), reference)| {
            let total = Reduced::combine(&frames);
            let failures = judge(x, &total);
            MetricReport {
                name: x.name.clone(),
                use_: x.use_.clone(),
                library_version: library_version(m, &x.use_),
                reference,
                frames,
                total,
                limits: Limits {
                    mean: x.mean,
                    max: x.max,
                },
                pass: failures.is_empty(),
                failures,
                heatmap,
            }
        })
        .collect();
    Ok(Report {
        schema: 1,
        engine: env!("CARGO_PKG_VERSION"),
        film: m
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        size: [cfg.size.0 as u32, cfg.size.1 as u32],
        reference: cfg.reference,
        frames,
        pass: metrics.iter().all(|x| x.pass),
        metrics,
    })
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_metric(
    gl: &glow::Context,
    prog: glow::Program,
    vao: glow::VertexArray,
    map: &Target,
    frame: glow::Texture,
    reference: Option<glow::Texture>,
    info: &FrameInfo,
    (w, h): (usize, usize),
) {
    unsafe {
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(map.fbo));
        gl.viewport(0, 0, w as i32, h as i32);
        gl.disable(glow::BLEND);
        gl.disable(glow::SCISSOR_TEST);
        gl.use_program(Some(prog));
        gl.bind_vertex_array(Some(vao));
        for (unit, tex) in [(0u32, Some(frame)), (1, reference.or(Some(frame)))] {
            gl.active_texture(glow::TEXTURE0 + unit);
            gl.bind_sampler(unit, None);
            gl.bind_texture(glow::TEXTURE_2D, tex);
        }
        let loc = |n: &str| gl.get_uniform_location(prog, n);
        gl.uniform_1_i32(loc("sg_frame").as_ref(), 0);
        gl.uniform_1_i32(loc("sg_reference").as_ref(), 1);
        gl.uniform_3_f32(loc("iResolution").as_ref(), w as f32, h as f32, 1.0);
        gl.uniform_1_f32(loc("iTime").as_ref(), info.t);
        gl.uniform_1_i32(loc("iFrame").as_ref(), info.frame);
        gl.draw_arrays(glow::TRIANGLES, 0, 3);
        unbind(gl);
    }
}

/// Reduce the metric map down the levels; the last level's single texel.
unsafe fn reduce_map(
    gl: &glow::Context,
    prog: glow::Program,
    vao: glow::VertexArray,
    map: &Target,
    levels: &[Target],
) -> [f32; 4] {
    unsafe {
        gl.use_program(Some(prog));
        gl.bind_vertex_array(Some(vao));
        gl.disable(glow::BLEND);
        gl.disable(glow::SCISSOR_TEST);
        gl.uniform_1_i32(gl.get_uniform_location(prog, "sg_src").as_ref(), 0);
        let mut src = map;
        for (i, dst) in levels.iter().enumerate() {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(dst.fbo));
            gl.viewport(0, 0, dst.size.0, dst.size.1);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_sampler(0, None);
            gl.bind_texture(glow::TEXTURE_2D, Some(src.tex));
            gl.uniform_2_i32(
                gl.get_uniform_location(prog, "sg_size").as_ref(),
                src.size.0,
                src.size.1,
            );
            gl.uniform_1_i32(
                gl.get_uniform_location(prog, "sg_first").as_ref(),
                (i == 0) as i32,
            );
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            src = dst;
        }
        unbind(gl);
        if levels.is_empty() {
            // A 1×1 map: reduce on the CPU.
            let v = read_float(gl, map)[0];
            let ok = v[3].is_finite();
            return if ok {
                [v[3], v[3], 1.0, 0.0]
            } else {
                [0.0, -3.0e38, 0.0, 1.0]
            };
        }
        read_float(gl, src)[0]
    }
}

unsafe fn unbind(gl: &glow::Context) {
    unsafe {
        gl.use_program(None);
        gl.bind_vertex_array(None);
        for unit in [1u32, 0] {
            gl.active_texture(glow::TEXTURE0 + unit);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
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

[[metric]]
use = "fx/palette.glsl"
name = "pal"
params = { tol = 2, dir = [1, 0.5], ink = "#ff8000", colours = ["#000000", "#ffffff"] }
mean = { min = 0.9 }
"##,
        )
        .unwrap();
        assert_eq!(c.count, 4);
        assert_eq!(c.size, (320, 180));
        assert_eq!(c.reference, 3);
        assert_eq!(c.metrics[0].name, "alias");
        assert_eq!(c.metrics[0].mean.max, Some(0.02));
        assert_eq!(c.metrics[1].name, "pal");
        let d: std::collections::HashMap<_, _> = c.metrics[1].defines.iter().cloned().collect();
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
        assert!(!c.metrics[0].mean.is_set());
    }

    #[test]
    fn refuses_bad_configs() {
        let bad = [
            ("", "no [[metric]]"),
            ("[[metric]]\nuse = \"alias\"\n", "needs a library"),
            ("[[metric]]\nuse = \"q/m\"\nbogus = 1\n", "unknown field"),
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
    fn combines_frames_and_judges() {
        let a = Reduced {
            mean: Some(1.0),
            max: Some(2.0),
            pixels: 10,
            invalid: 0,
        };
        let b = Reduced {
            mean: Some(4.0),
            max: Some(5.0),
            pixels: 30,
            invalid: 1,
        };
        let none = Reduced {
            mean: None,
            max: None,
            pixels: 0,
            invalid: 3,
        };
        let t = Reduced::combine(&[a, b, none]);
        assert_eq!(t.mean, Some(3.25));
        assert_eq!(t.max, Some(5.0));
        assert_eq!((t.pixels, t.invalid), (40, 4));
        let m = Metric {
            name: "x".into(),
            use_: "q/x".into(),
            defines: vec![],
            mean: Bound {
                min: None,
                max: Some(3.0),
            },
            max: Bound {
                min: Some(1.0),
                max: None,
            },
        };
        let f = judge(&m, &t);
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f[0].starts_with("invalid: 4"));
        assert!(f[1].starts_with("mean 3.25"));
        assert!(judge(&m, &Reduced { invalid: 0, ..a }).is_empty());
        // A set bound with nothing finite to measure fails.
        assert!(!judge(&m, &Reduced { invalid: 0, ..none }).is_empty());
    }

    #[test]
    fn texel_to_reduced() {
        let r = Reduced::from_texel([6.0, 3.0, 4.0, 2.0]);
        assert_eq!(
            r,
            Reduced {
                mean: Some(1.5),
                max: Some(3.0),
                pixels: 4,
                invalid: 2
            }
        );
        assert_eq!(Reduced::from_texel([0.0, -3.0e38, 0.0, 5.0]).max, None);
    }

    #[test]
    fn box_filter_averages_blocks_bottom_row_first() {
        let mut f = Frame::init(4, 2, [0, 0, 0, 255]);
        f[0][0] = [255, 0, 0, 255];
        f[1][3] = [0, 255, 0, 255];
        let px = box_filter(&f, 2);
        assert_eq!(px.len(), 2);
        assert_eq!(px[0], [0.25, 0.0, 0.0, 1.0]);
        assert_eq!(px[1], [0.0, 0.25, 0.0, 1.0]);
    }

    #[test]
    fn reference_use_is_detected_outside_the_header() {
        let a = |s: &str| Assembled {
            text: format!("{METRIC_HEADER}{s}"),
            dialect: Dialect::BASELINE,
            files: vec![],
            warnings: vec![],
            clashes: vec![],
        };
        assert!(reads_reference(&a("v = getReference(uv);")));
        assert!(!reads_reference(&a("v = getFrame(uv); // mygetReferenceX")));
    }
}
