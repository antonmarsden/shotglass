//! `shotglass check --strict` from the command line: every warning check prints
//! fails it, whatever printed it, and "ok" comes only after the verdict. Runs with
//! `--no-gpu`, so it needs no OpenGL (and runs with SHOTGLASS_SKIP_GPU_TESTS).

use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// (success, stdout, stderr) of `shotglass check <film> --no-gpu [extra]`.
fn check(film: &Path, extra: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_shotglass"))
        .arg("check")
        .arg(film)
        .arg("--no-gpu")
        .args(extra)
        .output()
        .expect("couldn't run the shotglass binary");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    );
    print!("{stdout}");
    eprint!("{stderr}");
    (out.status.success(), stdout, stderr)
}

fn ends_ok(stdout: &str) -> bool {
    stdout.trim_end().ends_with("\nok")
}

#[test]
fn strict_passes_a_film_without_warnings() {
    let film = root().join("tests/films/layers");
    for extra in [&[][..], &["--strict"]] {
        let (ok, stdout, stderr) = check(&film, extra);
        assert!(ok && ends_ok(&stdout), "{extra:?}: {stderr}");
        assert!(!stdout.contains("warning:") && !stderr.contains("warning:"));
    }
}

#[test]
fn strict_fails_on_dropped_subtitle_styling() {
    let film = root().join("tests/films/glyphs");
    let (ok, stdout, _) = check(&film, &[]);
    assert!(ok && ends_ok(&stdout), "a warning alone mustn't fail check");
    assert!(
        stdout.contains("warning: text \"subs\": subs.srt: tags <i>"),
        "{stdout}"
    );
    let (ok, stdout, stderr) = check(&film, &["--strict"]);
    assert!(!ok, "check --strict passed with a warning");
    assert!(!ends_ok(&stdout), "\"ok\" printed before a strict failure");
    assert!(stderr.contains("1 warning(s), and --strict"), "{stderr}");
}

/// Another kind of warning (a font with no licence), so the count isn't only the
/// subtitles': two warnings, counted together.
#[test]
fn strict_counts_every_kind_of_warning() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cli-strict");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("fonts")).unwrap();
    std::fs::copy(
        root().join("tests/films/text/fonts/NotoSans-subset.ttf"),
        dir.join("fonts/sans.ttf"),
    )
    .unwrap();
    std::fs::write(dir.join("shotglass.toml"), "shotglass = \"0.1.8\"\n").unwrap();
    std::fs::write(
        dir.join("subs.srt"),
        "1\n00:00:00,000 --> 00:00:01,000\n<b>bold</b>\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("s.glsl"),
        "void mainImage(out vec4 c, in vec2 p) { c = texelFetch(iChannel0, ivec2(p), 0); }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("movie.toml"),
        "title = \"strict\"\nfps = 30\n\
         [fonts]\nsans = { file = \"fonts/sans.ttf\" }\n\
         [texts]\nsubs = { subtitles = \"subs.srt\", font = \"sans\", glyphs = true }\n\
         [[shot]]\nid = \"a\"\nscene = \"s.glsl\"\nduration = 2\nchannels = [\"subs\"]\n",
    )
    .unwrap();
    let (ok, stdout, _) = check(&dir, &[]);
    assert!(ok, "warnings alone mustn't fail check");
    assert!(stdout.contains("has no licence"), "{stdout}");
    let (ok, _, stderr) = check(&dir, &["--strict"]);
    assert!(!ok);
    assert!(stderr.contains("2 warning(s)"), "{stderr}");
}
