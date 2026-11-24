//! Metric tests (`*.test.toml`, run by `shotglass test`): a quality metric on fixed
//! inputs, judged by the limits `quality.toml` uses, expected to pass or to fail.
//!
//! ```toml
//! use = "quality/banding"          # defaults for every case: use, params, range,
//! params = { STEP = 4 }            # threshold, size, samples
//!
//! [[case]]
//! name = "smooth ramp"
//! frame = "fixtures/ramp.png"      # @frame: a PNG, or a .glsl with mainImage
//! mean = { max = 0.01 }            # limits, as in quality.toml
//!
//! [[case]]
//! name = "posterised ramp"
//! frame = "fixtures/steps.glsl"
//! expect = "fail"                  # some limit must fail
//! mean = { max = 0.01 }
//! ```
//!
//! A picture case binds `frame` (required), `reference`, `prev` and `next`: each the
//! variant its metric reads (`getFrame`, …), and each a variant the metric reads must
//! be given. A sound case binds `sound`: a 48 kHz 16-bit stereo WAV, or a .glsl with
//! `vec2 mainSound(int samp, float time)` run for `samples` samples. Paths are relative
//! to the test file; `use` resolves like the film's (a library, or a file relative to
//! the test file). Generated pictures are `size` (default 64x64) and stored at 8 bits,
//! as a rendered frame is. Uniforms are those of time 0, frame 0. A NaN/+Inf measure
//! is an error in either kind of case: a must-fail case must fail by its limits.

use super::config::{Metric, parse_metric};
use super::gpu::{self, Gpu, Texels};
#[cfg(test)]
use super::stats::Reduced;
use super::stats::{self, Totals};
use super::{
    FRAME_ZERO, Kind, Programs, SOUND_W, VARIANTS, frame_texels, loudness, metric_source_in,
    run_sound_metrics,
};
use crate::glsl_test::{Summary, TestFile};
use crate::shader::{Builder, Dialect, DriverGlsl};
use crate::validate::{Stage, validate};
use anyhow::{Context, Result, anyhow, bail};
use eframe::glow::{self, HasContext};
use std::path::{Path, PathBuf};
use toodee::TooDeeOps;

/// Keys a test file's top level may set as defaults for its cases.
const DEFAULTS: [&str; 6] = ["use", "params", "range", "threshold", "size", "samples"];
/// Keys of a case that aren't the metric's.
const CASE_KEYS: [&str; 9] = [
    "name",
    "expect",
    "frame",
    "reference",
    "prev",
    "next",
    "sound",
    "size",
    "samples",
];
/// Most samples a GLSL sound input may ask for.
const MAX_SAMPLES: usize = 1 << 22;

/// One `[[case]]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Case {
    pub name: String,
    pub metric: Metric,
    pub expect_fail: bool,
    pub inputs: Inputs,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Inputs {
    /// `frame`, then `reference`, `prev`, `next` (`VARIANTS` order); generated at `size`.
    Picture {
        frame: PathBuf,
        variants: [Option<PathBuf>; 3],
        size: (usize, usize),
    },
    /// A WAV, or GLSL run for `samples` samples.
    Sound { path: PathBuf, samples: usize },
}

impl Case {
    fn kind(&self) -> Kind {
        match self.inputs {
            Inputs::Picture { .. } => Kind::Picture,
            Inputs::Sound { .. } => Kind::Sound,
        }
    }
}

/// Parse a test file; paths in it are relative to `dir`.
pub fn parse(text: &str, dir: &Path) -> Result<Vec<Case>> {
    let mut top: toml::Table = toml::from_str(text)?;
    let cases = match top.remove("case") {
        Some(toml::Value::Array(a)) if a.iter().all(|x| x.is_table()) => a,
        Some(_) => bail!("`case` must be [[case]] tables"),
        None => bail!("no [[case]] entries"),
    };
    if let Some(k) = top.keys().find(|k| !DEFAULTS.contains(&k.as_str())) {
        bail!(
            "unknown top-level field `{k}` (expected [[case]], or defaults: {})",
            DEFAULTS.join(", ")
        );
    }
    let mut out: Vec<Case> = vec![];
    for c in cases {
        let mut t = top.clone();
        t.extend(c.as_table().unwrap().clone());
        let name = match t.remove("name") {
            Some(toml::Value::String(s)) if !s.trim().is_empty() => s,
            _ => bail!("a [[case]] has no name = \"…\""),
        };
        if out.iter().any(|x| x.name == name) {
            bail!("two cases are named \"{name}\"");
        }
        let case = parse_case(&name, t, dir).with_context(|| format!("case \"{name}\""))?;
        out.push(case);
    }
    Ok(out)
}

fn parse_case(name: &str, mut t: toml::Table, dir: &Path) -> Result<Case> {
    let mut path = |k: &str| -> Result<Option<PathBuf>> {
        match t.remove(k) {
            None => Ok(None),
            Some(toml::Value::String(s))
                if s.ends_with(".glsl")
                    || s.ends_with(if k == "sound" { ".wav" } else { ".png" }) =>
            {
                Ok(Some(dir.join(s)))
            }
            Some(_) if k == "sound" => bail!("sound must be a .wav or .glsl file"),
            Some(_) => bail!("{k} must be a .png or .glsl file"),
        }
    };
    let frame = path("frame")?;
    let variants = [path("reference")?, path("prev")?, path("next")?];
    let sound = path("sound")?;
    let size = match t.remove("size") {
        None => (64, 64),
        Some(toml::Value::String(s)) => {
            let (w, h) = crate::parse_size(&s)?;
            (w as usize, h as usize)
        }
        Some(_) => bail!("size must be \"WxH\""),
    };
    let samples = match t.remove("samples") {
        None => loudness::RATE as usize,
        Some(toml::Value::Integer(n)) if (1..=MAX_SAMPLES as i64).contains(&n) => n as usize,
        Some(_) => bail!("samples must be 1 to {MAX_SAMPLES}"),
    };
    let expect_fail = match t.remove("expect") {
        None => false,
        Some(toml::Value::String(s)) if s == "pass" => false,
        Some(toml::Value::String(s)) if s == "fail" => true,
        Some(_) => bail!("expect must be \"pass\" or \"fail\""),
    };
    debug_assert!(CASE_KEYS.iter().all(|k| !t.contains_key(*k)));
    let mut metric = parse_metric(&t, &[])?;
    metric.name = name.to_string();
    let inputs = match (frame, sound) {
        (Some(frame), None) => Inputs::Picture {
            frame,
            variants,
            size,
        },
        (None, Some(path)) if variants.iter().all(Option::is_none) => {
            Inputs::Sound { path, samples }
        }
        (None, Some(_)) => bail!("a sound case takes no picture inputs"),
        (Some(_), Some(_)) => bail!("set frame (a picture metric) or sound, not both"),
        (None, None) => bail!("no input: frame = \"…\" (picture) or sound = \"…\""),
    };
    Ok(Case {
        name: name.to_string(),
        metric,
        expect_fail,
        inputs,
    })
}

/// `*.test.toml` files: is `p` one?
pub fn is_test_file(p: &Path) -> bool {
    p.file_name()
        .is_some_and(|n| n.to_string_lossy().ends_with(".test.toml"))
}

const PICTURE_HEADER: &str = "\
uniform vec3 iResolution;
uniform float iTime;
uniform int iFrame;
out vec4 shotglass_out;
";
const PICTURE_FOOTER: &str = "void main(){\n  vec4 c = vec4(0.0);\n  mainImage(c, gl_FragCoord.xy);\n  shotglass_out = c;\n}";
const SOUND_GEN_HEADER: &str = "\
uniform vec3 iResolution;
uniform float iSampleRate;
out vec4 shotglass_out;
";
const SOUND_GEN_FOOTER: &str = "void main(){\n  ivec2 p = ivec2(gl_FragCoord.xy);\n  int s = p.y * SG_SOUND_W + p.x;\n  shotglass_out = vec4(mainSound(s, float(s) / iSampleRate), 0.0, 0.0);\n}";

/// A generator shader (a picture's `mainImage` or a sound's `mainSound`).
fn generator(
    d: Dialect,
    path: &Path,
    f: &TestFile,
    sound: bool,
) -> Result<crate::shader::Assembled> {
    let header = if sound {
        format!("#define SG_SOUND_W {SOUND_W}\n{SOUND_GEN_HEADER}")
    } else {
        PICTURE_HEADER.to_string()
    };
    let b = Builder::new(d, &header, &f.root)
        .libraries(&f.libs)
        .file(path)?;
    Ok(b.footer(if sound {
        SOUND_GEN_FOOTER
    } else {
        PICTURE_FOOTER
    })
    .build())
}

/// Every shader a case compiles, for the strict check.
fn shaders(
    d: Dialect,
    c: &Case,
    f: &TestFile,
    dir: &Path,
) -> Result<Vec<crate::shader::Assembled>> {
    let mut out = vec![metric_source_in(d, &c.metric, dir, &f.libs, c.kind())?];
    match &c.inputs {
        Inputs::Picture {
            frame, variants, ..
        } => {
            for p in std::iter::once(frame).chain(variants.iter().flatten()) {
                if p.extension().is_some_and(|e| e == "glsl") {
                    out.push(generator(d, p, f, false)?);
                }
            }
        }
        Inputs::Sound { path, .. } => {
            if path.extension().is_some_and(|e| e == "glsl") {
                out.push(generator(d, path, f, true)?);
            }
        }
    }
    Ok(out)
}

/// Compile `src` or say why not.
fn compile(gl: &glow::Context, src: &crate::shader::Assembled) -> Result<glow::Program> {
    unsafe { crate::shader::compile(gl, src) }.map_err(|e| {
        anyhow!(
            "compile failed on this driver:\n  {}",
            src.explain(e).join("\n  ")
        )
    })
}

/// A picture input as GL texels: a PNG, or GLSL drawn at `size` and kept at 8 bits.
fn picture(
    gl: &glow::Context,
    d: Dialect,
    p: &Path,
    f: &TestFile,
    size: (usize, usize),
) -> Result<Texels> {
    if p.extension().is_some_and(|e| e == "png") {
        return Ok(frame_texels(&crate::golden::load_png(p)?));
    }
    let src = generator(d, p, f, false)?;
    let prog = compile(gl, &src)?;
    let out = Gpu::with(gl, d, size, |g| unsafe {
        g.draw_metric(gl, prog, &[], &FRAME_ZERO);
        let mut t = g.map.read(gl, 0);
        for px in t.data_mut() {
            *px = px.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() / 255.0);
        }
        Ok(t)
    });
    unsafe { gl.delete_program(prog) };
    out
}

/// A sound input as interleaved stereo samples at 48 kHz.
fn sound(gl: &glow::Context, d: Dialect, p: &Path, f: &TestFile, n: usize) -> Result<Vec<f32>> {
    if p.extension().is_some_and(|e| e == "wav") {
        let (rate, s) = crate::golden::load_wav(p)?;
        if rate != loudness::RATE {
            bail!(
                "{}: {rate} Hz; sound metrics measure at {} Hz",
                p.display(),
                loudness::RATE
            );
        }
        return Ok(s);
    }
    let src = generator(d, p, f, true)?;
    let prog = compile(gl, &src)?;
    let out = Gpu::with(gl, d, (SOUND_W, n.div_ceil(SOUND_W)), |g| unsafe {
        gl.use_program(Some(prog));
        let loc = gl.get_uniform_location(prog, "iSampleRate");
        gl.uniform_1_f32(loc.as_ref(), loudness::RATE as f32);
        g.draw_metric(gl, prog, &[], &FRAME_ZERO);
        let t = g.map.read(gl, 0);
        Ok(t.data()[..n].iter().flat_map(|v| [v[0], v[1]]).collect())
    });
    unsafe { gl.delete_program(prog) };
    out
}

/// What a case measured: totals and the judge's failures.
pub struct Measured {
    pub total: Totals,
    pub failures: Vec<String>,
}

/// Run one case's metric on its inputs.
fn measure_case(
    gl: &glow::Context,
    d: Dialect,
    c: &Case,
    f: &TestFile,
    dir: &Path,
) -> Result<Measured> {
    let src = (dir, f.libs.as_slice());
    let x = std::slice::from_ref(&c.metric);
    let (frames, hists) = match &c.inputs {
        Inputs::Sound { path, samples } => {
            let s = sound(gl, d, path, f, *samples)?;
            if s.is_empty() {
                bail!("{}: no samples", path.display());
            }
            let (mut col, _) = run_sound_metrics(gl, d, src, x, &s, false)?;
            let col = col.remove(0);
            (col.frames, col.histograms)
        }
        Inputs::Picture {
            frame,
            variants,
            size,
        } => {
            let progs = Programs::compile(gl, d, src, x, Kind::Picture)?;
            let reads = progs.reads[0];
            let out = (|| {
                for (i, (v, _, _)) in VARIANTS.iter().enumerate() {
                    match (reads[i], &variants[i]) {
                        (true, None) => bail!("the metric reads @{v}: give {v} = \"…\""),
                        (false, Some(_)) => bail!("{v} is given but the metric never reads @{v}"),
                        _ => {}
                    }
                }
                let frame = picture(gl, d, frame, f, *size)?;
                let size = (frame.num_cols(), frame.num_rows());
                let mut texels = vec![frame];
                for p in variants.iter().flatten() {
                    let t = picture(gl, d, p, f, size)?;
                    if (t.num_cols(), t.num_rows()) != size {
                        bail!(
                            "{} is {}x{}, @frame is {}x{}",
                            p.display(),
                            t.num_cols(),
                            t.num_rows(),
                            size.0,
                            size.1
                        );
                    }
                    texels.push(t);
                }
                let mut tex = vec![];
                for t in &texels {
                    match unsafe { gpu::upload(gl, t) } {
                        Ok(t) => tex.push(t),
                        Err(e) => {
                            tex.iter().for_each(|t| unsafe { gl.delete_texture(*t) });
                            return Err(e);
                        }
                    }
                }
                let mut given = tex[1..].iter();
                let inputs: Vec<(&str, glow::Texture)> = std::iter::once(("sg_frame", tex[0]))
                    .chain(VARIANTS.iter().zip(variants).map(|((_, _, s), v)| {
                        (
                            *s,
                            v.as_ref()
                                .and_then(|_| given.next().copied())
                                .unwrap_or(tex[0]),
                        )
                    }))
                    .collect();
                let r = Gpu::with(gl, d, size, |g| unsafe {
                    let m = super::measure(
                        gl,
                        g,
                        progs.progs[0],
                        &c.metric,
                        &inputs,
                        &FRAME_ZERO,
                        false,
                    );
                    Ok((vec![m.reduced], vec![m.histogram]))
                });
                tex.iter().for_each(|t| unsafe { gl.delete_texture(*t) });
                r
            })();
            progs.delete(gl);
            out?
        }
    };
    let total = Totals::combine(&c.metric, &frames, &hists);
    let failures = stats::judge(&c.metric, &total);
    Ok(Measured { total, failures })
}

/// The verdict: Ok(summary line) if the case did what it expects, else why not.
pub fn verdict(c: &Case, m: &Measured) -> std::result::Result<String, Vec<String>> {
    let fmt = |v: Option<f64>| v.map_or("-".into(), |v| format!("{v:.6}"));
    let t = &m.total;
    let line = format!(
        "mean {}  max {}  ({} measured{})",
        fmt(t.mean),
        fmt(t.max),
        t.pixels,
        if t.unmeasured > 0 {
            format!(", {} not measured", t.unmeasured)
        } else {
            String::new()
        }
    );
    if t.invalid > 0 {
        let mut why = vec![format!("{} NaN/+Inf measure(s)", t.invalid)];
        why.extend(
            m.failures
                .iter()
                .filter(|f| !f.starts_with("invalid"))
                .cloned(),
        );
        return Err(why);
    }
    match (c.expect_fail, m.failures.is_empty()) {
        (false, true) => Ok(line),
        (true, false) => Ok(format!(
            "{line}; fails as expected: {}",
            m.failures.join("; ")
        )),
        (false, false) => Err(m.failures.clone()),
        (true, true) => {
            if c.metric.limits.is_empty() {
                Err(vec!["expect = \"fail\" but the case sets no limits".into()])
            } else {
                Err(vec![format!(
                    "expected to fail, but every limit holds: {line}"
                )])
            }
        }
    }
}

/// Run every metric test file (`filter`: only cases whose name contains it).
pub fn run(
    gl: &glow::Context,
    files: &[TestFile],
    filter: Option<&str>,
    out: &mut dyn FnMut(&str),
) -> Result<Summary> {
    let driver = unsafe {
        DriverGlsl::detect(
            &gl.get_parameter_string(glow::SHADING_LANGUAGE_VERSION),
            &gl.get_parameter_string(glow::VERSION),
        )
    };
    let mut sum = Summary::default();
    for f in files {
        out(&f.display);
        let dir = f.path.parent().map(Path::to_path_buf).unwrap_or_default();
        let dir = if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir
        };
        let cases = match std::fs::read_to_string(&f.path)
            .map_err(anyhow::Error::from)
            .and_then(|t| parse(&t, &dir))
        {
            Ok(c) => c,
            Err(e) => {
                out(&format!("  {}: {e:#}", f.display));
                sum.broken += 1;
                continue;
            }
        };
        let opt_in = f
            .libs
            .iter()
            .filter_map(|l| l.glsl)
            .max()
            .filter(|&v| v > 330);
        let strict: Vec<Dialect> = match opt_in {
            _ if !cfg!(feature = "strict-glsl") => vec![],
            None => vec![Dialect::BASELINE, Dialect::WEB],
            Some(v) => vec![Dialect::desktop(Some(v))],
        };
        let d = match driver.dialect(opt_in) {
            Ok(d) => d,
            Err(e) => {
                out(&format!("  {e}"));
                sum.broken += 1;
                continue;
            }
        };
        for c in cases
            .iter()
            .filter(|c| filter.is_none_or(|x| c.name.contains(x)))
        {
            let checked = (|| -> Result<()> {
                for sd in &strict {
                    for src in shaders(*sd, c, f, &dir)? {
                        if let Err(e) = validate(&src, Stage::Fragment) {
                            let e: Vec<String> = e
                                .into_iter()
                                .filter(|m| !m.contains(": warning:"))
                                .collect();
                            bail!(
                                "strict check ({sd}) failed:\n  {}",
                                src.explain(e).join("\n  ")
                            );
                        }
                    }
                }
                Ok(())
            })();
            match checked.and_then(|()| measure_case(gl, d, c, f, &dir)) {
                Err(e) => {
                    sum.broken += 1;
                    out(&format!("  ERROR {}", c.name));
                    for l in format!("{e:#}").lines() {
                        out(&format!("        {l}"));
                    }
                }
                Ok(m) => {
                    sum.samples += m.total.pixels + m.total.unmeasured + m.total.invalid;
                    match verdict(c, &m) {
                        Ok(line) => {
                            sum.passed += 1;
                            out(&format!("  ok    {}  {line}", c.name));
                        }
                        Err(why) => {
                            sum.failed += 1;
                            out(&format!("  FAIL  {}", c.name));
                            for l in why {
                                out(&format!("        {l}"));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(sum)
}

/// A reduced value per case, for tests of `verdict`.
#[cfg(test)]
fn measured(c: &Case, mean: f64, invalid: u64) -> Measured {
    let r = Reduced {
        mean: Some(mean),
        min: Some(mean),
        max: Some(mean),
        over: None,
        pixels: 10,
        invalid,
        unmeasured: 0,
    };
    let total = Totals::combine(&c.metric, &[r], &[vec![0; super::config::BINS]]);
    let failures = stats::judge(&c.metric, &total);
    Measured { total, failures }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"
use = "quality/banding"
params = { STEP = 4 }

[[case]]
name = "smooth"
frame = "a.png"
mean = { max = 0.1 }

[[case]]
name = "steps"
frame = "gen/steps.glsl"
reference = "a.png"
size = "32x16"
expect = "fail"
mean = { max = 0.1 }
params = { STEP = 2 }

[[case]]
name = "clicks"
use = "quality/clicks"
sound = "s.glsl"
samples = 1000
max = { max = 0.5 }
"#;

    #[test]
    fn parses_cases_with_defaults() {
        let cs = parse(FILE, Path::new("t")).unwrap();
        assert_eq!(cs.len(), 3);
        assert_eq!(cs[0].metric.use_, "quality/banding");
        assert_eq!(cs[0].metric.name, "smooth");
        assert_eq!(cs[0].metric.defines, [("STEP".into(), "4.0".into())]);
        assert!(!cs[0].expect_fail);
        assert_eq!(
            cs[1].inputs,
            Inputs::Picture {
                frame: Path::new("t").join("gen/steps.glsl"),
                variants: [Some(Path::new("t").join("a.png")), None, None],
                size: (32, 16),
            }
        );
        assert!(cs[1].expect_fail);
        assert_eq!(cs[1].metric.defines, [("STEP".into(), "2.0".into())]);
        assert_eq!(
            cs[2].inputs,
            Inputs::Sound {
                path: Path::new("t").join("s.glsl"),
                samples: 1000
            }
        );
        assert_eq!(cs[2].kind(), Kind::Sound);
    }

    #[test]
    fn refuses_bad_files() {
        for (bad, why) in [
            ("use = \"q/m\"", "no [[case]]"),
            (
                "count = 3\n[[case]]\nname = \"a\"\nuse = \"q/m\"\nframe = \"a.png\"",
                "unknown top-level",
            ),
            ("[[case]]\nuse = \"q/m\"\nframe = \"a.png\"", "no name"),
            ("[[case]]\nname = \"a\"\nuse = \"q/m\"", "no input"),
            (
                "[[case]]\nname = \"a\"\nuse = \"q/m\"\nframe = \"a.jpg\"",
                ".png or .glsl",
            ),
            (
                "[[case]]\nname = \"a\"\nuse = \"q/m\"\nsound = \"a.png\"",
                ".wav or .glsl",
            ),
            (
                "[[case]]\nname = \"a\"\nuse = \"q/m\"\nframe = \"a.png\"\nsound = \"s.wav\"",
                "not both",
            ),
            (
                "[[case]]\nname = \"a\"\nuse = \"q/m\"\nsound = \"s.wav\"\nprev = \"a.png\"",
                "no picture",
            ),
            (
                "[[case]]\nname = \"a\"\nuse = \"q/m\"\nframe = \"a.png\"\nexpect = \"maybe\"",
                "expect must",
            ),
            (
                "[[case]]\nname = \"a\"\nuse = \"q/m\"\nframe = \"a.png\"\nmedian = { max = 1 }",
                "unknown field",
            ),
            (
                "[[case]]\nname = \"a\"\nuse = \"q/m\"\nframe = \"a.png\"\n[[case]]\nname = \"a\"\nuse = \"q/m\"\nframe = \"a.png\"",
                "two cases",
            ),
        ] {
            let e = format!("{:#}", parse(bad, Path::new(".")).unwrap_err());
            assert!(e.contains(why), "{bad}: {e}");
        }
    }

    #[test]
    fn verdicts() {
        let cs = parse(FILE, Path::new(".")).unwrap();
        let (pass, fail) = (&cs[0], &cs[1]);
        assert!(verdict(pass, &measured(pass, 0.05, 0)).is_ok());
        assert_eq!(
            verdict(pass, &measured(pass, 0.5, 0)).unwrap_err(),
            ["mean 0.500000 > max 0.1"]
        );
        assert!(
            verdict(fail, &measured(fail, 0.5, 0))
                .unwrap()
                .contains("fails as expected")
        );
        assert!(
            verdict(fail, &measured(fail, 0.05, 0)).unwrap_err()[0].contains("expected to fail")
        );
        // NaN is never the expected failure.
        assert!(verdict(fail, &measured(fail, 0.5, 3)).unwrap_err()[0].contains("NaN"));
        let mut none = fail.clone();
        none.metric.limits.clear();
        assert!(verdict(&none, &measured(&none, 0.5, 0)).unwrap_err()[0].contains("no limits"));
    }
}
