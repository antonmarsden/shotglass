//! Golden-image regression tests: renders a contact sheet of the demo film and of the
//! `shotglass new` template and compares each with a reference PNG in tests/golden/,
//! so "compiles but looks wrong" fails `cargo test` (and CI, on software OpenGL).
//!
//! - On a mismatch the actual strip and a diff image (differences in magenta) are
//!   written to target/tmp/golden/ (CI uploads that folder).
//! - To update the references on purpose, run `SHOTGLASS_BLESS=1 cargo test --test golden`
//!   (on Linux without a display: under `xvfb-run -a`) and commit the PNGs with the
//!   change that caused them.
//! - Needs OpenGL like tests/gpu.rs; SHOTGLASS_SKIP_GPU_TESTS=1 skips it. The films'
//!   libraries are fetched (pinned by shotglass.lock) on first use.
//!
//! The references are rendered with Mesa's software OpenGL (llvmpipe), as in CI.
//! The tolerance (`strip --compare`'s defaults, see `golden::Tolerance`) absorbs the
//! rounding differences of real GPUs. The selftest's fixture films aren't here: their
//! checks already compare exact pixel values against expected colours.

use std::path::{Path, PathBuf};
use std::process::Command;

const SIZE: &str = "320x180";
const COUNT: &str = "8";
const COLS: &str = "4";

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

/// `shotglass strip <movie> --compare tests/golden/<name>.png` (or `--bless`).
fn golden(name: &str, movie: &Path) {
    let reference = root()
        .join("tests")
        .join("golden")
        .join(format!("{name}.png"));
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
    if std::env::var_os("SHOTGLASS_BLESS").is_some() {
        args.push("--bless".as_ref());
    }
    let out = shotglass(&args);
    assert!(
        out.status.success(),
        "{name}: the strip doesn't match tests/golden/{name}.png (see the message above; \
         actual and diff images are in {})",
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

#[test]
fn new_template() {
    if skip() {
        return;
    }
    let dir = out_dir().join("template-film");
    let _ = std::fs::remove_dir_all(&dir);
    let out = shotglass(&["new".as_ref(), dir.as_os_str()]);
    assert!(out.status.success(), "shotglass new failed");
    // The template follows the kit's main branch; pin it to the demo's commit so the
    // picture only changes when this repo does.
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
    golden("template", &dir);
}
