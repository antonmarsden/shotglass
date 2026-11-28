//! Golden regression tests: renders a contact sheet and the soundtrack of the demo film
//! (and contact sheets of tests/films/channels, the
//! iChannel textures film, tests/films/text, the text film, tests/films/layers and
//! tests/films/blends, the layer films, tests/films/footage, image sequences, tests/films/glyphs, glyph runs and
//! subtitles, tests/films/feedback, feedback buffers, tests/films/comp-feedback, buffers
//! in comps, tests/films/warm-loop, `loop = "warm"`, tests/films/layer-feedback, buffers
//! in the layer stack, and both
//! of tests/films/loop, a `loop = true` film) and compares them with reference PNGs and WAVs in tests/golden/, so "compiles but looks (or sounds) wrong" fails `cargo test` (and
//! CI, on software OpenGL).
//!
//! - On a mismatch the actual strip and a diff image (differences in magenta), or the
//!   actual soundtrack and the difference as a WAV, are written to target/tmp/golden/
//!   (CI uploads that folder). Sound failures name the first time that differs.
//! - To update the references on purpose, run `SHOTGLASS_BLESS=1 cargo test --test golden`
//!   (on Linux without a display: under `xvfb-run -a`) and commit the PNGs and WAVs
//!   with the change that caused them.
//! - Needs OpenGL like tests/gpu.rs; SHOTGLASS_SKIP_GPU_TESTS=1 skips it.
//! - Every film here is self-contained: the demo keeps copies of the kit files it uses
//!   in `movies/big-shadow/lib/`, so nothing is fetched and a kit change never moves a
//!   reference. The `shotglass new` template follows the kit's main branch and has no
//!   golden: the kit canary (.github/canary.sh) checks it.
//!
//! The references are rendered with Mesa's software OpenGL (llvmpipe), as in CI.
//!
//! **Local baseline on other renderers.** A real GPU (NVIDIA, AMD, Intel, Apple
//! silicon) draws too differently from llvmpipe for one tolerance to cover both, and
//! its look moves with driver updates, so its references aren't committed. Instead,
//! on a renderer that isn't in `CI_RENDERERS`, the tests compare against a local
//! baseline in `target/golden-local/<renderer>-<GL version>/`: the first run writes
//! it, later runs compare with it (same tolerances, same diff files), and
//! `SHOTGLASS_BLESS=1` rewrites it. It catches "different from what this GPU drew
//! before", fast; only CI (or llvmpipe locally, see CONTRIBUTING.md) checks the
//! committed references. `SHOTGLASS_GOLDEN=ci` or `=local` forces a mode.
//! The tolerance (`strip --compare`'s defaults, see `golden::Tolerance`) absorbs the
//! rounding differences of real GPUs; `wav --compare`'s (see `golden::SoundTolerance`)
//! does the same for sound. Soundtracks are rendered at 8 kHz to keep the references
//! small (under 1 MB). The selftest's fixture films aren't here: their checks already
//! compare exact pixel values against expected colours, and synthesise their sound in
//! several ways that must agree bit for bit.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Renderers the committed references are for (substrings of GL_RENDERER): CI's.
const CI_RENDERERS: &[&str] = &["llvmpipe", "Apple Software Renderer"];

const SIZE: &str = "320x180";
const COUNT: &str = "8";
const COLS: &str = "4";
const RATE: &str = "8000";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn out_dir() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("golden");
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn skip() -> bool {
    let skip = std::env::var_os("SHOTGLASS_SKIP_GPU_TESTS").is_some();
    if skip {
        println!("golden-image test skipped (SHOTGLASS_SKIP_GPU_TESTS is set)");
    }
    skip
}

fn shotglass(args: &[&std::ffi::OsStr]) -> std::process::Output {
    let out = Command::new(env!("CARGO_BIN_EXE_shotglass"))
        .args(args)
        .output()
        .expect("couldn't run the shotglass binary");
    print!("{}", String::from_utf8_lossy(&out.stdout));
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    out
}

/// Where the references are: the committed ones (CI's renderers) or this machine's
/// local baseline.
struct Baseline {
    dir: PathBuf,
    local: bool,
}

/// Renderer and GL version as a folder name.
fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '.' {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

fn baseline() -> &'static Baseline {
    static B: OnceLock<Baseline> = OnceLock::new();
    B.get_or_init(|| {
        let committed = root().join("tests").join("golden");
        let forced = std::env::var("SHOTGLASS_GOLDEN").ok();
        if forced.as_deref() == Some("ci") {
            return Baseline {
                dir: committed,
                local: false,
            };
        }
        let out = Command::new(env!("CARGO_BIN_EXE_shotglass"))
            .arg("renderer")
            .output()
            .expect("couldn't run the shotglass binary");
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let mut lines = text.lines();
        let renderer = lines.next().unwrap_or("").trim().to_string();
        let version = lines.next().unwrap_or("").trim().to_string();
        assert!(
            out.status.success() && !renderer.is_empty(),
            "couldn't ask OpenGL for its renderer: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let ci = CI_RENDERERS.iter().any(|r| renderer.contains(r));
        if ci && forced.as_deref() != Some("local") {
            return Baseline {
                dir: committed,
                local: false,
            };
        }
        let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let target = tmp.parent().unwrap_or(&tmp);
        let dir = target
            .join("golden-local")
            .join(slug(&format!("{renderer} {version}")));
        println!(
            "golden: {renderer} isn't CI's renderer, comparing against the local baseline \
             in {} (not the committed references)",
            dir.display()
        );
        Baseline { dir, local: true }
    })
}

fn reference(file: &str) -> PathBuf {
    baseline().dir.join(file)
}

/// `--bless` when asked to, or when a local baseline doesn't have this reference yet
/// (the first run on this GPU and driver writes it).
fn bless(args: &mut Vec<&std::ffi::OsStr>, reference: &Path) {
    let first = baseline().local && !reference.exists();
    if first {
        std::fs::create_dir_all(reference.parent().unwrap()).unwrap();
        println!(
            "golden: no local baseline at {} yet, writing it",
            reference.display()
        );
    }
    if first || std::env::var_os("SHOTGLASS_BLESS").is_some() {
        args.push("--bless".as_ref());
    }
}

/// How a failure names the reference, and what to do about an intended change.
fn hint(reference: &Path) -> String {
    if baseline().local {
        format!(
            "the local baseline {} (this GPU's earlier render; if the change is intended \
             or the driver changed, re-run with SHOTGLASS_BLESS=1)",
            reference.display()
        )
    } else {
        reference.display().to_string()
    }
}

/// `shotglass strip <movie> --compare tests/golden/<name>.png` (or `--bless`).
fn golden(name: &str, movie: &Path) {
    golden_frames(name, movie, &["--count", COUNT], &[]);
}

/// `golden` with these frames (`--count n` or `--at times`) and extra `strip` options
/// (a looser tolerance).
fn golden_frames(name: &str, movie: &Path, frames: &[&str], extra: &[&str]) {
    let reference = reference(&format!("{name}.png"));
    let actual = out_dir().join(format!("{name}.png"));
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "strip".as_ref(),
        movie.as_os_str(),
        "--size".as_ref(),
        SIZE.as_ref(),
        frames[0].as_ref(),
        frames[1].as_ref(),
        "--cols".as_ref(),
        COLS.as_ref(),
        "--out".as_ref(),
        actual.as_os_str(),
        "--compare".as_ref(),
        reference.as_os_str(),
    ];
    args.extend(extra.iter().map(std::ffi::OsStr::new));
    bless(&mut args, &reference);
    let out = shotglass(&args);
    assert!(
        out.status.success(),
        "{name}: the strip doesn't match {} (see the message above; \
         actual and diff images are in {})",
        hint(&reference),
        out_dir().display()
    );
}

/// `shotglass wav <movie> --compare tests/golden/<name>.wav` (or `--bless`).
fn golden_sound(name: &str, movie: &Path) {
    let reference = reference(&format!("{name}.wav"));
    let actual = out_dir().join(format!("{name}.wav"));
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "wav".as_ref(),
        movie.as_os_str(),
        "--rate".as_ref(),
        RATE.as_ref(),
        "--out".as_ref(),
        actual.as_os_str(),
        "--compare".as_ref(),
        reference.as_os_str(),
    ];
    bless(&mut args, &reference);
    let out = shotglass(&args);
    assert!(
        out.status.success(),
        "{name}: the soundtrack doesn't match {} (see the message above; \
         the actual soundtrack and the difference are in {})",
        hint(&reference),
        out_dir().display()
    );
}

#[test]
fn demo_film() {
    if skip() {
        return;
    }
    golden("big-shadow", &root().join("movies").join("big-shadow"));
}

/// `shotglass quality` on the demo (its quality.toml: an `@reference` metric and a
/// plain one): the heatmaps match tests/golden/quality/big-shadow/<metric>.png, and the
/// metrics stay within the demo's limits.
///
/// Its pixel budget is looser than the default 0.1%: `alias` is the difference of two
/// renders (1× and the 4× reference) magnified 4×, so a silhouette pixel where a
/// raymarch hits on one CPU and misses on another lights up fully. llvmpipe JITs for
/// the runner's CPU, and CI runners differ: one run of the same commit had 543 of
/// 483600 pixels (0.112%) off, mean 0.048/255, while a parallel run matched. A broken
/// metric or variant moves far more than 0.5%.
#[test]
fn demo_quality_heatmaps() {
    if skip() {
        return;
    }
    let movie = root().join("movies").join("big-shadow");
    let refs = reference("quality").join("big-shadow");
    let actual = out_dir().join("quality-big-shadow");
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "quality".as_ref(),
        movie.as_os_str(),
        "--size".as_ref(),
        SIZE.as_ref(),
        "--count".as_ref(),
        COUNT.as_ref(),
        "--out".as_ref(),
        actual.as_os_str(),
        "--compare".as_ref(),
        refs.as_os_str(),
        "--budget".as_ref(),
        "0.5%".as_ref(),
    ];
    bless(&mut args, &refs);
    let out = shotglass(&args);
    assert!(
        out.status.success(),
        "demo quality: a heatmap differs from {} or a metric failed (see the message \
         above; actual and diff images are in {})",
        hint(&refs),
        actual.display()
    );
}

/// iChannel textures: a small film with a checked-in 64×64 PNG and JPEG (mipmaps,
/// repeat, nearest, clamp, mirror, vflip, iChannelResolution, a frozen transition).
///
/// Its mean limit is looser than the default 0.25: how a renderer picks and blends
/// mipmap levels on the receding plane is up to it. The Apple Software Renderer (CI's
/// macOS job) is within 3/255 everywhere but averages 0.253/255 from llvmpipe. A broken
/// texture option (filter, wrap, vflip, a missing mipmap chain) moves many pixels by far
/// more than 8/255, which the per-pixel budget still catches.
#[test]
fn channels_film() {
    if skip() {
        return;
    }
    golden_frames(
        "channels",
        &root().join("tests").join("films").join("channels"),
        &["--count", COUNT],
        &["--max-mean", "0.4"],
    );
}

/// Text (#68): tests/films/text, a title's coverage and its distance field magnified
/// 4× with an outline, a wrapped centred paragraph and a right-to-left line on
/// iChannel6/7. The blocks are rasterised on the CPU, so the GPU only filters them.
#[test]
fn text_film() {
    if skip() {
        return;
    }
    golden_frames(
        "text",
        &root().join("tests").join("films").join("text"),
        &["--count", COUNT],
        &[],
    );
}

/// Layers (#69): tests/films/layers, a half-resolution layer over the first shot, a
/// title spanning a dissolve (steady while the shots mix underneath) with fades, and
/// a film-wide vignette that reads the picture below it (`@input`).
#[test]
fn layers_film() {
    if skip() {
        return;
    }
    golden_frames(
        "layers",
        &root().join("tests").join("films").join("layers"),
        &["--count", COUNT],
        &[],
    );
}

/// Layers phase 2 (#72): tests/films/blends, a bloom from a bright pass read back with
/// `@glow` and screen-blended, a multiply-blended texture with fades, and a
/// picture-in-picture of the picture track (`@picture`).
#[test]
fn blends_film() {
    if skip() {
        return;
    }
    golden_frames(
        "blends",
        &root().join("tests").join("films").join("blends"),
        &["--count", COUNT],
        &[],
    );
}

/// Layers phase 3 (#74): tests/films/comps, comp shots (one with a matted
/// picture-in-picture of a sized, looping comp), a comp read as a channel with a
/// time offset, and a layer matted by the luma of a comp.
#[test]
fn comps_film() {
    if skip() {
        return;
    }
    golden_frames(
        "comps",
        &root().join("tests").join("films").join("comps"),
        &["--count", COUNT],
        &[],
    );
}

/// Feedback buffers (#134): tests/films/feedback, set up as a film would (a background
/// plate, the element on transparent in its own buffer): a trail reset at the cut, an 8-frame
/// echo from a pure history buffer (reading the init state before the first frame) and
/// reaction-diffusion from an init pass with its buffer-local iFrame; the frames cross
/// the reset, and every fold is short (errors compound across stacks).
#[test]
fn feedback_film() {
    if skip() {
        return;
    }
    golden_frames(
        "feedback",
        &root().join("tests").join("films").join("feedback"),
        &["--at", "0.1,0.2,0.3,0.5,1.0,1.1,1.2,1.4"],
        &[],
    );
}

/// Buffers in comps (#138): tests/films/comp-feedback, a sized comp with a trail buffer
/// that folds over comp-local frames (reset at the comp's start and at 1 s of comp time,
/// its iFrame as a bar), shown as a layer half-transparent from 0.3 s and held past its
/// end. The frames cross the comp's reset and the hold; folds stay short.
#[test]
fn comp_feedback_film() {
    if skip() {
        return;
    }
    golden_frames(
        "comp-feedback",
        &root().join("tests").join("films").join("comp-feedback"),
        &["--at", "0.2,0.4,0.7,1.2,1.3,1.45,2.0,2.5"],
        &[],
    );
}

/// Buffers in the layer stack (#138): tests/films/layer-feedback, a buffer stepping
/// after a grade layer smears the composite so far (`@input`), and one stepping after a
/// title layer trails that layer's own picture (`@title`, unbound before the layer
/// starts), reset at 1 s; a layer above both shows them. Folds stay short.
#[test]
fn layer_feedback_film() {
    if skip() {
        return;
    }
    golden_frames(
        "layer-feedback",
        &root().join("tests").join("films").join("layer-feedback"),
        &["--at", "0.1,0.2,0.3,0.5,0.8,1.0,1.1,1.4"],
        &[],
    );
}

/// Warm loops (#138): tests/films/warm-loop, `loop = "warm"`: a trail that spans the seam
/// is already there at frame 0 (it follows the period's last frame), next to one that
/// starts at 0.2 s and is empty every period.
#[test]
fn warm_loop_film() {
    if skip() {
        return;
    }
    golden_frames(
        "warm-loop",
        &root().join("tests").join("films").join("warm-loop"),
        &["--at", "0,0.0333,0.1,0.2333,0.5,1.0,1.4,1.4667"],
        &[],
    );
}

/// Footage (#70): tests/films/footage, a 24 fps plate (a frame counter and a bar that
/// moves 2 px a frame) in a 30 fps film under a 12 fps matte on the film's clock;
/// re-timed (`in`, `speed`), started at a frame and black past its end, with
/// iChannelTime as a bar. A timing error shows as a wrong counter.
#[test]
fn footage_film() {
    if skip() {
        return;
    }
    golden_frames(
        "footage",
        &root().join("tests").join("films").join("footage"),
        &["--count", COUNT],
        &[],
    );
}

/// Glyph runs (#71): tests/films/glyphs, a two-line glyph run staggered by grapheme
/// and tinted by word, drawn from the MTSDF atlas through the glyph table; SRT
/// subtitles on a film-wide layer (a two-line cue, tags dropped, an entity decoded);
/// a clock drawn from a glyph set. Film-local GLSL reads the table, so a change to
/// the table format, the atlas or the layout shows here.
#[test]
fn glyphs_film() {
    if skip() {
        return;
    }
    golden_frames(
        "glyphs",
        &root().join("tests").join("films").join("glyphs"),
        &["--count", COUNT],
        &[],
    );
}

/// `loop = true`: a 2 s loop film. The strip's second row is two seconds (one period)
/// after the first, so it must show the same frames; past the end is the start again.
#[test]
fn loop_film() {
    if skip() {
        return;
    }
    golden_frames(
        "loop",
        &root().join("tests").join("films").join("loop"),
        &["--at", "0.1,0.6,1.0,1.9,2.1,2.6,3.0,3.9"],
        &[],
    );
}

/// One period of the loop film's sound: its mix echo rings over the seam, so the
/// start already carries the echo of the end.
#[test]
fn loop_soundtrack() {
    if skip() {
        return;
    }
    golden_sound("loop", &root().join("tests").join("films").join("loop"));
}

#[test]
fn demo_soundtrack() {
    if skip() {
        return;
    }
    golden_sound("big-shadow", &root().join("movies").join("big-shadow"));
}
