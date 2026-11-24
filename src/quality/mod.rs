//! `shotglass quality` (#96): pluggable quality metrics.
//!
//! The engine provides the measurement contract, not the metrics: a metric is GLSL
//! from a library (`use = "quality/alias"`) or the film (`use = "fx/m.glsl"`) with the
//! entry point `void mainMetric(out vec4 value, in vec2 fragCoord)`. `value.a` is the
//! per-pixel measure, `value.rgb` its heatmap colour. It reads render variants of the
//! sample frame through the metric header, each rendered only for metrics that call
//! its function:
//!
//! - `@frame` (`getFrame(uv)`): the frame at 1×;
//! - `@reference` (`getReference(uv)`): rendered at N×N and box-filtered to 1×;
//! - `@prev` / `@next` (`getPrev(uv)` / `getNext(uv)`): film frames n − 1 and n + 1
//!   (clamped to the film, wrapped in a loop film).
//!
//! Measures are reduced on the GPU (`gpu.rs`: sum, min, max, over-threshold, finite and
//! non-finite counts in 4×4 blocks, and a 256-bin histogram for percentiles). Two
//! checks are fixed in the engine: `invalid` (non-finite measures: always fails, and
//! left out of every reduction) and `cost` (GPU time per frame: reported, never gated).
//! The config is `quality.toml` next to `movie.toml` (`config.rs`), and `--tighten`
//! moves its limits towards the measured values (the ratchet).

mod config;
mod gpu;
mod stats;

pub use config::{Config, tighten};
pub use stats::Report;

use crate::gfx::{Frame, FrameInfo, Renderer};
use crate::movie::{Movie, TransitionKind};
use crate::shader::{Assembled, Builder, Dialect, mentions_ident};
use crate::validate::{Source, Stage};
use anyhow::{Context, Result, anyhow, bail};
use config::Metric;
use eframe::glow::{self, HasContext};
use stats::{FrameReport, MetricReport, Reduced, Totals};
use std::sync::Arc;
use toodee::TooDeeOps;

pub const QUALITY_FILE: &str = "quality.toml";

const METRIC_HEADER: &str = "\
uniform vec3 iResolution;
uniform float iTime;
uniform int iFrame;
uniform sampler2D sg_frame;
uniform sampler2D sg_reference;
uniform sampler2D sg_prev;
uniform sampler2D sg_next;
out vec4 shotglass_out;
vec4 getFrame(vec2 uv) { return texture(sg_frame, uv); }
vec4 getReference(vec2 uv) { return texture(sg_reference, uv); }
vec4 getPrev(vec2 uv) { return texture(sg_prev, uv); }
vec4 getNext(vec2 uv) { return texture(sg_next, uv); }
";

const METRIC_FOOTER: &str = "void main(){\n  vec4 v = vec4(0.0);\n  mainMetric(v, gl_FragCoord.xy);\n  shotglass_out = v;\n}";

/// Render variants beyond `@frame`: (name, the function that reads it, its sampler).
const VARIANTS: [(&str, &str, &str); 3] = [
    ("reference", "getReference", "sg_reference"),
    ("prev", "getPrev", "sg_prev"),
    ("next", "getNext", "sg_next"),
];

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

/// Which of `VARIANTS` a metric reads (calls their function outside the header).
pub fn variants_read(src: &Assembled) -> [bool; 3] {
    let text = src.text.replacen(METRIC_HEADER, "", 1);
    VARIANTS.map(|(_, f, _)| mentions_ident(&text, f))
}

/// The film frame `delta` frames from `n`: clamped to the film, or wrapped in a loop
/// film (whose frame before 0 is the period's last).
pub fn neighbour(m: &Movie, n: u64, delta: i64) -> u64 {
    let k = n as i64 + delta;
    match m.loop_frames {
        Some(f) => k.rem_euclid(f as i64) as u64,
        None => k.clamp(0, m.frame_count().saturating_sub(1) as i64) as u64,
    }
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

impl Config {
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

/// A rendered variant: its texels (bottom row first) and the film frame it shows
/// (`@prev`/`@next`; None for `@reference`).
type Variant = (Vec<[f32; 4]>, Option<u64>);

/// Render the frame `info` at w×h and read it back.
fn render(r: &mut Renderer, m: &Movie, (w, h): (usize, usize), info: &FrameInfo) -> Result<Frame> {
    r.render(m, w as i32, h as i32, info, true)
        .ok_or_else(|| anyhow!("{:.3}s: nothing rendered", info.t))?;
    if let Some(e) = r.footage_failure() {
        bail!("{:.3}s: {e}", info.t);
    }
    r.read_output().ok_or_else(|| anyhow!("nothing rendered"))
}

/// Render `times` of `m` and measure them with `cfg`'s metrics. `heatmaps` keeps a
/// contact sheet per metric (as `strip` lays frames out).
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

    let mut progs = vec![];
    let mut reads = vec![];
    for x in &cfg.metrics {
        let src = metric_source(dialect, x, m).with_context(|| format!("metric \"{}\"", x.name))?;
        reads.push(variants_read(&src));
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
    let wanted: [bool; 3] = std::array::from_fn(|v| reads.iter().any(|r| r[v]));
    if wanted[0] {
        let max = unsafe { gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) } as usize;
        if w * n > max || h * n > max {
            for p in &progs {
                unsafe { gl.delete_program(*p) };
            }
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
    let mut hists: Vec<Vec<Vec<u64>>> = vec![vec![]; cfg.metrics.len()];
    let mut frames = vec![];

    let result = (|| -> Result<()> {
        let g = unsafe { gpu::Gpu::new(&gl, dialect, w as i32, h as i32)? };
        let out = (|| -> Result<()> {
            for (i, &t) in times.iter().enumerate() {
                let info = FrameInfo::at(m, t);
                let fnum = info.frame.max(0) as u64;
                r.gpu_ms(); // drop older samples
                let frame = render(&mut r, m, (w, h), &info)?;
                r.finish();
                let cost_ms = r.gpu_ms();
                // Variants: (texels, film frame shown) for reference, prev, next.
                let mut variants: [Option<Variant>; 3] = [None, None, None];
                if wanted[0] {
                    let big = render(&mut r, m, (w * n, h * n), &info)?;
                    variants[0] = Some((box_filter(&big, n), None));
                }
                for (v, delta) in [(1, -1i64), (2, 1)] {
                    if wanted[v] {
                        let k = neighbour(m, fnum, delta);
                        let f = render(&mut r, m, (w, h), &FrameInfo::frame(m, k))?;
                        variants[v] = Some((frame_texels(&f), Some(k)));
                    }
                }
                let frame_tex = unsafe { gpu::upload(&gl, w, h, &frame_texels(&frame))? };
                let mut textures = vec![("sg_frame", frame_tex)];
                for (v, var) in variants.iter().enumerate() {
                    if let Some((px, _)) = var {
                        textures.push((VARIANTS[v].2, unsafe { gpu::upload(&gl, w, h, px)? }));
                    }
                }
                for (k, prog) in progs.iter().enumerate() {
                    let x = &cfg.metrics[k];
                    // Unread variants' samplers still need a texture: the frame.
                    let inputs: Vec<(&str, glow::Texture)> =
                        std::iter::once(("sg_frame", frame_tex))
                            .chain(VARIANTS.iter().map(|(_, _, s)| {
                                let t = textures
                                    .iter()
                                    .find(|(n, _)| n == s)
                                    .map_or(frame_tex, |t| t.1);
                                (*s, t)
                            }))
                            .collect();
                    let (a, b, hist) = unsafe {
                        g.draw_metric(&gl, *prog, &inputs, &info);
                        let (a, b) = g.reduce(&gl, x.threshold);
                        (a, b, g.histogram(&gl, x.range))
                    };
                    per_metric[k].push(Reduced::from_texels(a, b, x.threshold.is_some()));
                    hists[k].push(hist);
                    if let Some(sheet) = &mut sheets[k] {
                        let px = unsafe { g.map.read(&gl, 0) };
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
                    for (_, t) in &textures {
                        gl.delete_texture(*t);
                    }
                }
                frames.push(FrameReport {
                    index: i,
                    time: t,
                    frame: info.frame,
                    prev: variants[1].as_ref().and_then(|v| v.1),
                    next: variants[2].as_ref().and_then(|v| v.1),
                    cost_ms,
                });
            }
            Ok(())
        })();
        unsafe { g.delete(&gl) };
        out
    })();
    unsafe {
        for p in &progs {
            gl.delete_program(*p);
        }
    }
    result?;

    let metrics: Vec<MetricReport> = cfg
        .metrics
        .iter()
        .zip(per_metric)
        .zip(hists)
        .zip(sheets)
        .zip(reads)
        .map(|((((x, frames), hists), heatmap), read)| {
            let total = Totals::combine(x, &frames, &hists);
            let failures = stats::judge(x, &total);
            let mut variants = vec!["frame"];
            variants.extend(
                VARIANTS
                    .iter()
                    .zip(read)
                    .filter(|(_, r)| *r)
                    .map(|(v, _)| v.0),
            );
            MetricReport {
                name: x.name.clone(),
                use_: x.use_.clone(),
                library_version: library_version(m, &x.use_),
                variants,
                threshold: x.threshold,
                frames,
                total,
                limits: x.limits.iter().map(|(s, b)| (s.to_string(), *b)).collect(),
                pass: failures.is_empty(),
                failures,
                heatmap,
            }
        })
        .collect();
    Ok(Report {
        schema: 2,
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn variant_use_is_detected_outside_the_header() {
        let a = |s: &str| Assembled {
            text: format!("{METRIC_HEADER}{s}"),
            dialect: Dialect::BASELINE,
            files: vec![],
            warnings: vec![],
            clashes: vec![],
        };
        assert_eq!(
            variants_read(&a("v = getReference(uv);")),
            [true, false, false]
        );
        assert_eq!(
            variants_read(&a("v = getPrev(uv) - getNext(uv);")),
            [false, true, true]
        );
        assert_eq!(
            variants_read(&a("v = getFrame(uv); // mygetReferenceX getPrevious")),
            [false, false, false]
        );
    }
}
