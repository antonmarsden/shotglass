//! `shotglass selftest`: checks the renderer and the soundtrack on *this* GPU and
//! driver, using small fixture films written to a temporary folder.
//!
//! `cargo test` runs it too (tests/gpu.rs), so CI exercises it on software OpenGL
//! and you can run it on a real GPU after a driver update.

use crate::gfx::{Frame, FrameInfo, Renderer};
use crate::movie::{At, Movie};
use crate::shader::Dialect;
use crate::sound::Soundtrack;
use anyhow::{Result, anyhow, bail};
use eframe::glow::{self, HasContext};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use toodee::{CopyOps, TooDeeOps, TooDeeOpsMut};

const W: i32 = 160;
const H: i32 = 90;

type Gl = Arc<eframe::glow::Context>;

struct Check {
    name: &'static str,
    run: fn(&Gl, &Path) -> Result<String>,
}

const CHECKS: &[Check] = &[
    Check {
        name: "transitions start on the old shot and end on the new one",
        run: transition_endpoints,
    },
    Check {
        name: "crossfade blends live and frozen shots correctly",
        run: crossfade_and_freeze,
    },
    Check {
        name: "reduced render scale has no edge bleed",
        run: render_scale_edges,
    },
    Check {
        name: "slow frames drawn in strips match a single draw",
        run: tiled_drawing,
    },
    Check {
        name: "readback into a larger frame lands in place and restores pack state",
        run: readback_into,
    },
    Check {
        name: "export YUV pass: BT.709 limited range, chroma sited left, read back pipelined",
        run: yuv_pass,
    },
    Check {
        name: "playback doesn't stall on driver error checks",
        run: error_check_rate,
    },
    Check {
        name: "compile errors point at the right file and line",
        run: error_mapping,
    },
    Check {
        name: "library files are included once, library transitions render, and errors point at name/file:line",
        run: library_errors,
    },
    Check {
        name: "a broken edit keeps the last working version on screen",
        run: keep_last_working,
    },
    Check {
        name: "program cache reloads identically and survives corruption",
        run: program_cache,
    },
    Check {
        name: "evicted programs come back correctly",
        run: eviction,
    },
    Check {
        name: "non-blocking render waits for compiles, then matches",
        run: lazy_compile,
    },
    Check {
        name: "shaders get the baseline #version, or the film's opt-in",
        run: version_line,
    },
    Check {
        name: "texture channels bind the right image to the right unit, upright as on Shadertoy",
        run: channel_binding,
    },
    Check {
        name: "texture filter, wrap, mipmaps and sRGB follow movie.toml; one upload per file",
        run: channel_sampling,
    },
    Check {
        name: "playback never blocks on a texture: uploads spread over frames, then match",
        run: texture_streaming,
    },
    Check {
        name: "a broken texture edit keeps the last working one; texture edits compile nothing",
        run: texture_edits,
    },
    Check {
        name: "evicted textures come back identically",
        run: texture_eviction,
    },
    Check {
        name: "frozen transitions use the outgoing shot's textures, and refreeze after an edit",
        run: texture_freeze,
    },
    Check {
        name: "texture uploads ignore and restore stray unpack state",
        run: texture_unpack_state,
    },
    Check {
        name: "iChannel4..7 bind like 0..3, with resolution and black when unbound",
        run: channels_high,
    },
    Check {
        name: "text textures: drawn without blocking, iChannelMeta reaches the shader, edits compile nothing",
        run: text_textures,
    },
    Check {
        name: "layers: source-over with opacity, @input, spans and fades",
        run: layers_compose,
    },
    Check {
        name: "layers: playback waits for every active layer; a broken layer edit keeps the last version",
        run: layers_ready_and_fallback,
    },
    Check {
        name: "layers: fixed scale ignores the render scale, auto follows it",
        run: layers_scale,
    },
    Check {
        name: "layers: blend passes, @picture and @<layer> (an earlier layer's own output)",
        run: layers_blend_and_refs,
    },
    Check {
        name: "layers: hidden layers (mute/solo) leave the picture, and their readers see black",
        run: layers_hidden,
    },
    Check {
        name: "footage: sequence frames map exactly (24 in 30, in, speed, loop, black, iChannelTime, a layer's clock)",
        run: footage_mapping,
    },
    Check {
        name: "footage: playback never blocks on a frame and shows only the exact one",
        run: footage_streaming,
    },
    Check {
        name: "footage: cuts and loop seams are prefetched (no stall)",
        run: footage_prefetch,
    },
    Check {
        name: "footage: play, still and export agree, after seeking back and eviction",
        run: footage_agree,
    },
    Check {
        name: "footage: resident frames stay in the texture budget; textures are reused",
        run: footage_budget,
    },
    Check {
        name: "footage: a broken frame keeps the last working one; a missing frame refuses the edit",
        run: footage_broken,
    },
    Check {
        name: "footage: frame uploads ignore and restore stray unpack state",
        run: footage_unpack_state,
    },
    Check {
        name: "chunked sound matches a single-pass render",
        run: sound_chunk_equivalence,
    },
    Check {
        name: "banded sound blocks match single draws",
        run: sound_banding,
    },
    Check {
        name: "streamed sound never waits for the GPU and matches the blocking render",
        run: sound_streaming,
    },
    Check {
        name: "sound has no clicks at cuts, fades and chunk edges",
        run: sound_no_clicks,
    },
    Check {
        name: "sound shader errors report the file and line and respect fallback timing",
        run: sound_errors,
    },
    Check {
        name: "export writes every frame and sample, matching still and wav",
        run: export_lossless,
    },
    Check {
        name: "shotglass test: assertions, property grids and failure reports",
        run: glsl_tests,
    },
    Check {
        name: "shotglass test: metric tests on PNG, GLSL and WAV inputs, expected failures",
        run: metric_tests,
    },
    Check {
        name: "loop films: the frame at t + k·period is the frame at t",
        run: loop_picture,
    },
    Check {
        name: "loop films: the soundtrack is seamless at the wrap and matches a long render",
        run: loop_sound,
    },
    Check {
        name: "quality metrics: @frame, @reference, GPU mean/max reductions, invalid, limits, heatmaps",
        run: quality_metrics,
    },
    Check {
        name: "quality metrics: @prev/@next (clamped at the ends), min, over, GPU histogram and percentiles",
        run: quality_temporal,
    },
    Check {
        name: "quality sound: BS.1770 loudness and true peak of a synthesised sine, sound-only config, limits",
        run: quality_sound,
    },
    Check {
        name: "quality sound metrics: a click across a block edge, its time, halo, partial last block",
        run: quality_clicks,
    },
];

/// `[[sound.metric]]`: a quiet 220 Hz sine with a 0.2 step at sample 2^20, the first
/// sample of the second block, so the second difference there reads the previous
/// block's samples through the halo. |second difference| is ~1.7e-4 everywhere but
/// at the step (0.2, at samples 2^20 and 2^20 + 1). 25 s is two blocks, the second
/// partly empty: every sample is counted once.
fn quality_clicks(gl: &Gl, dir: &Path) -> Result<String> {
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
fn quality_sound(gl: &Gl, dir: &Path) -> Result<String> {
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
fn quality_temporal(gl: &Gl, dir: &Path) -> Result<String> {
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
fn quality_metrics(gl: &Gl, dir: &Path) -> Result<String> {
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

pub fn run(filter: Option<String>) -> Result<()> {
    let dir = std::env::temp_dir().join(format!("shotglass-selftest-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let dir2 = dir.clone();
    let result = crate::headless::with_gl(move |gl| {
        let r = Renderer::new(gl.clone())?;
        println!("GPU: {}", r.gl_info);
        println!(
            "parallel shader compile: {}",
            if r.parallel { "yes" } else { "no" }
        );
        drop(r);
        let mut failed = vec![];
        let mut ran = 0;
        for (i, c) in CHECKS.iter().enumerate() {
            if filter
                .as_ref()
                .is_some_and(|f| !c.name.contains(f.as_str()))
            {
                continue;
            }
            ran += 1;
            let sub = dir2.join(format!("check{i}"));
            std::fs::create_dir_all(&sub)?;
            let t = Instant::now();
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (c.run)(&gl, &sub)))
                .unwrap_or_else(|p| {
                    let msg = p
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_default();
                    Err(anyhow!("panicked: {msg}"))
                });
            let ms = t.elapsed().as_secs_f32() * 1000.0;
            match res {
                Ok(note) => {
                    println!(
                        "  ok    {} ({ms:.0} ms){}",
                        c.name,
                        if note.is_empty() {
                            String::new()
                        } else {
                            format!(" — {note}")
                        }
                    )
                }
                Err(e) => {
                    println!("  FAIL  {} ({ms:.0} ms)\n        {e:#}", c.name);
                    failed.push(c.name);
                }
            }
        }
        if failed.is_empty() {
            println!("\n{ran} checks passed");
            Ok(())
        } else {
            bail!("{} of {ran} checks failed", failed.len())
        }
    });
    let _ = std::fs::remove_dir_all(&dir);
    result
}

// ------------------------------------------------------------------ helpers

fn write(dir: &Path, files: &[(&str, &str)]) -> Result<()> {
    for (name, text) in files {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(p, text)?;
    }
    Ok(())
}

/// `shotglass test` on a fixture project (a path library and one test file): the exact
/// report pins the first failure per test, its input and values, NaN handling, grid
/// endpoints, banded draws (140 000 samples: three bands), "ran no assertions" and
/// expect_rel, and integers (uint, large int) shown exactly, on this driver.
fn glsl_tests(gl: &Gl, dir: &Path) -> Result<String> {
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
    let sum = run(gl, &files, None, &mut |l| {
        report += l;
        report.push('\n');
    });
    unsafe { gl.delete_buffer(junk) };
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
fn metric_tests(gl: &Gl, dir: &Path) -> Result<String> {
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
    let sum = crate::quality::testing::run(gl, &files, None, &mut |l| {
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

fn scene(body: &str) -> String {
    format!(
        "void mainImage(out vec4 fragColor, in vec2 fragCoord) {{\n  vec2 uv = fragCoord / iResolution.xy;\n  {body}\n}}\n"
    )
}

fn info(t: f32, scale: f32) -> FrameInfo {
    FrameInfo {
        t,
        dt: 1.0 / 30.0,
        frame: (t * 30.0) as i32,
        scale,
    }
}

/// A fixture film's metric shaders (`quality.toml`) must pass the strict check, at the
/// baseline and as GLSL ES, like its other shaders ([`strict`]).
fn strict_metrics(m: &Movie) -> Result<()> {
    if !crate::validate::AVAILABLE {
        return Ok(());
    }
    for d in [Dialect::BASELINE, Dialect::WEB] {
        let errors = crate::validate::validate_sources(&crate::quality::sources(m, d)?);
        if !errors.is_empty() {
            bail!(
                "metric fixtures aren't strictly valid {d}:\n{}",
                crate::gfx::join_errors(&errors)
            );
        }
    }
    Ok(())
}

/// Fixture films must pass the strict check too, at the baseline and as GLSL ES, so
/// the selftest doubles as the strict check's test corpus (and CI runs it).
fn strict(m: &Movie) -> Result<()> {
    if !crate::validate::AVAILABLE {
        return Ok(());
    }
    for d in [Dialect::desktop(m.glsl), Dialect::WEB] {
        let errors = crate::validate::validate_movie(m, d);
        if !errors.is_empty() {
            bail!(
                "fixture isn't strictly valid {d}:\n{}",
                crate::gfx::join_errors(&errors)
            );
        }
    }
    Ok(())
}

/// Renderers whose sound isn't bit-exact across draw sizes, so "chunked / banded /
/// streamed / exported sound matches exactly" can't hold there. Apple's software
/// renderer (macOS CI) differs by about 1e-5 depending on the block drawn; those
/// comparisons are reported, not failed. Real GPUs and llvmpipe must match exactly.
const INEXACT_SOUND_RENDERERS: &[&str] = &["Apple Software Renderer"];

/// None if sound must match exactly on this driver, else the renderer's name.
fn inexact_sound(gl: &Gl) -> Option<String> {
    use eframe::glow::{self, HasContext};
    let name = unsafe { gl.get_parameter_string(glow::RENDERER) };
    INEXACT_SOUND_RENDERERS
        .contains(&name.as_str())
        .then_some(name)
}

/// Fail with `msg`, or on a renderer listed in `INEXACT_SOUND_RENDERERS` note it.
fn sound_mismatch(gl: &Gl, notes: &mut Vec<String>, msg: String) -> Result<()> {
    match inexact_sound(gl) {
        Some(name) => {
            notes.push(format!("exact match skipped on {name}: {msg}"));
            Ok(())
        }
        None => bail!(msg),
    }
}

fn renderer(gl: &Gl, m: &Movie) -> Result<Renderer> {
    strict(m)?;
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None); // checks must not depend on (or pollute) the user's cache
    r.load_all(m)
        .map_err(|e| anyhow!("{}", e.iter().map(|e| e.to_string()).collect::<String>()))?;
    Ok(r)
}

/// Render and read back RGBA8, top row first.
fn shot(r: &mut Renderer, m: &Movie, at: At, scale: f32) -> Result<Frame> {
    let t = match at {
        At::Shot { shot, local } => m.shots[shot].start + local,
        At::Transition { to, to_local, .. } => m.shots[to].start + to_local,
    };
    r.render_at(m, W, H, &info(t, scale), at, true)
        .ok_or_else(|| anyhow!("nothing rendered"))?;
    r.read_output().ok_or_else(|| anyhow!("no output"))
}

/// (largest channel difference, fraction of pixels differing by more than `tol`).
fn diff(a: &Frame, b: &Frame, tol: u8) -> (u8, f32) {
    let mut max = 0u8;
    let mut over = 0usize;
    for (pa, pb) in a.cells().zip(b.cells()) {
        let d = (0..3).map(|i| pa[i].abs_diff(pb[i])).max().unwrap();
        max = max.max(d);
        over += (d > tol) as usize;
    }
    (max, over as f32 / a.data().len() as f32)
}

/// One pixel as [r, g, b, a].
fn px(f: &Frame, col: usize, row: usize) -> [u8; 4] {
    f[(col, row)]
}

fn expect_same(what: &str, a: &Frame, b: &Frame, tol: u8, frac: f32) -> Result<()> {
    // Bulk compare first (TooDee's PartialEq: same size, same cells); only walk the
    // pixels for the tolerance check and the error message when they differ.
    if a == b {
        return Ok(());
    }
    if a.size() != b.size() {
        bail!("{what}: frames are {:?} and {:?}", a.size(), b.size());
    }
    let (max, over) = diff(a, b, tol);
    if over > frac {
        bail!(
            "{what}: {:.2}% of pixels differ by more than {tol}/255 (max {max})",
            over * 100.0
        );
    }
    Ok(())
}

/// Transitions the fixture films use, as files in the film (`type = "fx/wipe.glsl"`):
/// shotglass has no built-in looks, and the selftest must not need the network.
const FX: &[(&str, &str)] = &[
    (
        "fx/crossfade.glsl",
        "vec4 transition(vec2 uv) { return mix(getFromColor(uv), getToColor(uv), progress); }\n",
    ),
    (
        "fx/wipe.glsl",
        "#ifndef direction\n#define direction vec2(1.0, 0.0)\n#endif\n\
         #ifndef softness\n#define softness 0.01\n#endif\n\
         vec4 transition(vec2 uv) {\n\
         \x20   vec2 d = normalize(direction);\n\
         \x20   float lo = min(min(0.0, d.x), min(d.y, d.x + d.y));\n\
         \x20   float hi = max(max(0.0, d.x), max(d.y, d.x + d.y));\n\
         \x20   float edge = mix(lo - softness, hi + softness, progress);\n\
         \x20   float m = smoothstep(edge - softness, edge + softness, dot(uv, d));\n\
         \x20   return mix(getToColor(uv), getFromColor(uv), m);\n\
         }\n",
    ),
];

/// Write a fixture film (one text: `shotglass = …` and `[libraries]` go to its
/// shotglass.toml) and load it.
fn movie(dir: &Path, toml: &str) -> Result<Movie> {
    write(dir, FX)?;
    crate::project::write_fixture(dir, toml)?;
    Movie::load(dir)
}

// ------------------------------------------------------------------ picture checks

/// What the driver actually compiled: pictures and sound report `__VERSION__`, which
/// the driver sets from the `#version` line the harness emitted.
fn version_line(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            (
                "v.glsl",
                &scene("fragColor = vec4(float(__VERSION__) / 1000.0, 0.0, 0.0, 1.0);"),
            ),
            (
                "music.glsl",
                "vec2 mainSound(int samp, float time) {\n  return vec2(float(__VERSION__) / 1000.0);\n}\n",
            ),
        ],
    )?;
    let film = |glsl: Option<u32>| -> Result<Movie> {
        let line = glsl.map_or(String::new(), |v| format!("glsl = \"{v}\"\n"));
        movie(
            dir,
            &format!(
                "{line}[sound]\nmusic = \"music.glsl\"\n[[shot]]\nid=\"v\"\nscene=\"v.glsl\"\nduration=0.1\n"
            ),
        )
    };
    // What the driver compiled each part as (picture, sound), or why it refused the film.
    let compiled = |m: &Movie| -> Result<std::result::Result<(u32, u32), String>> {
        let mut r = Renderer::new(gl.clone())?;
        r.set_cache_dir(None);
        if let Err(e) = r.load_all(m) {
            return Ok(Err(e.iter().map(|e| e.to_string()).collect()));
        }
        let f = shot(
            &mut r,
            m,
            At::Shot {
                shot: 0,
                local: 0.0,
            },
            1.0,
        )?;
        let picture = (px(&f, W as usize / 2, H as usize / 2)[0] as f32 / 255.0 * 1000.0).round();
        let mut st = Soundtrack::new();
        if let Err(e) = st.set_movie(&mut r, m, 48000) {
            return Ok(Err(e.iter().map(|e| e.to_string()).collect()));
        }
        let mut first = None;
        st.render_all(&mut r, |d| {
            first.get_or_insert(d[0]);
        })
        .map_err(|e| anyhow!(e))?;
        let sound = (first.ok_or_else(|| anyhow!("no sound"))? * 1000.0).round();
        Ok(Ok((picture as u32, sound as u32)))
    };
    // 8-bit colour carries the version to within ±2 (4 per step); sound is exact.
    let near = |got: u32, want: u32| got.abs_diff(want) <= 2;
    let probe = Renderer::new(gl.clone())?;
    let driver = probe.driver;
    drop(probe);

    let baseline = if driver.es { 300 } else { 330 };
    let (p, snd) = compiled(&film(None)?)?.map_err(|e| anyhow!("baseline film refused: {e}"))?;
    if !near(p, baseline) || snd != baseline {
        bail!(
            "a film without `glsl` compiled as {p} (picture) / {snd} (sound), want {baseline} \
             on this GLSL {} driver",
            driver.max
        );
    }
    let mut notes = vec![format!("baseline {baseline}")];
    for v in [410, 430] {
        let got = compiled(&film(Some(v))?)?;
        let possible = !driver.es && driver.max >= v;
        match (possible, got) {
            (true, Ok((p, snd))) if near(p, v) && snd == v => notes.push(format!("{v} ok")),
            (true, Ok((p, snd))) => {
                bail!("glsl = \"{v}\" compiled as {p} (picture) / {snd} (sound)")
            }
            (true, Err(e)) => bail!(
                "glsl = \"{v}\" refused on a GLSL {} driver: {e}",
                driver.max
            ),
            (false, Ok(_)) => bail!("glsl = \"{v}\" accepted on a GLSL {} driver", driver.max),
            (false, Err(e)) if e.contains("glsl") => notes.push(format!("{v} refused")),
            (false, Err(e)) => bail!("glsl = \"{v}\" refused without saying why: {e}"),
        }
    }
    Ok(notes.join(", "))
}

fn transition_endpoints(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            ("a.glsl", &scene("fragColor = vec4(uv.x, uv.y, 0.25, 1.0);")),
            (
                "b.glsl",
                &scene("fragColor = vec4(0.9 - uv.y, 0.3, uv.x * 0.8, 1.0);"),
            ),
        ],
    )?;
    let kinds = [
        ("fx/crossfade.glsl", ""),
        ("fx/wipe.glsl", ""),
        ("fx/wipe.glsl", "params = { direction = [-0.4, 1.0] }, "),
        (
            "fx/wipe.glsl",
            "params = { direction = [0.0, -1.0], softness = 0.2 }, ",
        ),
    ];
    for (kind, params) in kinds {
        let m = movie(
            dir,
            &format!(
                "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=3\n[[shot]]\nid=\"b\"\nscene=\"b.glsl\"\nduration=3\n\
                 transition = {{ type = \"{kind}\", {params}duration = 1 }}\n"
            ),
        )?;
        let mut r = renderer(gl, &m)?;
        let a = shot(
            &mut r,
            &m,
            At::Shot {
                shot: 0,
                local: 2.0,
            },
            1.0,
        )?;
        let b = shot(
            &mut r,
            &m,
            At::Shot {
                shot: 1,
                local: 0.0,
            },
            1.0,
        )?;
        let tr = |p| At::Transition {
            from: 0,
            to: 1,
            from_local: 2.0,
            to_local: 0.0,
            progress: p,
        };
        let p0 = shot(&mut r, &m, tr(0.0), 1.0)?;
        let p1 = shot(&mut r, &m, tr(1.0), 1.0)?;
        // A pixel can sit exactly on a wipe's edge: allow a handful.
        expect_same(&format!("{kind} {params}at progress 0"), &p0, &a, 2, 0.002)?;
        expect_same(&format!("{kind} {params}at progress 1"), &p1, &b, 2, 0.002)?;
    }
    Ok(format!("{} variants", kinds.len()))
}

fn crossfade_and_freeze(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            (
                "a.glsl",
                &scene("fragColor = vec4(fract(iTime * 0.37), 0.5 * uv.x, 0.2, 1.0);"),
            ),
            (
                "b.glsl",
                &scene("fragColor = vec4(0.1, fract(iTime * 0.23), 0.7 * uv.y, 1.0);"),
            ),
        ],
    )?;
    for freeze in [false, true] {
        let m = movie(
            dir,
            &format!(
                "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=3\n[[shot]]\nid=\"b\"\nscene=\"b.glsl\"\nduration=3\n\
                 transition = {{ type = \"fx/crossfade.glsl\", duration = 1, ease = \"linear\", freeze = {freeze} }}\n"
            ),
        )?;
        let mut r = renderer(gl, &m)?;
        // B starts at 2.0; at 2.5 the crossfade is half way.
        let a_local = if freeze { 2.0 } else { 2.5 };
        let a = shot(
            &mut r,
            &m,
            At::Shot {
                shot: 0,
                local: a_local,
            },
            1.0,
        )?;
        let b = shot(
            &mut r,
            &m,
            At::Shot {
                shot: 1,
                local: 0.5,
            },
            1.0,
        )?;
        let got = shot(&mut r, &m, m.at(2.5), 1.0)?;
        let mut expected = a.clone();
        for (e, y) in expected.cells_mut().zip(b.cells()) {
            for c in 0..4 {
                e[c] = ((e[c] as f32 + y[c] as f32) * 0.5).round() as u8;
            }
        }
        expect_same(
            if freeze {
                "frozen crossfade"
            } else {
                "live crossfade"
            },
            &got,
            &expected,
            2,
            0.0,
        )?;
    }
    Ok(String::new())
}

fn render_scale_edges(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            ("a.glsl", &scene("fragColor = vec4(0.8, 0.2, 0.4, 1.0);")),
            ("b.glsl", &scene("fragColor = vec4(0.1, 0.6, 0.3, 1.0);")),
        ],
    )?;
    let m = movie(
        dir,
        "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=3\n[[shot]]\nid=\"b\"\nscene=\"b.glsl\"\nduration=3\n\
         transition = { type = \"fx/crossfade.glsl\", duration = 1, ease = \"linear\" }\n",
    )?;
    let mut r = renderer(gl, &m)?;
    let solid = |c: [f32; 3]| -> Frame {
        let q = |x: f32| (x * 255.0).round() as u8;
        Frame::init(W as usize, H as usize, [q(c[0]), q(c[1]), q(c[2]), 255])
    };
    for scale in [0.77, 0.5, 0.35] {
        let a = shot(
            &mut r,
            &m,
            At::Shot {
                shot: 0,
                local: 1.0,
            },
            scale,
        )?;
        expect_same(
            &format!("shot at scale {scale}"),
            &a,
            &solid([0.8, 0.2, 0.4]),
            2,
            0.0,
        )?;
        let mid = shot(&mut r, &m, m.at(2.5), scale)?;
        expect_same(
            &format!("transition at scale {scale}"),
            &mid,
            &solid([0.45, 0.4, 0.35]),
            2,
            0.0,
        )?;
    }
    Ok(String::new())
}

fn tiled_drawing(gl: &Gl, dir: &Path) -> Result<String> {
    // The strip count is pinned (otherwise the GPU timer puts it back to 1 on the
    // next frame and nothing is tested), the picture changes with time, and every
    // tiled frame is drawn over a different one: a strip the driver skips shows up as
    // stale pixels instead of hiding behind an identical previous frame. (The Apple
    // Software Renderer dropped parts of scissored strips at 320x180 with 40 and 64
    // strips: the demo's golden strip showed rows from the frame before.)
    write(
        dir,
        &[(
            "s.glsl",
            &scene("fragColor = vec4(uv, fract(uv.x * 7.0 + uv.y * 3.0 + iTime), 1.0);"),
        )],
    )?;
    let m = movie(
        dir,
        "[[shot]]
id=\"a\"
scene=\"s.glsl\"
duration=2
",
    )?;
    let mut r = renderer(gl, &m)?;
    let draw = |r: &mut Renderer, (w, h): (i32, i32), tiles: u32, local: f32, scale: f32| {
        r.set_tiles(Some(tiles));
        let t = m.shots[0].start + local;
        r.render_at(&m, w, h, &info(t, scale), At::Shot { shot: 0, local }, true)
            .ok_or_else(|| anyhow!("nothing rendered"))?;
        r.read_output().ok_or_else(|| anyhow!("no output"))
    };
    for size in [(W, H), (320, 180)] {
        for scale in [1.0, 0.6] {
            let want = draw(&mut r, size, 1, 0.5, scale)?;
            for tiles in [2, 7, 40, 64] {
                draw(&mut r, size, 1, 1.25, scale)?;
                let got = draw(&mut r, size, tiles, 0.5, scale)?;
                expect_same(
                    &format!("{tiles} strips at {}x{} scale {scale}", size.0, size.1),
                    &got,
                    &want,
                    0,
                    0.0,
                )?;
            }
        }
    }
    r.set_tiles(None);
    Ok(String::new())
}

fn dirty_pack_state(gl: &Gl) -> Result<glow::Buffer> {
    unsafe {
        let buf = gl.create_buffer().map_err(|e| anyhow!(e))?;
        gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(buf));
        gl.buffer_data_size(glow::PIXEL_PACK_BUFFER, 4096, glow::STREAM_READ);
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 8);
        gl.pixel_store_i32(glow::PACK_ROW_LENGTH, 1);
        gl.pixel_store_i32(glow::PACK_SKIP_PIXELS, 1);
        gl.pixel_store_i32(glow::PACK_SKIP_ROWS, 1);
        if !gl.version().is_embedded {
            gl.pixel_store_i32(glow::PACK_SWAP_BYTES, 1);
        }
        Ok(buf)
    }
}

/// The export's YUV pass (#116) against the CPU reference (`yuv::reference`, itself
/// pinned to BT.709 limited-range values by unit tests), at 10 and 8 bits, flipped and
/// plain outputs, through the pipelined loop the export uses: frames come out in
/// order, and the loop waits on the GPU only when its ring is full (on a fast driver,
/// never before the final drain).
fn yuv_pass(gl: &Gl, dir: &Path) -> Result<String> {
    use crate::yuv::{YuvReader, reference, samples};
    // Hard colours in the corners (red, blue, white, black), gradients elsewhere, a
    // one-pixel red column at x = 0 for chroma siting, and a different picture each
    // frame, so a stale or reordered frame can't pass.
    write(
        dir,
        &[(
            "a.glsl",
            &scene(
                "vec2 p = floor(fragCoord);\n  \
                 fragColor = vec4(fract(uv.x * 3.0 + float(iFrame) * 0.1), uv.y, fract(p.x * 0.37 + p.y * 0.11), 1.0);\n  \
                 if (p.x < 8.0 && p.y < 8.0) fragColor = vec4(1.0, 0.0, 0.0, 1.0);\n  \
                 if (p.x >= iResolution.x - 8.0 && p.y < 8.0) fragColor = vec4(0.0, 0.0, 1.0, 1.0);\n  \
                 if (p.x < 8.0 && p.y >= iResolution.y - 8.0) fragColor = vec4(1.0);\n  \
                 if (p.x >= iResolution.x - 8.0 && p.y >= iResolution.y - 8.0) fragColor = vec4(0.0, 0.0, 0.0, 1.0);\n  \
                 if (p.x == 0.0 && p.y > 20.0 && p.y < 40.0) fragColor = vec4(1.0, 0.0, 0.0, 1.0);",
            ),
        )],
    )?;
    let m = movie(dir, "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=1\n")?;
    let (w, h) = (W as usize, H as usize);
    let frames = 12u64;
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let mut notes = vec![];
    for (depth, flipped) in [(10u8, true), (8, false)] {
        let mut r = renderer(gl, &m)?;
        r.set_readback(flipped);
        let mut want = vec![];
        for n in 0..frames {
            r.render(&m, W, H, &FrameInfo::frame(&m, n), true)
                .ok_or_else(|| anyhow!("frame {n} didn't render"))?;
            want.push(reference(&r.read_output().unwrap(), depth));
        }
        let mut reader = YuvReader::new(&r, w, h, depth, 4)?;
        let mut next = 0;
        let mut off_by_one = 0;
        crate::export::render_yuv(
            &mut r,
            &m,
            (w, h),
            0..frames,
            &mut reader,
            &cancel,
            |rd, f| {
                if f.n != next {
                    bail!("{depth}-bit: got frame {} when expecting {next}", f.n);
                }
                let got = samples(&f.data, depth);
                let want = &want[f.n as usize];
                if got.len() != want.len() {
                    bail!("{depth}-bit: {} samples, want {}", got.len(), want.len());
                }
                for (i, (g, e)) in got.iter().zip(want).enumerate() {
                    let d = g.abs_diff(*e);
                    if d > 1 {
                        let plane = if i < w * h { "Y" } else { "CbCr" };
                        bail!(
                            "{depth}-bit, {}: frame {}, {plane} sample {i}: {g}, want {e}",
                            if flipped {
                                "flipped output"
                            } else {
                                "plain output"
                            },
                            f.n
                        );
                    }
                    off_by_one += d as usize;
                }
                rd.recycle(f.data);
                next += 1;
                Ok(())
            },
        )?;
        if next != frames {
            bail!("{depth}-bit: {next} of {frames} frames came out");
        }
        // The corners are exact: no rounding ties there.
        let got = {
            let mut reader = YuvReader::new(&r, w, h, depth, 1)?;
            let tex = r
                .render(&m, W, H, &FrameInfo::frame(&m, 0), true)
                .ok_or_else(|| anyhow!("frame 0 didn't render"))?;
            reader.submit(&r, tex, 0)?;
            samples(&reader.collect(true).unwrap().data, depth)
        };
        let y = |x: usize, yy: usize| got[yy * w + x];
        let corners = [
            // fragCoord.y = 0 is the bottom row: red and blue are at the bottom.
            ("red", y(2, h - 3), if depth == 10 { 250 } else { 63 }),
            ("blue", y(w - 3, h - 3), if depth == 10 { 127 } else { 32 }),
            ("white", y(2, 2), if depth == 10 { 940 } else { 235 }),
            ("black", y(w - 3, 2), if depth == 10 { 64 } else { 16 }),
        ];
        for (what, g, e) in corners {
            if g != e {
                bail!("{depth}-bit {what} Y = {g}, want {e}");
            }
        }
        // The last chroma row (bottom), Cr, column 1 (centred on luma column 2).
        let cr_red = got[w * h + (h / 2 - 1) * w + w / 2 + 1];
        if cr_red != if depth == 10 { 960 } else { 240 } {
            bail!("{depth}-bit red Cr = {cr_red}");
        }
        notes.push(format!(
            "{depth}-bit: {} waits, {off_by_one} samples off by one",
            reader.waits
        ));
        if reader.waits > 4 {
            bail!(
                "{depth}-bit: the loop waited for the GPU {} times in {frames} frames (ring of 4)",
                reader.waits
            );
        }
    }
    Ok(notes.join("; "))
}

fn readback_into(gl: &Gl, dir: &Path) -> Result<String> {
    // Top and bottom differ, so an upside-down readback can't pass.
    write(
        dir,
        &[
            (
                "a.glsl",
                &scene(
                    "fragColor = vec4(uv.x, uv.y * uv.y, fract(uv.x * 5.0 + uv.y * 11.0), 1.0);",
                ),
            ),
            (
                "b.glsl",
                &scene("fragColor = vec4(1.0 - uv.y, 0.3, uv.x * uv.y, 1.0);"),
            ),
        ],
    )?;
    let m = movie(
        dir,
        "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=2\n[[shot]]\nid=\"b\"\nscene=\"b.glsl\"\nduration=2\n\
         transition = { type = \"fx/wipe.glsl\", duration = 1 }\n",
    )?;
    let defaults = [4, 0, 0, 0, 0];
    let mut plain = renderer(gl, &m)?;
    let mut flipped = renderer(gl, &m)?;
    flipped.set_readback(true);
    if plain.pack_state() != defaults {
        bail!(
            "pack state isn't the default before any readback: {:?}",
            plain.pack_state()
        );
    }
    let (dw, dh) = (W as usize + 37, H as usize + 23);
    let bg = Frame::init(dw, dh, [7, 8, 9, 10]);
    let mut notes = 0;
    // A plain shot (copy pass) and a mid-wipe transition (transition pass).
    for (what, at) in [("shot", m.at(0.5)), ("transition", m.at(1.5))] {
        let want_frame = shot(&mut plain, &m, at, 1.0)?;
        for (x, y) in [(0, 0), (13, 11), (37, 23)] {
            let mut want = bg.clone();
            want.view_mut((x, y), (x + W as usize, y + H as usize))
                .copy_from_toodee(&want_frame);
            for (name, r) in [("plain", &mut plain), ("flipped composite", &mut flipped)] {
                r.render_at(&m, W, H, &info(at_time(&m, at), 1.0), at, true)
                    .ok_or_else(|| anyhow!("nothing rendered"))?;
                let mut dst = bg.clone();
                let junk = dirty_pack_state(gl)?;
                r.read_output_into(&mut dst, (x, y));
                let state = r.pack_state();
                unsafe { gl.delete_buffer(junk) };
                let window = (x, y, x + W as usize, y + H as usize);
                if dst.view((window.0, window.1), (window.2, window.3)) != want_frame {
                    bail!("{what}, {name}, at ({x}, {y}): the window differs from read_output");
                }
                if dst != want {
                    bail!("{what}, {name}, at ({x}, {y}): pixels outside the window changed");
                }
                if state != defaults {
                    bail!("{what}, {name}: pack state left at {state:?}");
                }
                // read_output (used by `still`) agrees as well.
                expect_same(
                    &format!("{what}, {name}: read_output"),
                    &r.read_output().unwrap(),
                    &want_frame,
                    0,
                    0.0,
                )?;
                notes += 1;
            }
        }
    }
    // A window that doesn't fit must panic before GL writes anything. The first
    // probe overflows the right edge but stays inside the buffer's memory, so the
    // bounds check is the only thing that can catch it (and dropping it is safe to
    // try); toodee isn't involved, since the flipped renderer needs no row flip.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {})); // the panics are expected: keep them quiet
    let mut accepted = vec![];
    for (size, at) in [((W + 1, H + 5), (2, 0)), ((W, H + 1), (0, 2))] {
        let mut small = Frame::init(size.0 as usize, size.1 as usize, [0; 4]);
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            flipped.read_output_into(&mut small, at)
        }));
        if res.is_ok() {
            accepted.push(format!("{W}x{H} at {at:?} in {}x{}", size.0, size.1));
        }
    }
    std::panic::set_hook(hook);
    if !accepted.is_empty() {
        bail!(
            "read_output_into accepted windows that don't fit: {}",
            accepted.join(", ")
        );
    }
    Ok(format!("{notes} readbacks"))
}

fn at_time(m: &Movie, at: At) -> f32 {
    match at {
        At::Shot { shot, local } => m.shots[shot].start + local,
        At::Transition { to, to_local, .. } => m.shots[to].start + to_local,
    }
}

fn error_check_rate(gl: &Gl, dir: &Path) -> Result<String> {
    // glGetError makes the CPU wait for threaded drivers (NVIDIA): the player must
    // not call it every frame. 120 frames (~2 s of 60 Hz playback) → at most a few.
    write(
        dir,
        &[("s.glsl", &scene("fragColor = vec4(uv, 0.3, 1.0);"))],
    )?;
    let m = movie(dir, "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=4\n")?;
    let mut r = renderer(gl, &m)?;
    // Playing: never, unless there's a sign of trouble (even a once-a-second check
    // shows as a regular hitch in the frame graph).
    for i in 0..120 {
        r.render(&m, W, H, &info(i as f32 / 60.0, 1.0), true);
        if r.context_lost(false, false) {
            bail!("reported a lost context on a healthy GPU");
        }
    }
    if r.error_checks != 0 {
        bail!(
            "{} driver error checks in 120 frames of playback; expected none",
            r.error_checks
        );
    }
    // A sign of trouble (a long frame gap) does ask, so a reset is still noticed.
    if r.context_lost(false, true) || r.error_checks != 1 {
        bail!("a suspicious frame didn't ask the driver (or got a lost context)");
    }
    // Paused: about once a second (so a reset while paused is still noticed).
    let start = Instant::now();
    let before = r.error_checks;
    let mut frames = 0;
    while start.elapsed().as_secs_f32() < 1.3 {
        r.render(&m, W, H, &info(frames as f32 / 60.0, 1.0), true);
        if r.context_lost(true, false) {
            bail!("reported a lost context on a healthy GPU");
        }
        frames += 1;
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let secs = start.elapsed().as_secs_f32();
    let paused = r.error_checks - before;
    let allowed = 1 + secs.ceil() as usize;
    if paused == 0 || paused > allowed {
        bail!(
            "{paused} driver error checks in {frames} paused frames ({secs:.1} s); \
             expected 1 to {allowed}"
        );
    }
    Ok(format!(
        "none while playing, {paused} in {secs:.1} s paused"
    ))
}

fn error_mapping(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            (
                "lib/bad.glsl",
                "// helper\n// line two\nfloat broken() { return undefinedThing; }\n",
            ),
            (
                "s.glsl",
                &format!(
                    "// scene\n#include \"lib/bad.glsl\"\n{}",
                    scene("fragColor = vec4(broken());")
                ),
            ),
            (
                "t.glsl",
                "// scene\nvoid mainImage(out vec4 fragColor, in vec2 fragCoord) {\n  vec2 uv = fragCoord / iResolution.xy;\n  // line four\n  fragColor = vec4(alsoUndefined);\n}\n",
            ),
        ],
    )?;
    let mut notes = vec![];
    for (scene_file, expect) in [("s.glsl", "lib/bad.glsl:3:"), ("t.glsl", "t.glsl:5:")] {
        let m = movie(
            dir,
            &format!("[[shot]]\nid=\"a\"\nscene=\"{scene_file}\"\nduration=1\n"),
        )?;
        let mut r = Renderer::new(gl.clone())?;
        r.set_cache_dir(None);
        let errors = match r.load_all(&m) {
            Ok(_) => bail!("{scene_file} compiled, but it has an error"),
            Err(e) => e,
        };
        let text: String = errors.iter().map(|e| e.to_string()).collect();
        if !text.contains(expect) {
            bail!("expected an error at {expect}, got:\n{text}");
        }
        notes.push(expect.trim_end_matches(':').to_string());
    }
    Ok(notes.join(", "))
}

/// A film-local library (`path = …`) under its local name `kit`: a working include
/// renders (and only compiles if each file is included once), `type = "kit/split"` runs
/// the library's `split.glsl` with the film's params, and an error inside the library is
/// reported as `kit/noise.glsl:3`, not by its folder on disk.
fn library_errors(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            (
                "vendor/kit-core/library.toml",
                "name = \"shotglass-kit/core\"\nversion = \"0.4.0\"\nlicences = [\"MIT\"]\n",
            ),
            (
                "vendor/kit-core/hash.glsl",
                "float kit_level() { return 0.75; }\n",
            ),
            (
                "vendor/kit-core/fx.glsl",
                "#include \"hash.glsl\"\nvec3 kit_fx() { return vec3(kit_level(), 0.0, 0.25); }\n",
            ),
            (
                "vendor/kit-core/noise.glsl",
                "// kit noise\n// line two\nfloat kit_noise() { return notDeclaredAnywhere; }\n",
            ),
            (
                "vendor/kit-core/split.glsl",
                "#ifndef edge\n#define edge 0.5\n#endif\n\
                 vec4 transition(vec2 uv) { return uv.x < edge ? getFromColor(uv) : getToColor(uv); }\n",
            ),
            (
                "green.glsl",
                &scene("fragColor = vec4(0.0, 1.0, 0.0, 1.0);"),
            ),
            (
                "good.glsl",
                &format!(
                    "#include <kit/hash.glsl>\n#include <kit/fx.glsl>\n{}",
                    scene("fragColor = vec4(kit_fx(), 1.0);")
                ),
            ),
            (
                "bad.glsl",
                &format!(
                    "// scene\n#include <kit/noise.glsl>\n{}",
                    scene("fragColor = vec4(kit_noise());")
                ),
            ),
        ],
    )?;
    let toml = |scene_file: &str| {
        format!(
            "[libraries]\nkit = {{ path = \"vendor/kit-core\", version = \"0.4\" }}\n\
             [[shot]]\nid=\"a\"\nscene=\"{scene_file}\"\nduration=1\n"
        )
    };
    let m = movie(dir, &toml("good.glsl"))?;
    let mut r = renderer(gl, &m)?;
    let f = shot(
        &mut r,
        &m,
        At::Shot {
            shot: 0,
            local: 0.5,
        },
        1.0,
    )?;
    let p = px(&f, W as usize / 2, H as usize / 2);
    if p[0].abs_diff(191) > 2 || p[1] > 2 || p[2].abs_diff(64) > 2 {
        bail!("library include rendered {p:?}, expected about [191, 0, 64]");
    }

    // A library transition by name; `edge` comes from the film, overriding the default.
    let m = movie(
        dir,
        &format!(
            "{}[[shot]]\nid=\"b\"\nscene=\"green.glsl\"\nduration=1\n\
             transition = {{ type = \"kit/split\", duration = 0.5, params = {{ edge = 0.25 }} }}\n",
            toml("good.glsl")
        ),
    )?;
    let mut r = renderer(gl, &m)?;
    let f = shot(
        &mut r,
        &m,
        At::Transition {
            from: 0,
            to: 1,
            from_local: 0.75,
            to_local: 0.25,
            progress: 0.5,
        },
        1.0,
    )?;
    let (left, mid) = (
        px(&f, W as usize / 8, H as usize / 2),
        px(&f, W as usize / 2, H as usize / 2),
    );
    if left[0].abs_diff(191) > 2 || mid[1] < 253 || mid[0] > 2 {
        bail!(
            "kit/split at edge 0.25 rendered {left:?} at x = 1/8 and {mid:?} at x = 1/2, \
             expected the first shot then green"
        );
    }

    let m = movie(dir, &toml("bad.glsl"))?;
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None);
    let errors = match r.load_all(&m) {
        Ok(_) => bail!("bad.glsl compiled, but its library has an error"),
        Err(e) => e,
    };
    let text: String = errors.iter().map(|e| e.to_string()).collect();
    let expect = "kit/noise.glsl:3:";
    if !text.contains(expect) {
        bail!("expected an error at {expect}, got:\n{text}");
    }
    Ok("kit/split, kit/noise.glsl:3".into())
}

fn keep_last_working(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[("s.glsl", &scene("fragColor = vec4(1.0, 0.0, 0.0, 1.0);"))],
    )?;
    let toml = "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\n";
    let m = movie(dir, toml)?;
    let mut r = renderer(gl, &m)?;
    let red = shot(
        &mut r,
        &m,
        At::Shot {
            shot: 0,
            local: 1.0,
        },
        1.0,
    )?;
    if px(&red, 0, 0)[0] < 250 || px(&red, 0, 0)[1] > 5 {
        bail!("first version didn't render red");
    }
    write(
        dir,
        &[("s.glsl", &scene("fragColor = vec4(oops, 1.0, 0.0, 1.0);"))],
    )?;
    let m2 = movie(dir, toml)?;
    r.set_movie(&m2)
        .map_err(|_| anyhow!("set_movie refused a compile error (should defer it)"))?;
    let still = shot(
        &mut r,
        &m2,
        At::Shot {
            shot: 0,
            local: 1.0,
        },
        1.0,
    )?;
    expect_same("after a broken edit", &still, &red, 1, 0.0)?;
    if r.errors().is_empty() {
        bail!("the broken edit didn't report an error");
    }
    write(
        dir,
        &[("s.glsl", &scene("fragColor = vec4(0.0, 1.0, 0.0, 1.0);"))],
    )?;
    let m3 = movie(dir, toml)?;
    r.set_movie(&m3).map_err(|e| anyhow!("{e:?}"))?;
    let green = shot(
        &mut r,
        &m3,
        At::Shot {
            shot: 0,
            local: 1.0,
        },
        1.0,
    )?;
    if px(&green, 0, 0)[1] < 250 || px(&green, 0, 0)[0] > 5 {
        bail!("the fixed version didn't replace the old one");
    }
    if !r.errors().is_empty() {
        bail!("errors remained after the fix");
    }
    Ok(String::new())
}

fn program_cache(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[(
            "s.glsl",
            &scene("fragColor = vec4(uv, 0.5 + 0.5 * sin(uv.x * 20.0), 1.0);"),
        )],
    )?;
    let m = movie(dir, "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\n")?;
    let cache: PathBuf = dir.join("cache");
    let make = |cache_on: bool| -> Result<Renderer> {
        let mut r = Renderer::new(gl.clone())?;
        if cache_on {
            r.set_cache_dir(Some(cache.clone()));
            if r.shader_cache_dir().is_none() {
                return Ok(r);
            }
        }
        r.load_all(&m)
            .map_err(|e| anyhow!("{}", e.iter().map(|e| e.to_string()).collect::<String>()))?;
        Ok(r)
    };
    let mut r1 = make(true)?;
    if r1.shader_cache_dir().is_none() {
        return Ok("skipped: this driver offers no program binary formats".into());
    }
    let img1 = shot(
        &mut r1,
        &m,
        At::Shot {
            shot: 0,
            local: 0.5,
        },
        1.0,
    )?;
    let files: Vec<_> = std::fs::read_dir(&cache)?.flatten().collect();
    if files.is_empty() {
        bail!("nothing was written to the program cache");
    }
    drop(r1);
    let mut r2 = make(true)?;
    let img2 = shot(
        &mut r2,
        &m,
        At::Shot {
            shot: 0,
            local: 0.5,
        },
        1.0,
    )?;
    if r2.binary_hits == 0 {
        bail!("the second load didn't use the cache");
    }
    expect_same("cached program", &img2, &img1, 0, 0.0)?;
    drop(r2);
    for f in &files {
        std::fs::write(
            f.path(),
            b"\x01\x02\x03\x04 definitely not a program binary",
        )?;
    }
    let mut r3 = make(true)?;
    let img3 = shot(
        &mut r3,
        &m,
        At::Shot {
            shot: 0,
            local: 0.5,
        },
        1.0,
    )?;
    expect_same("after cache corruption", &img3, &img1, 0, 0.0)?;
    Ok(format!("{} cached program(s)", files.len()))
}

fn eviction(gl: &Gl, dir: &Path) -> Result<String> {
    // 71 shots joined by 70 distinct transitions (one program each) — more than the
    // renderer keeps, so early ones get evicted and must come back correctly.
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.0, 0.0, 0.0, 1.0);")),
            (
                "k.glsl",
                "vec4 transition(vec2 uv) { return vec4(k / 100.0, 0.5, 0.25, 1.0); }\n",
            ),
        ],
    )?;
    let n = 71;
    let mut toml = String::new();
    for i in 0..n {
        toml += &format!("[[shot]]\nid=\"s{i}\"\nscene=\"s.glsl\"\nduration=2\n");
        if i > 0 {
            toml += &format!(
                "transition = {{ type = \"k.glsl\", duration = 1, params = {{ k = {i} }} }}\n"
            );
        }
    }
    let m = movie(dir, &toml)?;
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None);
    r.set_movie(&m).map_err(|e| anyhow!("{e:?}"))?;
    let check = |r: &mut Renderer, i: usize| -> Result<()> {
        let at = m.at(m.shots[i].start + 0.5);
        let frame = shot(r, &m, at, 1.0)?;
        let want = (i as f32 / 100.0 * 255.0).round() as i32;
        let red = px(&frame, 0, 0)[0];
        if (red as i32 - want).abs() > 1 {
            bail!("transition into shot {i}: red {red} (wanted {want})");
        }
        Ok(())
    };
    let mut peak = 0;
    for i in 1..n {
        check(&mut r, i)?;
        for _ in 0..31 {
            r.pump(&m, m.shots[i].start, false);
        }
        peak = peak.max(r.ready_count());
    }
    for i in 1..6 {
        check(&mut r, i)?; // evicted by now; recompiled on demand
    }
    if peak > 64 + 8 {
        bail!("{peak} programs held at once; eviction isn't working");
    }
    Ok(format!("peak {peak} programs held for {} needed", n))
}

fn lazy_compile(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[("s.glsl", &scene("fragColor = vec4(uv.y, 0.3, uv.x, 1.0);"))],
    )?;
    let m = movie(dir, "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\n")?;
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None);
    r.set_movie(&m).map_err(|e| anyhow!("{e:?}"))?;
    let mut frames = 0;
    let got = loop {
        if let Some(_t) = r.render(&m, W, H, &info(0.5, 1.0), false) {
            break r.read_output().unwrap();
        }
        frames += 1;
        if frames > 2000 {
            bail!("the shot never became ready without blocking");
        }
        r.pump(&m, 0.5, false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    let want = shot(
        &mut r,
        &m,
        At::Shot {
            shot: 0,
            local: 0.5,
        },
        1.0,
    )?;
    expect_same("non-blocking render", &got, &want, 0, 0.0)?;
    Ok(format!("ready after {frames} frame(s)"))
}

// ------------------------------------------------------------------ sound checks

const SOUND_MOVIE: &str = r#"
[sound]
music = "music.glsl"
mix = "mix.glsl"
mix_lookback = 0.5

[[shot]]
id = "a"
scene = "s.glsl"
duration = 1.03
uniforms.uPitch = 1.0
sound = "tone.glsl"
sound_tail = 0.2

[[shot]]
id = "b"
scene = "s.glsl"
duration = 0.937
sound = "tone.glsl"
sound_lead = 0.1
uniforms.uPitch = 1.5
transition = { type = "fx/crossfade.glsl", duration = 0.4 }

[[shot]]
id = "c"
scene = "s.glsl"
duration = 0.8
sound = "tone.glsl"
uniforms.uPitch = 2.0
"#;

fn sound_fixture(dir: &Path) -> Result<Movie> {
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.0, 0.0, 0.0, 1.0);")),
            (
                "music.glsl",
                "vec2 mainSound(int samp, float time) {\n  float s = 0.2 * sin(6.2831853 * 220.0 * time + 2.0 * sin(6.2831853 * 3.0 * time));\n  return vec2(s, s * 0.9);\n}\n",
            ),
            (
                "tone.glsl",
                "vec2 mainSound(int samp, float time) {\n  float s = 0.2 * sin(6.2831853 * 337.0 * uPitch * time + 0.7);\n  return vec2(s * 0.8, s);\n}\n",
            ),
            (
                "mix.glsl",
                "vec2 mainMix(int samp, float time) {\n  int ms = int(iSampleRate * 0.001);\n  vec2 echo = music(samp - 300 * ms) * 0.3;\n  float duck = 1.0 - 0.3 * smoothstep(0.05, 0.3, sfxLevel(samp));\n  return (music(samp) + echo) * duck + sfx(samp);\n}\n",
            ),
        ],
    )?;
    movie(dir, SOUND_MOVIE)
}

fn render_sound(gl: &Gl, m: &Movie, chunk: usize) -> Result<Vec<f32>> {
    strict(m)?;
    let mut r = Renderer::new(gl.clone())?;
    let mut st = Soundtrack::with_chunk(chunk);
    st.set_movie(&mut r, m, 48000)
        .map_err(|e| anyhow!("{}", e.iter().map(|e| e.to_string()).collect::<String>()))?;
    let mut out = vec![];
    st.render_all(&mut r, |d| out.extend_from_slice(d))
        .map_err(|e| anyhow!(e))?;
    Ok(out)
}

fn sound_chunk_equivalence(gl: &Gl, dir: &Path) -> Result<String> {
    let m = sound_fixture(dir)?;
    let whole = render_sound(gl, &m, crate::gfx::AUDIO_BLOCK)?; // the 2 s film fits one chunk
    let mut notes = vec![];
    let mut skipped = vec![];
    for chunk in [32768, 7919] {
        let parts = render_sound(gl, &m, chunk)?;
        if parts.len() != whole.len() {
            bail!(
                "chunk {chunk}: {} samples, expected {}",
                parts.len(),
                whole.len()
            );
        }
        let (i, d) = parts
            .iter()
            .zip(&whole)
            .map(|(a, b)| (a - b).abs())
            .enumerate()
            .fold(
                (0, 0f32),
                |acc, (i, d)| if d > acc.1 { (i, d) } else { acc },
            );
        notes.push(format!("{chunk}"));
        if d > 1e-5 {
            let msg = format!(
                "chunk {chunk}: differs by {d} at {:.4}s",
                (i / 2) as f32 / 48000.0
            );
            sound_mismatch(gl, &mut skipped, msg)?;
        }
    }
    if whole.iter().all(|x| x.abs() < 1e-4) {
        bail!("the fixture rendered silence");
    }
    skipped.insert(0, format!("chunks of {} samples", notes.join(" and ")));
    Ok(skipped.join("; "))
}

fn sound_banding(gl: &Gl, dir: &Path) -> Result<String> {
    let m = sound_fixture(dir)?;
    let render = |banded: bool| -> Result<Vec<f32>> {
        let mut r = Renderer::new(gl.clone())?;
        let mut st = Soundtrack::with_chunk(32768);
        st.set_movie(&mut r, &m, 48000)
            .map_err(|e| anyhow!("{e:?}"))?;
        let mut out = vec![];
        r.set_audio_banded(banded);
        let n = st.total_chunks();
        for c in 0..n {
            r.set_audio_banded(banded); // the renderer adapts after each block; pin it
            out.extend_from_slice(&st.chunk(&mut r, c).map_err(|e| anyhow!(e))?);
        }
        Ok(out)
    };
    let plain = render(false)?;
    let banded = render(true)?;
    let d = plain
        .iter()
        .zip(&banded)
        .map(|(a, b)| (a - b).abs())
        .fold(0f32, f32::max);
    if plain.len() != banded.len() {
        bail!(
            "banded synthesis gave {} samples, not {}",
            banded.len(),
            plain.len()
        );
    }
    let mut notes = vec![];
    if d > 1e-6 {
        sound_mismatch(gl, &mut notes, format!("banded synthesis differs by {d}"))?;
    }
    Ok(notes.join("; "))
}

/// Playback-style synthesis: `Soundtrack::stream` once per "frame", as the player
/// does, must never wait on a readback, and must produce the blocking render exactly.
fn sound_streaming(gl: &Gl, dir: &Path) -> Result<String> {
    let m = sound_fixture(dir)?;
    let mut notes = vec![];
    for (chunk, persistent) in [(32768, true), (7919, true), (32768, false), (7919, false)] {
        let blocking = render_sound(gl, &m, chunk)?;
        let mut r = Renderer::new(gl.clone())?;
        if r.set_persistent_buffers(persistent) != persistent {
            notes.push("no persistent buffers on this driver".into());
            continue;
        }
        let mut st = Soundtrack::with_chunk(chunk);
        st.set_movie(&mut r, &m, 48000)
            .map_err(|e| anyhow!("{e:?}"))?;
        let n = st.total_chunks();
        let mut got: Vec<Option<Arc<Vec<f32>>>> = vec![None; n];
        let mut frames = 0;
        let mut max_in_flight = 0;
        let mut cancelled = false;
        let start = Instant::now();
        while got.iter().any(|c| c.is_none()) {
            if start.elapsed().as_secs() > 60 {
                bail!("chunk {chunk}: streaming didn't finish in 60 s ({frames} frames)");
            }
            let wanted: Vec<usize> = (0..n).filter(|&c| got[c].is_none()).collect();
            let deadline = Instant::now() + std::time::Duration::from_millis(4);
            for (c, d) in st
                .stream(&mut r, &wanted, 2, deadline)
                .map_err(|e| anyhow!(e))?
            {
                got[c] = Some(d);
            }
            max_in_flight = max_in_flight.max(r.audio_in_flight());
            if !cancelled && r.audio_in_flight() > 0 {
                st.cancel_all(&mut r);
                if st.in_flight() != 0 || r.audio_in_flight() != 0 {
                    bail!("chunk {chunk}: cancellation left blocks in flight");
                }
                cancelled = true;
            }
            frames += 1;
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        if r.blocking_audio_reads != 0 {
            bail!(
                "chunk {chunk}: streaming waited for the GPU {} time(s)",
                r.blocking_audio_reads
            );
        }
        if st.in_flight() != 0 || r.audio_in_flight() != 0 {
            bail!("chunk {chunk}: blocks left in flight after the last chunk");
        }
        if max_in_flight == 0 {
            bail!("chunk {chunk}: nothing was ever queued asynchronously");
        }
        let streamed: Vec<f32> = got.into_iter().flat_map(|c| c.unwrap().to_vec()).collect();
        if streamed.len() != blocking.len() {
            bail!(
                "chunk {chunk}: {} samples streamed, {} blocking",
                streamed.len(),
                blocking.len()
            );
        }
        if let Some(i) = streamed
            .iter()
            .zip(&blocking)
            .position(|(a, b)| a.to_bits() != b.to_bits())
        {
            let msg = format!(
                "chunk {chunk}: streamed sound differs at {:.4}s ({} vs {})",
                (i / 2) as f32 / 48000.0,
                streamed[i],
                blocking[i]
            );
            sound_mismatch(gl, &mut notes, msg)?;
        }
        notes.push(format!(
            "{chunk}{}: {frames} frames, ≤{max_in_flight} queued",
            if persistent { " persistent" } else { "" }
        ));
    }
    // The blocking path does count its waits, so the zero above means something.
    let mut r = Renderer::new(gl.clone())?;
    let mut st = Soundtrack::new();
    st.set_movie(&mut r, &m, 48000)
        .map_err(|e| anyhow!("{e:?}"))?;
    st.chunk(&mut r, 0).map_err(|e| anyhow!(e))?;
    if r.blocking_audio_reads == 0 {
        bail!("a blocking chunk wasn't counted as a blocking readback");
    }
    Ok(notes.join("; "))
}

fn sound_no_clicks(gl: &Gl, dir: &Path) -> Result<String> {
    let m = sound_fixture(dir)?;
    let s = render_sound(gl, &m, 7919)?;
    // Every source is a ≤0.25-amplitude sine below 700 Hz (phases chosen so no
    // window edge falls on a zero crossing): the steepest legal step is
    // about 2π·700·0.45/48000 ≈ 0.04 per sample. A click is a much bigger jump.
    let mut worst = (0f32, 0usize);
    for ch in 0..2 {
        for i in 1..s.len() / 2 {
            let d = (s[i * 2 + ch] - s[(i - 1) * 2 + ch]).abs();
            if d > worst.0 {
                worst = (d, i);
            }
        }
    }
    if worst.0 > 0.06 {
        bail!(
            "a jump of {:.3} at {:.4}s",
            worst.0,
            worst.1 as f32 / 48000.0
        );
    }
    Ok(format!("largest step {:.3}", worst.0))
}

fn sound_errors(gl: &Gl, dir: &Path) -> Result<String> {
    let m = sound_fixture(dir)?;
    let mut r = Renderer::new(gl.clone())?;
    let mut st = Soundtrack::new();
    st.set_movie(&mut r, &m, 48000)
        .map_err(|e| anyhow!("{e:?}"))?;
    let version = st.version();
    let last = st.total_chunks() - 1;
    let samples = st.chunk(&mut r, last).map_err(|e| anyhow!(e))?;
    let music = dir.join("music.glsl");
    let working = std::fs::read(&music)?;
    std::fs::write(
        &music,
        "// music\n\nvec2 mainSound(int samp, float time) {\n  return vec2(nope);\n}\n",
    )?;
    match st.set_movie(&mut r, &m, 48000) {
        Ok(()) => bail!("a broken music shader was accepted"),
        Err(e) => {
            let text: String = e.iter().map(|e| e.to_string()).collect();
            if !text.contains("music.glsl:4:") {
                bail!("expected an error at music.glsl:4, got:\n{text}");
            }
        }
    }
    if st.version() != version
        || !std::sync::Arc::ptr_eq(&samples, &st.chunk(&mut r, last).map_err(|e| anyhow!(e))?)
    {
        bail!("a broken shader discarded timing-compatible fallback audio");
    }
    let changed = movie(
        dir,
        &SOUND_MOVIE.replace("duration = 1.03", "duration = 2.03"),
    )?;
    if changed.sound_samples(48000) <= m.sound_samples(48000) {
        bail!("the reload fixture did not extend the soundtrack");
    }
    if st.set_movie(&mut r, &changed, 48000).is_ok()
        || st.version().is_some()
        || st.total_chunks() != 0
        || st.in_flight() != 0
    {
        bail!("a broken reload with a longer film kept stale soundtrack timing");
    }
    std::fs::write(music, working)?;
    st.set_movie(&mut r, &changed, 48000)
        .map_err(|e| anyhow!("{e:?}"))?;
    if st.version().is_none() || st.total_samples() != changed.sound_samples(48000) {
        bail!("fixing the shader did not restore audio at the new film length");
    }
    Ok(String::new())
}

// ------------------------------------------------------------------ export

const EXPORT_MOVIE: &str = r#"
fps = "30000/1001"
[sound]
music = "music.glsl"

[[shot]]
id = "a"
scene = "a.glsl"
duration = 0.8

[[shot]]
id = "b"
scene = "b.glsl"
duration = 0.9
transition = { type = "fx/crossfade.glsl", duration = 0.4 }
"#;

/// Decode a video's frames as RGBA and its audio as 16-bit PCM bytes, with ffmpeg.
fn decode(path: &Path, what: &[&str]) -> Result<Vec<u8>> {
    let out = std::process::Command::new(crate::export::ffmpeg_bin())
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(what)
        .arg("-")
        .stdin(std::process::Stdio::null())
        .output()?;
    if !out.status.success() {
        bail!("decoding failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(out.stdout)
}

fn export_lossless(gl: &Gl, dir: &Path) -> Result<String> {
    use crate::export::{self, Format, Options};
    use std::sync::atomic::{AtomicBool, Ordering};
    // Every frame differs (iTime and iFrame), and top differs from bottom.
    write(
        dir,
        &[
            (
                "a.glsl",
                &scene(
                    "fragColor = vec4(fract(iTime * 1.7 + uv.x), uv.y, fract(float(iFrame) / 7.0), 1.0);",
                ),
            ),
            (
                "b.glsl",
                &scene(
                    "fragColor = vec4(uv.y * uv.y, fract(iTime * 0.9), fract(float(iFrame) * 0.37 + uv.x), 1.0);",
                ),
            ),
            (
                "music.glsl",
                "vec2 mainSound(int samp, float time) {\n  float s = 0.3 * sin(6.2831853 * 330.0 * time);\n  return vec2(s, -0.5 * s);\n}\n",
            ),
        ],
    )?;
    let m = movie(dir, EXPORT_MOVIE)?;
    let total = m.frame_count();
    let (w, h) = (W as usize, H as usize);

    // The references: what `still` and `wav` produce.
    let mut reference = renderer(gl, &m)?;
    let wav = dir.join("ref.wav");
    let samples = export::write_wav(&mut reference, &m, 48000, &wav, 1)?;
    let pcm = std::fs::read(&wav)?[44..].to_vec();
    let mut still = |n: u64| -> Result<Frame> {
        reference
            .render(&m, W, H, &FrameInfo::at(&m, m.frame_time(n)), true)
            .ok_or_else(|| anyhow!("nothing rendered"))?;
        Ok(reference.read_output().unwrap())
    };
    let probes = [0, 1, total / 2, total - 1];
    let no_cancel = AtomicBool::new(false);
    let run = |format: Format, out: PathBuf, cancel: &AtomicBool, stop_at: u64| {
        let mut r = renderer(gl, &m)?;
        r.set_readback(true);
        let opts = Options {
            out,
            w,
            h,
            format,
            rate: 48000,
            loops: 1,
            av1: export::Av1Options::default(),
        };
        export::export(&mut r, &m, &opts, cancel, |done, _| {
            if done == stop_at {
                cancel.store(true, Ordering::Relaxed);
            }
        })
    };

    // PNG sequence + WAV: needs no ffmpeg, so it always runs.
    let png_dir = dir.join("png");
    let s = run(Format::Png, png_dir.clone(), &no_cancel, u64::MAX)?;
    let pngs = std::fs::read_dir(&png_dir)?
        .filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.path().extension().is_some_and(|x| x == "png"))
        })
        .count() as u64;
    if s.frames != total || pngs != total {
        bail!(
            "png: {pngs} files ({} reported), expected {total}",
            s.frames
        );
    }
    let mut notes = vec![];
    if s.audio_frames != samples {
        bail!(
            "png: sound.wav has {} samples, not {samples}",
            s.audio_frames
        );
    }
    if std::fs::read(png_dir.join("sound.wav"))?[44..] != pcm[..] {
        sound_mismatch(gl, &mut notes, "png: sound.wav differs from `wav`".into())?;
    }
    for n in probes {
        let img = image::open(png_dir.join(format!("frame_{n:06}.png")))?.into_rgba8();
        let got = Frame::from_vec(w, h, img.pixels().map(|p| p.0).collect());
        expect_same(&format!("png frame {n}"), &got, &still(n)?, 0, 0.0)?;
    }

    #[cfg(feature = "webm")]
    notes.push(export_webm(
        &m,
        &mut still,
        &pcm,
        samples,
        dir,
        &|f, out, c, stop| run(f, out, c, stop),
    )?);

    if let Err(e) = export::ffmpeg_encoders() {
        let first = e.to_string().lines().next().unwrap_or_default().to_string();
        notes.insert(0, format!("{total} frames as PNG; video skipped: {first}"));
        return Ok(notes.join("; "));
    }
    // FFV1 + FLAC: lossless, so frames and samples must come back exactly.
    let mkv = dir.join("film.mkv");
    let previous = b"previous export";
    std::fs::write(&mkv, previous)?;
    let old_wav_path = dir.join("film.mkv.shotglass-sound.wav");
    std::fs::write(&old_wav_path, b"unrelated WAV")?;
    let snapshot = || -> Result<Vec<_>> {
        let mut names = std::fs::read_dir(dir)?
            .map(|entry| entry.map(|e| e.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    };
    let before = snapshot()?;
    let bad = Format::Ffmpeg(vec!["-c:v".into(), "shotglass_no_such_encoder".into()]);
    match run(bad, mkv.clone(), &no_cancel, u64::MAX) {
        Err(e) if e.to_string().starts_with("ffmpeg failed") => {}
        Err(e) => bail!("failed video export reported: {e:#}"),
        Ok(_) => bail!("export accepted an invalid encoder"),
    }
    if std::fs::read(&mkv)? != previous || snapshot()? != before {
        bail!("failed export changed the existing output or left staged files");
    }
    let s = run(Format::Ffv1, mkv.clone(), &no_cancel, u64::MAX)?;
    if snapshot()? != before || std::fs::read(&old_wav_path)? != b"unrelated WAV" {
        bail!("video export changed an unrelated file or left staged files");
    }
    let video = decode(
        &mkv,
        &["-map", "0:v:0", "-f", "rawvideo", "-pix_fmt", "rgba"],
    )?;
    let frame_bytes = w * h * 4;
    if s.frames != total || video.len() != total as usize * frame_bytes {
        bail!(
            "mkv: {} frames decoded ({} reported), expected {total}",
            video.len() as f32 / frame_bytes as f32,
            s.frames
        );
    }
    for n in probes {
        let bytes = &video[n as usize * frame_bytes..][..frame_bytes];
        let got = Frame::from_vec(w, h, bytes.as_chunks::<4>().0.to_vec());
        expect_same(&format!("mkv frame {n}"), &got, &still(n)?, 0, 0.0)?;
    }
    let audio = decode(&mkv, &["-map", "0:a:0", "-f", "s16le"])?;
    if audio.len() == samples * 4 && audio != pcm {
        sound_mismatch(gl, &mut notes, "mkv: audio differs from `wav`".into())?;
    } else if audio.len() != samples * 4 {
        bail!(
            "mkv: audio is {} stereo samples{}, expected {samples} identical to `wav`",
            audio.len() / 4,
            if audio.len() == samples * 4 {
                " but differs"
            } else {
                ""
            }
        );
    }

    // Cancelling stops early and leaves no half-written file.
    let cancelled = dir.join("cancelled.mkv");
    let stop = AtomicBool::new(false);
    match run(Format::Ffv1, cancelled.clone(), &stop, 3) {
        Err(e) if e.to_string() == "cancelled" => {}
        Err(e) => bail!("cancelled export failed oddly: {e:#}"),
        Ok(_) => bail!("cancelling didn't stop the export"),
    }
    if snapshot()? != before {
        bail!("a cancelled export left files behind");
    }
    // A cancellation reported by the final progress callback must also abort.
    let video_file = std::fs::read(&mkv)?;
    let stop = AtomicBool::new(false);
    match run(Format::Ffv1, mkv.clone(), &stop, total) {
        Err(e) if e.to_string() == "cancelled" => {}
        Err(e) => bail!("last-frame cancellation failed oddly: {e:#}"),
        Ok(_) => bail!("last-frame cancellation published a video"),
    }
    if std::fs::read(&mkv)? != video_file || snapshot()? != before {
        bail!("last-frame cancellation changed the existing video or left staged files");
    }
    notes.insert(
        0,
        format!("{total} frames, {samples} samples, as PNG + WAV and FFV1 + FLAC"),
    );
    Ok(notes.join("; "))
}

/// The built-in WebM export (#116): its structure read back with our own EBML reader
/// (one block per frame at its time, keyframes cued, Opus with its delay), a cancelled
/// export leaves nothing, and, where ffmpeg is installed, the decoded planes are close
/// to `yuv::reference` of `still`'s frames (much closer than to the neighbouring
/// frame) and the decoded sound has exactly `wav`'s length and is close to it.
#[cfg(feature = "webm")]
#[allow(clippy::type_complexity)]
fn export_webm(
    m: &Movie,
    still: &mut dyn FnMut(u64) -> Result<Frame>,
    pcm: &[u8],
    samples: usize,
    dir: &Path,
    run: &dyn Fn(
        crate::export::Format,
        PathBuf,
        &std::sync::atomic::AtomicBool,
        u64,
    ) -> Result<crate::export::Summary>,
) -> Result<String> {
    use crate::export::Format;
    use crate::webm::mux::{id, read};
    use std::sync::atomic::AtomicBool;
    let total = m.frame_count();
    let (w, h) = (W as usize, H as usize);
    let out = dir.join("film.webm");
    let s = run(Format::Webm, out.clone(), &AtomicBool::new(false), u64::MAX)?;
    if s.frames != total || s.audio_frames != samples {
        bail!(
            "webm: {} frames, {} samples reported",
            s.frames,
            s.audio_frames
        );
    }
    let file = std::fs::read(&out)?;
    let top = read::children(&file, 0)?;
    let seg = read::children(top[1].data, 0)?;
    let mut video_blocks = vec![];
    let mut audio_blocks = 0;
    for c in seg.iter().filter(|e| e.id == id::CLUSTER) {
        let kids = read::children(c.data, 0)?;
        let base = read::uint(kids[0].data) as i64;
        for b in &kids[1..] {
            let data = if b.id == id::BLOCK_GROUP {
                read::find(&read::children(b.data, 0)?, id::BLOCK)
                    .unwrap()
                    .data
            } else {
                b.data
            };
            let t = base + i16::from_be_bytes([data[1], data[2]]) as i64;
            if data[0] & 0x7f == 1 {
                video_blocks.push((t, data[3] & 0x80 != 0));
            } else {
                audio_blocks += 1;
            }
        }
    }
    let want: Vec<i64> = (0..total)
        .map(|n| ((n as f64 * 1001.0 / 30.0).round()) as i64)
        .collect();
    let times: Vec<i64> = video_blocks.iter().map(|b| b.0).collect();
    if times != want {
        bail!("webm: video block times {times:?}, want {want:?}");
    }
    if !video_blocks[0].1 {
        bail!("webm: the first frame isn't a keyframe");
    }
    if audio_blocks != (samples + 312).div_ceil(960) {
        bail!("webm: {audio_blocks} Opus packets for {samples} samples");
    }
    if read::find(&seg, id::CUES).is_none() {
        bail!("webm: no cues");
    }

    // Cancelling leaves no file and no staged part.
    let cancelled = dir.join("cancelled.webm");
    let names = || -> Result<usize> { Ok(std::fs::read_dir(dir)?.count()) };
    let before = names()?;
    match run(Format::Webm, cancelled.clone(), &AtomicBool::new(false), 3) {
        Err(e) if e.to_string() == "cancelled" => {}
        Err(e) => bail!("cancelled webm export failed oddly: {e:#}"),
        Ok(_) => bail!("cancelling didn't stop the webm export"),
    }
    if cancelled.exists() || names()? != before {
        bail!("a cancelled webm export left files behind");
    }

    if crate::export::ffmpeg_encoders().is_err() {
        return Ok(format!("webm: {total} frames (not decoded: no ffmpeg)"));
    }
    let planes = decode(
        &out,
        &["-map", "0:v:0", "-f", "rawvideo", "-pix_fmt", "yuv420p10le"],
    )?;
    let per = w * h * 3 / 2 * 2;
    if planes.len() != per * total as usize {
        bail!(
            "webm: decoded {} bytes of video, want {}",
            planes.len(),
            per * total as usize
        );
    }
    // Our interleaved-row I420 as planar (ffmpeg's yuv420p10le).
    let planar = |i420: Vec<u16>| -> Vec<u16> {
        let (cw, ch) = (w / 2, h / 2);
        let mut v = i420[..w * h].to_vec();
        for plane in 0..2 {
            for row in 0..ch {
                let at = w * h + row * w + plane * cw;
                v.extend_from_slice(&i420[at..at + cw]);
            }
        }
        v
    };
    let mean_err = |a: &[u16], b: &[u16]| -> f64 {
        a.iter()
            .zip(b)
            .map(|(x, y)| x.abs_diff(*y) as f64)
            .sum::<f64>()
            / a.len() as f64
    };
    let mut worst: f64 = 0.0;
    for n in [0, 1, total / 2, total - 1] {
        let got = crate::yuv::samples(&planes[n as usize * per..][..per], 10);
        let want = planar(crate::yuv::reference(&still(n)?, 10));
        let err = mean_err(&got, &want);
        let other = if n == 0 { 1 } else { n - 1 };
        let neighbour = mean_err(&got, &planar(crate::yuv::reference(&still(other)?, 10)));
        if err > 6.0 || err * 2.0 > neighbour {
            bail!(
                "webm frame {n}: mean error {err:.2} (10-bit codes), {neighbour:.2} against frame {other}"
            );
        }
        worst = worst.max(err);
    }
    let audio = decode(&out, &["-map", "0:a:0", "-f", "s16le", "-ar", "48000"])?;
    if audio.len() != samples * 4 {
        bail!(
            "webm: decoded {} stereo samples, want {samples}",
            audio.len() / 4
        );
    }
    let pcm16 = |b: &[u8]| -> Vec<f64> {
        b.as_chunks::<2>()
            .0
            .iter()
            .map(|s| i16::from_le_bytes(*s) as f64)
            .collect()
    };
    let (a, b) = (pcm16(&audio), pcm16(pcm));
    let noise = a.iter().zip(&b).map(|(x, y)| (x - y).powi(2)).sum::<f64>();
    let signal = b.iter().map(|y| y * y).sum::<f64>();
    let snr = 10.0 * (signal / noise.max(1.0)).log10();
    if snr < 20.0 {
        bail!("webm: the decoded sound is {snr:.1} dB from `wav`");
    }
    Ok(format!(
        "webm: {total} frames, mean error ≤ {worst:.2} (10-bit codes), sound {snr:.0} dB SNR"
    ))
}

// ------------------------------------------------------------------ texture checks

/// Write a PNG of `w` × `h` pixels, `f(x, y)` = RGBA with y = 0 the top row.
fn png(dir: &Path, name: &str, w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Result<()> {
    let p = dir.join(name);
    std::fs::create_dir_all(p.parent().unwrap())?;
    image::RgbaImage::from_fn(w, h, |x, y| image::Rgba(f(x, y))).save(p)?;
    Ok(())
}

fn near(got: [u8; 4], want: [u8; 3], tol: u8) -> bool {
    (0..3).all(|i| got[i].abs_diff(want[i]) <= tol)
}

fn expect_px(what: &str, f: &Frame, col: usize, row: usize, want: [u8; 3], tol: u8) -> Result<()> {
    let got = px(f, col, row);
    if !near(got, want, tol) {
        bail!(
            "{what}: pixel ({col}, {row}) is {:?}, wanted {want:?} (±{tol})",
            &got[..3]
        );
    }
    Ok(())
}

fn at_shot(i: usize, local: f32) -> At {
    At::Shot { shot: i, local }
}

/// Quadrants of a 2×2 image, top row first: red, green / blue, white.
fn quadrants(x: u32, y: u32) -> [u8; 4] {
    match (x, y) {
        (0, 0) => [255, 0, 0, 255],
        (1, 0) => [0, 255, 0, 255],
        (0, 1) => [0, 0, 255, 255],
        _ => [255, 255, 255, 255],
    }
}

fn channel_binding(gl: &Gl, dir: &Path) -> Result<String> {
    png(dir, "t/quad.png", 2, 2, quadrants)?;
    png(dir, "t/magenta.png", 3, 5, |_, _| [200, 0, 200, 255])?;
    // Quarters across: iChannel0 stretched (orientation), iChannel1 (unbound),
    // iChannel2, and iChannelResolution as colours.
    write(
        dir,
        &[(
            "s.glsl",
            &scene(
                "float q = floor(uv.x * 4.0);\n  \
                 vec3 r = vec3(iChannelResolution[0].xy, iChannelResolution[1].x + iChannelResolution[2].x + iChannelResolution[2].y + iChannelResolution[2].z * 2.0) / 10.0;\n  \
                 vec3 c = q < 1.0 ? texture(iChannel0, vec2(uv.x * 4.0, uv.y)).rgb\n    \
                 : q < 2.0 ? texture(iChannel1, uv).rgb + 0.2 * iChannelResolution[1].z\n    \
                 : q < 3.0 ? texture(iChannel2, uv).rgb : r;\n  \
                 fragColor = vec4(c, 1.0);",
            ),
        )],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.1\"\n[textures]\nquad = { file = \"t/quad.png\", filter = \"nearest\" }\n\
         [[shot]]\nid=\"up\"\nscene=\"s.glsl\"\nduration=1\n\
         channels = [\"quad\", \"\", { file = \"t/magenta.png\", filter = \"nearest\" }]\n\
         [[shot]]\nid=\"down\"\nscene=\"s.glsl\"\nduration=1\n\
         channels = [{ file = \"t/quad.png\", filter = \"nearest\", vflip = false }, \"\", { file = \"t/magenta.png\", filter = \"nearest\" }]\n",
    )?;
    let mut r = renderer(gl, &m)?;
    let up = shot(&mut r, &m, at_shot(0, 0.5), 1.0)?;
    // Default vflip: the image's top-left texel at the frame's top-left, as on Shadertoy.
    expect_px("iChannel0 top-left", &up, 10, 10, [255, 0, 0], 1)?;
    expect_px("iChannel0 top-right", &up, 30, 10, [0, 255, 0], 1)?;
    expect_px("iChannel0 bottom-left", &up, 10, 80, [0, 0, 255], 1)?;
    expect_px("iChannel0 bottom-right", &up, 30, 80, [255, 255, 255], 1)?;
    expect_px(
        "unbound iChannel1 (black, resolution 0)",
        &up,
        60,
        45,
        [0, 0, 0],
        1,
    )?;
    expect_px("iChannel2", &up, 100, 45, [200, 0, 200], 1)?;
    // iChannelResolution: [0] = (2, 2, 1), [1] = 0, [2] = (3, 5, 1) → b = (3 + 5 + 2) / 10.
    expect_px("iChannelResolution", &up, 140, 45, [51, 51, 255], 1)?;
    let down = shot(&mut r, &m, at_shot(1, 0.5), 1.0)?;
    // vflip = false: the top-left texel lands at the bottom-left.
    expect_px("vflip = false, bottom-left", &down, 10, 80, [255, 0, 0], 1)?;
    expect_px("vflip = false, top-left", &down, 10, 10, [0, 0, 255], 1)?;
    Ok(String::new())
}

fn channel_sampling(gl: &Gl, dir: &Path) -> Result<String> {
    // 4×1 greys: texel centres at u = 0.125, 0.375, 0.625, 0.875.
    png(dir, "ramp.png", 4, 1, |x, _| {
        let v = [0, 85, 170, 255][x as usize];
        [v, v, v, 255]
    })?;
    // Eighths across, each one grey value.
    write(
        dir,
        &[
            (
                "a.glsl",
                &scene(
                    "int k = int(uv.x * 8.0);\n  float v = 0.0;\n  \
                     if (k == 0) v = texture(iChannel0, vec2(0.24, 0.5)).r;      // nearest: texel 0\n  \
                     if (k == 1) v = texture(iChannel1, vec2(0.24, 0.5)).r;      // linear: between 0 and 1\n  \
                     if (k == 2) v = texture(iChannel0, vec2(1.375, 0.5)).r;     // repeat → 0.375\n  \
                     if (k == 3) v = texture(iChannel1, vec2(1.375, 0.5)).r;     // clamp → edge\n  \
                     if (k == 4) v = texture(iChannel2, vec2(1.375, 0.5)).r;     // mirror → 0.625\n  \
                     if (k == 5) v = textureLod(iChannel3, vec2(0.375, 0.5), 0.0).r;\n  \
                     if (k == 6) v = textureLod(iChannel3, vec2(0.375, 0.5), 2.0).r;  // 1×1 mip\n  \
                     if (k == 7) v = textureLod(iChannel3, vec2(0.25, 0.5), 1.0).r;   // 2×1 mip\n  \
                     fragColor = vec4(vec3(v), 1.0);",
                ),
            ),
            (
                "b.glsl",
                &scene(
                    "int k = int(uv.x * 8.0);\n  float v = 0.0;\n  \
                     if (k == 0) v = texture(iChannel0, vec2(0.375, 0.5)).r;\n  \
                     if (k == 1) v = textureLod(iChannel1, vec2(0.375, 0.5), 2.0).r;\n  \
                     fragColor = vec4(vec3(v), 1.0);",
                ),
            ),
        ],
    )?;
    let m = movie(
        dir,
        "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=1\n\
         channels = [{ file = \"ramp.png\", filter = \"nearest\" }, { file = \"ramp.png\", filter = \"linear\", wrap = \"clamp\" }, \
         { file = \"ramp.png\", filter = \"linear\", wrap = \"mirror\" }, \"ramp.png\"]\n\
         [[shot]]\nid=\"b\"\nscene=\"b.glsl\"\nduration=1\n\
         channels = [{ file = \"ramp.png\", filter = \"nearest\", srgb = true }, { file = \"ramp.png\", srgb = true }]\n",
    )?;
    let mut r = renderer(gl, &m)?;
    let a = shot(&mut r, &m, at_shot(0, 0.5), 1.0)?;
    let col = |k: usize| k * W as usize / 8 + 10;
    for (k, what, want, tol) in [
        (0, "nearest", 0, 1),
        (1, "linear", 39, 3),
        (2, "repeat", 85, 1),
        (3, "clamp", 255, 1),
        (4, "mirror", 170, 1),
        (5, "mipmap, level 0", 85, 1),
        (6, "mipmap, 1×1 level (the driver's filter)", 128, 4),
        (7, "mipmap, 2×1 level (the driver's filter)", 43, 4),
    ] {
        expect_px(what, &a, col(k), 45, [want; 3], tol)?;
    }
    let b = shot(&mut r, &m, at_shot(1, 0.5), 1.0)?;
    // sRGB 85 decodes to linear 0.0908 (23/255). Its 1×1 mip is the linear-space average
    // of (0, 0.0908, 0.402, 1.0) = 0.373 (95/255); a gamma-space average would be 54.
    expect_px("srgb", &b, col(0), 45, [23; 3], 2)?;
    let srgb_mip = px(&b, col(1), 45)[0];
    let mut note = String::new();
    if !(85..=105).contains(&srgb_mip) {
        if (45..=65).contains(&srgb_mip) {
            note = format!(
                "this driver averages sRGB mipmaps in gamma space ({srgb_mip}, linear would be 95)"
            );
        } else {
            bail!("srgb mipmap: {srgb_mip}, wanted about 95 (a linear-space average)");
        }
    }
    // One file, five samplers: two uploads (plain and sRGB), not five.
    if r.textures_ready() != 2 {
        bail!(
            "{} textures uploaded for one file used plain and as sRGB (wanted 2)",
            r.textures_ready()
        );
    }
    Ok(note)
}

fn texture_streaming(gl: &Gl, dir: &Path) -> Result<String> {
    png(dir, "big.png", 512, 512, |x, y| {
        [(x / 2) as u8, (y / 2) as u8, ((x ^ y) & 255) as u8, 255]
    })?;
    write(
        dir,
        &[(
            "s.glsl",
            &scene("fragColor = vec4(texture(iChannel0, uv).rgb, 1.0);"),
        )],
    )?;
    let m = movie(
        dir,
        "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\nchannels = [\"big.png\"]\n",
    )?;
    let want = {
        let mut r = renderer(gl, &m)?;
        shot(&mut r, &m, at_shot(0, 0.5), 1.0)?
    };
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None);
    r.set_upload_budget(64 << 10); // 1 MB of texels: 16+ frames of uploads
    r.set_movie(&m).map_err(|e| anyhow!("{e:?}"))?;
    let mut frames = 0;
    let got = loop {
        if let Some(_t) = r.render(&m, W, H, &info(0.5, 1.0), false) {
            break r.read_output().unwrap();
        }
        if r.textures_ready() > 0 && frames > 0 {
            bail!("the texture was ready but the shot didn't draw");
        }
        frames += 1;
        if frames > 5000 {
            bail!("the shot never became ready without blocking");
        }
        r.pump(&m, 0.5, false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    if frames == 0 {
        bail!("a non-blocking render drew before the texture was loaded");
    }
    if r.blocking_texture_loads != 0 {
        bail!(
            "playback loaded {} texture(s) on the spot",
            r.blocking_texture_loads
        );
    }
    if r.texture_bands < 16 {
        bail!(
            "the upload took {} band(s); the per-frame budget should spread it over 16+",
            r.texture_bands
        );
    }
    expect_same("non-blocking render with a texture", &got, &want, 0, 0.0)?;
    Ok(format!(
        "ready after {frames} frame(s), {} upload bands",
        r.texture_bands
    ))
}

fn channels_high(gl: &Gl, dir: &Path) -> Result<String> {
    png(dir, "t/quad.png", 2, 2, quadrants)?;
    png(dir, "t/magenta.png", 3, 5, |_, _| [200, 0, 200, 255])?;
    // Quarters across: iChannel7 stretched (orientation), iChannel5 (unbound),
    // iChannel4, and iChannelResolution[4] / [5] / [7] as colours.
    write(
        dir,
        &[(
            "s.glsl",
            &scene(
                "float q = floor(uv.x * 4.0);\n  \
                 vec3 r = vec3(iChannelResolution[7].xy, iChannelResolution[5].x + iChannelResolution[4].x + iChannelResolution[4].y + iChannelResolution[4].z * 2.0) / 10.0;\n  \
                 vec3 c = q < 1.0 ? texture(iChannel7, vec2(uv.x * 4.0, uv.y)).rgb\n    \
                 : q < 2.0 ? texture(iChannel5, uv).rgb + 0.2 * iChannelResolution[5].z\n    \
                 : q < 3.0 ? texture(iChannel4, uv).rgb : r;\n  \
                 fragColor = vec4(c, 1.0);",
            ),
        )],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.3\"\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\n\
         channels = [\"\", \"\", \"\", \"\", { file = \"t/magenta.png\", filter = \"nearest\" }, \"\", \"\", \
         { file = \"t/quad.png\", filter = \"nearest\" }]\n",
    )?;
    let mut r = renderer(gl, &m)?;
    let f = shot(&mut r, &m, at_shot(0, 0.5), 1.0)?;
    expect_px("iChannel7 top-left", &f, 10, 10, [255, 0, 0], 1)?;
    expect_px("iChannel7 bottom-right", &f, 30, 80, [255, 255, 255], 1)?;
    expect_px(
        "unbound iChannel5 (black, resolution 0)",
        &f,
        60,
        45,
        [0, 0, 0],
        1,
    )?;
    expect_px("iChannel4", &f, 100, 45, [200, 0, 200], 1)?;
    // [7] = (2, 2, 1), [5] = 0, [4] = (3, 5, 1) → b = (3 + 5 + 2) / 10.
    expect_px("iChannelResolution[4..7]", &f, 140, 45, [51, 51, 255], 1)?;
    Ok(String::new())
}

/// The fixture font, embedded so the selftest runs from an installed binary too.
const TEST_FONT: &[u8] = include_bytes!("../tests/films/text/fonts/NotoSans-subset.ttf");

fn text_textures(gl: &Gl, dir: &Path) -> Result<String> {
    std::fs::create_dir_all(dir.join("fonts"))?;
    std::fs::write(dir.join("fonts/sans.ttf"), TEST_FONT)?;
    png(dir, "t/grey.png", 2, 2, |_, _| [128, 128, 128, 255])?;
    // Left half: the text block stretched over it (coverage in red, distance in green).
    // Right half: iChannelMeta[0] (em, padding, pitch) / 255, plus iChannelMeta[1] (an
    // image: must be 0) as blue.
    write(
        dir,
        &[(
            "s.glsl",
            &scene(
                "vec4 t = texture(iChannel0, vec2(uv.x * 2.0, uv.y));\n  \
                 vec3 meta = vec3(iChannelMeta[0].x, iChannelMeta[0].w, iChannelMeta[0].z) / 255.0;\n  \
                 meta.b += dot(iChannelMeta[1], vec4(1.0));\n  \
                 fragColor = vec4(uv.x < 0.5 ? vec3(t.r, t.g, t.a) : meta, 1.0);",
            ),
        )],
    )?;
    let toml = |words: &str| {
        format!(
            "shotglass = \"0.1.3\"\n[fonts]\nsans = {{ file = \"fonts/sans.ttf\", licence = \"OFL-1.1\" }}\n\
             [texts]\ntitle = {{ text = \"{words}\", font = \"sans\", size = 120, sdf_spread = 6 }}\n\
             [[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\nchannels = [\"title\", \"t/grey.png\"]\n"
        )
    };
    let m = movie(dir, &toml("HI"))?;
    // Blocking render (as still/export): the reference.
    let want = {
        let mut r = renderer(gl, &m)?;
        shot(&mut r, &m, at_shot(0, 0.5), 1.0)?
    };
    // iChannelMeta: em 120, padding 6, pitch 1.2 × 120 = 144; an image's meta is 0.
    expect_px("iChannelMeta", &want, 120, 45, [120, 6, 144], 1)?;
    // The block: padding is empty (coverage 0, distance far outside); the letters are ink.
    expect_px("text padding", &want, 1, 1, [0, 0, 0], 2)?;
    let inked = (0..80)
        .flat_map(|c| (0..90).map(move |r| (c, r)))
        .filter(|&(c, r)| px(&want, c, r)[0] == 255)
        .count();
    if inked < 300 {
        bail!("the text block has {inked} fully covered pixels; expected the letters' ink");
    }
    for (c, r) in (0..80).flat_map(|c| (0..90).map(move |r| (c, r))) {
        let p = px(&want, c, r);
        if p[0] != p[2] {
            bail!("text alpha differs from coverage at ({c}, {r}): {p:?}");
        }
    }
    // Playing from cold: frames are held (never drawn half-made) until the block is
    // rasterised and uploaded, nothing loads on the spot, and the frame matches still.
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None);
    r.set_movie(&m).map_err(|e| anyhow!("{e:?}"))?;
    let mut frames = 0;
    let got = loop {
        if r.render(&m, W, H, &info(0.5, 1.0), false).is_some() {
            break r.read_output().unwrap();
        }
        frames += 1;
        if frames > 5000 {
            bail!("the text shot never became ready without blocking");
        }
        r.pump(&m, 0.5, false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    if frames == 0 {
        bail!("a non-blocking render drew before the text was rasterised");
    }
    if r.blocking_texture_loads != 0 {
        bail!(
            "playback rasterised {} text(s) on the spot",
            r.blocking_texture_loads
        );
    }
    expect_same("non-blocking render of a text", &got, &want, 0, 0.0)?;
    // New words: a new texture, no program compiled.
    let compiles = r.compiles_started;
    let m2 = movie(dir, &toml("Hi there"))?;
    r.set_movie(&m2).map_err(|e| anyhow!("{e:?}"))?;
    let changed = shot(&mut r, &m2, at_shot(0, 0.5), 1.0)?;
    if diff(&changed, &want, 0).1 == 0.0 {
        bail!("editing the text didn't change the picture");
    }
    if r.compiles_started != compiles {
        bail!(
            "a text edit compiled {} program(s)",
            r.compiles_started - compiles
        );
    }
    // A broken font refuses the edit; the last working version stays up.
    std::fs::write(dir.join("fonts/sans.ttf"), b"<html>not a font</html>")?;
    match movie(dir, &toml("Bye")) {
        Ok(_) => bail!("a broken font was accepted on load"),
        Err(e) if !format!("{e:#}").contains("not a font") => {
            bail!("the broken font wasn't explained: {e:#}")
        }
        Err(_) => {}
    }
    let after = shot(&mut r, &m2, at_shot(0, 0.5), 1.0)?;
    expect_same("after a refused text edit", &after, &changed, 0, 0.0)?;
    Ok(format!("ready after {frames} frame(s)"))
}

fn layers_compose(gl: &Gl, dir: &Path) -> Result<String> {
    // A solid picture (0.2, 0.4, 0.6); "half" (red, alpha 0.5, opacity 0.5) over
    // [0.5, 1.5); "invert" reads the picture below (@input on iChannel3) from 1.0 on.
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.2, 0.4, 0.6, 1.0);")),
            ("half.glsl", &scene("fragColor = vec4(1.0, 0.0, 0.0, 0.5);")),
            (
                "invert.glsl",
                &scene(
                    "fragColor = vec4(1.0 - texture(iChannel3, fragCoord / iChannelResolution[3].xy).rgb, 1.0);",
                ),
            ),
            ("fade.glsl", &scene("fragColor = vec4(1.0, 1.0, 1.0, 1.0);")),
        ],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.4\"\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=4\n\
         [[layer]]\nid=\"half\"\nscene=\"half.glsl\"\nfrom=0.5\nto=1.5\nopacity=0.5\n\
         [[layer]]\nid=\"invert\"\nscene=\"invert.glsl\"\nfrom=1.0\nto=2.0\nchannels=[\"\", \"\", \"\", \"@input\"]\n\
         [[layer]]\nid=\"fade\"\nscene=\"fade.glsl\"\nfrom=2.0\nto=4.0\nfade=[1.0, 1.0]\n",
    )?;
    let mut r = renderer(gl, &m)?;
    // @input is another pass's picture, not a texture: nothing to load, nothing pending
    // (the player's "loading textures" would otherwise never finish).
    if r.texture_progress() != (0, 0) {
        bail!(
            "an @input channel counts as a texture: {:?}",
            r.texture_progress()
        );
    }
    let at = |r: &mut Renderer, t: f32| shot(r, &m, at_shot(0, t), 1.0);
    // Before any layer: the picture as it is.
    expect_px("no layer", &at(&mut r, 0.25)?, 80, 45, [51, 102, 153], 1)?;
    // Premultiplied over at opacity 0.5: a = 0.25 → 0.25·red + 0.75·below.
    expect_px("half over", &at(&mut r, 0.75)?, 80, 45, [102, 77, 115], 1)?;
    // @input sees everything below (half included) and inverts it.
    expect_px(
        "invert of half over",
        &at(&mut r, 1.25)?,
        80,
        45,
        [153, 178, 140],
        1,
    )?;
    // Half's span is half-open: at 1.5 only invert is left.
    expect_px("span ends", &at(&mut r, 1.75)?, 80, 45, [204, 153, 102], 1)?;
    // A 1 s smooth fade: halfway in, half the white; fully in at the middle of the span.
    expect_px("mid fade", &at(&mut r, 2.5)?, 80, 45, [153, 178, 204], 2)?;
    expect_px("faded in", &at(&mut r, 3.0)?, 80, 45, [255, 255, 255], 1)?;
    Ok(String::new())
}

fn layers_ready_and_fallback(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.0, 0.0, 1.0, 1.0);")),
            (
                "l.glsl",
                &scene("fragColor = vec4(1.0, 0.0, 0.0, uv.x < 0.5 ? 1.0 : 0.0);"),
            ),
        ],
    )?;
    let toml = "shotglass = \"0.1.4\"\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\n\
                [[layer]]\nid=\"left\"\nscene=\"l.glsl\"\n";
    let m = movie(dir, toml)?;
    let want = {
        let mut r = renderer(gl, &m)?;
        shot(&mut r, &m, at_shot(0, 1.0), 1.0)?
    };
    expect_px("layer drawn (left)", &want, 20, 45, [255, 0, 0], 1)?;
    expect_px("layer transparent (right)", &want, 140, 45, [0, 0, 255], 1)?;
    // The shot alone first, so its program is ready; then the layer arrives (a new id:
    // no fallback). Until the layer compiles, playback holds the frame: never the
    // picture without its layer. Then exactly the blocking frame.
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None);
    let alone = movie(dir, "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\n")?;
    r.set_movie(&alone).map_err(|e| anyhow!("{e:?}"))?;
    shot(&mut r, &alone, at_shot(0, 1.0), 1.0)?;
    let m = movie(dir, toml)?;
    r.set_movie(&m).map_err(|e| anyhow!("{e:?}"))?;
    if r.render(&m, W, H, &info(1.0, 1.0), false).is_some() {
        bail!("playback drew the shot before its new layer was compiled");
    }
    let mut frames = 0;
    let got = loop {
        if r.render(&m, W, H, &info(1.0, 1.0), false).is_some() {
            break r.read_output().unwrap();
        }
        frames += 1;
        if frames > 5000 {
            bail!("the layered shot never became ready without blocking");
        }
        r.pump(&m, 1.0, false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    expect_same("non-blocking render with a layer", &got, &want, 0, 0.0)?;
    // A compile error in the layer: the last working layer stays on screen.
    write(dir, &[("l.glsl", &scene("fragColor = vec4(oops);"))])?;
    let m2 = movie(dir, toml)?;
    r.set_movie(&m2)
        .map_err(|_| anyhow!("set_movie refused a layer compile error (should defer it)"))?;
    let still = shot(&mut r, &m2, at_shot(0, 1.0), 1.0)?;
    expect_same("after a broken layer edit", &still, &want, 0, 0.0)?;
    if !r.errors().iter().any(|e| e.what.contains("layer \"left\"")) {
        bail!("the broken layer wasn't reported: {:?}", r.errors());
    }
    Ok(format!("ready after {frames} frame(s)"))
}

fn layers_scale(gl: &Gl, dir: &Path) -> Result<String> {
    // Each layer writes its own iResolution.x / 255 into red: what it was drawn at.
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.0, 0.0, 0.0, 1.0);")),
            (
                "res.glsl",
                &scene(
                    "fragColor = vec4(iResolution.x / 255.0, 0.0, 0.0, uv.y > 0.5 ? 1.0 : 0.0);",
                ),
            ),
            (
                "res2.glsl",
                &scene(
                    "fragColor = vec4(0.0, iResolution.x / 255.0, 0.0, uv.y <= 0.5 ? 1.0 : 0.0);",
                ),
            ),
        ],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.4\"\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\n\
         [[layer]]\nid=\"fixed\"\nscene=\"res.glsl\"\nscale=0.5\n\
         [[layer]]\nid=\"auto\"\nscene=\"res2.glsl\"\n",
    )?;
    let mut r = renderer(gl, &m)?;
    // At render scale 1 (W = 160): fixed draws at 80, auto at 160.
    let full = shot(&mut r, &m, at_shot(0, 0.5), 1.0)?;
    expect_px("scale 0.5 at render scale 1", &full, 80, 20, [80, 0, 0], 1)?;
    expect_px("auto at render scale 1", &full, 80, 70, [0, 160, 0], 1)?;
    // At render scale 0.5: fixed still 80, auto follows to 80.
    let half = shot(&mut r, &m, at_shot(0, 0.5), 0.5)?;
    expect_px(
        "scale 0.5 at render scale 0.5",
        &half,
        80,
        20,
        [80, 0, 0],
        1,
    )?;
    expect_px("auto at render scale 0.5", &half, 80, 70, [0, 80, 0], 1)?;
    Ok(String::new())
}

/// A film-local multiply blend, written against the blend interface: the layer arrives
/// premultiplied with its opacity applied, so unpremultiply, multiply, mix by coverage.
const MULTIPLY: &str = "vec4 blend(vec2 uv) {\n  vec4 b = getBase(uv);\n  vec4 l = getLayer(uv);\n  vec3 m = b.rgb * l.rgb / max(l.a, 1e-5);\n  return vec4(mix(b.rgb, m, l.a), 1.0);\n}\n";

fn layers_blend_and_refs(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.2, 0.4, 0.6, 1.0);")),
            (
                "green.glsl",
                &scene("fragColor = vec4(0.5, 1.0, 0.5, 1.0);"),
            ),
            ("white.glsl", &scene("fragColor = vec4(1.0);")),
            (
                "pic.glsl",
                &scene(
                    "fragColor = vec4(texture(iChannel0, fragCoord / iChannelResolution[0].xy).rgb, 1.0);",
                ),
            ),
            ("red.glsl", &scene("fragColor = vec4(1.0, 0.0, 0.0, 1.0);")),
            (
                "copy.glsl",
                &scene(
                    "fragColor = vec4(iChannelResolution[1].x / 255.0, texture(iChannel1, uv).r, 0.0, 1.0);",
                ),
            ),
            ("fx/multiply.glsl", MULTIPLY),
        ],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.5\"\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=5\n\
         [[layer]]\nid=\"mul\"\nscene=\"green.glsl\"\nto=1.0\nblend=\"fx/multiply.glsl\"\n\
         [[layer]]\nid=\"half\"\nscene=\"green.glsl\"\nfrom=1.0\nto=2.0\nopacity=0.5\nblend=\"fx/multiply.glsl\"\n\
         [[layer]]\nid=\"white\"\nscene=\"white.glsl\"\nfrom=2.0\nto=3.0\n\
         [[layer]]\nid=\"pic\"\nscene=\"pic.glsl\"\nfrom=2.0\nto=3.0\nchannels=[\"@picture\"]\n\
         [[layer]]\nid=\"src\"\nscene=\"red.glsl\"\nfrom=3.0\nto=4.0\nscale=0.5\nopacity=0.0\n\
         [[layer]]\nid=\"copy\"\nscene=\"copy.glsl\"\nfrom=3.0\nto=5.0\nchannels=[\"\", \"@src\"]\n",
    )?;
    let mut r = renderer(gl, &m)?;
    let at = |r: &mut Renderer, t: f32| shot(r, &m, at_shot(0, t), 1.0);
    // Multiply at opacity 1: (0.2, 0.4, 0.6) × (0.5, 1, 0.5).
    expect_px(
        "multiply blend",
        &at(&mut r, 0.5)?,
        80,
        45,
        [26, 102, 77],
        1,
    )?;
    // At opacity 0.5 the blend sees alpha 0.5: halfway between the base and the product.
    expect_px(
        "multiply at opacity 0.5",
        &at(&mut r, 1.5)?,
        80,
        45,
        [38, 102, 115],
        1,
    )?;
    // @picture sees the picture track past the white layer below.
    expect_px(
        "@picture under a white layer",
        &at(&mut r, 2.5)?,
        80,
        45,
        [51, 102, 153],
        1,
    )?;
    // @src: the earlier layer's own output (red), though it composites at opacity 0,
    // at its own size (scale 0.5 of 160 = 80).
    expect_px("@<layer>", &at(&mut r, 3.5)?, 80, 45, [80, 255, 0], 1)?;
    // After src's span: copy still runs and reads black, resolution 0.
    expect_px(
        "@<layer> while it's inactive",
        &at(&mut r, 4.5)?,
        80,
        45,
        [0, 0, 0],
        1,
    )?;
    Ok(String::new())
}

fn layers_hidden(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.2, 0.4, 0.6, 1.0);")),
            ("red.glsl", &scene("fragColor = vec4(1.0, 0.0, 0.0, 1.0);")),
            (
                "copy.glsl",
                &scene(
                    "fragColor = vec4(0.0, texture(iChannel0, uv).r, 0.0, uv.x < 0.5 ? 1.0 : 0.0);",
                ),
            ),
        ],
    )?;
    let m = movie(
        dir,
        "shotglass = \"0.1.5\"\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\n\
         [[layer]]\nid=\"red\"\nscene=\"red.glsl\"\n\
         [[layer]]\nid=\"copy\"\nscene=\"copy.glsl\"\nchannels=[\"@red\"]\n",
    )?;
    let mut r = renderer(gl, &m)?;
    let both = shot(&mut r, &m, at_shot(0, 0.5), 1.0)?;
    expect_px("both layers: copy (left)", &both, 20, 45, [0, 255, 0], 1)?;
    expect_px("both layers: red (right)", &both, 140, 45, [255, 0, 0], 1)?;
    // Mute "red": the picture shows through on the right; copy reads black on the left.
    r.set_hidden_layers(["red".to_string()].into());
    let muted = shot(&mut r, &m, at_shot(0, 0.5), 1.0)?;
    expect_px("red muted: copy reads black", &muted, 20, 45, [0, 0, 0], 1)?;
    expect_px("red muted: the picture", &muted, 140, 45, [51, 102, 153], 1)?;
    // Unmuted again: the same frame as before.
    r.set_hidden_layers(Default::default());
    let back = shot(&mut r, &m, at_shot(0, 0.5), 1.0)?;
    expect_same("after unmuting", &back, &both, 0, 0.0)?;
    Ok(String::new())
}

fn texture_edits(gl: &Gl, dir: &Path) -> Result<String> {
    png(dir, "t.png", 4, 4, |_, _| [255, 0, 0, 255])?;
    write(
        dir,
        &[(
            "s.glsl",
            &scene("fragColor = vec4(texture(iChannel0, uv).rgb, 1.0);"),
        )],
    )?;
    let toml = "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\nchannels = [\"t.png\"]\n";
    let m = movie(dir, toml)?;
    let mut r = renderer(gl, &m)?;
    let red = shot(&mut r, &m, at_shot(0, 1.0), 1.0)?;
    expect_px("first version", &red, 80, 45, [255, 0, 0], 1)?;
    let compiles = r.compiles_started;
    // An editor saving in pieces: half the file.
    let bytes = std::fs::read(dir.join("t.png"))?;
    std::fs::write(dir.join("t.png"), &bytes[..bytes.len() / 2])?;
    let m2 = movie(dir, toml)?;
    r.set_movie(&m2)
        .map_err(|_| anyhow!("set_movie refused a broken image (should fall back)"))?;
    let playing = r
        .render(&m2, W, H, &info(1.0, 1.0), false)
        .ok_or_else(|| anyhow!("playback showed nothing instead of the last working texture"))?;
    let _ = playing;
    let still = shot(&mut r, &m2, at_shot(0, 1.0), 1.0)?;
    expect_same("after a broken texture edit", &still, &red, 1, 0.0)?;
    let errors: String = r.errors().iter().map(|e| e.to_string()).collect();
    if !errors.contains("t.png") || !errors.contains("still being saved") {
        bail!("the broken texture wasn't reported: {errors:?}");
    }
    png(dir, "t.png", 8, 8, |_, _| [0, 255, 0, 255])?;
    let m3 = movie(dir, toml)?;
    r.set_movie(&m3).map_err(|e| anyhow!("{e:?}"))?;
    let green = shot(&mut r, &m3, at_shot(0, 1.0), 1.0)?;
    expect_px("fixed texture", &green, 80, 45, [0, 255, 0], 1)?;
    if !r.errors().is_empty() {
        bail!("errors remained after the fix");
    }
    if r.compiles_started != compiles {
        bail!(
            "texture-only edits compiled {} program(s)",
            r.compiles_started - compiles
        );
    }
    // A missing file refuses the whole new version; the old one keeps drawing.
    std::fs::remove_file(dir.join("t.png"))?;
    if movie(dir, toml).is_ok() {
        bail!("a missing texture file was accepted on load");
    }
    if r.set_movie(&m3).is_ok() {
        bail!("set_movie accepted a version whose texture file is gone");
    }
    let after = shot(&mut r, &m3, at_shot(0, 1.0), 1.0)?;
    expect_same("after a refused version", &after, &green, 0, 0.0)?;
    Ok(String::new())
}

fn texture_eviction(gl: &Gl, dir: &Path) -> Result<String> {
    png(dir, "a.png", 16, 16, |x, y| {
        [(x * 16) as u8, (y * 16) as u8, 90, 255]
    })?;
    png(dir, "b.png", 16, 16, |_, _| [10, 20, 30, 255])?;
    write(
        dir,
        &[(
            "s.glsl",
            &scene("fragColor = vec4(texture(iChannel0, uv).rgb, 1.0);"),
        )],
    )?;
    let m = movie(
        dir,
        "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\nchannels = [\"a.png\"]\n\
         [[shot]]\nid=\"b\"\nscene=\"s.glsl\"\nduration=60\nchannels = [\"b.png\"]\n",
    )?;
    let mut r = renderer(gl, &m)?;
    r.set_texture_budget(1);
    let first = shot(&mut r, &m, at_shot(0, 1.0), 1.0)?;
    shot(&mut r, &m, at_shot(1, 40.0), 1.0)?;
    for _ in 0..31 {
        r.pump(&m, 42.0, false);
    }
    if r.textures_ready() != 1 {
        bail!(
            "{} textures resident over a 1-byte budget (wanted only the one in use)",
            r.textures_ready()
        );
    }
    let again = shot(&mut r, &m, at_shot(0, 1.0), 1.0)?;
    expect_same("evicted texture after re-upload", &again, &first, 0, 0.0)?;
    Ok(String::new())
}

fn texture_freeze(gl: &Gl, dir: &Path) -> Result<String> {
    png(dir, "t.png", 2, 2, |_, _| [255, 0, 0, 255])?;
    write(
        dir,
        &[
            (
                "a.glsl",
                &scene("fragColor = vec4(texture(iChannel0, uv).rgb, 1.0);"),
            ),
            ("b.glsl", &scene("fragColor = vec4(0.0, 1.0, 0.0, 1.0);")),
            (
                "fx/mix.glsl",
                "vec4 transition(vec2 uv) { return mix(getFromColor(uv), getToColor(uv), progress); }\n",
            ),
        ],
    )?;
    let toml = "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=2\nchannels = [\"t.png\"]\n\
                [[shot]]\nid=\"b\"\nscene=\"b.glsl\"\nduration=2\n\
                transition = { type = \"fx/mix.glsl\", duration = 1, freeze = true }\n";
    let at = At::Transition {
        from: 0,
        to: 1,
        from_local: 1.0,
        to_local: 0.0,
        progress: 0.0,
    };
    let m = movie(dir, toml)?;
    let mut r = renderer(gl, &m)?;
    let f = shot(&mut r, &m, at, 1.0)?;
    expect_px("frozen outgoing shot", &f, 80, 45, [255, 0, 0], 1)?;
    png(dir, "t.png", 3, 3, |_, _| [0, 0, 255, 255])?;
    let m2 = movie(dir, toml)?;
    r.set_movie(&m2).map_err(|e| anyhow!("{e:?}"))?;
    // Playing: the edited texture isn't loaded yet, so the freeze uses the last
    // working one; once it's loaded, the frozen frame must be redrawn with it.
    let t = m2.shots[1].start;
    r.render_at(&m2, W, H, &info(t, 1.0), at, false)
        .ok_or_else(|| anyhow!("nothing drawn while the edited texture loads"))?;
    let f = r.read_output().unwrap();
    expect_px(
        "frozen shot while the edit loads",
        &f,
        80,
        45,
        [255, 0, 0],
        1,
    )?;
    for _ in 0..2000 {
        r.pump(&m2, t, false);
        if r.texture_progress().0 == 1 && r.textures_ready() == 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    for _ in 0..31 {
        r.pump(&m2, t, false); // drops the fallback (and the old texture)
    }
    r.render_at(&m2, W, H, &info(t, 1.0), at, false)
        .ok_or_else(|| anyhow!("nothing drawn after the edited texture loaded"))?;
    let f = r.read_output().unwrap();
    expect_px(
        "frozen shot after the edit loaded",
        &f,
        80,
        45,
        [0, 0, 255],
        1,
    )?;
    let f = shot(&mut r, &m2, at, 1.0)?;
    expect_px(
        "frozen shot after a texture edit",
        &f,
        80,
        45,
        [0, 0, 255],
        1,
    )?;
    Ok(String::new())
}

fn texture_unpack_state(gl: &Gl, dir: &Path) -> Result<String> {
    use eframe::glow::{self, HasContext};
    // 3 px wide: 12-byte rows, which a stray alignment of 8 or row length would misread.
    png(dir, "t.png", 3, 3, |x, y| {
        [(x * 100) as u8, (y * 100) as u8, 77, 255]
    })?;
    write(
        dir,
        &[(
            "s.glsl",
            &scene("fragColor = vec4(texelFetch(iChannel0, ivec2(fragCoord) % 3, 0).rgb, 1.0);"),
        )],
    )?;
    let m = movie(
        dir,
        "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\nchannels = [{ file = \"t.png\", vflip = false }]\n",
    )?;
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None);
    r.set_movie(&m).map_err(|e| anyhow!("{e:?}"))?;
    // What other code (the sound path's staging buffers, egui) may leave behind.
    let junk = unsafe {
        let b = gl.create_buffer().map_err(|e| anyhow!(e))?;
        gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(b));
        gl.buffer_data_u8_slice(glow::PIXEL_UNPACK_BUFFER, &[9u8; 4096], glow::STATIC_DRAW);
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 8);
        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 5);
        gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 1);
        b
    };
    let f = shot(&mut r, &m, at_shot(0, 0.5), 1.0)?;
    let state = r.unpack_state();
    unsafe { gl.delete_buffer(junk) };
    // Rows of the frame are top first; texelFetch row 0 is the frame's bottom row,
    // and vflip = false keeps the file's top row as texel row 0.
    for (x, y) in [(0usize, 0usize), (1, 0), (2, 1), (1, 2)] {
        let row = H as usize - 1 - y;
        expect_px(
            &format!("texel ({x}, {y})"),
            &f,
            x,
            row,
            [(x * 100) as u8, (y * 100) as u8, 77],
            1,
        )?;
    }
    if state != [4, 0, 0, 0, 0] {
        bail!(
            "unpack state after an upload is {state:?} (alignment, row length, skip rows, \
             skip pixels, unpack buffer); wanted [4, 0, 0, 0, 0]"
        );
    }
    Ok(String::new())
}

// ------------------------------------------------------------------ footage checks

/// An image sequence `<prefix>00.png …` of `count` flat frames, `w` × `h`, frame k
/// coloured (10k, 250 − 10k, 0) so a readback says which frame was drawn.
fn sequence(dir: &Path, prefix: &str, count: u32, w: u32, h: u32) -> Result<()> {
    for k in 0..count {
        png(dir, &format!("{prefix}{k:02}.png"), w, h, |_, _| {
            [(10 * k) as u8, (250 - 10 * k) as u8, 0, 255]
        })?;
    }
    Ok(())
}

/// The sequence frame a pixel shows (None: black).
fn frame_of(p: [u8; 4]) -> Option<u64> {
    (p[0] != 0 || p[1] != 0).then(|| (p[0] as f32 / 10.0).round() as u64)
}

/// A scene showing iChannel0 (texel 0, so no derivatives) and iChannelTime[0] / 2 in blue.
fn footage_scene() -> String {
    scene("fragColor = vec4(texelFetch(iChannel0, ivec2(0), 0).rg, iChannelTime[0] / 2.0, 1.0);")
}

/// Play film frames `ns` without blocking (one `pump` and a 1 ms pause per try, at most
/// `tries` per frame), checking each drawn frame shows exactly the source frame
/// `source_frame` says. Returns the tries that drew nothing, per film frame.
fn play_footage(
    r: &mut Renderer,
    m: &Movie,
    item: crate::movie::ItemRef,
    ns: impl Iterator<Item = u64>,
    tries: usize,
) -> Result<Vec<(u64, usize)>> {
    let mut stalls = vec![];
    for n in ns {
        let t = m.frame_time(m.wrap_frame(n));
        let mut waited = 0;
        loop {
            r.pump(m, t, false);
            if r.render(m, W, H, &FrameInfo::frame(m, n), false).is_some() {
                let f = r.read_output().ok_or_else(|| anyhow!("no output"))?;
                let want = m.source_frame(item, 0, n);
                let got = frame_of(px(&f, 80, 45));
                if got != want {
                    bail!("film frame {n}: playback drew source frame {got:?}, wanted {want:?}");
                }
                break;
            }
            waited += 1;
            if waited >= tries {
                bail!("film frame {n} never drew without blocking ({waited} tries)");
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        stalls.push((n, waited));
    }
    Ok(stalls)
}

fn footage_mapping(gl: &Gl, dir: &Path) -> Result<String> {
    use crate::movie::ItemRef;
    sequence(dir, "f/s_", 24, 4, 2)?;
    // Quarters across: four channels of one 24 fps sequence in a 30 fps film, each
    // drawn as its texel and iChannelTime / 2 in blue.
    write(
        dir,
        &[
            (
                "s.glsl",
                &scene(
                    "int q = int(uv.x * 4.0);\n  \
                 vec4 c = q == 0 ? texelFetch(iChannel0, ivec2(0), 0) : q == 1 ? texelFetch(iChannel1, ivec2(0), 0)\n    \
                 : q == 2 ? texelFetch(iChannel2, ivec2(0), 0) : texelFetch(iChannel3, ivec2(0), 0);\n  \
                 float t = q == 0 ? iChannelTime[0] : q == 1 ? iChannelTime[1] : q == 2 ? iChannelTime[2] : iChannelTime[3];\n  \
                 fragColor = vec4(c.rg, t / 2.0, 1.0);",
                ),
            ),
            (
                // A layer drawing its channel over the bottom rows only (alpha 0 elsewhere).
                "l.glsl",
                &scene(
                    "fragColor = vec4(texelFetch(iChannel0, ivec2(0), 0).rgb, uv.y < 0.1 ? 1.0 : 0.0);",
                ),
            ),
        ],
    )?;
    // A lead shot first, so the second's clock ("shot") starts at film frame 15; a
    // layer from 1.3 s (film frame 39), whose clock is its own.
    let m = movie(
        dir,
        "shotglass = \"0.1.6\"\nfps = 30\n[textures]\nseq = { frames = \"f/s_##.png\", fps = 24 }\n\
         [[shot]]\nid=\"lead\"\nscene=\"s.glsl\"\nduration=0.5\n\
         [[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=3\n\
         channels = [\"seq\", { texture = \"seq\", in = 0.25, speed = 0.5 }, \
         { texture = \"seq\", speed = 2, end = \"loop\", clock = \"movie\" }, \
         { texture = \"seq\", in_frame = 20, end = \"black\" }]\n\
         [[layer]]\nid=\"l\"\nscene=\"l.glsl\"\nfrom = \"a + 0.8\"\nchannels = [{ texture = \"seq\", speed = 0.5 }]\n",
    )?;
    let item = ItemRef::Shot(1);
    // The 24-in-30 pattern: one repeat every five film frames.
    let first: Vec<u64> = (15..27)
        .filter_map(|n| m.source_frame(item, 0, n))
        .collect();
    if first != [0, 0, 1, 2, 3, 4, 4, 5, 6, 7, 8, 8] {
        bail!("source_frame's 24-in-30 pattern is {first:?}");
    }
    let mut r = renderer(gl, &m)?;
    r.set_readback(true);
    let mut black = 0;
    for n in 15..105u64 {
        r.render(&m, W, H, &FrameInfo::frame(&m, n), true)
            .ok_or_else(|| anyhow!("frame {n}: nothing rendered"))?;
        let f = r.read_output().ok_or_else(|| anyhow!("no output"))?;
        for q in 0..4 {
            let p = px(&f, q * 40 + 20, 45);
            let want = m.source_frame(item, q, n);
            let got = frame_of(p);
            if got != want {
                bail!("film frame {n}, iChannel{q}: drew source frame {got:?}, wanted {want:?}");
            }
            let time = want.map_or(0.0, |k| m.frame_map(item, q).unwrap().source_time(k));
            let blue = (time / 2.0 * 255.0).round();
            if (p[2] as f32 - blue).abs() > 1.0 {
                bail!(
                    "film frame {n}, iChannel{q}: iChannelTime reads {}, wanted {time}",
                    p[2] as f32 / 127.5
                );
            }
            black += want.is_none() as usize;
        }
        // The layer's channel, on the layer's clock (the shot's below, before it starts).
        let want = if n >= 39 {
            m.source_frame(ItemRef::Layer(0), 0, n)
        } else {
            m.source_frame(item, 0, n)
        };
        if frame_of(px(&f, 20, 86)) != want {
            bail!(
                "film frame {n}, the layer's iChannel0: drew source frame {:?}, wanted {want:?}",
                frame_of(px(&f, 20, 86))
            );
        }
    }
    if m.source_frame(ItemRef::Layer(0), 0, 39) != Some(0) {
        bail!("a layer's footage clock doesn't start at the layer");
    }
    if black == 0 {
        bail!("end = \"black\" never read black");
    }
    if r.blocking_texture_loads != 0 {
        bail!("footage frames were counted as texture loads");
    }
    // Four channels of one sequence share its frames: one load per frame.
    if r.blocking_footage_loads != 24 {
        bail!(
            "{} frame loads for the 24 frames of one sequence on four channels",
            r.blocking_footage_loads
        );
    }
    Ok("90 film frames × 4 channels and a layer".into())
}

fn footage_streaming(gl: &Gl, dir: &Path) -> Result<String> {
    use crate::movie::ItemRef;
    sequence(dir, "f/s_", 24, 4, 2)?;
    write(dir, &[("s.glsl", &footage_scene())])?;
    let m = movie(
        dir,
        "fps = 30\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=3\n\
         channels = [{ frames = \"f/s_##.png\", fps = 24, end = \"loop\" }]\n",
    )?;
    let mut r = renderer(gl, &m)?; // programs compiled; no footage frame loaded
    r.set_readback(true);
    if r.render(&m, W, H, &FrameInfo::frame(&m, 0), false)
        .is_some()
    {
        bail!("a non-blocking render drew before its footage frame was loaded");
    }
    let stalls = play_footage(&mut r, &m, ItemRef::Shot(0), 0..60, 5000)?;
    if r.blocking_footage_loads != 0 || r.blocking_texture_loads != 0 {
        bail!(
            "playback loaded {} footage frame(s) and {} texture(s) on the spot",
            r.blocking_footage_loads,
            r.blocking_texture_loads
        );
    }
    let waited: usize = stalls.iter().map(|(_, w)| w).sum();
    // Seeking while playing: frames asked for where the playhead was (decoding, or
    // decoded and not uploaded) mustn't hold up the ones where it is now. A 1 fps
    // sequence: each second of the film needs its own frame.
    sequence(dir, "g/s_", 24, 4, 2)?;
    write(dir, &[("g.glsl", &footage_scene())])?;
    let g = movie(
        dir,
        "fps = 30\n[[shot]]\nid=\"a\"\nscene=\"g.glsl\"\nduration=24\n\
         channels = [{ frames = \"g/s_##.png\", fps = 1 }]\n",
    )?;
    let mut r = renderer(gl, &g)?;
    r.set_readback(true);
    for s in 0..20u64 {
        r.pump(&g, s as f32 + 0.1, false);
    }
    play_footage(&mut r, &g, ItemRef::Shot(0), [693].into_iter(), 2000)
        .map_err(|e| anyhow!("after 20 seeks: {e:#}"))?;
    if r.blocking_footage_loads != 0 {
        bail!("seeking while playing loaded frames on the spot");
    }
    Ok(format!(
        "60 frames, {waited} waits (all at the start: {})",
        stalls[0].1
    ))
}

fn footage_prefetch(gl: &Gl, dir: &Path) -> Result<String> {
    use crate::movie::ItemRef;
    sequence(dir, "f/a_", 24, 4, 2)?;
    write(dir, &[("s.glsl", &footage_scene())])?;
    // A loop film of three footage shots (78 frames): a cut at frame 30, a frozen
    // transition at 48–59 (shot b's source frame 18 held under it, which b's own window
    // has passed by frame 51), the seam at 78.
    let m = movie(
        dir,
        "fps = 30\nloop = true\n\
         [[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\nchannels = [{ frames = \"f/a_##.png\", in_frame = 2 }]\n\
         [[shot]]\nid=\"b\"\nscene=\"s.glsl\"\nduration=1\nchannels = [{ frames = \"f/a_##.png\" }]\n\
         [[shot]]\nid=\"c\"\nscene=\"s.glsl\"\nduration=1\nchannels = [{ frames = \"f/a_##.png\", in_frame = 5 }]\n\
         transition = { type = \"fx/crossfade.glsl\", duration = 0.4, freeze = true }\n",
    )?;
    let mut r = renderer(gl, &m)?;
    r.set_readback(true);
    // No room to spare: only the frames the playhead asks for may stay.
    r.set_texture_budget(1);
    // Warm up mid-shot, then play one try per film frame (as playback does) across the
    // cut and the seam: their first frames must already be there.
    let item = |n: u64| {
        ItemRef::Shot(match m.wrap_frame(n) {
            0..30 => 0,
            30..48 => 1,
            _ => 2,
        })
    };
    let mut stalled = vec![];
    play_footage(&mut r, &m, item(15), 15..16, 5000)?;
    for n in 16..110u64 {
        let t = m.frame_time(m.wrap_frame(n));
        std::thread::sleep(std::time::Duration::from_millis(2));
        r.pump(&m, t, false);
        match r.render(&m, W, H, &FrameInfo::frame(&m, n), false) {
            Some(_) => {
                // Under the transition the picture is a mix; elsewhere one exact frame.
                if matches!(m.at(t), At::Transition { .. }) {
                    continue;
                }
                let f = r.read_output().ok_or_else(|| anyhow!("no output"))?;
                let want = m.source_frame(item(n), 0, n);
                if frame_of(px(&f, 80, 45)) != want {
                    bail!("film frame {n}: wrong source frame (wanted {want:?})");
                }
            }
            None => stalled.push(n),
        }
    }
    if !stalled.is_empty() {
        bail!(
            "playback stalled at film frame(s) {stalled:?} (the cut is 30, the frozen \
             transition 48–59, the seam 78)"
        );
    }
    Ok(String::new())
}

fn footage_agree(gl: &Gl, dir: &Path) -> Result<String> {
    sequence(dir, "f/s_", 24, 8, 4)?;
    write(
        dir,
        &[(
            "s.glsl",
            &scene("fragColor = vec4(texture(iChannel0, uv).rg, fract(iChannelTime[0]), 1.0);"),
        )],
    )?;
    // 30000/1001 in a 30000/1001 film: the identity, which a `t * fps` rounding breaks.
    let m = movie(
        dir,
        "fps = \"30000/1001\"\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=0.8\n\
         channels = [{ frames = \"f/s_##.png\", fps = \"30000/1001\" }]\n",
    )?;
    let count = m.frame_count();
    let export: Vec<Frame> = {
        let mut r = renderer(gl, &m)?;
        r.set_readback(true);
        (0..count)
            .map(|n| {
                r.render(&m, W, H, &FrameInfo::frame(&m, n), true)
                    .ok_or_else(|| anyhow!("export frame {n}: nothing rendered"))?;
                r.read_output().ok_or_else(|| anyhow!("no output"))
            })
            .collect::<Result<_>>()?
    };
    for (n, f) in export.iter().enumerate() {
        if frame_of(px(f, 80, 45)) != Some(n as u64) {
            bail!(
                "export frame {n} shows source frame {:?}",
                frame_of(px(f, 80, 45))
            );
        }
    }
    // `still` by time, backwards, with a budget so small every frame is evicted between.
    let mut r = renderer(gl, &m)?;
    r.set_readback(true);
    r.set_texture_budget(1);
    for n in (0..count).rev() {
        let t = m.frame_time(n);
        r.render(&m, W, H, &FrameInfo::at(&m, t), true)
            .ok_or_else(|| anyhow!("still {t}: nothing rendered"))?;
        let f = r.read_output().ok_or_else(|| anyhow!("no output"))?;
        expect_same(
            &format!("still at frame {n}'s time vs export"),
            &f,
            &export[n as usize],
            0,
            0.0,
        )?;
        for _ in 0..3 {
            r.pump(&m, m.frame_time((n + count / 2) % count), false);
        }
    }
    Ok(format!("{count} frames"))
}

fn footage_budget(gl: &Gl, dir: &Path) -> Result<String> {
    use crate::movie::ItemRef;
    sequence(dir, "f/s_", 24, 32, 32)?;
    write(dir, &[("s.glsl", &footage_scene())])?;
    let m = movie(
        dir,
        "fps = 30\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=3\n\
         channels = [{ frames = \"f/s_##.png\", fps = 30, end = \"loop\" }]\n",
    )?;
    let frame = 32 * 32 * 4;
    let budget = 8 * frame;
    let mut r = renderer(gl, &m)?;
    r.set_readback(true);
    r.set_texture_budget(budget);
    let mut worst = 0;
    for n in 0..72u64 {
        play_footage(&mut r, &m, ItemRef::Shot(0), n..n + 1, 5000)?;
        worst = worst.max(r.texture_bytes());
        if r.texture_bytes() > budget {
            bail!(
                "film frame {n}: {} bytes of textures resident, over the {budget}-byte budget",
                r.texture_bytes()
            );
        }
    }
    // 72 frames shown (24 distinct, three times); evicted textures are reused.
    if r.texture_creates > 20 {
        bail!(
            "{} textures created for 72 frames of footage (evicted ones should be reused)",
            r.texture_creates
        );
    }
    Ok(format!(
        "at most {} of {} KB resident, {} textures created",
        worst / 1024,
        budget / 1024,
        r.texture_creates
    ))
}

fn footage_broken(gl: &Gl, dir: &Path) -> Result<String> {
    sequence(dir, "f/s_", 10, 4, 2)?;
    write(dir, &[("s.glsl", &footage_scene())])?;
    let toml = "fps = 10\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\n\
                channels = [{ frames = \"f/s_##.png\" }]\n";
    let m = movie(dir, toml)?;
    let mut r = renderer(gl, &m)?;
    r.set_readback(true);
    let draw = |r: &mut Renderer, m: &Movie, blocking: bool| -> Result<Option<u64>> {
        r.render(m, W, H, &FrameInfo::frame(m, 5), blocking)
            .ok_or_else(|| anyhow!("frame 5: nothing rendered"))?;
        Ok(frame_of(px(&r.read_output().unwrap(), 80, 45)))
    };
    if draw(&mut r, &m, true)? != Some(5) {
        bail!("the first version didn't draw source frame 5");
    }
    let compiles = r.compiles_started;
    // A renderer rewriting frame 5 in place, caught half-written.
    let path = dir.join("f/s_05.png");
    let bytes = std::fs::read(&path)?;
    std::fs::write(&path, &bytes[..bytes.len() / 2])?;
    let m2 = movie(dir, toml)?;
    r.set_movie(&m2).map_err(|_| {
        anyhow!("set_movie refused a version with a broken frame (should fall back)")
    })?;
    // Reload on a healthy frame, then run the fallback cleanup before seeking.
    r.render(&m2, W, H, &FrameInfo::frame(&m2, 4), true)
        .ok_or_else(|| anyhow!("healthy frame 4: nothing rendered"))?;
    let healthy = px(&r.read_output().unwrap(), 80, 45);
    if frame_of(healthy) != Some(4) {
        bail!("the replacement version didn't draw healthy frame 4");
    }
    r.pump(&m2, m2.frame_time(4), false);
    if draw(&mut r, &m2, false)? != Some(5) || draw(&mut r, &m2, true)? != Some(5) {
        bail!("a broken frame didn't fall back to the last working version's frame");
    }
    let errors: String = r.errors().iter().map(|e| e.to_string()).collect();
    if !errors.contains("s_05.png") {
        bail!("the broken frame wasn't reported: {errors:?}");
    }
    if r.compiles_started != compiles {
        bail!(
            "a footage edit compiled {} program(s)",
            r.compiles_started - compiles
        );
    }
    // With no working version to fall back on, the frame reads black and headless
    // commands are told why.
    let mut fresh = renderer(gl, &m2)?;
    fresh.set_readback(true);
    if draw(&mut fresh, &m2, true)?.is_some() || fresh.footage_failure().is_none() {
        bail!("a broken frame without a fallback should read black and be reported");
    }
    // Fixed: the new frame shows.
    png(dir, "f/s_05.png", 4, 2, |_, _| [70, 180, 0, 255])?;
    let m3 = movie(dir, toml)?;
    r.set_movie(&m3).map_err(|e| anyhow!("{e:?}"))?;
    if draw(&mut r, &m3, true)? != Some(7) {
        bail!("the re-rendered frame didn't replace the old one");
    }
    // A missing middle frame is a gap: the new version is refused.
    std::fs::remove_file(dir.join("f/s_03.png"))?;
    match movie(dir, toml) {
        Ok(_) => bail!("a sequence with a missing frame was accepted on load"),
        Err(e) if !format!("{e:#}").contains("frame 3 is missing") => {
            bail!("a missing frame gave the wrong error: {e:#}")
        }
        Err(_) => {}
    }
    Ok(String::new())
}

fn footage_unpack_state(gl: &Gl, dir: &Path) -> Result<String> {
    use eframe::glow::{self, HasContext};
    // 3 px wide frames (12-byte rows), each texel (x·100, y·100, 10·k).
    for k in 0..4u32 {
        png(dir, &format!("f/u_{k}.png"), 3, 3, |x, y| {
            [(x * 100) as u8, (y * 100) as u8, (10 * k) as u8, 255]
        })?;
    }
    write(
        dir,
        &[(
            "s.glsl",
            &scene("fragColor = vec4(texelFetch(iChannel0, ivec2(fragCoord) % 3, 0).rgb, 1.0);"),
        )],
    )?;
    let m = movie(
        dir,
        "fps = 4\n[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=1\n\
         channels = [{ frames = \"f/u_#.png\", vflip = false }]\n",
    )?;
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None);
    r.set_readback(true);
    r.set_upload_budget(12); // one row per band
    r.set_movie(&m).map_err(|e| anyhow!("{e:?}"))?;
    // What the sound path's staging buffers (or egui) may leave behind, before every pump.
    let junk = unsafe {
        let b = gl.create_buffer().map_err(|e| anyhow!(e))?;
        gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(b));
        gl.buffer_data_u8_slice(glow::PIXEL_UNPACK_BUFFER, &[9u8; 4096], glow::STATIC_DRAW);
        gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
        b
    };
    let stray = || unsafe {
        gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(junk));
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 8);
        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 5);
        gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 1);
    };
    let mut result = Ok(());
    'frames: for n in 0..4u64 {
        let mut tries = 0;
        let f = loop {
            stray();
            let bands = r.texture_bands;
            r.pump(&m, m.frame_time(n), false);
            let state = r.unpack_state();
            if r.texture_bands > bands && state != [4, 0, 0, 0, 0] {
                result = Err(anyhow!(
                    "unpack state after a footage upload is {state:?}; wanted [4, 0, 0, 0, 0]"
                ));
                break 'frames;
            }
            if r.render(&m, W, H, &FrameInfo::frame(&m, n), false)
                .is_some()
            {
                break r.read_output().unwrap();
            }
            tries += 1;
            if tries > 5000 {
                result = Err(anyhow!("frame {n} never drew"));
                break 'frames;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        for (x, y) in [(0usize, 0usize), (1, 0), (2, 1), (1, 2)] {
            let row = H as usize - 1 - y;
            if let Err(e) = expect_px(
                &format!("frame {n}, texel ({x}, {y})"),
                &f,
                x,
                row,
                [(x * 100) as u8, (y * 100) as u8, (10 * n) as u8],
                1,
            ) {
                result = Err(e);
                break 'frames;
            }
        }
    }
    unsafe {
        gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
        gl.delete_buffer(junk);
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
        gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 0);
    }
    if r.texture_bands < 4 * 3 {
        result = result.and(Err(anyhow!(
            "{} upload bands for four 3-row frames (wanted one row per band)",
            r.texture_bands
        )));
    }
    result.map(|_| format!("{} bands", r.texture_bands))
}

// ------------------------------------------------------------------ loop films

const LOOP_PICTURE_MOVIE: &str = r#"
loop = true
fps = 30

[[shot]]
id = "a"
scene = "p.glsl"
duration = 1.0
uniforms.uK = { keys = [[0, 0.0], [1, 1.0]] }

[[shot]]
id = "b"
scene = "q.glsl"
duration = 1.0
transition = { type = "fx/crossfade.glsl", duration = 0.4 }
"#;

/// A loop film (48 frames) whose every frame differs: frame n of loop k must be frame n
/// of loop 0, by frame number exactly and by time (f32 times, wrapped in f64).
fn loop_picture(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            (
                "p.glsl",
                &scene(
                    "fragColor = vec4(fract(iTime * 0.37 + uv.x + uK), fract(float(iFrame) / 7.0 + uv.y), fract(iMovieTime * 0.53 + iShotTime * 0.11), 1.0);",
                ),
            ),
            (
                "q.glsl",
                &scene(
                    "fragColor = vec4(fract(iMovieTime * 0.71 + uv.y), fract(iTime * 1.3 + uv.x * uv.x), fract(float(iFrame) * 0.13 + iShotProgress), 1.0);",
                ),
            ),
        ],
    )?;
    let m = movie(dir, LOOP_PICTURE_MOVIE)?;
    if m.loop_frames != Some(48) {
        bail!(
            "the fixture should loop every 48 frames, not {:?}",
            m.loop_frames
        );
    }
    let mut r = renderer(gl, &m)?;
    r.set_readback(true);
    let mut grab = |info: FrameInfo| -> Result<Frame> {
        r.render(&m, W, H, &info, true)
            .ok_or_else(|| anyhow!("nothing rendered"))?;
        r.read_output().ok_or_else(|| anyhow!("no output"))
    };
    // 0, a shot, the transition, the last frame (just before the seam).
    let probes = [0u64, 7, 24, 47];
    let mut base = vec![];
    for &n in &probes {
        base.push(grab(FrameInfo::frame(&m, n))?);
    }
    if diff(&base[0], &base[3], 0).0 == 0 {
        bail!("the fixture's first and last frames are the same; it can't show a wrap");
    }
    for (i, &n) in probes.iter().enumerate() {
        for k in [1u64, 5, 1_000_000] {
            let f = grab(FrameInfo::frame(&m, n + k * 48))?;
            if diff(&f, &base[i], 0).0 != 0 {
                bail!(
                    "frame {} (loop {k}, frame {n}) differs from frame {n}",
                    n + k * 48
                );
            }
        }
        // By time, as `still --at` and the player pass it: f32 seconds, a few loops in.
        for k in [1u64, 2, 3] {
            let t = m.frame_time(n + k * 48);
            let f = grab(FrameInfo::at(&m, t))?;
            let (max, frac) = diff(&f, &base[i], 1);
            if max > 1 || frac > 0.0 {
                bail!(
                    "t = {t}s (loop {k}, frame {n}) differs from frame {n} by up to {max} \
                     ({:.2}% of pixels)",
                    frac * 100.0
                );
            }
        }
    }
    // The frame after the last is frame 0.
    let f = grab(FrameInfo::at(&m, m.frame_time(48)))?;
    if diff(&f, &base[0], 1).0 > 1 {
        bail!("the frame after the last isn't frame 0");
    }
    // The same film without `loop` doesn't wrap: t + period is the end of the film.
    let once = movie(dir, &LOOP_PICTURE_MOVIE.replace("loop = true", ""))?;
    let mut r = renderer(gl, &once)?;
    r.set_readback(true);
    r.render(
        &once,
        W,
        H,
        &FrameInfo::at(&once, once.frame_time(7 + 48)),
        true,
    )
    .ok_or_else(|| anyhow!("nothing rendered"))?;
    let f = r.read_output().ok_or_else(|| anyhow!("no output"))?;
    if diff(&f, &base[1], 1).0 <= 1 {
        bail!("without loop = true, t + period still looks like t: the check proves nothing");
    }
    Ok(format!(
        "{} frames × loops 1, 5, 10⁶ by frame number, 1–3 by time",
        probes.len()
    ))
}

/// Shots for the loop sound fixture: shot "a" (hard cut in), then "b" crossfading in.
fn loop_sound_shots(suffix: &str) -> String {
    format!(
        r#"
[[shot]]
id = "a{suffix}"
scene = "s.glsl"
duration = 0.5
sound = "tone.glsl"
uniforms.uPitch = 1.0

[[shot]]
id = "b{suffix}"
scene = "s.glsl"
duration = 0.5
sound = "tone.glsl"
uniforms.uPitch = 1.5
transition = {{ type = "fx/crossfade.glsl", duration = 0.2 }}
"#
    )
}

const LOOP_SOUND_HEADER: &str = r#"
fps = 30
[sound]
music = "music.glsl"
mix = "mix.glsl"
mix_lookback = 0.5
"#;

/// Sound across the seam: a 0.8 s loop film whose mix echoes music and shot sounds
/// by 300 ms (so the end of each loop rings into the next) and ducks with a look-ahead.
/// One looped period must (1) be the steady state of a long render of the same
/// material played three times without `loop` (so: no click at the wrap, and the echo
/// carries over), (2) step no more at the seam than anywhere else, (3) be the same
/// whatever the chunk size, and (4) stream (starting just before the seam, as the
/// player does when it plays across it) exactly as it renders blocking.
fn loop_sound(gl: &Gl, dir: &Path) -> Result<String> {
    write(
        dir,
        &[
            ("s.glsl", &scene("fragColor = vec4(0.0, 0.0, 0.0, 1.0);")),
            // Exactly periodic in samples: 192 samples = 250 Hz, 200 cycles per loop.
            (
                "music.glsl",
                "vec2 mainSound(int samp, float time) {\n  float s = 0.2 * sin(6.2831853 * float(samp % 192) / 192.0);\n  return vec2(s, s * 0.9);\n}\n",
            ),
            (
                "tone.glsl",
                "vec2 mainSound(int samp, float time) {\n  float s = 0.2 * sin(6.2831853 * 337.0 * uPitch * time + 0.7);\n  return vec2(s * 0.8, s);\n}\n",
            ),
            (
                "mix.glsl",
                "vec2 mainMix(int samp, float time) {\n  int ms = int(iSampleRate * 0.001);\n  vec2 echo = (music(samp - 300 * ms) + sfx(samp - 300 * ms)) * 0.4;\n  float duck = 1.0 - 0.3 * smoothstep(0.05, 0.3, sfxLevel(samp));\n  return (music(samp) + echo) * duck + sfx(samp);\n}\n",
            ),
        ],
    )?;
    let looped_toml = format!("loop = true\n{LOOP_SOUND_HEADER}{}", loop_sound_shots(""));
    let m = movie(dir, &looped_toml)?;
    let p = m.sound_samples(48000);
    if p != 38400 {
        bail!("the fixture's period should be 38400 samples, not {p}");
    }
    let looped = render_sound(gl, &m, crate::gfx::AUDIO_BLOCK)?; // one chunk
    if looped.len() != p * 2 {
        bail!("one period has {} samples, not {p}", looped.len() / 2);
    }
    if looped.iter().all(|x| x.abs() < 1e-4) {
        bail!("the fixture rendered silence");
    }
    let mut notes = vec![];

    // (1) The steady state of a long, unlooped render: its second period.
    let long_toml = format!(
        "{LOOP_SOUND_HEADER}{}{}{}",
        loop_sound_shots("1"),
        loop_sound_shots("2"),
        loop_sound_shots("3")
    );
    let long_dir = dir.join("long");
    std::fs::create_dir_all(&long_dir)?;
    for f in ["s.glsl", "music.glsl", "tone.glsl", "mix.glsl"] {
        std::fs::copy(dir.join(f), long_dir.join(f))?;
    }
    let long_m = movie(&long_dir, &long_toml)?;
    let long = render_sound(gl, &long_m, crate::gfx::AUDIO_BLOCK)?;
    if long.len() < 3 * p * 2 {
        bail!("the long render is too short ({} samples)", long.len() / 2);
    }
    let steady = &long[p * 2..p * 4];
    let (i, d) = looped
        .iter()
        .zip(steady)
        .map(|(a, b)| (a - b).abs())
        .enumerate()
        .fold(
            (0, 0f32),
            |acc, (i, d)| if d > acc.1 { (i, d) } else { acc },
        );
    // Shot sounds see f32 times a period later in the long film: tiny phase drift.
    if d > 2e-3 {
        bail!(
            "the looped period differs from the long render's steady state by {d} at {:.4}s \
             (the mix should read across the seam)",
            (i / 2) as f32 / 48000.0
        );
    }

    // (2) No click: the seam steps no more than the largest step inside the period,
    // and just as the long render steps there.
    let step = |a: &[f32], b: &[f32]| (a[0] - b[0]).abs().max((a[1] - b[1]).abs());
    let largest = looped
        .as_chunks::<2>()
        .0
        .windows(2)
        .map(|w| step(&w[0], &w[1]))
        .fold(0f32, f32::max);
    let seam = step(&looped[0..2], &looped[p * 2 - 2..p * 2]);
    let long_seam = step(&long[p * 2..p * 2 + 2], &long[p * 2 - 2..p * 2]);
    if seam > largest + 1e-4 || (seam - long_seam).abs() > 2e-3 {
        bail!(
            "the sound clicks at the loop seam: it steps by {seam} there (largest step elsewhere \
             {largest}, the long render steps by {long_seam})"
        );
    }

    // (3) Whatever the chunk size (one that doesn't divide the period, too).
    for chunk in [32768, 7919] {
        let parts = render_sound(gl, &m, chunk)?;
        let d = parts
            .iter()
            .zip(&looped)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        if parts.len() != looped.len() || d > 1e-5 {
            sound_mismatch(
                gl,
                &mut notes,
                format!("chunk {chunk}: {} samples, differs by {d}", parts.len() / 2),
            )?;
        }
    }

    // (4) Streamed, from the last chunk across the seam, as the player asks for it.
    let chunk = 7919;
    let blocking = render_sound(gl, &m, chunk)?;
    let mut r = Renderer::new(gl.clone())?;
    let mut st = Soundtrack::with_chunk(chunk);
    st.set_movie(&mut r, &m, 48000)
        .map_err(|e| anyhow!("{e:?}"))?;
    let n = st.total_chunks();
    let mut got: Vec<Option<Arc<Vec<f32>>>> = vec![None; n];
    let start = Instant::now();
    while got.iter().any(|c| c.is_none()) {
        if start.elapsed().as_secs() > 60 {
            bail!("streaming didn't finish in 60 s");
        }
        let wanted: Vec<usize> = (0..n)
            .map(|k| (n - 1 + k) % n)
            .filter(|&c| got[c].is_none())
            .collect();
        let deadline = Instant::now() + std::time::Duration::from_millis(4);
        for (c, d) in st
            .stream(&mut r, &wanted, 2, deadline)
            .map_err(|e| anyhow!(e))?
        {
            got[c] = Some(d);
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    if r.blocking_audio_reads != 0 {
        bail!(
            "streaming waited for the GPU {} time(s)",
            r.blocking_audio_reads
        );
    }
    let streamed: Vec<f32> = got.into_iter().flat_map(|c| c.unwrap().to_vec()).collect();
    if streamed.len() != blocking.len()
        || streamed
            .iter()
            .zip(&blocking)
            .any(|(a, b)| a.to_bits() != b.to_bits())
    {
        sound_mismatch(
            gl,
            &mut notes,
            "streamed loop sound differs from the blocking render".into(),
        )?;
    }
    notes.insert(
        0,
        format!("long render within {d:.1e}; seam step {seam:.4} (largest {largest:.4})"),
    );
    Ok(notes.join("; "))
}
