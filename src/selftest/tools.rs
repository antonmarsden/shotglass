//! Author tooling: `shotglass quality` metrics and `shotglass test` (GLSL and metric tests).

use super::*;

/// `[[sound.metric]]`: a quiet 220 Hz sine with a 0.2 step at sample 2^20, the first
/// sample of the second block, so the second difference there reads the previous
/// block's samples through the halo. |second difference| is ~1.7e-4 everywhere but
/// at the step (0.2, at samples 2^20 and 2^20 + 1). 25 s is two blocks, the second
/// partly empty: every sample is counted once.
pub(super) fn quality_clicks(gl: &Gl, dir: &Path) -> Result<String> {
    const K: usize = 1 << 20;
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.0, 0.0, 0.0, 1.0);")),
            (
                "music.glsl",
                &format!(
                    "vec2 mainSound(int samp, float time) {{\n  \
                     float s = 0.2 * sin(6.2831853 * 220.0 * float(samp % 48000) / 48000.0) + (samp >= {K} ? 0.2 : 0.0);\n  \
                     return vec2(s, 0.5 * s);\n}}\n"
                ),
            ),
            (
                "q/clicks.glsl",
                "void mainSoundMetric(out float value, in int samp) {
  vec2 d = getSound(samp) - 2.0 * getSound(samp - 1) + getSound(samp - 2);
  value = max(abs(d.x), abs(d.y));
}
",
            ),
            (
                "quality.toml",
                "[sound]\n[[sound.metric]]\nuse = \"q/clicks.glsl\"\nthreshold = 0.1\nmax = { max = 0.01 }\n",
            ),
        ],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.4\"\nfps = 30\n[sound]\nmusic = \"music.glsl\"\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=25\n",
    )?;
    strict_metrics(&m)?;
    let cfg = crate::quality::Config::of_film(&m)?.ok_or_else(|| anyhow!("no quality.toml"))?;
    let rep = crate::quality::run(gl.clone(), &m, &cfg, &[0.0], false)?;
    let s = rep
        .sound
        .as_ref()
        .ok_or_else(|| anyhow!("no sound report"))?;
    let c = s
        .metrics
        .first()
        .ok_or_else(|| anyhow!("no sound metric"))?;
    let n = m.sound_samples(48_000) as u64;
    let t = &c.total;
    if c.frames.len() != 2 || t.pixels != n || t.invalid != 0 {
        bail!(
            "{} blocks, {} samples measured of {n}, {} invalid",
            c.frames.len(),
            t.pixels,
            t.invalid
        );
    }
    if t.max.is_none_or(|x| (x - 0.2).abs() > 1e-3) {
        bail!("max {:?}, want 0.2", t.max);
    }
    let at = c.max_at.unwrap_or(-1.0) * 48_000.0;
    if !(K as f64 - 0.01..=K as f64 + 1.01).contains(&at) {
        bail!("max at sample {at}, want {K} or {}", K + 1);
    }
    let over = t.over.unwrap_or(0.0) * n as f64;
    if (over - 2.0).abs() > 1e-3 {
        bail!("{over} samples over the threshold, want 2");
    }
    // Block 1 never sees the click: its largest is the film's start (silence before
    // sample 0, so 0.2·sin(2π·220/48000) ≈ 0.0058 at sample 1).
    if c.frames[0].max.is_none_or(|x| x > 0.01) {
        bail!("block 1 max {:?}: the step leaked into it", c.frames[0].max);
    }
    if c.pass || rep.pass || !c.failures[0].starts_with("max 0.2") {
        bail!("failures: {:?}", c.failures);
    }
    Ok(format!(
        "max {:.4} at {:.5}s",
        t.max.unwrap(),
        c.max_at.unwrap()
    ))
}

/// Phase 3 of `shotglass quality` (#96): music is a 997 Hz sine at amplitude 0.1 in
/// both channels, which BS.1770 defines as -20 LUFS (K-weighting is ~0 dB there);
/// its peaks are -20 dBFS / dBTP. A `[sound]`-only config renders no picture, and a
/// true-peak limit below the peak fails.
pub(super) fn quality_sound(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.0, 0.0, 0.0, 1.0);")),
            (
                "music.glsl",
                "vec2 mainSound(int samp, float time) {
  return vec2(0.1 * sin(6.2831853 * 997.0 * time));
}
",
            ),
            (
                "quality.toml",
                "[sound]
integrated = { min = -21, max = -19 }
true_peak = { max = -21 }
",
            ),
        ],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.4\"
fps = 30
[sound]
music = \"music.glsl\"
[[shot]]
id=\"a\"
scene=\"s.glsl\"
duration=5
",
    )?;
    let cfg = crate::quality::Config::of_film(&m)?.ok_or_else(|| anyhow!("no quality.toml"))?;
    let rep = crate::quality::run(gl.clone(), &m, &cfg, &[0.0, 1.0], false)?;
    if !rep.frames.is_empty() || !rep.metrics.is_empty() {
        bail!("a [sound]-only config rendered {} frames", rep.frames.len());
    }
    let s = rep
        .sound
        .as_ref()
        .ok_or_else(|| anyhow!("no sound report"))?;
    let l = s
        .measured
        .as_ref()
        .ok_or_else(|| anyhow!("sound not measured"))?;
    let checks = [
        ("integrated", l.integrated, -20.0, 0.05),
        ("momentary max", l.momentary_max, -20.0, 0.05),
        ("short-term max", l.short_term_max, -20.0, 0.05),
        ("sample peak", l.sample_peak, -20.0, 0.01),
        ("true peak", l.true_peak, -20.0, 0.05),
    ];
    for (what, got, want, tol) in checks {
        if got.is_none_or(|g| (g - want).abs() > tol) {
            bail!("{what}: got {got:?}, want {want} ± {tol}");
        }
    }
    if (l.seconds - 5.0).abs() > 0.01 {
        bail!("measured {} s of a 5 s film", l.seconds);
    }
    if rep.pass || s.failures.len() != 1 || !s.failures[0].starts_with("true_peak -19.99") {
        bail!("sound failures: {:?}", s.failures);
    }
    Ok(format!(
        "integrated {:.3} LUFS, true peak {:.3} dBTP",
        l.integrated.unwrap(),
        l.true_peak.unwrap()
    ))
}

/// Phase 2 of `shotglass quality` (#96): red flips every film frame (0, 1, 0, …), so
/// |frame − prev| + 2·|frame − next| is 3 inside the film, 2 on frame 0 (prev clamps to
/// it) and 1 on the last frame (next clamps). A second metric measures (x + ½)/256 per
/// column: exact in float, so its histogram is exactly 270 per bin for bins 0..157,
/// and min, max, over-threshold and percentiles have exact values.
pub(super) fn quality_temporal(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            (
                "s.glsl",
                &scene("fragColor = vec4(float(iFrame % 2), 0.0, 0.0, 1.0);"),
            ),
            (
                "q/flip.glsl",
                "void mainMetric(out vec4 value, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  float r = getFrame(uv).r;
  float d = abs(r - getPrev(uv).r) + 2.0 * abs(r - getNext(uv).r);
  value = vec4(vec3(d / 3.0), d);
}
",
            ),
            (
                "q/ramp.glsl",
                "void mainMetric(out vec4 value, in vec2 fragCoord) {
  float v = (floor(fragCoord.x) + 0.5) / 256.0;
  value = vec4(vec3(v), v);
}
",
            ),
            (
                "quality.toml",
                "size = \"158x90\"
                 [[metric]]
use = \"q/flip.glsl\"
                 [[metric]]
use = \"q/ramp.glsl\"
threshold = 0.3
p95 = { max = 0.5 }
over = { max = 1 }
",
            ),
        ],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.4\"
fps = 30
[[shot]]
id=\"a\"
scene=\"s.glsl\"
duration=2
",
    )?;
    strict_metrics(&m)?;
    let last = m.frame_count() - 1;
    let cfg = crate::quality::Config::of_film(&m)?.ok_or_else(|| anyhow!("no quality.toml"))?;
    let times = [0.0, m.frame_time(30), m.frame_time(last)];
    let rep = crate::quality::run(gl.clone(), &m, &cfg, &times, false)?;
    let ends: Vec<_> = rep
        .frames
        .iter()
        .map(|f| (f.frame, f.prev, f.next))
        .collect();
    let want = [
        (0, Some(0), Some(1)),
        (30, Some(29), Some(31)),
        (last as i32, Some(last - 1), Some(last)),
    ];
    if ends != want {
        bail!("frames (n, prev, next): {ends:?}, want {want:?}");
    }
    let flip = &rep.metrics[0];
    let means: Vec<_> = flip.frames.iter().map(|f| f.mean).collect();
    if means != [Some(2.0), Some(3.0), Some(1.0)] || flip.variants != ["frame", "prev", "next"] {
        bail!("flip: means {means:?}, variants {:?}", flip.variants);
    }
    let ramp = &rep.metrics[1];
    let t = &ramp.total;
    let n = 158 * 90 * 3;
    let hist_ok = t
        .histogram
        .iter()
        .enumerate()
        .all(|(i, &c)| c == if i < 158 { 270 } else { 0 });
    if !hist_ok || t.pixels != n {
        bail!("ramp histogram: {:?} ({} pixels)", t.histogram, t.pixels);
    }
    let checks = [
        ("min", t.min, 0.5 / 256.0),
        ("max", t.max, 157.5 / 256.0),
        ("over", t.over, 81.0 / 158.0), // (x + ½)/256 > 0.3 from x = 77
        ("p50", t.percentiles.get("p50").copied(), 79.0 / 256.0),
        ("p95", t.percentiles.get("p95").copied(), 150.1 / 256.0),
    ];
    for (what, got, want) in checks {
        if got.is_none_or(|g| (g - want).abs() > 1e-6) {
            bail!("ramp {what}: got {got:?}, want {want}");
        }
    }
    if ramp.pass || ramp.failures.len() != 1 || !ramp.failures[0].starts_with("p95 0.58") {
        bail!("ramp failures: {:?}", ramp.failures);
    }
    Ok(format!(
        "p95 {:.4}, over {:.4}",
        t.percentiles["p95"],
        t.over.unwrap()
    ))
}

/// `shotglass quality` (#96) on a fixture film: an edge at x = 0.5094 (pixel 80 of
/// 158 is lit at 1×, half lit in the 4×4 reference) and a green ramp of exact 8-bit values. Pins the reduced
/// numbers exactly, through the 4×4 reduction levels (158×90 → 40×23 → 10×6 → 3×2 → 1×1:
/// partial blocks at every edge, so an off-by-one bounds test reads past it), the `invalid` count (NaN and Inf left out of the
/// mean), limits, and a heatmap pixel.
pub(super) fn quality_metrics(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            (
                "s.glsl",
                &scene(
                    "fragColor = vec4(step(0.5094, uv.x), floor(fragCoord.x) / 255.0, 0.0, 1.0);",
                ),
            ),
            (
                "q/green.glsl",
                "void mainMetric(out vec4 value, in vec2 fragCoord) {\n  float g = getFrame(fragCoord / iResolution.xy).g * GAIN;\n  value = vec4(vec3(g), g);\n}\n",
            ),
            (
                "q/alias.glsl",
                "void mainMetric(out vec4 value, in vec2 fragCoord) {\n  vec2 uv = fragCoord / iResolution.xy;\n  float d = abs(getFrame(uv).r - getReference(uv).r);\n  value = vec4(vec3(d), d);\n}\n",
            ),
            (
                "q/bad.glsl",
                "void mainMetric(out vec4 value, in vec2 fragCoord) {\n  float x = 1.0;\n  if (fragCoord.x < 10.0) x = uintBitsToFloat(0x7fc00000u);\n  if (fragCoord.y > 89.0) x = uintBitsToFloat(0x7f800000u);\n  value = vec4(0.0, 0.0, 0.0, x);\n}\n",
            ),
            (
                "q/skip.glsl",
                "void mainMetric(out vec4 value, in vec2 fragCoord) {\n  value = vec4(0.0, 0.0, 0.0, fragCoord.x < 79.0 ? notMeasured() : 1.0);\n}\n",
            ),
            (
                "quality.toml",
                "size = \"158x90\"\nreference = 4\n\
                 [[metric]]\nuse = \"q/green.glsl\"\nparams = { GAIN = 2 }\nmean = { max = 2.0 }\n\
                 [[metric]]\nuse = \"q/alias.glsl\"\nmax = { max = 0.4 }\n\
                 [[metric]]\nuse = \"q/bad.glsl\"\n\
                 [[metric]]\nuse = \"q/skip.glsl\"\nrange = [0, 2]\nmin = { min = 1 }\np50 = { min = 0.99 }\n",
            ),
        ],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.4\"\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\n",
    )?;
    strict_metrics(&m)?;
    let cfg = crate::quality::Config::of_film(&m)?.ok_or_else(|| anyhow!("no quality.toml"))?;
    let rep = crate::quality::run(gl.clone(), &m, &cfg, &[1.0], true)?;
    let close = |what: &str, got: Option<f64>, want: f64| -> Result<()> {
        match got {
            Some(g) if (g - want).abs() <= 1e-5 * want.abs().max(1.0) => Ok(()),
            _ => bail!("{what}: got {got:?}, want {want}"),
        }
    };
    // green: 2 × the ramp x/255 (exact in 8 bits); max at the last column.
    let ramp = |x: usize| x as f64 / 255.0;
    let mean = (0..158).map(|x| 2.0 * ramp(x)).sum::<f64>() / 158.0;
    let g = &rep.metrics[0];
    close("green mean", g.total.mean, mean)?;
    close("green max", g.total.max, 2.0 * ramp(157))?;
    if g.variants != ["frame"] || !g.pass || g.total.pixels != 158 * 90 {
        bail!("green: {g:?}");
    }
    // alias: one column differs by 0.5.
    let a = &rep.metrics[1];
    close("alias mean", a.total.mean, 0.5 / 158.0)?;
    close("alias max", a.total.max, 0.5)?;
    if a.variants != ["frame", "reference"]
        || a.pass
        || !a.failures.iter().any(|f| f.starts_with("max 0.5"))
    {
        bail!(
            "alias: variants {:?}, failures {:?}",
            a.variants,
            a.failures
        );
    }
    let heat = a.heatmap.as_ref().ok_or_else(|| anyhow!("no heatmap"))?;
    for (x, want) in [(79, 0u8), (80, 128), (81, 0)] {
        let px = heat[4 + 45][4 + x];
        if px[0].abs_diff(want) > 1 {
            bail!("alias heatmap at x = {x}: {px:?}, want {want}");
        }
    }
    // bad: NaN in 10 columns, Inf in the top row (overlap counted once).
    let b = &rep.metrics[2];
    let invalid = 10 * 90 + 148;
    if b.total.invalid != invalid || b.total.pixels != 158 * 90 - invalid || b.pass {
        bail!("bad: {:?}, failures {:?}", b.total, b.failures);
    }
    close("bad mean (finite only)", b.total.mean, 1.0)?;
    // skip: the left 79 columns are notMeasured(): in no reduction, not invalid.
    let k = &rep.metrics[3];
    let left = 79 * 90;
    if k.total.unmeasured != left
        || k.total.pixels != 158 * 90 - left
        || k.total.invalid != 0
        || k.total.histogram.iter().sum::<u64>() != k.total.pixels
        || !k.pass
    {
        bail!("skip: {:?}, failures {:?}", k.total, k.failures);
    }
    close("skip mean (measured only)", k.total.mean, 1.0)?;
    if rep.pass {
        bail!("the report passes with failing metrics");
    }
    Ok(format!(
        "alias {:.6}/{:.2}, {} invalid",
        a.total.mean.unwrap(),
        a.total.max.unwrap(),
        b.total.invalid
    ))
}

/// `shotglass test` on a fixture project (a path library and one test file): the exact
/// report pins the first failure per test, its input and values, NaN handling, grid
/// endpoints, banded draws (140 000 samples: three bands), "ran no assertions" and
/// expect_rel, and integers (uint, large int) shown exactly, on this driver.
pub(super) fn glsl_tests(gl: &Gl, dir: &Path) -> Result<String> {
    use crate::glsl_test::{TestFile, run};
    write(
        dir,
        &[
            (
                "shotglass.toml",
                "shotglass = \"0.1\"\n[libraries]\nfx = { path = \"lib\" }\n",
            ),
            (
                "lib/library.toml",
                "name = \"selftest/fx\"\nversion = \"0.1.0\"\nlicences = [\"MIT\"]\n",
            ),
            (
                "lib/fx.glsl",
                "float fx_double(float x) { return 2.0 * x; }\n",
            ),
            (
                "unit/broken.test.glsl",
                "#include <fx/fx.glsl>\n\nvoid test_typo() { expect(fx_doubel(1.0) == 2.0); }\n",
            ),
            (
                "unit/fx.test.glsl",
                "#include <fx/fx.glsl>\n\
                 void test_pass() { expect_near(fx_double(1.5), 3.0, 0.0); expect_eq(ivec2(1, 2), ivec2(1, 2)); }\n\
                 void test_first_failure() {\n\
                 \x20   expect(true); expect_eq(1, 2); expect_eq(3, 4);\n\
                 }\n\
                 // samples: 11\n\
                 void test_threshold(float t) {\n\
                 \x20   expect(fx_double(t) < 1.5, fx_double(t));\n\
                 }\n\
                 void test_nan(vec2 p) {\n\
                 \x20   expect_finite(vec2(1.0, uintBitsToFloat(0x7fc00000u + uint(p.x * 0.0))));\n\
                 }\n\
                 void test_nan1(float t) { expect_finite(uintBitsToFloat(0x7fc00000u + uint(t * 0.0))); }\n\
                 void test_corner(vec3 c) { expect(!(c.x == 1.0 && c.y == 1.0 && c.z == 1.0), c); }\n\
                 // samples: 140000\n\
                 void test_bands(float t) { expect(t >= 0.0 && t <= 1.0); }\n\
                 void test_empty(float t) { if (t > 2.0) expect(false); }\n\
                 void test_rel() {\n\
                 \x20   expect_rel(100.0, 101.0, 0.02);\n\
                 \x20   expect_rel(vec2(1.0, 0.0), vec2(1.0, 1e-3), 0.5);\n\
                 }\n\
                 void test_uint() { expect_eq(uvec2(7u, 0xffffffffu), uvec2(7u, 0xfffffffeu)); }\n\
                 void test_bigint() { expect_eq(-16777217, -16777216); }\n",
            ),
        ],
    )?;
    let p = crate::project::Project::read(dir)?;
    let libs = crate::library::resolve(dir, &p.specs, &Default::default(), None, dir)?;
    // The broken file comes first: the fixture file must still run after it.
    let files = ["unit/broken.test.glsl", "unit/fx.test.glsl"].map(|f| TestFile {
        path: dir.join(f),
        root: dir.to_path_buf(),
        libs: libs.clone(),
        display: f.into(),
    });
    let mut report = String::new();
    let junk = dirty_pack_state(gl)?;
    let sum = run(gl, &files, None, Default::default(), &mut |l| {
        report += l;
        report.push('\n');
    });
    unsafe { gl.free_buffer(junk) };
    let sum = sum?;
    let expected = "\
unit/fx.test.glsl
  ok    test_pass
  FAIL  test_first_failure
        unit/fx.test.glsl:4: expect_eq(1, 2)
        got 1, expected 2
  FAIL  test_threshold (11 samples)
        unit/fx.test.glsl:8: expect(fx_double(t) < 1.5, fx_double(t))
        at t = 0.8: got 1.6
        3 of 11 samples failed
  FAIL  test_nan (65536 samples)
        unit/fx.test.glsl:11: expect_finite(vec2(1.0, uintBitsToFloat(0x7fc00000u + uint(p.x * 0.0))))
        at p = (0.0, 0.0): got (1.0, NaN)
        65536 of 65536 samples failed
  FAIL  test_nan1 (4096 samples)
        unit/fx.test.glsl:13: expect_finite(uintBitsToFloat(0x7fc00000u + uint(t * 0.0)))
        at t = 0.0: got NaN
        4096 of 4096 samples failed
  FAIL  test_corner (262144 samples)
        unit/fx.test.glsl:14: expect(!(c.x == 1.0 && c.y == 1.0 && c.z == 1.0), c)
        at c = (1.0, 1.0, 1.0): got (1.0, 1.0, 1.0)
        1 of 262144 samples failed
  ok    test_bands (140000 samples)
  FAIL  test_empty (4096 samples)
        unit/fx.test.glsl:17: test_empty ran no assertions
  FAIL  test_rel
        unit/fx.test.glsl:20: expect_rel(vec2(1.0, 0.0), vec2(1.0, 1e-3), 0.5)
        got (1.0, 0.0), expected (1.0, 0.001)
  FAIL  test_uint
        unit/fx.test.glsl:22: expect_eq(uvec2(7u, 0xffffffffu), uvec2(7u, 0xfffffffeu))
        got (7, 4294967295), expected (7, 4294967294)
  FAIL  test_bigint
        unit/fx.test.glsl:23: expect_eq(-16777217, -16777216)
        got -16777217, expected -16777216
";
    let Some((broken, report)) = report.split_once("unit/fx.test.glsl\n") else {
        bail!("the file after a broken one didn't run:\n{report}");
    };
    let report = format!("unit/fx.test.glsl\n{report}");
    if !(broken.starts_with("unit/broken.test.glsl\n")
        && broken.contains("unit/broken.test.glsl:3: ")
        && broken.contains("fx_doubel"))
    {
        bail!("a compile error should name unit/broken.test.glsl:3, got:\n{broken}");
    }
    if report != expected {
        bail!("report differs:\n--- expected\n{expected}--- got\n{report}");
    }
    if (sum.passed, sum.failed, sum.broken) != (2, 9, 1) {
        bail!("summary {sum:?}, expected 2 passed, 9 failed, 1 broken");
    }
    Ok(format!("{} samples", sum.samples))
}

/// Metric tests (`*.test.toml`, #106): library metrics on PNG and GLSL pictures
/// (`@frame`, `@reference`, `@prev` or `@next` alone), `notMeasured()` pixels, a missing variant, and GLSL and WAV
/// sound; must-pass and must-fail cases, an unexpected pass. Pins the whole report.
pub(super) fn metric_tests(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            (
                "shotglass.toml",
                "shotglass = \"0.1\"\n[libraries]\nq = { path = \"lib\" }\n",
            ),
            (
                "lib/library.toml",
                "name = \"selftest/q\"\nversion = \"0.1.0\"\nlicences = [\"MIT\"]\n",
            ),
            (
                "lib/edge.glsl",
                "void mainMetric(out vec4 value, in vec2 fragCoord) {\n  vec2 uv = fragCoord / iResolution.xy;\n  float d = abs(getFrame(uv).r - getReference(uv).r);\n  value = vec4(vec3(d), d);\n}\n",
            ),
            (
                "lib/ink.glsl",
                "void mainMetric(out vec4 value, in vec2 fragCoord) {\n  vec4 c = getFrame(fragCoord / iResolution.xy);\n  value = vec4(c.rgb, c.g < 0.5 ? notMeasured() : c.r);\n}\n",
            ),
            (
                "lib/behind.glsl",
                "void mainMetric(out vec4 value, in vec2 fragCoord) {\n  vec2 uv = fragCoord / iResolution.xy;\n  float d = abs(getFrame(uv).r - getPrev(uv).r);\n  value = vec4(vec3(d), d);\n}\n",
            ),
            (
                "lib/ahead.glsl",
                "void mainMetric(out vec4 value, in vec2 fragCoord) {\n  vec2 uv = fragCoord / iResolution.xy;\n  float d = abs(getFrame(uv).r - getNext(uv).r);\n  value = vec4(vec3(d), d);\n}\n",
            ),
            (
                "lib/flicker.glsl",
                "void mainMetric(out vec4 value, in vec2 fragCoord) {\n  vec2 uv = fragCoord / iResolution.xy;\n  float d = abs(getNext(uv).r - getPrev(uv).r);\n  value = vec4(vec3(d), d);\n}\n",
            ),
            (
                "lib/clicks.glsl",
                "void mainSoundMetric(out float value, in int samp) {\n  vec2 d = getSound(samp) - 2.0 * getSound(samp - 1) + getSound(samp - 2);\n  value = max(abs(d.x), abs(d.y));\n}\n",
            ),
            (
                "t/in/soft.glsl",
                "void mainImage(out vec4 fragColor, in vec2 fragCoord) {\n  fragColor = vec4(fragCoord.x < 4.0 ? 0.75 : 0.0, 0.0, 0.0, 1.0);\n}\n",
            ),
            (
                "t/in/ink.glsl",
                "void mainImage(out vec4 fragColor, in vec2 fragCoord) {\n  fragColor = vec4(fragCoord.x < 5.0 ? 0.0 : 1.0, step(5.0, fragCoord.x), 0.0, 1.0);\n}\n",
            ),
            // Exactly representable pulses keep the report independent of GLSL sin accuracy.
            (
                "t/in/pulses.glsl",
                "vec2 mainSound(int samp, float time) {\n  return vec2(samp % 4 == 0 ? 0.015625 : 0.0);\n}\n",
            ),
            (
                "t/q.test.toml",
                "[[case]]\nname = \"edge: same picture\"\nuse = \"q/edge\"\nframe = \"in/half.png\"\nreference = \"in/half.png\"\nmax = { max = 0 }\n\
                 [[case]]\nname = \"edge: soft reference\"\nuse = \"q/edge\"\nframe = \"in/half.png\"\nreference = \"in/soft.glsl\"\nexpect = \"fail\"\nmax = { max = 0.1 }\n\
                 [[case]]\nname = \"ink: left half not measured\"\nuse = \"q/ink\"\nframe = \"in/ink.glsl\"\nsize = \"10x4\"\nmean = { min = 1 }\n\
                 [[case]]\nname = \"behind: prev only\"\nuse = \"q/behind\"\nframe = \"in/half.png\"\nprev = \"in/soft.glsl\"\nmean = { min = 0.1 }\n\
                 [[case]]\nname = \"behind: GLSL frame and prev\"\nuse = \"q/behind\"\nframe = \"in/ink.glsl\"\nprev = \"in/soft.glsl\"\nsize = \"8x4\"\nmean = { min = 0.5 }\n\
                 [[case]]\nname = \"ahead: next only\"\nuse = \"q/ahead\"\nframe = \"in/half.png\"\nnext = \"in/soft.glsl\"\nmean = { min = 0.1 }\n\
                 [[case]]\nname = \"flicker: no prev\"\nuse = \"q/flicker\"\nframe = \"in/half.png\"\nnext = \"in/half.png\"\n\
                 [[case]]\nname = \"clicks: quiet pulses\"\nuse = \"q/clicks\"\nsound = \"in/pulses.glsl\"\nsamples = 4800\nmax = { max = 0.05 }\n\
                 [[case]]\nname = \"clicks: click\"\nuse = \"q/clicks\"\nsound = \"in/click.wav\"\nexpect = \"fail\"\nmax = { max = 0.01 }\n\
                 [[case]]\nname = \"clicks: unexpected pass\"\nuse = \"q/clicks\"\nsound = \"in/pulses.glsl\"\nsamples = 4800\nexpect = \"fail\"\nmax = { max = 0.5 }\n",
            ),
        ],
    )?;
    // 8x4, the left half red.
    let mut half = Frame::init(8, 4, [0, 0, 0, 255]);
    for row in half.rows_mut() {
        row[..4].fill([255, 0, 0, 255]);
    }
    crate::save_png(&dir.join("t/in/half.png"), &half)?;
    // 100 silent stereo samples, one of them 0.5.
    let mut wav = crate::audio::WavWriter::create(&dir.join("t/in/click.wav"), 100, 48000)?;
    let mut click = vec![0.0f32; 200];
    click[100] = 0.5;
    wav.write(&click)?;
    wav.finish()?;
    let p = crate::project::Project::read(dir)?;
    let libs = crate::library::resolve(dir, &p.specs, &Default::default(), None, dir)?;
    let files = [crate::glsl_test::TestFile {
        path: dir.join("t/q.test.toml"),
        root: dir.to_path_buf(),
        libs,
        display: "t/q.test.toml".into(),
    }];
    let mut report = String::new();
    let sum = crate::quality::testing::run(gl, &files, None, Default::default(), &mut |l| {
        report += l;
        report.push('\n');
    })?;
    let expected = "\
t/q.test.toml
  ok    edge: same picture  mean 0.000000  max 0.000000  (32 measured)
  ok    edge: soft reference  mean 0.125490  max 0.250980  (32 measured); fails as expected: max 0.250980 > max 0.1
  ok    ink: left half not measured  mean 1.000000  max 1.000000  (20 measured, 20 not measured)
  ok    behind: prev only  mean 0.125490  max 0.250980  (32 measured)
  ok    behind: GLSL frame and prev  mean 0.749510  max 1.000000  (32 measured)
  ok    ahead: next only  mean 0.125490  max 0.250980  (32 measured)
  ERROR flicker: no prev
        the metric reads @prev: give prev = \"…\"
  ok    clicks: quiet pulses  mean 0.015625  max 0.031250  (4800 measured)
  ok    clicks: click  mean 0.019999  max 0.999969  (100 measured); fails as expected: max 0.999969 > max 0.01
  FAIL  clicks: unexpected pass
        expected to fail, but every limit holds: mean 0.015625  max 0.031250  (4800 measured)
";
    if report != expected {
        bail!("report differs:\n--- expected\n{expected}--- got\n{report}");
    }
    if (sum.passed, sum.failed, sum.broken) != (8, 1, 1) {
        bail!("summary {sum:?}, expected 8 passed, 1 failed, 1 broken");
    }
    Ok(format!("{} cases", sum.passed + sum.failed + sum.broken))
}
