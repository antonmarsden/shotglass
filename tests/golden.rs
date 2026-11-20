//! Golden regression tests: renders a contact sheet and the soundtrack of the demo film
//! and of the `shotglass new` template and compares them with reference PNGs and WAVs
//! in tests/golden/, so "compiles but looks (or sounds) wrong" fails `cargo test` (and
//! CI, on software OpenGL).
//!
//! - On a mismatch the actual strip and a diff image (differences in magenta), or the
//!   actual soundtrack and the difference as a WAV, are written to target/tmp/golden/
//!   (CI uploads that folder). Sound failures name the first time that differs.
//! - To update the references on purpose, run `SHOTGLASS_BLESS=1 cargo test --test golden`
//!   (on Linux without a display: under `xvfb-run -a`) and commit the PNGs and WAVs
//!   with the change that caused them.
//! - Needs OpenGL like tests/gpu.rs; SHOTGLASS_SKIP_GPU_TESTS=1 skips it. The films'
//!   libraries are fetched (pinned by shotglass.lock) on first use.
//!
//! The references are rendered with Mesa's software OpenGL (llvmpipe), as in CI.
//! The tolerance (`strip --compare`'s defaults, see `golden::Tolerance`) absorbs the
//! rounding differences of real GPUs; `wav --compare`'s (see `golden::SoundTolerance`)
//! does the same for sound. Soundtracks are rendered at 8 kHz to keep the references
//! small (under 1 MB). The selftest's fixture films aren't here: their checks already
//! compare exact pixel values against expected colours, and synthesise their sound in
//! several ways that must agree bit for bit.

use std::path::{Path, PathBuf};
use std::process::Command;

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

fn reference(file: &str) -> PathBuf {
    root().join("tests").join("golden").join(file)
}

fn bless(args: &mut Vec<&std::ffi::OsStr>) {
    if std::env::var_os("SHOTGLASS_BLESS").is_some() {
        args.push("--bless".as_ref());
    }
}

/// `shotglass strip <movie> --compare tests/golden/<name>.png` (or `--bless`).
fn golden(name: &str, movie: &Path) {
    let reference = reference(&format!("{name}.png"));
    let actual = out_dir().join(format!("{name}.png"));
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "strip".as_ref(),
        movie.as_os_str(),
        "--size".as_ref(),
        SIZE.as_ref(),
        "--count".as_ref(),
        COUNT.as_ref(),
        "--cols".as_ref(),
        COLS.as_ref(),
        "--out".as_ref(),
        actual.as_os_str(),
        "--compare".as_ref(),
        reference.as_os_str(),
    ];
    bless(&mut args);
    let out = shotglass(&args);
    assert!(
        out.status.success(),
        "{name}: the strip doesn't match tests/golden/{name}.png (see the message above; \
         actual and diff images are in {})",
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
    bless(&mut args);
    let out = shotglass(&args);
    assert!(
        out.status.success(),
        "{name}: the soundtrack doesn't match tests/golden/{name}.wav (see the message above; \
         the actual soundtrack and the difference are in {})",
        out_dir().display()
    );
}

/// `shotglass new` into target/tmp/golden/<dir>, with the template's `transitions`
/// library pinned to the demo's commit (the template follows the kit's main branch), so
/// the result only changes when this repo does. Each test uses its own folder.
fn template_film(dir: &str) -> PathBuf {
    let dir = out_dir().join(dir);
    let _ = std::fs::remove_dir_all(&dir);
    let out = shotglass(&["new".as_ref(), dir.as_os_str()]);
    assert!(out.status.success(), "shotglass new failed");
    let lock = std::fs::read_to_string(root().join("movies/big-shadow/shotglass.lock")).unwrap();
    let mut blocks = lock.split("[[library]]");
    let mut pinned = blocks.next().unwrap().to_string();
    for b in blocks.filter(|b| b.contains("name = \"transitions\"")) {
        pinned += "[[library]]";
        pinned += b;
    }
    assert!(
        pinned.contains("transitions"),
        "no transitions pin in the demo's lock"
    );
    std::fs::write(dir.join("shotglass.lock"), pinned).unwrap();
    dir
}

#[test]
fn demo_film() {
    if skip() {
        return;
    }
    golden("big-shadow", &root().join("movies").join("big-shadow"));
}

#[test]
fn new_template() {
    if skip() {
        return;
    }
    golden("template", &template_film("template-film"));
}

#[test]
fn demo_soundtrack() {
    if skip() {
        return;
    }
    golden_sound("big-shadow", &root().join("movies").join("big-shadow"));
}

#[test]
fn template_soundtrack() {
    if skip() {
        return;
    }
    golden_sound("template", &template_film("template-film-sound"));
}
