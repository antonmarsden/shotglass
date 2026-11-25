//! `shotglass test`: unit and property tests for GLSL, run on the GPU (#63).
//!
//! A test file (`*.test.glsl`) defines `void test_<name>()` functions, or property tests
//! `void test_<name>(float|vec2|vec3 x)` whose parameter sweeps a grid over `[0, 1]` per
//! component, and checks things with the engine's `expect*` assertions (the prelude
//! below: an engine contract, like the uniforms). The engine assembles the file with its
//! project's libraries, adds a dispatcher (pixel → test sample), draws each test into
//! integer render targets and reads them back once: per sample, whether every assertion
//! held, and for the first one that didn't, its id and the values involved.
//!
//! Assertion ids: before assembly, every `expect*(` call in the test file becomes
//! `expect*(<id>, ` and the engine keeps id → (line, call text), so a failure names the
//! exact call, even several on one line, without GLSL needing strings.

use crate::WebChecks;
use crate::gfx::{ReadFormat, read_pixels_into};
use crate::library::Library;
use crate::shader::{Assembled, Builder, Dialect, DriverGlsl, compile, top_level_definitions};
#[cfg(all(test, feature = "strict-glsl"))]
use crate::validate::validate;
use crate::validate::{self, Stage};
use anyhow::{Result, anyhow, bail};
use eframe::glow::{self, HasContext};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The assertion functions test files call (with the id the engine inserts first).
pub const EXPECTS: &[&str] = &[
    "expect",
    "expect_eq",
    "expect_near",
    "expect_rel",
    "expect_finite",
];

/// Most samples one property test may ask for (`// samples: N`).
pub const MAX_SAMPLES: u32 = 1 << 22;
/// Width of the result targets; a test's samples fill rows of this many pixels.
const WIDTH: u32 = 1024;
/// Rows drawn per draw call: big tests are drawn in viewport strips (never scissored,
/// see the CLAUDE.md invariant), so no single draw runs long enough to trip a watchdog.
const BAND_ROWS: u32 = 64;

/// What a test function takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Param {
    None,
    Float,
    Vec2,
    Vec3,
}

impl Param {
    fn dims(self) -> u32 {
        match self {
            Param::None => 0,
            Param::Float => 1,
            Param::Vec2 => 2,
            Param::Vec3 => 3,
        }
    }
    fn default_samples(self) -> u32 {
        match self {
            Param::None => 1,
            Param::Float => 4096,
            Param::Vec2 => 256 * 256,
            Param::Vec3 => 64 * 64 * 64,
        }
    }
}

/// One `test_*` function of a test file.
#[derive(Clone, Debug, PartialEq)]
pub struct TestFn {
    pub name: String,
    pub param: Param,
    /// The parameter's name, for "at t = 0.25".
    pub param_name: String,
    /// Line of the function's name in the test file.
    pub line: usize,
    pub grid: Grid,
}

/// The regular grid a property test's parameter sweeps: `side` points per axis, both
/// ends included (0 and 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    pub dims: u32,
    pub side: u32,
}

impl Grid {
    /// The smallest grid with at least `samples` points.
    pub fn new(dims: u32, samples: u32) -> Grid {
        let side = match dims {
            0 => 1,
            1 => samples.max(2),
            d => {
                let mut s = (samples as f64).powf(1.0 / d as f64).round().max(2.0) as u32;
                while s.pow(d) < samples {
                    s += 1;
                }
                s
            }
        };
        Grid { dims, side }
    }

    pub fn samples(&self) -> u32 {
        self.side.pow(self.dims)
    }

    /// Sample `i`'s parameter, computed as the dispatcher does.
    pub fn point(&self, i: u32) -> [f32; 3] {
        let axis = |j: u32| {
            if self.side > 1 {
                j as f32 / (self.side - 1) as f32
            } else {
                0.0
            }
        };
        let k = self.side;
        let mut p = [0.0; 3];
        if self.dims >= 1 {
            p[0] = axis(i % k);
        }
        if self.dims >= 2 {
            p[1] = axis((i / k) % k);
        }
        if self.dims >= 3 {
            p[2] = axis(i / (k * k));
        }
        p
    }
}

/// The test functions `src` (a test file) defines, in file order.
pub fn scan_tests(src: &str) -> std::result::Result<Vec<TestFn>, Vec<String>> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out: Vec<TestFn> = vec![];
    let mut errors = vec![];
    for (key, line) in top_level_definitions(src) {
        let Some((name, rest)) = key.split_once('(') else {
            continue; // a struct
        };
        if !name.starts_with("test_") {
            continue;
        }
        let types = rest.trim_end_matches(')');
        let param = match types {
            "" => Param::None,
            "float" => Param::Float,
            "vec2" => Param::Vec2,
            "vec3" => Param::Vec3,
            _ => {
                errors.push(format!(
                    "{line}: {key}: a test takes no parameter, or one float, vec2 or vec3"
                ));
                continue;
            }
        };
        if let Some(t) = out.iter().find(|t| t.name == name) {
            errors.push(format!(
                "{line}: {name} is defined twice (also at line {}); test names must be unique",
                t.line
            ));
            continue;
        }
        let param_name = param_name(&lines, line, name).unwrap_or_else(|| "x".into());
        let mut samples = param.default_samples();
        if let Some(prev) = line.checked_sub(2).and_then(|i| lines.get(i))
            && let Some(n) = prev.trim().strip_prefix("// samples:")
        {
            match n.trim().replace('_', "").parse::<u32>() {
                Ok(_) if param == Param::None => {
                    errors.push(format!(
                        "{}: `// samples:` is for property tests (a float, vec2 or vec3 \
                         parameter); {name} takes none",
                        line - 1
                    ));
                }
                Ok(n) if (2..=MAX_SAMPLES).contains(&n) => samples = n,
                _ => errors.push(format!(
                    "{}: `// samples:` takes a whole number from 2 to {MAX_SAMPLES}",
                    line - 1
                )),
            }
        }
        let grid = Grid::new(param.dims(), samples);
        if grid.samples() > MAX_SAMPLES {
            errors.push(format!(
                "{line}: {name}: {} samples is more than {MAX_SAMPLES}",
                grid.samples()
            ));
            continue;
        }
        out.push(TestFn {
            name: name.to_string(),
            param,
            param_name,
            line,
            grid,
        });
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

/// The parameter name of `name(...)` defined at `line` (1-based), if it can be read.
fn param_name(lines: &[&str], line: usize, name: &str) -> Option<String> {
    let text: String = lines
        .get(line - 1..)?
        .iter()
        .take(4)
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    let start = text.find(&format!("{name}("))? + name.len() + 1;
    let inner = &text[start..start + text[start..].find(')')?];
    let last = inner.split_whitespace().last()?;
    Some(
        last.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .to_string(),
    )
    .filter(|s| !s.is_empty())
}

/// One `expect*` call in a test file.
#[derive(Clone, Debug, PartialEq)]
pub struct Assertion {
    /// 1-based line of the call.
    pub line: usize,
    /// The call as written (whitespace collapsed), without the inserted id.
    pub text: String,
}

/// Insert an id as the first argument of every `expect*(` call in `src` (outside
/// comments and preprocessor lines). Returns the new text (same lines) and the calls;
/// call `k` (0-based) gets id `k + 1`.
pub fn number_assertions(src: &str) -> (String, Vec<Assertion>) {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len() + 64);
    let mut calls = vec![];
    let mut i = 0;
    let mut line = 1;
    let mut line_start = true;
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    while i < b.len() {
        let c = b[i];
        let directive = line_start && src[i..].trim_start_matches([' ', '\t']).starts_with('#');
        if b[i..].starts_with(b"//") || directive {
            let end = src[i..].find('\n').map_or(b.len(), |e| i + e);
            out.push_str(&src[i..end]);
            i = end;
            continue;
        }
        if b[i..].starts_with(b"/*") {
            let end = src[i + 2..].find("*/").map_or(b.len(), |e| i + 2 + e + 2);
            line += src[i..end].matches('\n').count();
            out.push_str(&src[i..end]);
            i = end;
            line_start = false;
            continue;
        }
        if c == b'\n' {
            line += 1;
            line_start = true;
            out.push('\n');
            i += 1;
            continue;
        }
        if !c.is_ascii_whitespace() {
            line_start = false;
        }
        if (c.is_ascii_alphabetic() || c == b'_')
            && (i == 0 || !(is_ident(b[i - 1]) || b[i - 1] == b'.'))
        {
            let mut j = i;
            while j < b.len() && is_ident(b[j]) {
                j += 1;
            }
            let word = &src[i..j];
            let mut k = j;
            while k < b.len() && (b[k] == b' ' || b[k] == b'\t') {
                k += 1;
            }
            if EXPECTS.contains(&word) && k < b.len() && b[k] == b'(' {
                calls.push(Assertion {
                    line,
                    text: call_text(&src[i..]),
                });
                out.push_str(&src[i..=k]);
                let _ = write!(out, "{}, ", calls.len());
                i = k + 1;
                continue;
            }
            out.push_str(word);
            i = j;
            continue;
        }
        let ch = src[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, calls)
}

/// `expect_near(f(x), 1.0, 1e-6)` from text starting at the call: up to the matching
/// parenthesis, whitespace and comments collapsed.
fn call_text(s: &str) -> String {
    let mut depth = 0;
    let mut out = String::new();
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if s[i..].starts_with("//") {
            while chars.peek().is_some_and(|x| x.1 != '\n') {
                chars.next();
            }
            out.push(' ');
            continue;
        }
        if s[i..].starts_with("/*") {
            let end = s[i + 2..].find("*/").map_or(s.len(), |e| i + 2 + e + 2);
            while chars.peek().is_some_and(|x| x.0 < end) {
                chars.next();
            }
            out.push(' ');
            continue;
        }
        out.push(c);
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.replace("( ", "(").replace(" )", ")")
}

/// Result kinds the prelude records (`sgtest_kind`): how to show the values of a failure.
const KIND_CONDITION: u32 = 0;
const KIND_VALUE: u32 = 1;
const KIND_FLOATS: u32 = 2;
const KIND_INTS: u32 = 3;
const KIND_BOOLS: u32 = 4;
const KIND_UINTS: u32 = 5;
/// Kinds from this one on record their values as integer bits (exact, any size).
const KIND_FIRST_BITS: u32 = KIND_INTS;

/// The test prelude: the `expect*` assertions (engine contract, semver) and the state
/// the dispatcher reads back. Only test shaders get it, never films.
pub fn prelude() -> String {
    let mut s = String::from(
        "// shotglass test prelude: assertions for *.test.glsl (see docs/testing-and-quality.md, \"Testing GLSL\").\n\
         int sgtest_count = 0;\n\
         int sgtest_fail = 0;\n\
         int sgtest_kind = 0;\n\
         int sgtest_comps = 0;\n\
         vec4 sgtest_actual = vec4(0.0);\n\
         vec4 sgtest_expected = vec4(0.0);\n\
         uvec4 sgtest_abits = uvec4(0u);\n\
         uvec4 sgtest_ebits = uvec4(0u);\n\
         void sgtest_record(int id, bool ok, int kind, int comps, vec4 a, vec4 e) {\n\
         \x20   sgtest_count += 1;\n\
         \x20   if (!ok && sgtest_fail == 0) {\n\
         \x20       sgtest_fail = id; sgtest_kind = kind; sgtest_comps = comps;\n\
         \x20       sgtest_actual = a; sgtest_expected = e;\n\
         \x20   }\n\
         }\n\
         void sgtest_record_bits(int id, bool ok, int kind, int comps, uvec4 a, uvec4 e) {\n\
         \x20   sgtest_count += 1;\n\
         \x20   if (!ok && sgtest_fail == 0) {\n\
         \x20       sgtest_fail = id; sgtest_kind = kind; sgtest_comps = comps;\n\
         \x20       sgtest_abits = a; sgtest_ebits = e;\n\
         \x20   }\n\
         }\n",
    );
    let _ = writeln!(
        s,
        "void expect(int id, bool cond) {{ sgtest_record(id, cond, {KIND_CONDITION}, 0, vec4(0.0), vec4(0.0)); }}"
    );
    let _ = writeln!(
        s,
        "void expect_eq(int id, bool a, bool b) {{ sgtest_record_bits(id, a == b, {KIND_BOOLS}, 1, uvec4(a ? 1u : 0u, 0u, 0u, 0u), uvec4(b ? 1u : 0u, 0u, 0u, 0u)); }}"
    );
    for (t, it, ut, n) in [
        ("float", "int", "uint", 1),
        ("vec2", "ivec2", "uvec2", 2),
        ("vec3", "ivec3", "uvec3", 3),
        ("vec4", "ivec4", "uvec4", 4),
    ] {
        // An int or uint vector as uvec4 bits (uint(int) keeps the bit pattern).
        let u4 = |x: &str| match n {
            1 => format!("uvec4(uint({x}), 0u, 0u, 0u)"),
            2 => format!("uvec4(uvec2({x}), 0u, 0u)"),
            3 => format!("uvec4(uvec3({x}), 0u)"),
            _ => format!("uvec4({x})"),
        };
        let v4 = |x: &str| match n {
            1 => format!("vec4({x}, 0.0, 0.0, 0.0)"),
            2 => format!("vec4({x}, 0.0, 0.0)"),
            3 => format!("vec4({x}, 0.0)"),
            _ => format!("vec4({x})"),
        };
        type Un = fn(&str) -> String;
        type Bin = fn(&str, &str) -> String;
        let (fin, le, eq): (Un, Bin, Bin) = if n == 1 {
            (
                |x| format!("(!isnan({x}) && !isinf({x}))"),
                |a, b| format!("({a} <= {b})"),
                |a, b| format!("({a} == {b})"),
            )
        } else {
            (
                |x| format!("(!any(isnan({x})) && !any(isinf({x})))"),
                |a, b| format!("all(lessThanEqual({a}, {b}))"),
                |a, b| format!("all(equal({a}, {b}))"),
            )
        };
        let rec = |ok: String, kind: u32, a: &str, e: &str| {
            format!("sgtest_record(id, {ok}, {kind}, {n}, {}, {})", v4(a), v4(e))
        };
        let _ = writeln!(
            s,
            "void expect(int id, bool cond, {t} a) {{ {}; }}",
            rec("cond".into(), KIND_VALUE, "a", &format!("{t}(0.0)"))
        );
        let _ = writeln!(
            s,
            "void expect_finite(int id, {t} a) {{ {}; }}",
            rec(fin("a"), KIND_VALUE, "a", &format!("{t}(0.0)"))
        );
        let _ = writeln!(
            s,
            "void expect_eq(int id, {t} a, {t} b) {{ {}; }}",
            rec(eq("a", "b"), KIND_FLOATS, "a", "b")
        );
        for (ty, kind) in [(it, KIND_INTS), (ut, KIND_UINTS)] {
            let _ = writeln!(
                s,
                "void expect_eq(int id, {ty} a, {ty} b) {{ sgtest_record_bits(id, {}, {kind}, {n}, {}, {}); }}",
                eq("a", "b"),
                u4("a"),
                u4("b")
            );
        }
        let near = format!(
            "{} && {} && {}",
            fin("a"),
            fin("b"),
            le("abs(a - b)", &format!("{t}(tol)"))
        );
        let _ = writeln!(
            s,
            "void expect_near(int id, {t} a, {t} b, float tol) {{ {}; }}",
            rec(near, KIND_FLOATS, "a", "b")
        );
        let rel = format!(
            "{} && {} && {}",
            fin("a"),
            fin("b"),
            le("abs(a - b)", "rel * max(abs(a), abs(b))")
        );
        let _ = writeln!(
            s,
            "void expect_rel(int id, {t} a, {t} b, float rel) {{ {}; }}",
            rec(rel, KIND_FLOATS, "a", "b")
        );
    }
    s
}

const UNIFORMS: &str = "uniform int sgtest_test;\nuniform int sgtest_side;\nuniform int sgtest_n;\nuniform int sgtest_width;\n";

/// `main()`: pixel → (test, sample), then the results into three integer targets.
pub fn dispatcher(tests: &[TestFn]) -> String {
    let mut s = String::from(
        "layout(location = 0) out uvec4 sgtest_out0;\n\
         layout(location = 1) out uvec4 sgtest_out1;\n\
         layout(location = 2) out uvec4 sgtest_out2;\n\
         float sgtest_axis(int j) { return sgtest_side > 1 ? float(j) / float(sgtest_side - 1) : 0.0; }\n\
         void main() {\n\
         \x20   ivec2 px = ivec2(gl_FragCoord.xy);\n\
         \x20   int i = px.y * sgtest_width + px.x;\n\
         \x20   if (i >= sgtest_n) { sgtest_out0 = uvec4(0u); sgtest_out1 = uvec4(0u); sgtest_out2 = uvec4(0u); return; }\n\
         \x20   float x = sgtest_axis(i % sgtest_side);\n\
         \x20   float y = sgtest_axis((i / sgtest_side) % sgtest_side);\n\
         \x20   float z = sgtest_axis(i / (sgtest_side * sgtest_side));\n",
    );
    for (k, t) in tests.iter().enumerate() {
        let arg = match t.param {
            Param::None => "",
            Param::Float => "x",
            Param::Vec2 => "vec2(x, y)",
            Param::Vec3 => "vec3(x, y, z)",
        };
        let _ = writeln!(
            s,
            "    {}if (sgtest_test == {k}) {}({arg});",
            if k == 0 { "" } else { "else " },
            t.name
        );
    }
    let _ = write!(
        s,
        "    sgtest_out0 = uvec4(1u, uint(sgtest_fail), uint(sgtest_count), uint(sgtest_comps + 8 * sgtest_kind));\n\
         \x20   bool bits = sgtest_kind >= {KIND_FIRST_BITS};\n\
         \x20   sgtest_out1 = bits ? sgtest_abits : floatBitsToUint(sgtest_actual);\n\
         \x20   sgtest_out2 = bits ? sgtest_ebits : floatBitsToUint(sgtest_expected);\n\
         }}\n"
    );
    s
}

/// A test file ready to run: where it is and the libraries of its project.
pub struct TestFile {
    pub path: PathBuf,
    /// Its project root (where `shotglass.toml` is), or its folder if it has none.
    pub root: PathBuf,
    pub libs: Vec<Library>,
    /// How to show the path in messages: relative to `root`, as the compiler's messages
    /// show it.
    pub display: String,
}

impl TestFile {
    /// The newest desktop dialect required by the project's libraries.
    pub fn desktop_opt_in(&self, web_checks: WebChecks) -> Option<u32> {
        if web_checks.web {
            // Web preflight checks the libraries each assembled shader actually includes.
            return None;
        }
        self.libs
            .iter()
            .filter_map(|l| l.glsl)
            .max()
            .filter(|&v| v > Dialect::BASELINE.version)
    }
}

pub fn test_dialects(opt_in: Option<u32>) -> Vec<Dialect> {
    match opt_in {
        _ if !validate::AVAILABLE => vec![],
        None => vec![Dialect::BASELINE, Dialect::WEB],
        Some(v) => vec![Dialect::desktop(Some(v))],
    }
}

/// Shared preflight for GLSL unit tests and metric tests.
pub fn check_test_shader(
    src: &Assembled,
    web_checks: WebChecks,
    out: &mut dyn FnMut(&str),
) -> Result<()> {
    if let Some(v) = src
        .library_glsl
        .filter(|&v| web_checks.web && v > Dialect::BASELINE.version)
    {
        bail!(
            "--web: an included test library requires desktop GLSL {v}, which is incompatible with GLSL ES 3.00"
        );
    }
    let (result, warnings) = validate::validate_with_warnings(src, Stage::Fragment);
    if web_checks.web {
        for warning in &warnings {
            out(&format!("  {warning}"));
        }
    }
    if let Err(errors) = result {
        let errors = errors
            .into_iter()
            .filter(|m| !m.contains(": warning:"))
            .collect();
        anyhow::bail!(
            "strict check ({}) failed:\n  {}",
            src.dialect,
            src.explain(errors).join("\n  ")
        );
    }
    if web_checks.rejects_warnings(&warnings) {
        anyhow::bail!("shader has browser compatibility warnings");
    }
    Ok(())
}

/// A file assembled for one dialect, with its tests and assertions.
pub struct Prepared {
    pub tests: Vec<TestFn>,
    pub assertions: Vec<Assertion>,
    pub asm: Assembled,
}

/// Assemble `f` in dialect `d`: prelude, the numbered test file, the dispatcher.
pub fn prepare(f: &TestFile, d: Dialect) -> std::result::Result<Prepared, Vec<String>> {
    let src = std::fs::read_to_string(&f.path).map_err(|e| vec![format!("{}: {e}", f.display)])?;
    let tests = scan_tests(&src).map_err(|errs| {
        errs.into_iter()
            .map(|e| format!("{}:{e}", f.display))
            .collect::<Vec<_>>()
    })?;
    let (numbered, assertions) = number_assertions(&src);
    let asm = Builder::new(d, &format!("{}{UNIFORMS}", prelude()), &f.root)
        .libraries(&f.libs)
        .file_text(&f.path, &numbered)
        .map_err(|e| vec![format!("{e:#}")])?
        .footer(&dispatcher(&tests))
        .build();
    Ok(Prepared {
        tests,
        assertions,
        asm,
    })
}

/// What one test did.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    pub samples: u32,
    /// Samples where an assertion failed.
    pub failed: u32,
    /// Samples where no assertion ran.
    pub empty: u32,
    /// Samples the GPU didn't draw (a driver problem).
    pub undrawn: u32,
    /// The lowest failing sample.
    pub first: Option<Failure>,
}

impl Outcome {
    pub fn passed(&self) -> bool {
        self.failed == 0 && self.undrawn == 0 && self.empty < self.samples
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub sample: u32,
    /// 1-based assertion id.
    pub id: u32,
    pub kind: u32,
    pub comps: u32,
    /// The values' bits: floats, or ints / uints / bools for those kinds.
    pub actual: [u32; 4],
    pub expected: [u32; 4],
}

/// Read the first target (`(drawn, failed id, count, comps + 8·kind)` per sample) into an
/// outcome; `values(sample)` fetches the actual and expected values of the first failure.
pub fn decode(
    out0: &[[u32; 4]],
    samples: u32,
    values: impl FnOnce(u32) -> ([u32; 4], [u32; 4]),
) -> Outcome {
    let mut o = Outcome {
        samples,
        failed: 0,
        empty: 0,
        undrawn: 0,
        first: None,
    };
    let mut first = None;
    for (i, px) in out0.iter().take(samples as usize).enumerate() {
        if px[0] != 1 {
            o.undrawn += 1;
        } else if px[1] != 0 {
            o.failed += 1;
            first.get_or_insert((i as u32, *px));
        } else if px[2] == 0 {
            o.empty += 1;
        }
    }
    o.undrawn += samples.saturating_sub(out0.len() as u32);
    if let Some((i, px)) = first {
        let (a, e) = values(i);
        o.first = Some(Failure {
            sample: i,
            id: px[1],
            kind: px[3] / 8,
            comps: px[3] % 8,
            actual: a,
            expected: e,
        });
    }
    o
}

fn show_values(v: &[u32; 4], comps: u32, kind: u32) -> String {
    let one = |x: u32| match kind {
        KIND_INTS => format!("{}", x as i32),
        KIND_UINTS => format!("{x}"),
        KIND_BOOLS => (if x != 0 { "true" } else { "false" }).to_string(),
        _ => format!("{:?}", f32::from_bits(x)),
    };
    let n = comps.clamp(1, 4) as usize;
    if n == 1 {
        one(v[0])
    } else {
        format!(
            "({})",
            v[..n]
                .iter()
                .map(|&x| one(x))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// The report lines for a failed test (indented by the caller).
pub fn describe(t: &TestFn, o: &Outcome, asserts: &[Assertion], file: &str) -> Vec<String> {
    let mut lines = vec![];
    if let Some(f) = &o.first {
        let a = asserts.get(f.id as usize - 1);
        lines.push(match a {
            Some(a) => format!("{file}:{}: {}", a.line, a.text),
            None => format!("{file}: assertion #{} (unknown)", f.id),
        });
        let at = match t.param {
            Param::None => String::new(),
            _ => {
                let p = t.grid.point(f.sample);
                let v = match t.param {
                    Param::Float => format!("{:?}", p[0]),
                    Param::Vec2 => format!("({:?}, {:?})", p[0], p[1]),
                    _ => format!("({:?}, {:?}, {:?})", p[0], p[1], p[2]),
                };
                format!("at {} = {v}: ", t.param_name)
            }
        };
        let values = match f.kind {
            KIND_CONDITION => "false".to_string(),
            KIND_VALUE => format!("got {}", show_values(&f.actual, f.comps, f.kind)),
            _ => format!(
                "got {}, expected {}",
                show_values(&f.actual, f.comps, f.kind),
                show_values(&f.expected, f.comps, f.kind)
            ),
        };
        lines.push(format!("{at}{values}"));
        if t.param != Param::None {
            lines.push(format!("{} of {} samples failed", o.failed, o.samples));
        }
    }
    if o.empty == o.samples {
        lines.push(format!("{file}:{}: {} ran no assertions", t.line, t.name));
    }
    if o.undrawn > 0 {
        lines.push(format!(
            "{} of {} samples weren't drawn (a driver problem?)",
            o.undrawn, o.samples
        ));
    }
    lines
}

/// Totals over a run.
#[derive(Default, Debug)]
pub struct Summary {
    pub passed: usize,
    pub failed: usize,
    /// Files that couldn't be assembled, validated or compiled.
    pub broken: usize,
    pub samples: u64,
}

/// The GL objects one run draws with.
struct Gpu {
    vao: glow::VertexArray,
    fbo: glow::Framebuffer,
    tex: [glow::Texture; 3],
    height: u32,
}

/// Run every test file (`filter`: only tests whose name contains it), printing as it
/// goes. Needs a current GL context.
pub fn run(
    gl: &glow::Context,
    files: &[TestFile],
    filter: Option<&str>,
    web_checks: WebChecks,
    out: &mut dyn FnMut(&str),
) -> Result<Summary> {
    let (glsl, version) = unsafe {
        (
            gl.get_parameter_string(glow::SHADING_LANGUAGE_VERSION),
            gl.get_parameter_string(glow::VERSION),
        )
    };
    let driver = DriverGlsl::detect(&glsl, &version);
    let mut sum = Summary::default();
    let mut gpu = unsafe { Gpu::new(gl)? };
    for f in files {
        out(&f.display);
        let opt_in = f.desktop_opt_in(web_checks);
        let strict = test_dialects(opt_in);
        let mut errors = vec![];
        for d in &strict {
            match prepare(f, *d) {
                Err(e) => {
                    errors = e;
                    break;
                }
                Ok(p) => {
                    if let Err(e) = check_test_shader(&p.asm, web_checks, out) {
                        errors.extend(e.to_string().lines().map(str::to_owned));
                        break;
                    }
                }
            }
        }
        let prepared = if errors.is_empty() {
            driver
                .dialect(opt_in)
                .map_err(|e| vec![e])
                .and_then(|d| prepare(f, d))
        } else {
            Err(errors)
        };
        let p = match prepared {
            Ok(p) => p,
            Err(errs) => {
                for e in errs {
                    out(&format!("  {e}"));
                }
                sum.broken += 1;
                continue;
            }
        };
        let selected: Vec<(usize, &TestFn)> = p
            .tests
            .iter()
            .enumerate()
            .filter(|(_, t)| filter.is_none_or(|x| t.name.contains(x)))
            .collect();
        if p.tests.is_empty() {
            out("  (no test_ functions)");
        }
        if selected.is_empty() {
            continue;
        }
        let prog = match unsafe { compile(gl, &p.asm) } {
            Ok(prog) => prog,
            Err(errs) => {
                out("  compile failed on this driver:");
                for e in p.asm.explain(errs) {
                    out(&format!("    {e}"));
                }
                sum.broken += 1;
                continue;
            }
        };
        for (k, t) in selected {
            let o = unsafe { gpu.run_test(gl, prog, k, t)? };
            sum.samples += o.samples as u64;
            let n = if t.param == Param::None {
                String::new()
            } else {
                format!(" ({} samples)", o.samples)
            };
            if o.passed() {
                sum.passed += 1;
                out(&format!("  ok    {}{n}", t.name));
            } else {
                sum.failed += 1;
                out(&format!("  FAIL  {}{n}", t.name));
                for l in describe(t, &o, &p.assertions, &f.display) {
                    out(&format!("        {l}"));
                }
            }
        }
        unsafe { gl.delete_program(prog) };
    }
    unsafe { gpu.delete(gl) };
    Ok(sum)
}

impl Gpu {
    unsafe fn new(gl: &glow::Context) -> Result<Gpu> {
        unsafe {
            let vao = gl.create_vertex_array().map_err(|e| anyhow!(e))?;
            let fbo = gl.create_framebuffer().map_err(|e| anyhow!(e))?;
            let mut tex = vec![];
            for _ in 0..3 {
                tex.push(gl.create_texture().map_err(|e| anyhow!(e))?);
            }
            Ok(Gpu {
                vao,
                fbo,
                tex: [tex[0], tex[1], tex[2]],
                height: 0,
            })
        }
    }

    unsafe fn delete(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_vertex_array(self.vao);
            gl.delete_framebuffer(self.fbo);
            for t in self.tex {
                gl.delete_texture(t);
            }
        }
    }

    /// Integer targets at least `rows` high (WIDTH wide), attached to the framebuffer.
    unsafe fn ensure(&mut self, gl: &glow::Context, rows: u32) -> Result<()> {
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            if rows > self.height {
                for (i, t) in self.tex.iter().enumerate() {
                    gl.bind_texture(glow::TEXTURE_2D, Some(*t));
                    gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        glow::RGBA32UI as i32,
                        WIDTH as i32,
                        rows as i32,
                        0,
                        glow::RGBA_INTEGER,
                        glow::UNSIGNED_INT,
                        glow::PixelUnpackData::Slice(None),
                    );
                    gl.tex_parameter_i32(
                        glow::TEXTURE_2D,
                        glow::TEXTURE_MIN_FILTER,
                        glow::NEAREST as i32,
                    );
                    gl.tex_parameter_i32(
                        glow::TEXTURE_2D,
                        glow::TEXTURE_MAG_FILTER,
                        glow::NEAREST as i32,
                    );
                    gl.framebuffer_texture_2d(
                        glow::FRAMEBUFFER,
                        glow::COLOR_ATTACHMENT0 + i as u32,
                        glow::TEXTURE_2D,
                        Some(*t),
                        0,
                    );
                }
                gl.bind_texture(glow::TEXTURE_2D, None);
                self.height = rows;
                gl.draw_buffers(&[
                    glow::COLOR_ATTACHMENT0,
                    glow::COLOR_ATTACHMENT1,
                    glow::COLOR_ATTACHMENT2,
                ]);
                let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
                if status != glow::FRAMEBUFFER_COMPLETE {
                    bail!(
                        "this driver can't render to RGBA32UI targets (framebuffer status {status:#x})"
                    );
                }
            }
            Ok(())
        }
    }

    /// Draw test `index` of `prog` over its grid and read the results back.
    unsafe fn run_test(
        &mut self,
        gl: &glow::Context,
        prog: glow::Program,
        index: usize,
        t: &TestFn,
    ) -> Result<Outcome> {
        let n = t.grid.samples();
        let width = WIDTH.min(n);
        let rows = n.div_ceil(WIDTH);
        unsafe {
            self.ensure(gl, rows)?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            for i in 0..3 {
                gl.clear_buffer_u32_slice(glow::COLOR, i, &[0, 0, 0, 0]);
            }
            gl.bind_vertex_array(Some(self.vao));
            gl.use_program(Some(prog));
            let set = |name: &str, v: i32| {
                if let Some(l) = gl.get_uniform_location(prog, name) {
                    gl.uniform_1_i32(Some(&l), v);
                }
            };
            set("sgtest_test", index as i32);
            set("sgtest_side", t.grid.side as i32);
            set("sgtest_n", n as i32);
            set("sgtest_width", WIDTH as i32);
            let mut y = 0;
            while y < rows {
                let h = BAND_ROWS.min(rows - y);
                gl.viewport(0, y as i32, width as i32, h as i32);
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
                y += h;
            }
            gl.use_program(None);
            gl.bind_vertex_array(None);
            let read = |attachment: u32, x: u32, y: u32, w: u32, h: u32| -> Vec<[u32; 4]> {
                let mut px = vec![[0u32; 4]; (w * h) as usize];
                read_pixels_into(
                    gl,
                    (self.fbo, glow::COLOR_ATTACHMENT0 + attachment),
                    [x as i32, y as i32, w as i32, h as i32],
                    ReadFormat::RgbaU32,
                    bytemuck_slice(&mut px),
                    (w as usize, (0, 0)),
                );
                px
            };
            let out0 = read(0, 0, 0, width, rows);
            // out0 rows are `width` wide; samples are laid out WIDTH per row, and width
            // < WIDTH only when everything fits in one row, so indices agree.
            let o = decode(&out0, n, |i| {
                let (x, y) = (i % WIDTH, i / WIDTH);
                (read(1, x, y, 1, 1)[0], read(2, x, y, 1, 1)[0])
            });
            Ok(o)
        }
    }
}

/// `[[u32; 4]]` as bytes, for glReadPixels.
fn bytemuck_slice(px: &mut [[u32; 4]]) -> &mut [u8] {
    let len = std::mem::size_of_val(px);
    // SAFETY: [u32; 4] has no padding and any byte pattern is a valid u32.
    unsafe { std::slice::from_raw_parts_mut(px.as_mut_ptr().cast::<u8>(), len) }
}

/// `*.test.glsl` and `*.test.toml` (metric tests) files at or under `target` (a file is taken as it is), sorted. Hidden
/// folders, `target/` and frozen library copies are skipped.
pub fn find_files(target: &Path) -> Result<Vec<PathBuf>> {
    if target.is_file() {
        return Ok(vec![target.to_path_buf()]);
    }
    if !target.is_dir() {
        bail!("{}: no such file or folder", target.display());
    }
    let mut out = vec![];
    let mut stack = vec![target.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            let p = e.path();
            if e.file_type()?.is_dir() {
                if !(name.starts_with('.') || name == "target" || name == "shotglass-libs") {
                    stack.push(p);
                }
            } else if name.ends_with(".test.glsl") || name.ends_with(".test.toml") {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_test_functions_with_their_parameters() {
        let src = "\
// void test_commented() {}
/* void test_block() {} */
float my_test_helper(float x) { return x; }
void test_once() { }
void test_line(float t)
{
}
// samples: 100
void test_plane(vec2 p) {}
void test_cube(in vec3 c) {}
struct test_struct { float a; };
#if 0
void test_hidden() {}
#endif
";
        let t = scan_tests(src).unwrap();
        let names: Vec<_> = t.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["test_once", "test_line", "test_plane", "test_cube"]);
        assert_eq!(t[0].param, Param::None);
        assert_eq!(t[0].grid.samples(), 1);
        assert_eq!((t[1].param, t[1].line), (Param::Float, 5));
        assert_eq!(t[1].param_name, "t");
        assert_eq!(t[1].grid.samples(), 4096);
        assert_eq!(t[2].grid, Grid { dims: 2, side: 10 });
        assert_eq!(t[3].param_name, "c");
        assert_eq!(t[3].grid.samples(), 64 * 64 * 64);
    }

    #[test]
    fn refuses_overloads_bad_parameters_and_bad_sample_counts() {
        let e = scan_tests("void test_a() {}\nvoid test_a(float t) {}\n").unwrap_err();
        assert!(e[0].contains("test_a is defined twice"), "{e:?}");
        let e = scan_tests("void test_b(int i) {}\n").unwrap_err();
        assert!(e[0].contains("one float, vec2 or vec3"), "{e:?}");
        let e = scan_tests("void test_c(float a, float b) {}\n").unwrap_err();
        assert!(e[0].contains("test_c(float, float)"), "{e:?}");
        let e = scan_tests("// samples: 1\nvoid test_d(float t) {}\n").unwrap_err();
        assert!(e[0].starts_with("1: `// samples:`"), "{e:?}");
        let e = scan_tests("// samples: 10\nvoid test_e() {}\n").unwrap_err();
        assert!(e[0].contains("for property tests"), "{e:?}");
        let e = scan_tests("// samples: 99999999\nvoid test_f(vec3 c) {}\n").unwrap_err();
        assert!(e[0].contains("from 2 to"), "{e:?}");
    }

    #[test]
    fn grids_include_both_ends_and_cover_the_asked_samples() {
        let g = Grid::new(1, 5);
        assert_eq!(g.samples(), 5);
        let pts: Vec<f32> = (0..5).map(|i| g.point(i)[0]).collect();
        assert_eq!(pts, [0.0, 0.25, 0.5, 0.75, 1.0]);
        let g = Grid::new(2, 65536);
        assert_eq!(g.side, 256);
        assert_eq!(g.point(0), [0.0, 0.0, 0.0]);
        assert_eq!(g.point(255), [1.0, 0.0, 0.0]);
        assert_eq!(g.point(256), [0.0, 1.0 / 255.0, 0.0]);
        assert_eq!(g.point(65535), [1.0, 1.0, 0.0]);
        let g = Grid::new(3, 1000);
        assert_eq!(g.side, 10);
        assert_eq!(g.point(999), [1.0, 1.0, 1.0]);
        // Never fewer samples than asked.
        for n in [2, 3, 7, 50, 1001, 4097] {
            for d in 1..=3 {
                assert!(Grid::new(d, n).samples() >= n);
            }
        }
    }

    #[test]
    fn numbers_assertions_and_keeps_lines() {
        let src = "\
#include <kit/a.glsl>

#include \"b.glsl\"
void test_a() {
    expect_near(f(1.0), 1.0, 1e-6); expect(true);
    // expect(false) in a comment
    myexpect(1.0); s.expect(2.0);
    expect_eq(g(
        2),   /* two */ 4);
}
#define expect_x(a) expect(a)
";
        let (out, calls) = number_assertions(src);
        assert_eq!(out.lines().count(), src.lines().count());
        assert!(
            out.contains("expect_near(1, f(1.0), 1.0, 1e-6); expect(2, true);"),
            "{out}"
        );
        assert!(out.contains("// expect(false) in a comment"));
        assert!(out.contains("myexpect(1.0); s.expect(2.0);"));
        assert!(out.contains("expect_eq(3, g("));
        assert!(out.contains("#define expect_x(a) expect(a)"));
        assert_eq!(
            calls,
            vec![
                Assertion {
                    line: 5,
                    text: "expect_near(f(1.0), 1.0, 1e-6)".into()
                },
                Assertion {
                    line: 5,
                    text: "expect(true)".into()
                },
                Assertion {
                    line: 8,
                    text: "expect_eq(g(2), 4)".into()
                },
            ]
        );
    }

    #[test]
    fn decodes_the_first_failing_sample() {
        let ok = [1, 0, 3, 0];
        let fail = [1, 2, 3, 2 + 8 * KIND_FLOATS];
        let out0 = vec![ok, ok, fail, [1, 1, 1, 1], [1, 0, 0, 0]];
        let o = decode(&out0, 5, |i| {
            assert_eq!(i, 2);
            ([1.5f32.to_bits(), 0, 0, 0], [2.0f32.to_bits(), 0, 0, 0])
        });
        assert_eq!((o.failed, o.empty, o.undrawn), (2, 1, 0));
        let f = o.first.clone().unwrap();
        assert_eq!((f.sample, f.id, f.kind, f.comps), (2, 2, KIND_FLOATS, 2));
        assert_eq!(f32::from_bits(f.actual[0]), 1.5);
        assert!(!o.passed());
        // All samples empty: "ran no assertions"; undrawn samples fail too.
        let o = decode(&[[1, 0, 0, 0]; 3], 3, |_| unreachable!());
        assert!(!o.passed() && o.empty == 3);
        let o = decode(&[[1, 0, 1, 0], [0, 0, 0, 0]], 3, |_| unreachable!());
        assert_eq!(o.undrawn, 2);
        assert!(!o.passed());
        // Some samples skipping their assertions is fine.
        let o = decode(&[[1, 0, 1, 0], [1, 0, 0, 0]], 2, |_| unreachable!());
        assert!(o.passed());
    }

    #[test]
    fn describes_failures_with_input_values_and_call() {
        let t = scan_tests("void test_r(vec2 p) {}\n").unwrap().remove(0);
        let o = Outcome {
            samples: t.grid.samples(),
            failed: 3,
            empty: 0,
            undrawn: 0,
            first: Some(Failure {
                sample: 257,
                id: 1,
                kind: KIND_FLOATS,
                comps: 2,
                actual: [0.5, f32::NAN, 0.0, 0.0].map(f32::to_bits),
                expected: [0.5, 1.0, 0.0, 0.0].map(f32::to_bits),
            }),
        };
        let a = vec![Assertion {
            line: 7,
            text: "expect_near(f(p), g(p), 1e-3)".into(),
        }];
        let d = describe(&t, &o, &a, "t.test.glsl");
        assert_eq!(d[0], "t.test.glsl:7: expect_near(f(p), g(p), 1e-3)");
        let step = 1.0f32 / 255.0;
        assert_eq!(
            d[1],
            format!("at p = ({step:?}, {step:?}): got (0.5, NaN), expected (0.5, 1.0)")
        );
        assert_eq!(d[2], "3 of 65536 samples failed");
        // Integers are shown exactly, whatever their size.
        let t = scan_tests("void test_i() {}\n").unwrap().remove(0);
        let a_one = vec![Assertion {
            line: 1,
            text: "expect_eq(a, b)".into(),
        }];
        let show = |kind, comps, a: [u32; 4], e: [u32; 4]| {
            let o = Outcome {
                samples: 1,
                failed: 1,
                empty: 0,
                undrawn: 0,
                first: Some(Failure {
                    sample: 0,
                    id: 1,
                    kind,
                    comps,
                    actual: a,
                    expected: e,
                }),
            };
            describe(&t, &o, &a_one, "t.test.glsl")[1].clone()
        };
        assert_eq!(
            show(KIND_UINTS, 1, [u32::MAX, 0, 0, 0], [u32::MAX - 1, 0, 0, 0]),
            "got 4294967295, expected 4294967294"
        );
        assert_eq!(
            show(
                KIND_INTS,
                2,
                [(-2147483647i32) as u32, 16777217, 0, 0],
                [0, 16777216, 0, 0]
            ),
            "got (-2147483647, 16777217), expected (0, 16777216)"
        );
        assert_eq!(
            show(KIND_BOOLS, 1, [1, 0, 0, 0], [0, 0, 0, 0]),
            "got true, expected false"
        );
    }

    #[test]
    fn prelude_and_dispatcher_are_strict_glsl() {
        let dir = std::env::temp_dir().join(format!("sg-gtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.test.glsl");
        std::fs::write(
            &path,
            "void test_all() {\n\
             expect(true); expect(true, 1.0); expect(true, vec3(1.0));\n\
             expect_eq(1, 1); expect_eq(ivec2(1), ivec2(1)); expect_eq(true, true); expect_eq(vec4(1.0), vec4(1.0));\n\
             expect_near(1.0, 1.0, 0.1); expect_near(vec2(1.0), vec2(1.0), 0.1);\n\
             expect_rel(vec3(1.0), vec3(1.0), 0.1); expect_finite(vec4(1.0));\n\
             expect_eq(1u, 1u); expect_eq(uvec3(2u), uvec3(2u)); expect_eq(ivec4(-1), ivec4(-1));\n}\n\
             void test_p(float t) { expect(t >= 0.0); }\n\
             void test_q(vec2 p) { expect(p.x >= 0.0); }\n\
             void test_r(vec3 c) { expect(c.z >= 0.0); }\n",
        )
        .unwrap();
        let f = TestFile {
            path: path.clone(),
            root: dir.clone(),
            libs: vec![],
            display: "a.test.glsl".into(),
        };
        for d in [Dialect::BASELINE, Dialect::WEB, Dialect::desktop(Some(430))] {
            let p = prepare(&f, d).unwrap();
            assert_eq!(p.tests.len(), 4);
            assert_eq!(p.assertions.len(), 17);
            #[cfg(feature = "strict-glsl")]
            if let Err(e) = validate(&p.asm, Stage::Fragment) {
                panic!("{d}: {e:?}\n{}", p.asm.text);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "strict-glsl")]
    #[test]
    fn web_test_preflight_reports_included_warnings_and_strict_rejects_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.test.glsl");
        std::fs::write(
            dir.path().join("helper.glsl"),
            "float kit__unit() { return 1.0; }",
        )
        .unwrap();
        std::fs::write(
            &path,
            "#include \"helper.glsl\"\nvoid test_helper() { expect(kit__unit() == 1.0); }",
        )
        .unwrap();
        let file = TestFile {
            path,
            root: dir.path().into(),
            libs: vec![],
            display: "a.test.glsl".into(),
        };
        let src = prepare(&file, Dialect::WEB).unwrap().asm;
        let mut messages = Vec::new();
        check_test_shader(&src, WebChecks::default(), &mut |s| {
            messages.push(s.to_owned())
        })
        .unwrap();
        assert!(messages.is_empty());
        let mut web = WebChecks {
            web: true,
            strict: false,
        };
        check_test_shader(&src, web, &mut |s| messages.push(s.to_owned())).unwrap();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("helper.glsl:1: warning:"))
        );
        web.strict = true;
        let error = check_test_shader(&src, web, &mut |_| {}).unwrap_err();
        assert!(error.to_string().contains("browser compatibility warnings"));
    }

    #[cfg(feature = "strict-glsl")]
    #[test]
    fn web_tests_only_require_portability_of_included_libraries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let lib = root.join("desktop");
        std::fs::create_dir(&lib).unwrap();
        std::fs::write(
            lib.join("library.toml"),
            "name = \"local/desktop\"\nversion = \"1.0.0\"\nglsl = 430\n",
        )
        .unwrap();
        std::fs::write(lib.join("helper.glsl"), "float helper() { return 1.0; }").unwrap();
        let file = TestFile {
            path: root.join("a.test.glsl"),
            root: root.into(),
            libs: vec![Library::read("desktop", &lib, crate::library::Origin::Path).unwrap()],
            display: "a.test.glsl".into(),
        };
        let web = WebChecks {
            web: true,
            strict: true,
        };
        let portable = "void test_portable() { expect(true); }";
        std::fs::write(&file.path, portable).unwrap();
        assert_eq!(file.desktop_opt_in(WebChecks::default()), Some(430));
        assert_eq!(file.desktop_opt_in(web), None);
        let src = prepare(&file, Dialect::WEB).unwrap().asm;
        check_test_shader(&src, web, &mut |_| {}).unwrap();

        std::fs::write(
            root.join("wrapper.glsl"),
            "#include <desktop/helper.glsl>\n",
        )
        .unwrap();
        std::fs::write(&file.path, format!("#include \"wrapper.glsl\"\n{portable}")).unwrap();
        let src = prepare(&file, Dialect::WEB).unwrap().asm;
        let err = check_test_shader(&src, web, &mut |_| {}).unwrap_err();
        assert!(
            err.to_string()
                .contains("included test library requires desktop GLSL 430")
        );
    }

    #[test]
    fn finds_test_files_but_skips_hidden_target_and_frozen_folders() {
        let dir = std::env::temp_dir().join(format!("sg-gfind-{}", std::process::id()));
        for f in [
            "a.test.glsl",
            "sub/b.test.glsl",
            "sub/c.glsl",
            ".git/d.test.glsl",
            "target/e.test.glsl",
            "film/shotglass-libs/kit/f.test.glsl",
        ] {
            let p = dir.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "").unwrap();
        }
        let found = find_files(&dir).unwrap();
        let rel: Vec<_> = found
            .iter()
            .map(|p| {
                p.strip_prefix(&dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(rel, ["a.test.glsl", "sub/b.test.glsl"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
