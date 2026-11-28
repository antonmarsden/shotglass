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
//! checks are fixed in the engine: `invalid` (NaN/+Inf measures: always fails, and
//! left out of every reduction). A metric leaves a pixel out on purpose with
//! `notMeasured()` (-Inf): counted as `unmeasured`, in no reduction, never a failure and `cost` (GPU time per frame: reported, never gated).
//! The config is `quality.toml` next to `movie.toml` (`config.rs`), and `--tighten`
//! moves its limits towards the measured values (the ratchet).

mod config;
mod gpu;
mod loudness;
mod stats;
pub mod testing;

pub use config::{Config, Kind, QUALITY_FILE, SOUND_MARGIN_DB, tighten};
pub use stats::Report;

use crate::gfx::Tracked;
use crate::gfx::{Frame, FrameInfo, Renderer};
use crate::library::Library;
use crate::movie::{Movie, TransitionKind};
use crate::shader::{Assembled, Builder, Dialect, mentions_ident};
use crate::validate::{Source, Stage};
use anyhow::{Context, Result, anyhow, bail};
use config::Metric;
use eframe::glow::{self, HasContext};
use gpu::{Gpu, Texels};
use stats::{FrameReport, MetricReport, Reduced, SoundReport, Totals};
use std::path::Path;
use std::sync::Arc;
use toodee::{CopyOps, TooDeeOps, TooDeeOpsMut, TranslateOps};

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
float notMeasured() { return uintBitsToFloat(0xff800000u); }
";

/// The header of a sound metric (`[[sound.metric]]`): the soundtrack at
/// `iSampleRate`, `iSoundSamples` long, through `getSound` (silence outside the film,
/// wrapped in a loop film). Only `SOUND_HALO` samples either side of the block being
/// measured are there; further reads are silent.
const SOUND_HEADER: &str = "\
uniform vec3 iResolution;
uniform int iSampleRate;
uniform int iSoundSamples;
uniform int sg_first;
uniform int sg_tex_start;
uniform int sg_tex_len;
uniform sampler2D sg_sound;
out vec4 shotglass_out;
vec2 getSound(int samp) {
  int i = samp - sg_tex_start;
  if (i < 0 || i >= sg_tex_len) return vec2(0.0);
  return texelFetch(sg_sound, ivec2(i % SG_SOUND_W, i / SG_SOUND_W), 0).rg;
}
float notMeasured() { return uintBitsToFloat(0xff800000u); }
";

const SOUND_FOOTER: &str = "void main(){\n  ivec2 p = ivec2(gl_FragCoord.xy);\n  float v = 0.0;\n  mainSoundMetric(v, sg_first + p.y * SG_SOUND_W + p.x);\n  shotglass_out = vec4(0.0, 0.0, 0.0, v);\n}";

/// Sound metrics measure blocks of `SOUND_W × SOUND_ROWS` samples (the map), each
/// with `SOUND_HALO` samples (100 ms) of context either side in its texture.
const SOUND_W: usize = 4096;
const SOUND_ROWS: usize = 256;
const SOUND_BLOCK: usize = SOUND_W * SOUND_ROWS;
const SOUND_HALO: usize = loudness::RATE as usize / 10;

const METRIC_FOOTER: &str = "void main(){\n  vec4 v = vec4(0.0);\n  mainMetric(v, gl_FragCoord.xy);\n  shotglass_out = v;\n}";

/// Time 0, frame 0: the uniforms of a sound metric's pass and of a metric test.
const FRAME_ZERO: FrameInfo = FrameInfo {
    t: 0.0,
    dt: 0.0,
    frame: 0,
    scale: 1.0,
};

/// Render variants beyond `@frame`: (name, the function that reads it, its sampler).
const VARIANTS: [(&str, &str, &str); 3] = [
    ("reference", "getReference", "sg_reference"),
    ("prev", "getPrev", "sg_prev"),
    ("next", "getNext", "sg_next"),
];

/// The fragment shader for a metric: the metric header, its params as `#define`s,
/// the metric (a library's or a film file), the footer that calls `mainMetric`.
pub fn metric_source(d: Dialect, metric: &Metric, m: &Movie, kind: Kind) -> Result<Assembled> {
    metric_source_in(d, metric, &m.root, &m.libraries, kind)
}

/// [`metric_source`] with film-relative paths from `root` and these libraries.
pub fn metric_source_in(
    d: Dialect,
    metric: &Metric,
    root: &Path,
    libs: &[Library],
    kind: Kind,
) -> Result<Assembled> {
    let (header, footer) = match kind {
        Kind::Picture => (METRIC_HEADER, METRIC_FOOTER),
        Kind::Sound => (SOUND_HEADER, SOUND_FOOTER),
    };
    let mut header = match kind {
        Kind::Picture => header.to_string(),
        Kind::Sound => format!("#define SG_SOUND_W {SOUND_W}\n{header}"),
    };
    for (k, v) in &metric.defines {
        header += &format!("#define {k} {v}\n");
    }
    let b = Builder::new(d, &header, root).libraries(libs);
    let b = b.pass(root, "metric", &metric.use_, |n| {
        anyhow!("metric \"{n}\" needs a library or a .glsl file")
    })?;
    Ok(b.footer(footer).build())
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
        .all_metrics()
        .map(|(kind, x)| Source {
            what: format!("metric \"{}\" ({})", x.name, x.use_),
            stage: Stage::Fragment,
            src: metric_source(d, x, m, kind),
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

/// A frame (top row first) as GL texels in 0..1.
fn frame_texels(f: &Frame) -> Texels {
    let px = f.data().iter().map(|p| p.map(|c| c as f32 / 255.0));
    let mut t = Texels::from_vec(f.num_cols(), f.num_rows(), px.collect());
    t.flip_rows();
    t
}

/// The N×-sized frame `f` box-filtered down by `n` (averages of 8-bit values, as the
/// 1× frame is), as GL texels.
pub fn box_filter(f: &Frame, n: usize) -> Texels {
    let (w, h) = (f.num_cols() / n, f.num_rows() / n);
    let k = (n * n * 255) as f32;
    let mut out = Texels::init(w, h, [0.0; 4]);
    for (y, row) in out.rows_mut().enumerate() {
        for (x, px) in row.iter_mut().enumerate() {
            let block = f.view((x * n, y * n), (x * n + n, y * n + n));
            let sum = block.cells().fold([0u32; 4], |s, p| {
                std::array::from_fn(|c| s[c] + p[c] as u32)
            });
            *px = sum.map(|c| c as f32 / k);
        }
    }
    out.flip_rows();
    out
}

/// A metric map's heatmap colour (`.rgb`, clamped to 0..1) as a frame, top row first.
fn heatmap(px: &Texels) -> Frame {
    let c = |f: f32| (f.clamp(0.0, 1.0) * 255.0).round() as u8;
    let rgb = px.data().iter().map(|v| [c(v[0]), c(v[1]), c(v[2]), 255]);
    let mut f = Frame::from_vec(px.num_cols(), px.num_rows(), rgb.collect());
    f.flip_rows();
    f
}

/// Render the frame `info` at w×h and read it back.
fn render(r: &mut Renderer, m: &Movie, (w, h): (usize, usize), info: &FrameInfo) -> Result<Frame> {
    r.render(m, w as i32, h as i32, info, true)
        .ok_or_else(|| anyhow!("{:.3}s: nothing rendered", info.t))?;
    if let Some(e) = r.footage_failure() {
        bail!("{:.3}s: {e}", info.t);
    }
    r.read_output().ok_or_else(|| anyhow!("nothing rendered"))
}

/// The metrics compiled for this driver, and which variants each reads.
struct Programs {
    progs: Vec<glow::Program>,
    reads: Vec<[bool; 3]>,
}

impl Programs {
    fn compile(
        gl: &glow::Context,
        d: Dialect,
        (root, libs): (&Path, &[Library]),
        metrics: &[Metric],
        kind: Kind,
    ) -> Result<Programs> {
        let mut p = Programs {
            progs: vec![],
            reads: vec![],
        };
        for x in metrics {
            let built = metric_source_in(d, x, root, libs, kind)
                .with_context(|| format!("metric \"{}\"", x.name))
                .and_then(|src| {
                    let prog = unsafe { crate::shader::compile(gl, &src) }.map_err(|e| {
                        anyhow!(
                            "metric \"{}\" ({}) failed to compile:\n  {}",
                            x.name,
                            x.use_,
                            src.explain(e).join("\n  ")
                        )
                    })?;
                    let read = match kind {
                        Kind::Picture => variants_read(&src),
                        Kind::Sound => [false; 3],
                    };
                    Ok((prog, read))
                });
            match built {
                Ok((prog, read)) => {
                    p.progs.push(prog);
                    p.reads.push(read);
                }
                Err(e) => {
                    p.delete(gl);
                    return Err(e);
                }
            }
        }
        Ok(p)
    }

    /// Whether any metric reads variant `v`.
    fn wanted(&self) -> [bool; 3] {
        std::array::from_fn(|v| self.reads.iter().any(|r| r[v]))
    }

    fn delete(&self, gl: &glow::Context) {
        for p in &self.progs {
            unsafe { gl.free_program(*p) };
        }
    }
}

/// The variants of one sample frame that some metric reads, as GL textures (`VARIANTS`
/// order), and the film frames `@prev`/`@next` showed.
struct Variants {
    tex: [Option<glow::Texture>; 3],
    frames: [Option<u64>; 3],
}

impl Variants {
    fn render(
        gl: &glow::Context,
        r: &mut Renderer,
        m: &Movie,
        info: &FrameInfo,
        size: (usize, usize),
        n: usize,
        wanted: [bool; 3],
    ) -> Result<Variants> {
        let mut v = Variants {
            tex: [None; 3],
            frames: [None; 3],
        };
        let out = (|| -> Result<()> {
            if wanted[0] {
                let big = render(r, m, (size.0 * n, size.1 * n), info)?;
                v.tex[0] = Some(unsafe { gpu::upload(gl, &box_filter(&big, n))? });
            }
            for (i, delta) in [(1, -1i64), (2, 1)] {
                if wanted[i] {
                    let k = neighbour(m, info.frame.max(0) as u64, delta);
                    let f = render(r, m, size, &FrameInfo::frame(m, k))?;
                    v.tex[i] = Some(unsafe { gpu::upload(gl, &frame_texels(&f))? });
                    v.frames[i] = Some(k);
                }
            }
            Ok(())
        })();
        match out {
            Ok(()) => Ok(v),
            Err(e) => {
                v.delete(gl);
                Err(e)
            }
        }
    }

    /// The samplers a metric binds: every variant, the frame for any not rendered.
    fn inputs(&self, frame: glow::Texture) -> Vec<(&'static str, glow::Texture)> {
        std::iter::once(("sg_frame", frame))
            .chain(
                VARIANTS
                    .iter()
                    .zip(self.tex)
                    .map(|((_, _, s), t)| (*s, t.unwrap_or(frame))),
            )
            .collect()
    }

    fn delete(&self, gl: &glow::Context) {
        for t in self.tex.iter().flatten() {
            unsafe { gl.free_texture(*t) };
        }
    }
}

/// One metric on one sample frame: reduced, its histogram, its heatmap if wanted.
struct Measured {
    reduced: Reduced,
    histogram: Vec<u64>,
    heatmap: Option<Frame>,
}

unsafe fn measure(
    gl: &glow::Context,
    g: &Gpu,
    prog: glow::Program,
    x: &Metric,
    inputs: &[(&str, glow::Texture)],
    info: &FrameInfo,
    heat: bool,
) -> Measured {
    unsafe {
        g.draw_metric(gl, prog, inputs, info);
        let count = g.map.size.0 * g.map.size.1;
        let (a, b) = g.reduce(gl, x.threshold, count);
        Measured {
            reduced: Reduced::from_texels(a, b, x.threshold.is_some()),
            histogram: g.histogram(gl, x.range, count),
            heatmap: heat.then(|| heatmap(&g.map.read(gl, 0))),
        }
    }
}

/// What `run` collects per metric over the sample frames.
#[derive(Default)]
struct Collected {
    frames: Vec<Reduced>,
    histograms: Vec<Vec<u64>>,
    sheet: Option<Frame>,
}

/// Synthesise the whole soundtrack (one period of a loop film) at
/// [`loudness::RATE`] and meter it; `keep` also returns the samples (interleaved
/// stereo) for sound metrics. None: the film has no sound.
fn measure_sound(
    r: &mut Renderer,
    m: &Movie,
    keep: bool,
) -> Result<Option<(loudness::Loudness, Vec<f32>)>> {
    if !m.has_sound() {
        return Ok(None);
    }
    let mut st = crate::export::soundtrack(r, m, loudness::RATE)?;
    let mut meter = loudness::Meter::new();
    let mut kept = vec![];
    st.render_all(r, |d| {
        meter.push(d);
        if keep {
            kept.extend_from_slice(d);
        }
    })
    .map_err(|e| anyhow!(e))?;
    Ok(Some((meter.finish(), kept)))
}

/// The texture of the sound block starting at sample `start`: samples from
/// `start - SOUND_HALO` to the block's end plus the halo, as (L, R, 0, 0) texels
/// `SOUND_W` wide; the first sample's index and the length. Outside the film is
/// silence, or wraps in a loop film.
fn sound_texels(samples: &[f32], start: usize, wrap: bool) -> (Texels, i64, usize) {
    let n = (samples.len() / 2) as i64;
    let first = start as i64 - SOUND_HALO as i64;
    let len = (SOUND_BLOCK + 2 * SOUND_HALO).min((n - first) as usize + SOUND_HALO);
    let mut t = Texels::init(SOUND_W, len.div_ceil(SOUND_W), [0.0; 4]);
    for (i, px) in t.data_mut().iter_mut().take(len).enumerate() {
        let k = first + i as i64;
        let k = if wrap { k.rem_euclid(n) } else { k };
        if (0..n).contains(&k) {
            let k = k as usize * 2;
            *px = [samples[k], samples[k + 1], 0.0, 0.0];
        }
    }
    (t, first, len)
}

/// The largest finite measure (`.a`) among a map's first `count` texels, and its index.
fn largest(map: &Texels, count: usize) -> Option<(f32, usize)> {
    map.data()[..count]
        .iter()
        .enumerate()
        .filter(|(_, v)| v[3].is_finite())
        .max_by(|a, b| a.1[3].total_cmp(&b.1[3]))
        .map(|(i, v)| (v[3], i))
}

/// Sound metrics over the whole soundtrack, block by block, through the same
/// reductions and histogram as frames; and the time of each one's largest measure.
fn run_sound_metrics(
    gl: &glow::Context,
    d: Dialect,
    src: (&Path, &[Library]),
    metrics: &[Metric],
    samples: &[f32],
    wrap: bool,
) -> Result<(Vec<Collected>, Vec<Option<f64>>)> {
    let n = samples.len() / 2;
    let progs = Programs::compile(gl, d, src, metrics, Kind::Sound)?;
    let mut collected: Vec<Collected> = metrics.iter().map(|_| Collected::default()).collect();
    let mut worst: Vec<Option<(f32, usize)>> = vec![None; metrics.len()];
    let result = Gpu::with(gl, d, (SOUND_W, SOUND_ROWS), |g| {
        let info = FRAME_ZERO;
        for start in (0..n).step_by(SOUND_BLOCK) {
            let count = SOUND_BLOCK.min(n - start);
            let (texels, first, len) = sound_texels(samples, start, wrap);
            let tex = unsafe { gpu::upload(gl, &texels)? };
            let metric = metrics.iter().zip(&progs.progs);
            for ((x, prog), (c, w)) in metric.zip(collected.iter_mut().zip(&mut worst)) {
                unsafe {
                    gl.use_program(Some(*prog));
                    let loc = |s: &str| gl.get_uniform_location(*prog, s);
                    gl.uniform_1_i32(loc("iSampleRate").as_ref(), loudness::RATE as i32);
                    gl.uniform_1_i32(loc("iSoundSamples").as_ref(), n as i32);
                    gl.uniform_1_i32(loc("sg_first").as_ref(), start as i32);
                    gl.uniform_1_i32(loc("sg_tex_start").as_ref(), first as i32);
                    gl.uniform_1_i32(loc("sg_tex_len").as_ref(), len as i32);
                    g.draw_metric(gl, *prog, &[("sg_sound", tex)], &info);
                    let (a, b) = g.reduce(gl, x.threshold, count as i32);
                    let reduced = Reduced::from_texels(a, b, x.threshold.is_some());
                    // Where the maximum is: read back only a block that beats the
                    // largest so far.
                    if let Some(mx) = reduced.max
                        && w.is_none_or(|(v, _)| mx as f32 > v)
                    {
                        let at = largest(&g.map.read(gl, 0), count);
                        *w = at.map(|(v, i)| (v, start + i)).or(*w);
                    }
                    c.frames.push(reduced);
                    c.histograms.push(g.histogram(gl, x.range, count as i32));
                }
            }
            unsafe { gl.free_texture(tex) };
        }
        Ok(())
    });
    progs.delete(gl);
    result?;
    let at = worst
        .iter()
        .map(|w| w.map(|(_, i)| i as f64 / loudness::RATE as f64))
        .collect();
    Ok((collected, at))
}

/// Render `times` of `m` and measure them with `cfg`'s metrics, and the soundtrack
/// if `cfg` has `[sound]`. `heatmaps` keeps a contact sheet per metric (as `strip`
/// lays frames out).
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
        bail!("shader errors:\n{}", crate::gfx::join_errors(&errors));
    }
    let dialect = r.dialect_for(m).map_err(|e| anyhow!(e))?;
    let sound = match &cfg.sound {
        Some(sc) => {
            let measured = measure_sound(&mut r, m, !sc.metrics.is_empty())?;
            let mut metrics = vec![];
            if let Some((_, samples)) = &measured
                && !sc.metrics.is_empty()
            {
                let src = (m.root.as_path(), m.libraries.as_slice());
                let wrap = m.loop_frames.is_some();
                let (c, at) = run_sound_metrics(&gl, dialect, src, &sc.metrics, samples, wrap)?;
                for ((x, c), at) in sc.metrics.iter().zip(c).zip(at) {
                    let mut rep = metric_report(m, x, c, vec!["sound"]);
                    rep.max_at = at;
                    metrics.push(rep);
                }
            }
            Some(SoundReport::judge(sc, measured.map(|(l, _)| l), metrics))
        }
        None => None,
    };
    // Sound only: no picture to render.
    let times = if cfg.metrics.is_empty() { &[] } else { times };
    let size = (cfg.size.0 as usize, cfg.size.1 as usize);
    let n = cfg.reference as usize;
    let src = (m.root.as_path(), m.libraries.as_slice());
    let progs = Programs::compile(&gl, dialect, src, &cfg.metrics, Kind::Picture)?;
    let wanted = progs.wanted();
    let layout = crate::Sheet::new(times.len(), 4, size.0, size.1);
    let mut collected: Vec<Collected> = cfg
        .metrics
        .iter()
        .map(|_| Collected {
            sheet: heatmaps.then(|| layout.blank()),
            ..Default::default()
        })
        .collect();
    let mut frames = vec![];
    let result = (|| -> Result<()> {
        if wanted[0] {
            let max = unsafe { gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) } as usize;
            if size.0 * n > max || size.1 * n > max {
                bail!(
                    "@reference at {}x{} ({}x{} × {n}) is larger than this GPU's limit of {max}; \
                     lower reference or size in {QUALITY_FILE}",
                    size.0 * n,
                    size.1 * n,
                    size.0,
                    size.1
                );
            }
        }
        if times.is_empty() {
            return Ok(());
        }
        Gpu::with(&gl, dialect, size, |g| {
            for (i, &t) in times.iter().enumerate() {
                let info = FrameInfo::at(m, t);
                r.gpu_ms(); // drop older samples
                let frame = render(&mut r, m, size, &info)?;
                r.finish();
                let cost_ms = r.gpu_ms();
                let variants = Variants::render(&gl, &mut r, m, &info, size, n, wanted)?;
                let frame_tex = match unsafe { gpu::upload(&gl, &frame_texels(&frame)) } {
                    Ok(t) => t,
                    Err(e) => {
                        variants.delete(&gl);
                        return Err(e);
                    }
                };
                let inputs = variants.inputs(frame_tex);
                for ((x, prog), c) in cfg.metrics.iter().zip(&progs.progs).zip(&mut collected) {
                    let got =
                        unsafe { measure(&gl, g, *prog, x, &inputs, &info, c.sheet.is_some()) };
                    c.frames.push(got.reduced);
                    c.histograms.push(got.histogram);
                    if let (Some(sheet), Some(heat)) = (&mut c.sheet, &got.heatmap) {
                        let (x0, y0) = layout.origin(i);
                        sheet
                            .view_mut((x0, y0), (x0 + size.0, y0 + size.1))
                            .copy_from_toodee(heat);
                    }
                }
                unsafe { gl.free_texture(frame_tex) };
                variants.delete(&gl);
                frames.push(FrameReport {
                    index: i,
                    time: t,
                    frame: info.frame,
                    prev: variants.frames[1],
                    next: variants.frames[2],
                    cost_ms,
                });
            }
            Ok(())
        })
    })();
    progs.delete(&gl);
    result?;
    Ok(report(m, cfg, &progs.reads, collected, frames, sound))
}

/// One metric's totals over what it measured (frames, or sound blocks), judged.
fn metric_report(m: &Movie, x: &Metric, c: Collected, variants: Vec<&'static str>) -> MetricReport {
    let total = Totals::combine(x, &c.frames, &c.histograms);
    let failures = stats::judge(x, &total);
    MetricReport {
        name: x.name.clone(),
        use_: x.use_.clone(),
        library_version: library_version(m, &x.use_),
        variants,
        threshold: x.threshold,
        frames: c.frames,
        total,
        limits: x.limits.iter().map(|(s, b)| (s.to_string(), *b)).collect(),
        pass: failures.is_empty(),
        failures,
        max_at: None,
        heatmap: c.sheet,
    }
}

/// The report: each metric's totals over the frames, judged against its limits.
fn report(
    m: &Movie,
    cfg: &Config,
    reads: &[[bool; 3]],
    collected: Vec<Collected>,
    frames: Vec<FrameReport>,
    sound: Option<SoundReport>,
) -> Report {
    let metrics: Vec<MetricReport> = cfg
        .metrics
        .iter()
        .zip(collected)
        .zip(reads)
        .map(|((x, c), read)| {
            let mut variants = vec!["frame"];
            variants.extend(
                VARIANTS
                    .iter()
                    .zip(read)
                    .filter(|(_, r)| **r)
                    .map(|(v, _)| v.0),
            );
            metric_report(m, x, c, variants)
        })
        .collect();
    Report {
        schema: 4,
        engine: env!("CARGO_PKG_VERSION"),
        film: m
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        size: [cfg.size.0 as u32, cfg.size.1 as u32],
        reference: cfg.reference,
        frames,
        pass: metrics.iter().all(|x| x.pass) && sound.as_ref().is_none_or(|s| s.pass),
        metrics,
        sound,
    }
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
        assert_eq!((px.num_cols(), px.num_rows()), (2, 1));
        assert_eq!(px.data(), [[0.25, 0.0, 0.0, 1.0], [0.0, 0.25, 0.0, 1.0]]);
    }

    #[test]
    fn frame_texels_and_heatmaps_flip_rows() {
        let mut f = Frame::init(2, 2, [0, 0, 0, 255]);
        f[0][1] = [255, 0, 0, 255]; // top right
        let t = frame_texels(&f);
        assert_eq!(t[1][1], [1.0, 0.0, 0.0, 1.0]); // GL: top row last
        assert_eq!(heatmap(&t), f);
    }

    #[test]
    fn variant_use_is_detected_outside_the_header() {
        let a = |s: &str| Assembled {
            text: format!("{METRIC_HEADER}{s}"),
            dialect: Dialect::BASELINE,
            library_glsl: None,
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
