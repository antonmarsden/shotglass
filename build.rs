//! Builds the pinned msdfgen core and, with `webm`, SVT-AV1 (#116).
//!
//! Where SVT-AV1 comes from, first match wins:
//! - `SVT_AV1_DIR`: an installed SVT-AV1 (`include/svt-av1/EbSvtAv1Enc.h`, `lib/`),
//!   linked statically when a static library is there;
//! - `SVT_AV1_SRC`: a source tree, built here with CMake;
//! - `vendor/SVT-AV1`: the pinned git submodule (`git submodule update --init`).
//!
//! SVT-AV1's x86 SIMD needs nasm ≥ 2.14 (`NASM` or on the PATH). Without it the
//! library is built C-only, which works but encodes several times slower, and the
//! build says so.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(feature = "webm")]
    webm::build();
    build_msdfgen();
}

fn build_msdfgen() {
    use std::path::PathBuf;

    println!("cargo:rerun-if-changed=src/msdf/msdfgen.cpp");
    let root = PathBuf::from("vendor/msdfgen");
    assert!(
        root.join("msdfgen.h").is_file(),
        "msdfgen source is missing. Run `git submodule update --init`."
    );
    println!("cargo:rerun-if-changed={}", root.join("core").display());
    println!(
        "cargo:rerun-if-changed={}",
        root.join("msdfgen.h").display()
    );
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++14")
        .include(&root)
        .define("MSDFGEN_PUBLIC", Some(""))
        .define("MSDFGEN_USE_CPP11", None)
        .file("src/msdf/msdfgen.cpp");
    // Sign correction stores -1 in a plain char scratch buffer. MSVC uses
    // signed char by default, while GCC and Clang vary by target.
    if !build.get_compiler().is_like_msvc() {
        build.flag("-fsigned-char");
    }
    // Keep atlas generation responsive in development builds. Release builds
    // continue to use the Cargo profile's optimization level.
    if std::env::var("OPT_LEVEL").as_deref() == Ok("0") {
        build.opt_level(2);
    }
    for file in std::fs::read_dir(root.join("core")).expect("read msdfgen core") {
        let path = file.expect("read msdfgen core file").path();
        if path.extension().is_some_and(|ext| ext == "cpp") {
            build.file(path);
        }
    }
    build.compile("shotglass_msdfgen");
}

#[cfg(feature = "webm")]
mod webm {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    pub fn build() {
        for v in ["SVT_AV1_DIR", "SVT_AV1_SRC", "NASM"] {
            println!("cargo:rerun-if-env-changed={v}");
        }
        println!("cargo:rerun-if-changed=src/webm/svt_shim.c");
        let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
        let mut kind = "static";
        let include = if let Some(dir) = std::env::var_os("SVT_AV1_DIR") {
            let dir = PathBuf::from(dir);
            println!(
                "cargo:rustc-link-search=native={}",
                dir.join("lib").display()
            );
            let static_lib = ["libSvtAv1Enc.a", "SvtAv1Enc.lib"]
                .iter()
                .any(|f| dir.join("lib").join(f).exists());
            kind = if static_lib { "static" } else { "dylib" };
            dir.join("include").join("svt-av1")
        } else {
            let src = std::env::var_os("SVT_AV1_SRC")
                .map(PathBuf::from)
                .unwrap_or_else(|| manifest.join("vendor").join("SVT-AV1"));
            if !src.join("CMakeLists.txt").exists() {
                panic!(
                    "\n\nThe `webm` feature builds SVT-AV1 from {}, which is empty.\n\
                     Run `git submodule update --init`, or set SVT_AV1_SRC (a source tree) or \
                     SVT_AV1_DIR (an installed SVT-AV1), or build without it: \
                     `cargo build --no-default-features --features strict-glsl`.\n\n",
                    src.display()
                );
            }
            // A submodule bump or a local edit rebuilds it (cargo scans the tree).
            println!("cargo:rerun-if-changed={}", src.display());
            build_svt(&src);
            src.join("Source").join("API")
        };
        cc::Build::new()
            .file("src/webm/svt_shim.c")
            .include(&include)
            .warnings(false)
            .compile("sg_svt_shim");
        // After the shim (cc emits its own directive), so the linker resolves the
        // shim's references from SvtAv1Enc.
        println!("cargo:rustc-link-lib={kind}=SvtAv1Enc");
        let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        if os == "linux" {
            println!("cargo:rustc-link-lib=dylib=pthread");
            println!("cargo:rustc-link-lib=dylib=m");
        }
    }

    fn nasm() -> Option<PathBuf> {
        let bin = std::env::var_os("NASM")
            .map(PathBuf::from)
            .unwrap_or_else(|| "nasm".into());
        let ok = Command::new(&bin)
            .arg("-v")
            .output()
            .is_ok_and(|o| o.status.success());
        ok.then_some(bin)
    }

    fn build_svt(src: &Path) {
        let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
        let mut cfg = cmake::Config::new(src);
        // SVT-AV1 unoptimised is unusably slow, whatever the Rust profile.
        cfg.profile("Release")
            .define("BUILD_SHARED_LIBS", "OFF")
            .define("BUILD_APPS", "OFF")
            .define("BUILD_TESTING", "OFF")
            .define("SVT_AV1_LTO", "OFF")
            .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5");
        // SVT-AV1 writes its binaries to `<source>/Bin/<config>/` by default, which
        // dirties the source tree (`cargo publish` refuses that): keep them in OUT_DIR.
        // The install step still copies the library to `<out>/lib`.
        let bin_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("svt-bin");
        cfg.define("CMAKE_OUTPUT_DIRECTORY", &bin_dir);
        // cmake-rs replaces CMAKE_<LANG>_FLAGS_RELEASE with cc's flags for the Rust
        // profile (no optimisation in dev builds, no NDEBUG), so ask for them here:
        // cl and gcc/clang all accept these spellings.
        for f in ["-O2", "-DNDEBUG"] {
            cfg.cflag(f).cxxflag(f);
        }
        if arch == "x86_64" || arch == "x86" {
            match nasm() {
                Some(bin) => {
                    cfg.define("CMAKE_ASM_NASM_COMPILER", bin);
                }
                None => {
                    println!(
                        "cargo:warning=nasm not found: building SVT-AV1 without SIMD, so \
                         `export --format webm` will be several times slower. Install nasm \
                         2.14 or newer (or set NASM) and rebuild."
                    );
                    cfg.define("COMPILE_C_ONLY", "ON");
                }
            }
        }
        let out = cfg.build();
        for lib in ["lib", "lib64"] {
            println!("cargo:rustc-link-search=native={}", out.join(lib).display());
        }
    }
}
