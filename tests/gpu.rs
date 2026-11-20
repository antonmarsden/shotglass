//! Runs `shotglass selftest` (GPU and sound checks on fixture films) as part of
//! `cargo test`. It needs an OpenGL context: on Linux without a display, run
//! `xvfb-run -a cargo test`. Set SHOTGLASS_SKIP_GPU_TESTS=1 to skip.

use std::process::Command;

fn main() {
    if std::env::var_os("SHOTGLASS_SKIP_GPU_TESTS").is_some() {
        println!("gpu selftest skipped (SHOTGLASS_SKIP_GPU_TESTS is set)");
        return;
    }
    let status = Command::new(env!("CARGO_BIN_EXE_shotglass"))
        .arg("selftest")
        .status()
        .expect("couldn't run the shotglass binary");
    if !status.success() {
        eprintln!("gpu selftest failed");
        std::process::exit(1);
    }
}
