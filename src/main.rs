//! shotglass — compose short films from GLSL shaders.

mod anim;
mod app;
mod audio;
mod bundle;
mod export;
mod fonts;
mod footage;
mod gfx;
mod glsl_test;
mod golden;
mod headless;
mod library;
mod movie;
mod project;
mod quality;
mod selftest;
mod shader;
mod sound;
mod text;
mod texture;
mod validate;
mod webm;
mod yuv;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use gfx::{Frame, FrameInfo, Renderer};
use movie::{At, Movie};
use std::path::{Path, PathBuf};
use std::time::Instant;
use toodee::TooDeeOps;

#[derive(Parser)]
#[command(
    name = "shotglass",
    version,
    about = "Compose short films from GLSL shaders: shots, transitions and sound on one timeline."
)]
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Movie folder (or movie.toml) to play.
    movie: Option<PathBuf>,
}

#[derive(Subcommand)]
enum FontsCmd {
    /// Use Google Fonts families (github.com/google/fonts) in the film: each becomes a
    /// library in the project's shotglass.toml (`name = { google = "Family" }`), fetched
    /// and pinned in shotglass.lock like any library, and movie.toml gets a [fonts]
    /// entry per file ("<name/File.ttf>"). Needs git, like `fetch`.
    Add {
        /// Family names as fonts.google.com shows them ("Lobster", "Atkinson Hyperlegible").
        #[arg(required = true)]
        families: Vec<String>,
        /// The film (a folder with movie.toml).
        #[arg(long, default_value = ".")]
        film: PathBuf,
    },
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the player: scrub, loop, tweak shaders with live reload.
    Play {
        movie: PathBuf,
        /// Start at this time (seconds, a shot id, or id+seconds).
        #[arg(long)]
        at: Option<String>,
        /// Render scale: "auto" (adapts to the GPU) or a fixed fraction like 1.0 or 0.5.
        #[arg(long, default_value = "auto")]
        scale: String,
    },
    /// Render stills to PNG.
    Still {
        movie: PathBuf,
        /// Comma-separated times: seconds, a shot id (its middle), or id+seconds.
        #[arg(long, required = true)]
        at: String,
        #[arg(long, default_value = "1280x720")]
        size: String,
        #[arg(long, default_value = "stills")]
        out: PathBuf,
    },
    /// Render a contact sheet of evenly spaced frames to one PNG.
    Strip {
        movie: PathBuf,
        #[arg(long, default_value_t = 12)]
        count: usize,
        /// Explicit times instead of --count.
        #[arg(long)]
        at: Option<String>,
        /// Size of each frame.
        #[arg(long, default_value = "384x216")]
        size: String,
        #[arg(long, default_value_t = 4)]
        cols: usize,
        #[arg(long, default_value = "strip.png")]
        out: PathBuf,
        /// Compare against this reference PNG (a golden image) and fail if they differ
        /// beyond --tolerance and --budget; writes <out>.diff.png (differences in magenta).
        #[arg(long, value_name = "REF")]
        compare: Option<PathBuf>,
        /// With --compare: write this strip as the new reference instead of comparing.
        #[arg(long, requires = "compare")]
        bless: bool,
        /// With --compare: a pixel differs when R, G or B is off by more than this (0-255).
        #[arg(long, default_value_t = golden::Tolerance::default().per_pixel)]
        tolerance: u8,
        /// With --compare: how many pixels may differ, e.g. 0.1% or 0.001.
        #[arg(long, default_value = "0.1%")]
        budget: String,
        /// With --compare: the largest allowed average difference over all pixels (0-255).
        #[arg(long, default_value_t = golden::Tolerance::default().mean)]
        max_mean: f32,
    },
    /// Validate the movie and its GLSL (strictly, independent of this driver), compile
    /// every shader, synthesize the sound, and time each shot.
    Check {
        movie: PathBuf,
        /// Resolution to time frames at.
        #[arg(long, default_value = "1280x720")]
        size: String,
        /// Frames rendered per measurement.
        #[arg(long, default_value_t = 8)]
        frames: usize,
        /// Also require every shader to be valid GLSL ES 3.00 (WebGL 2), for the web player.
        #[arg(long)]
        web: bool,
        /// With --web, fail on browser compatibility warnings as well as GLSL errors.
        #[arg(long, requires = "web")]
        strict: bool,
        /// Only validate (movie.toml and the strict GLSL check); no GPU needed.
        #[arg(long)]
        no_gpu: bool,
        /// Also decode every frame of every image sequence (sizes, broken files).
        #[arg(long)]
        deep: bool,
    },
    /// Write the GLSL soundtrack to a WAV file.
    Wav {
        movie: PathBuf,
        #[arg(long, default_value = "sound.wav")]
        out: PathBuf,
        #[arg(long, default_value_t = 48000)]
        rate: u32,
        /// Write a silent WAV when the film has no music or shot sounds.
        #[arg(long)]
        allow_silence: bool,
        /// Compare against this reference WAV and fail if any 50 ms window of the
        /// difference is beyond --max-rms or --max-peak; writes <out>.diff.wav.
        #[arg(long, value_name = "REF")]
        compare: Option<PathBuf>,
        /// With --compare: write this soundtrack as the new reference instead of comparing.
        #[arg(long, requires = "compare")]
        bless: bool,
        /// With --compare: the largest allowed RMS of the difference in a window (full scale 1.0).
        #[arg(long, default_value_t = golden::SoundTolerance::default().rms)]
        max_rms: f32,
        /// With --compare: the largest allowed difference of a single sample (full scale 1.0).
        #[arg(long, default_value_t = golden::SoundTolerance::default().peak)]
        max_peak: f32,
    },
    /// Render the film to a video file (or a PNG sequence), frame- and sample-accurate.
    /// WebM (AV1 + Opus) is encoded by shotglass itself; the other formats need ffmpeg on
    /// the PATH (or SHOTGLASS_FFMPEG), except for --format png.
    Export {
        movie: PathBuf,
        /// Output file (.webm, .mkv) or folder (PNG sequence + sound.wav).
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value = "1920x1080")]
        size: String,
        /// webm (AV1 + Opus, built in), av1 (the same in .webm or .mkv), ffv1 (lossless
        /// with FLAC), png (frames + WAV), or ffmpeg (with --ffmpeg-args). Default: from
        /// the --out extension.
        #[arg(long)]
        format: Option<String>,
        /// Your own ffmpeg output arguments, e.g. "-c:v libx264 -crf 18 -c:a aac".
        #[arg(long, allow_hyphen_values = true)]
        ffmpeg_args: Option<String>,
        /// Audio sample rate.
        #[arg(long, default_value_t = 48000)]
        rate: u32,
        /// For a loop = true film: write this many periods, one after another. One period
        /// (the default) loops seamlessly itself, picture and sound, when the file loops.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..))]
        loops: u64,
        /// webm/av1: constant quality, 0 (best) to 63 (SVT-AV1 CRF). Default 30.
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=63), conflicts_with = "bitrate")]
        quality: Option<u8>,
        /// webm/av1: aim for this many kbit/s of video instead of a constant quality.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        bitrate: Option<u32>,
        /// webm/av1: encoder speed, 0 (slowest, smallest) to 13 (fastest). Default 6.
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=13))]
        speed: Option<u8>,
        /// webm/av1: bits per sample, 10 (default: no banding on gradients) or 8.
        #[arg(long, value_parser = ["8", "10"])]
        depth: Option<String>,
        /// webm/av1: film grain, 1 (light) to 50: denoised before encoding, synthesised
        /// again by the player. Default off.
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=50))]
        grain: Option<u8>,
    },
    /// Create a new movie folder from a template.
    New { dir: PathBuf },
    /// Download the project's libraries and pin them in shotglass.lock (keeps existing
    /// pins). Takes a film or a project root.
    Fetch { movie: PathBuf },
    /// Move libraries in the project's shotglass.lock to the newest version shotglass.toml
    /// allows. Takes a film or a project root; every film of the project follows.
    Update {
        movie: PathBuf,
        /// Library names (local names from shotglass.toml); all if none.
        names: Vec<String>,
    },
    /// List the libraries the film includes, with their licences (flags non-commercial
    /// ones), and the film's own fonts. A project root lists every film.
    Licenses { movie: PathBuf },
    /// Fonts for [fonts]: `shotglass fonts add "Lobster"` declares Google Fonts families
    /// as libraries, fetches and pins them, and adds their entries to movie.toml.
    Fonts {
        #[command(subcommand)]
        cmd: FontsCmd,
    },
    /// Write a self-contained copy of the film: its folder plus exactly the locked library
    /// files it includes, playable with no project, network, cache or overrides.
    Bundle {
        movie: PathBuf,
        /// Output folder (new or empty, outside the film folder).
        #[arg(long)]
        out: PathBuf,
    },
    /// Freeze a finished film: copy exactly the library files it includes into the film
    /// (shotglass-libs/), so project, kit and override changes can't change it.
    Freeze { movie: PathBuf },
    /// Undo freeze: the film follows its project's libraries again.
    Unfreeze { movie: PathBuf },
    /// Run GLSL unit and property tests (*.test.glsl) and quality metric tests (*.test.toml)
    /// on the GPU, with the libraries of each file's project. Takes a file or a folder.
    Test {
        /// A test file, or a folder to search for *.test.glsl and *.test.toml files.
        #[arg(default_value = ".")]
        target: PathBuf,
        /// Only run tests whose name contains this text.
        filter: Option<String>,
    },
    /// Measure the film's quality with the metrics in its quality.toml (library or film
    /// GLSL): sample frames as `strip` picks them, a report, quality.json and one heatmap
    /// sheet per metric. Fails when a metric is outside its limits or measures NaN/Inf.
    Quality {
        movie: PathBuf,
        /// How many evenly spaced frames (default: quality.toml's count, else 8).
        #[arg(long)]
        count: Option<usize>,
        /// Explicit times instead of --count.
        #[arg(long)]
        at: Option<String>,
        /// Frame size (default: quality.toml's size, else 384x216).
        #[arg(long)]
        size: Option<String>,
        /// Folder for quality.json and the heatmaps (a project gets one per film). A
        /// folder holding GLSL or a library.toml (a library's own `quality/`) is refused.
        #[arg(long, default_value = "quality-out")]
        out: PathBuf,
        /// Compare each metric's heatmap with <DIR>/<metric>.png (golden images, with
        /// `strip --compare`'s default tolerance); writes <metric>.diff.png on a mismatch.
        #[arg(long, value_name = "DIR")]
        compare: Option<PathBuf>,
        /// With --compare: write the heatmaps as the new references instead of comparing.
        #[arg(long, requires = "compare")]
        bless: bool,
        /// With --compare: how many pixels of each heatmap may differ, e.g. 0.1% or 0.001.
        #[arg(long, default_value = "0.1%", requires = "compare")]
        budget: String,
        /// After a passing run, move each limit in quality.toml to the measured value plus
        /// --margin (only ever stricter: a max goes down, a min goes up). The ratchet.
        #[arg(long)]
        tighten: bool,
        /// With --tighten: headroom over the measured value, e.g. 25% or 0.25. Leave room
        /// for other renderers: CI's llvmpipe and real GPUs can differ by over 10%.
        #[arg(long, default_value = "25%", requires = "tighten")]
        margin: String,
    },
    /// Check the renderer and soundtrack on this GPU and driver with built-in test films.
    Selftest {
        /// Only run checks whose name contains this text.
        filter: Option<String>,
    },
    /// Print the OpenGL renderer (GL_RENDERER) and version (GL_VERSION), one per line.
    /// The golden tests use them to pick CI's references or a local baseline.
    #[command(hide = true)]
    Renderer,
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let cmd = match (cli.cmd, cli.movie) {
        (Some(c), _) => c,
        (None, Some(m)) => Cmd::Play {
            movie: m,
            at: None,
            scale: "auto".into(),
        },
        (None, None) => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            return Ok(());
        }
    };
    match cmd {
        Cmd::Play { movie, at, scale } => {
            let movie = project::film(&movie)?;
            let m = load_movie(&movie)?;
            let start = match at {
                Some(a) => parse_times(&a, &m)?[0],
                None => 0.0,
            };
            let scale = match scale.as_str() {
                "auto" => None,
                s => {
                    let v: f32 = s
                        .parse()
                        .map_err(|_| anyhow!("--scale must be \"auto\" or a number like 1.0"))?;
                    if !(0.1..=1.0).contains(&v) {
                        bail!("--scale must be between 0.1 and 1.0");
                    }
                    Some(v)
                }
            };
            app::run(movie, start, scale)
        }
        Cmd::Still {
            movie,
            at,
            size,
            out,
        } => still(&movie, &at, &size, &out),
        Cmd::Strip {
            movie,
            count,
            at,
            size,
            cols,
            out,
            compare,
            bless,
            tolerance,
            budget,
            max_mean,
        } => {
            let golden = match compare {
                Some(reference) => Some(Golden {
                    reference,
                    bless,
                    tol: golden::Tolerance {
                        per_pixel: tolerance,
                        budget: golden::parse_budget(&budget)?,
                        mean: max_mean,
                    },
                }),
                None => None,
            };
            strip(&movie, count, at.as_deref(), &size, cols, &out, golden)
        }
        Cmd::Check {
            movie,
            size,
            frames,
            web,
            strict,
            no_gpu,
            deep,
        } => for_each_film(&movie, |f| {
            check(f, &size, frames, web, strict, no_gpu, deep)
        }),
        Cmd::Wav {
            movie,
            out,
            rate,
            allow_silence,
            compare,
            bless,
            max_rms,
            max_peak,
        } => {
            let golden = compare.map(|reference| GoldenSound {
                reference,
                bless,
                tol: golden::SoundTolerance {
                    rms: max_rms,
                    peak: max_peak,
                    ..Default::default()
                },
            });
            wav(&movie, &out, rate, allow_silence, golden)
        }
        Cmd::Export {
            movie,
            out,
            size,
            format,
            ffmpeg_args,
            rate,
            loops,
            quality,
            bitrate,
            speed,
            depth,
            grain,
        } => export(
            &movie,
            out,
            &size,
            format.as_deref(),
            ffmpeg_args.as_deref(),
            rate,
            loops,
            EncoderFlags {
                quality,
                bitrate,
                speed,
                depth: depth.map(|d| d.parse().unwrap()),
                grain,
            },
        ),
        Cmd::New { dir } => new_movie(&dir),
        Cmd::Fetch { movie } => fetch_libraries(
            &movie,
            library::FetchMode::Missing {
                skip: Default::default(),
            },
            true,
        ),
        Cmd::Update { movie, names } => {
            fetch_libraries(&movie, library::FetchMode::Update(names), true)
        }
        Cmd::Licenses { movie } => for_each_film(&movie, bundle::print_licences),
        Cmd::Fonts {
            cmd: FontsCmd::Add { families, film },
        } => fonts::run_add(&project::film(&film)?, &families),
        Cmd::Bundle { movie, out } => bundle::run_bundle(&project::film(&movie)?, &out),
        Cmd::Freeze { movie } => bundle::run_freeze(&project::film(&movie)?),
        Cmd::Unfreeze { movie } => bundle::run_unfreeze(&project::film(&movie)?),
        Cmd::Test { target, filter } => test_glsl(&target, filter),
        Cmd::Quality {
            movie,
            count,
            at,
            size,
            out,
            compare,
            bless,
            budget,
            tighten,
            margin,
        } => {
            let budget = golden::parse_budget(&budget).context("--budget")?;
            let margin = golden::parse_budget(&margin).context("--margin")? as f64;
            let tighten = tighten.then_some(margin);
            let single = matches!(project::target(&movie)?, project::Target::Film(_));
            for_each_film(&movie, |f| {
                let dir = if single {
                    out.clone()
                } else {
                    out.join(f.file_name().unwrap_or_default())
                };
                let golden = compare.as_ref().map(|c| {
                    if single {
                        c.clone()
                    } else {
                        c.join(f.file_name().unwrap_or_default())
                    }
                });
                quality_cmd(
                    f,
                    count,
                    at.as_deref(),
                    size.as_deref(),
                    &dir,
                    golden.as_deref().map(|g| (g, bless, budget)),
                    tighten,
                )
            })
        }
        Cmd::Selftest { filter } => selftest::run(filter),
        Cmd::Renderer => {
            let name = headless::with_gl(|gl| {
                use eframe::glow::HasContext;
                Ok(unsafe {
                    (
                        gl.get_parameter_string(eframe::glow::RENDERER),
                        gl.get_parameter_string(eframe::glow::VERSION),
                    )
                })
            })?;
            println!("{}\n{}", name.0, name.1);
            Ok(())
        }
    }
}

/// Run `f` on a film, or on every film of a project root (all of them, even after a
/// failure; fails if any did).
fn for_each_film(path: &Path, f: impl Fn(&Path) -> Result<()>) -> Result<()> {
    let p = match project::target(path)? {
        project::Target::Film(film) => return f(&film),
        project::Target::Project(p) => p,
    };
    let films = p.films()?;
    if films.is_empty() {
        bail!(
            "project {} has no films yet (folders with a movie.toml)",
            path.display()
        );
    }
    let mut failed = vec![];
    for film in &films {
        println!("== {}", film.display());
        if let Err(e) = f(film) {
            eprintln!("error: {e:#}");
            failed.push(film.display().to_string());
        }
        println!();
    }
    if !failed.is_empty() {
        bail!(
            "{} of {} films failed: {}",
            failed.len(),
            films.len(),
            failed.join(", ")
        );
    }
    println!("all {} films ok", films.len());
    Ok(())
}

/// `shotglass test`: find the test files, resolve each project's libraries (fetching
/// missing ones, as for films), then run them all in one GL context.
fn test_glsl(target: &Path, filter: Option<String>) -> Result<()> {
    let paths = glsl_test::find_files(target)?;
    if paths.is_empty() {
        bail!(
            "no test files (*.test.glsl, *.test.toml) at or under {}",
            target.display()
        );
    }
    let mut projects: std::collections::BTreeMap<PathBuf, Vec<library::Library>> =
        Default::default();
    let mut files = vec![];
    for path in paths {
        // A file in the current folder has the empty path as its parent: use ".".
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let root = project::find_root(&dir).unwrap_or(dir);
        if !projects.contains_key(&root) {
            let libs = project_libraries(&root)
                .with_context(|| format!("libraries for {}", path.display()))?;
            projects.insert(root.clone(), libs);
        }
        files.push(glsl_test::TestFile {
            display: rel_display(&path, &root),
            libs: projects[&root].clone(),
            path,
            root,
        });
    }
    let filter_text = filter.clone();
    let sum = headless::with_gl(move |gl| {
        use eframe::glow::HasContext;
        let renderer = unsafe { gl.get_parameter_string(eframe::glow::RENDERER) };
        let (metric, glsl): (Vec<_>, Vec<_>) = files
            .into_iter()
            .partition(|f| quality::testing::is_test_file(&f.path));
        let mut sum = glsl_test::run(&gl, &glsl, filter.as_deref(), &mut |l| println!("{l}"))?;
        let m = quality::testing::run(&gl, &metric, filter.as_deref(), &mut |l| println!("{l}"))?;
        sum.passed += m.passed;
        sum.failed += m.failed;
        sum.broken += m.broken;
        sum.samples += m.samples;
        Ok((sum, renderer))
    })?;
    let (sum, renderer) = sum;
    println!();
    let summary = format!(
        "{} passed, {} failed{} ({} samples) on {renderer}",
        sum.passed,
        sum.failed,
        if sum.broken > 0 {
            format!(", {} file(s) with errors", sum.broken)
        } else {
            String::new()
        },
        sum.samples
    );
    if sum.failed > 0 || sum.broken > 0 {
        bail!("{summary}");
    }
    if sum.passed == 0 {
        bail!(
            "no tests ran (no test_ functions{})",
            match &filter_text {
                Some(f) => format!(" whose name contains \"{f}\""),
                None => String::new(),
            }
        );
    }
    println!("{summary}");
    Ok(())
}

/// `path` relative to `root` (both made absolute first), with forward slashes.
fn rel_display(path: &Path, root: &Path) -> String {
    let abs = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let (p, r) = (abs(path), abs(root));
    p.strip_prefix(&r)
        .unwrap_or(&p)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The libraries of the project rooted at `root` (with a `shotglass.toml`, or none),
/// fetching missing ones first.
fn project_libraries(root: &Path) -> Result<Vec<library::Library>> {
    if !root.join(project::PROJECT_FILE).is_file() {
        return Ok(vec![]);
    }
    let p = project::Project::read(root)?;
    if p.specs.is_empty() {
        return Ok(vec![]);
    }
    let env = |k: &str| std::env::var(k).ok();
    let o = library::overrides(&p.root, &p.specs, &env)?;
    if p.specs.iter().any(|s| s.fetched()) {
        let skip = o.keys().cloned().collect();
        fetch_libraries(&p.root, library::FetchMode::Missing { skip }, false)?;
    }
    let lock = library::Lock::load(&p.root)?;
    library::resolve(&p.root, &p.specs, &o, lock.as_ref(), &library::cache_dir())
}

/// Load a film for a command, first fetching libraries that aren't locked or downloaded
/// yet (never moving an existing pin).
fn load_movie(path: &Path) -> Result<Movie> {
    let path = &project::film(path)?;
    let m = Movie::read(path)?;
    if m.library_specs.iter().any(|s| s.fetched()) {
        let env = |k: &str| std::env::var(k).ok();
        let skip = library::overrides(&m.project, &m.library_specs, &env)?
            .into_keys()
            .collect();
        fetch_libraries(&m.project, library::FetchMode::Missing { skip }, false)?;
    }
    let m = Movie::load(path)?;
    for w in &m.warnings {
        eprintln!("warning: {w}");
    }
    Ok(m)
}

/// `fetch`, `update`, and the automatic fetch before other commands (`verbose` = false:
/// only says something when it downloads or changes the lock). `path` is a film or a
/// project root; the lock is the project's.
fn fetch_libraries(path: &Path, mode: library::FetchMode, verbose: bool) -> Result<()> {
    let p = match project::target(path)? {
        project::Target::Film(film) => project::Project::for_film(&film)?,
        project::Target::Project(p) => p,
    };
    if p.frozen && matches!(mode, library::FetchMode::Update(_)) {
        bail!(
            "{} is frozen (its libraries are copies in shotglass-libs/ and never change); \
             `shotglass unfreeze` it first",
            p.root.display()
        );
    }
    if !p.specs.iter().any(|s| s.fetched()) {
        if verbose {
            println!(
                "{} has no libraries with a source; nothing to fetch",
                p.root.display()
            );
        }
        return Ok(());
    }
    let old = library::Lock::load(&p.root)?;
    let (lock, log) = library::fetch(
        &p.specs,
        old.as_ref(),
        &library::Git,
        &library::cache_dir(),
        &mode,
    )?;
    let changed = old.as_ref() != Some(&lock);
    if verbose || changed || log.iter().any(|l| l.contains("downloaded")) {
        for l in &log {
            println!("  {l}");
        }
    }
    if changed {
        let f = p.root.join(library::LOCK_FILE);
        std::fs::write(&f, lock.to_text()).with_context(|| format!("writing {}", f.display()))?;
        println!("wrote {}", f.display());
    } else if verbose {
        println!("{} is up to date", library::LOCK_FILE);
    }
    Ok(())
}

pub(crate) fn parse_size(s: &str) -> Result<(i32, i32)> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .ok_or_else(|| anyhow!("size must look like 1280x720"))?;
    let (w, h): (i32, i32) = (w.trim().parse()?, h.trim().parse()?);
    if w < 1 || h < 1 || w > 8192 || h > 8192 {
        bail!("size out of range");
    }
    Ok((w, h))
}

/// "3.5", "moon" (middle of the shot), "moon+1.5" (1.5 s into the shot).
fn parse_times(s: &str, m: &Movie) -> Result<Vec<f32>> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(|tok| {
            if let Ok(t) = tok.parse::<f32>() {
                return Ok(t);
            }
            let (id, off) = match tok.split_once('+') {
                Some((id, o)) => (
                    id,
                    Some(
                        o.parse::<f32>()
                            .with_context(|| format!("bad offset in \"{tok}\""))?,
                    ),
                ),
                None => (tok, None),
            };
            let shot = m
                .shot_by_id(id)
                .ok_or_else(|| anyhow!("\"{tok}\" is not a time or a shot id"))?;
            Ok(shot.start + off.unwrap_or(shot.duration * 0.5))
        })
        .collect()
}

fn load_renderer(gl: std::sync::Arc<eframe::glow::Context>, m: &Movie) -> Result<Renderer> {
    let mut r = Renderer::new(gl)?;
    if let Err(errors) = r.load_all(m) {
        let mut s = String::new();
        for e in errors {
            s += &e.to_string();
        }
        bail!("shader errors:\n{s}");
    }
    Ok(r)
}

fn frame_info(m: &Movie, t: f32) -> FrameInfo {
    FrameInfo::at(m, t)
}

fn still(movie: &Path, at: &str, size: &str, out: &Path) -> Result<()> {
    let m = load_movie(movie)?;
    let times = parse_times(at, &m)?;
    let (w, h) = parse_size(size)?;
    std::fs::create_dir_all(out)?;
    let out = out.to_path_buf();
    headless::with_gl(move |gl| {
        let mut r = load_renderer(gl, &m)?;
        r.set_readback(true);
        for t in times {
            r.render(&m, w, h, &frame_info(&m, t), true)
                .ok_or_else(|| anyhow!("nothing rendered"))?;
            if let Some(e) = r.footage_failure() {
                bail!("{t:.3}s: {e}");
            }
            let frame = r.read_output().ok_or_else(|| anyhow!("nothing rendered"))?;
            let path = out.join(format!("t{:08.3}.png", t));
            save_png(&path, &frame)?;
            println!("{}  {}", path.display(), describe(&m, t));
        }
        Ok(())
    })
}

/// A contact sheet's layout: `count` frames of w×h in rows of `cols`, 4 px apart on
/// dark grey (`strip`, and `quality`'s heatmaps).
pub(crate) struct Sheet {
    cols: usize,
    rows: usize,
    w: usize,
    h: usize,
}

impl Sheet {
    const GAP: usize = 4;

    pub(crate) fn new(count: usize, cols: usize, w: usize, h: usize) -> Sheet {
        let cols = cols.clamp(1, count.max(1));
        Sheet {
            cols,
            rows: count.div_ceil(cols).max(1),
            w,
            h,
        }
    }

    pub(crate) fn blank(&self) -> Frame {
        Frame::init(
            self.cols * (self.w + Self::GAP) + Self::GAP,
            self.rows * (self.h + Self::GAP) + Self::GAP,
            [24, 24, 24, 255],
        )
    }

    /// Top-left corner of frame `i`.
    pub(crate) fn origin(&self, i: usize) -> (usize, usize) {
        (
            (i % self.cols) * (self.w + Self::GAP) + Self::GAP,
            (i / self.cols) * (self.h + Self::GAP) + Self::GAP,
        )
    }
}

/// `strip --compare`: the reference picture and how close the strip must be.
struct Golden {
    reference: PathBuf,
    bless: bool,
    tol: golden::Tolerance,
}

fn strip(
    movie: &Path,
    count: usize,
    at: Option<&str>,
    size: &str,
    cols: usize,
    out: &Path,
    golden: Option<Golden>,
) -> Result<()> {
    let m = load_movie(movie)?;
    let times = match at {
        Some(a) => parse_times(a, &m)?,
        None => {
            let n = count.max(1);
            (0..n)
                .map(|i| m.duration * (i as f32 + 0.5) / n as f32)
                .collect()
        }
    };
    let (w, h) = parse_size(size)?;
    let layout = Sheet::new(times.len(), cols, w as usize, h as usize);
    let out = out.to_path_buf();
    headless::with_gl(move |gl| {
        let mut r = load_renderer(gl, &m)?;
        r.set_readback(true);
        let mut sheet = layout.blank();
        for (i, t) in times.iter().enumerate() {
            r.render(&m, w, h, &frame_info(&m, *t), true)
                .ok_or_else(|| anyhow!("nothing rendered"))?;
            if let Some(e) = r.footage_failure() {
                bail!("{t:.3}s: {e}");
            }
            r.read_output_into(&mut sheet, layout.origin(i));
            println!("#{i:<3} {:>7.2}s  {}", t, describe(&m, *t));
        }
        save_png(&out, &sheet)?;
        println!("wrote {}", out.display());
        match golden {
            Some(g) => compare_golden(&sheet, &out, &g),
            None => Ok(()),
        }
    })
}

/// `quality --out` writes reports, never into a folder of sources: a library's
/// `quality/` (metric GLSL, `library.toml`) is a likely accident.
fn refuse_source_folder(out: &Path) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(out) else {
        return Ok(()); // not there yet
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name == "library.toml" || name.ends_with(".glsl") {
            bail!(
                "--out {}: that folder holds sources ({name}); write the report elsewhere                  (the default is quality-out/)",
                out.display()
            );
        }
    }
    Ok(())
}

fn quality_cmd(
    movie: &Path,
    count: Option<usize>,
    at: Option<&str>,
    size: Option<&str>,
    out: &Path,
    golden: Option<(&Path, bool, f32)>,
    tighten: Option<f64>,
) -> Result<()> {
    let m = load_movie(movie)?;
    let golden = golden.map(|(d, b, budget)| (d.to_path_buf(), b, budget));
    let mut cfg = quality::Config::of_film(&m)?.ok_or_else(|| {
        anyhow!(
            "{} has no {} (metrics to run: see docs/testing-and-quality.md, \"Quality metrics\")",
            m.root.display(),
            quality::QUALITY_FILE
        )
    })?;
    if let Some(s) = size {
        cfg.size = parse_size(s)?;
    }
    // --at, then --count, then quality.toml's at, then its count.
    let toml_at = cfg
        .at
        .as_ref()
        .filter(|_| count.is_none())
        .map(|a| a.join(","));
    let times = match at.or(toml_at.as_deref()) {
        Some(a) => parse_times(a, &m).with_context(|| format!("sample times \"{a}\""))?,
        None => {
            let n = count.unwrap_or(cfg.count).max(1);
            (0..n)
                .map(|i| m.duration * (i as f32 + 0.5) / n as f32)
                .collect()
        }
    };
    refuse_source_folder(out)?;
    let out = out.to_path_buf();
    headless::with_gl(move |gl| {
        let report = quality::run(gl, &m, &cfg, &times, true)?;
        report.print();
        report.write(&out)?;
        println!("wrote {}", out.join("quality.json").display());
        // Heatmaps against golden images first: a limit failure shouldn't hide them.
        let mut mismatched = vec![];
        if let Some((dir, bless, budget)) = &golden {
            for x in &report.metrics {
                let Some(heat) = &x.heatmap else { continue };
                let g = Golden {
                    reference: dir.join(format!("{}.png", x.name)),
                    bless: *bless,
                    tol: golden::Tolerance {
                        budget: *budget,
                        ..Default::default()
                    },
                };
                if let Err(e) = compare_golden(heat, &out.join(format!("{}.png", x.name)), &g) {
                    eprintln!("error: {e:#}");
                    mismatched.push(x.name.as_str());
                }
            }
        }
        if !mismatched.is_empty() {
            bail!(
                "heatmaps differ from the references: {}",
                mismatched.join(", ")
            );
        }
        let failed = report.failed();
        if !failed.is_empty() {
            bail!("quality: {} failed", failed.join(", "));
        }
        if let Some(margin) = tighten {
            let path = m.root.join(quality::QUALITY_FILE);
            let text = std::fs::read_to_string(&path)?;
            let sound = |s| report.sound.as_ref().and_then(|x| x.value(s));
            let (new, changes) = quality::tighten(
                &text,
                margin,
                quality::SOUND_MARGIN_DB,
                |k, i, s| report.value(k, i, s),
                sound,
            )?;
            if changes.is_empty() {
                println!("tighten: every limit is already within the measured values + margin");
            } else {
                std::fs::write(&path, new)?;
                for c in &changes {
                    println!("tighten: {c}");
                }
                println!("wrote {}", path.display());
            }
        }
        Ok(())
    })
}

/// Every shader the film compiles plus its quality metrics, for the strict check.
fn strict_sources(m: &Movie, d: shader::Dialect) -> Result<Vec<validate::Source>> {
    let mut s = validate::movie_sources(m, d);
    s.extend(quality::sources(m, d)?);
    Ok(s)
}

fn compare_golden(sheet: &Frame, out: &Path, g: &Golden) -> Result<()> {
    if g.bless {
        if let Some(dir) = g.reference.parent() {
            std::fs::create_dir_all(dir)?;
        }
        save_png(&g.reference, sheet)?;
        println!("blessed: wrote the new reference {}", g.reference.display());
        return Ok(());
    }
    if !g.reference.exists() {
        bail!(
            "no reference at {} (add --bless to make this strip the reference)",
            g.reference.display()
        );
    }
    let reference = golden::load_png(&g.reference)?;
    let c = golden::compare(sheet, &reference, g.tol.per_pixel)
        .with_context(|| format!("comparing with {}", g.reference.display()))?;
    let diff_path = out.with_extension("diff.png");
    if c.passes(g.tol) {
        let _ = std::fs::remove_file(&diff_path); // don't leave a stale one behind
        println!("matches {}: {}", g.reference.display(), c.describe(g.tol));
        return Ok(());
    }
    save_png(&diff_path, &c.diff)?;
    bail!(
        "the strip doesn't match {}: {}\n  actual: {}\n  diff:   {} (differences in magenta)\n  \
         if the change is intended, re-run with --bless and commit the new reference",
        g.reference.display(),
        c.describe(g.tol),
        out.display(),
        diff_path.display()
    )
}

pub(crate) fn save_png(path: &Path, frame: &Frame) -> Result<()> {
    image::save_buffer(
        path,
        frame.data().as_flattened(),
        frame.num_cols() as u32,
        frame.num_rows() as u32,
        image::ColorType::Rgba8,
    )?;
    Ok(())
}

fn describe(m: &Movie, t: f32) -> String {
    match m.at(m.wrap(t)) {
        At::Shot { shot, local } => format!("{} +{local:.2}s", m.shots[shot].id),
        At::Transition {
            from, to, progress, ..
        } => format!(
            "{} → {} ({} {:.0}%)",
            m.shots[from].id,
            m.shots[to].id,
            m.shots[to].transition_in.as_ref().unwrap().kind,
            progress * 100.0
        ),
    }
}

/// The driver-independent part of `check`: every shader through the reference front
/// end at the film's desktop dialect, and as GLSL ES 3.00 (errors with `web`,
/// warnings otherwise).
fn strict_check(m: &Movie, web: bool, strict: bool) -> Result<()> {
    use shader::Dialect;
    if !validate::AVAILABLE {
        if web {
            bail!(
                "--web needs the strict GLSL check, but this build doesn't have it (strict-glsl feature)"
            );
        }
        println!("strict GLSL check: skipped (built without the strict-glsl feature)");
        return Ok(());
    }
    let report = |errors: &[gfx::ShaderError]| {
        for e in errors {
            eprint!("{e}");
        }
    };
    let opt_in = m.glsl.filter(|v| *v != Dialect::BASELINE.version);
    if let Some(v) = opt_in {
        println!(
            "warning: glsl = \"{v}\" narrows where this film runs: it needs an OpenGL {}.{} \
             driver (macOS stops at 4.1, Windows on ARM at 3.3) and can't run in a browser",
            v / 100,
            v % 100 / 10
        );
    }
    let desktop = Dialect::desktop(m.glsl);
    let sources = strict_sources(m, desktop)?;
    let errors = validate::validate_sources(&sources);
    if !errors.is_empty() {
        report(&errors);
        bail!(
            "{} shader(s) aren't valid {desktop} (strict check; your driver may be more lenient)",
            errors.len()
        );
    }
    println!("strict GLSL: {} shaders valid as {desktop}", sources.len());
    match opt_in {
        Some(v) if web => bail!(
            "--web: glsl = \"{v}\" films can't run in a browser (GLSL ES 3.00); remove the glsl line"
        ),
        Some(_) => {}
        None => {
            let (errors, warnings) =
                validate::validate_sources_with_warnings(&strict_sources(m, Dialect::WEB)?);
            report(&warnings);
            if errors.is_empty() {
                println!("strict GLSL: valid as {} (web)", Dialect::WEB);
            } else if web {
                report(&errors);
                bail!("{} shader(s) aren't valid {}", errors.len(), Dialect::WEB);
            } else {
                println!(
                    "warning: {} shader(s) aren't valid {}, so they won't run in a browser \
                     (`check --web` makes this an error):",
                    errors.len(),
                    Dialect::WEB
                );
                report(&errors);
            }
            if strict && !warnings.is_empty() {
                bail!(
                    "{} shader(s) have browser compatibility warnings",
                    warnings.len()
                );
            }
        }
    }
    Ok(())
}

/// The texture part of `check` (no GPU needed): decode every texture and lay out every
/// text (and each sequence's first frame, or with `deep` every frame), check the limits
/// and each shot's memory, and point out channels a scene reads but nothing is bound to.
fn check_textures(m: &Movie, web: bool, deep: bool) -> Result<()> {
    if let Some(w) = m.contract_warning() {
        println!("warning: {w}");
    }
    // (source, size, GPU bytes it needs at once)
    let mut sizes: Vec<(&movie::Source, (usize, usize), usize)> = vec![];
    let mut web_footage_bytes = 0u64;
    let mut failed = 0;
    let flags = |c: &movie::Channel| match (c.srgb, c.vflip) {
        (true, true) => ", srgb",
        (true, false) => ", srgb, vflip = false",
        (false, false) => ", vflip = false",
        (false, true) => "",
    };
    for c in m.textures() {
        let mut bytes = None;
        let size = match &c.source {
            movie::Source::Input
            | movie::Source::Picture
            | movie::Source::Layer { .. }
            | movie::Source::Comp { .. } => continue,
            movie::Source::Frames(seq) => {
                let label = &seq.pattern.label;
                let Some(scan) = &seq.scan else {
                    eprintln!("{label}: not scanned");
                    failed += 1;
                    continue;
                };
                let (num, den) = seq.fps.unwrap_or((m.fps_num, m.fps_den));
                let fps = if den == 1 {
                    format!("{num}")
                } else {
                    format!("{num}/{den}")
                };
                // The first frame always; every frame with --deep.
                let first = texture::load(&scan.path(0), label, c.vflip);
                let problems = match (&first, deep) {
                    (Err(e), _) => vec![e.clone()],
                    (Ok(_), true) => footage::check_frames(scan, label, 10),
                    (Ok(_), false) => vec![],
                };
                for p in &problems {
                    eprintln!("{p}");
                }
                if web {
                    web_footage_bytes += (0..scan.count)
                        .filter_map(|k| std::fs::metadata(scan.path(k)).ok())
                        .map(|md| md.len())
                        .sum::<u64>();
                }
                match (first, problems.is_empty()) {
                    (Ok(img), true) => {
                        let (w, h) = (img.width, img.height);
                        let b = texture::frame_bytes(w, h, c.filter == texture::Filter::Mipmap);
                        println!(
                            "frames {label}: {} frames ({}), {w}×{h} at {fps} fps, {:.2} s, {} \
                             per frame on the GPU{}{}",
                            scan.count,
                            scan.range(),
                            scan.count as f64 * den as f64 / num as f64,
                            texture::describe_bytes(b),
                            flags(c),
                            if deep { ", every frame decoded" } else { "" }
                        );
                        for warning in &img.warnings {
                            println!("warning: {warning}");
                        }
                        if w * h > 1920 * 1080 {
                            println!(
                                "warning: {label}: {w}×{h} frames are over 1920×1080; footage \
                                 this large may not play in real time (and is heavy for the web)"
                            );
                        }
                        bytes = Some(footage::MIN_FRAMES * b);
                        Some((w, h))
                    }
                    _ => None,
                }
            }
            movie::Source::Image(f) => match texture::load(&f.file, &f.label, c.vflip) {
                Ok(img) => {
                    let (w, h) = (img.width, img.height);
                    println!(
                        "texture {}: {w}×{h}, {} on the GPU{}",
                        f.label,
                        texture::describe_bytes(texture::gpu_bytes(w, h)),
                        flags(c)
                    );
                    for warning in &img.warnings {
                        println!("warning: {warning}");
                    }
                    Some((w, h))
                }
                Err(e) => {
                    eprintln!("{e}");
                    None
                }
            },
            movie::Source::Text(t) => match t.layout() {
                Ok((l, _)) => {
                    println!(
                        "text \"{}\": {}×{}, {} line{}, font{} {}, {} on the GPU{}",
                        t.name,
                        l.width,
                        l.height,
                        l.lines,
                        if l.lines == 1 { "" } else { "s" },
                        if t.fonts.len() == 1 { "" } else { "s" },
                        t.fonts
                            .iter()
                            .map(|f| f.name.as_str())
                            .collect::<Vec<_>>()
                            .join(" → "),
                        texture::describe_bytes(texture::gpu_bytes(l.width, l.height)),
                        flags(c)
                    );
                    for warning in &l.warnings {
                        println!("warning: text \"{}\": {warning}", t.name);
                    }
                    for f in t.fonts.iter().filter(|f| f.file.library.is_none()) {
                        if f.licence.is_none() {
                            println!(
                                "warning: font \"{}\" ({}) has no licence = \"…\"; say which \
                                 (e.g. \"OFL-1.1\") so `shotglass licenses` can list it",
                                f.name, f.file.label
                            );
                        }
                    }
                    Some((l.width, l.height))
                }
                Err(e) => {
                    eprintln!("{e:#}");
                    None
                }
            },
        };
        let Some((w, h)) = size else {
            failed += 1;
            continue;
        };
        if w.max(h) > texture::WEB_MAX_SIZE {
            let max = texture::WEB_MAX_SIZE;
            println!(
                "warning: {}: {w}×{h} is over {max}×{max}, the most some phones' browsers \
                 support{}",
                c.label(),
                if web {
                    ""
                } else {
                    " (matters for the web player)"
                }
            );
        }
        sizes.push((&c.source, (w, h), bytes.unwrap_or(texture::gpu_bytes(w, h))));
    }
    if failed > 0 {
        bail!("{failed} texture(s) can't be used");
    }
    if web_footage_bytes > 200 << 20 {
        println!(
            "warning: the film's image sequences are {} on disk; the web player preloads \
             footage, so keep it under about 200 MB (smaller frames, JPEG)",
            texture::describe_bytes(web_footage_bytes as usize)
        );
    }
    footage_warnings(m);
    let mut largest: Option<(usize, &str)> = None;
    let cues = m.cue_defines();
    for s in &m.shots {
        let mut sources: Vec<&movie::Source> =
            s.channels.iter().flatten().map(|c| &c.source).collect();
        sources.dedup();
        let mut counted: Vec<&movie::Source> = vec![];
        let mut total = 0;
        for src in sources {
            if counted.contains(&src) {
                continue;
            }
            counted.push(src);
            if let Some((_, _, b)) = sizes.iter().find(|(s, _, _)| *s == src) {
                total += b;
            }
        }
        if total > 0 && largest.is_none_or(|(b, _)| total > b) {
            largest = Some((total, &s.id));
        }
        if s.comp.is_some() {
            continue;
        }
        if let Ok(src) = gfx::scene_source(shader::Dialect::BASELINE, s, &cues, m) {
            for (n, reads) in gfx::channels_read(&src).iter().enumerate() {
                if *reads && s.channels[n].is_none() {
                    println!(
                        "warning: shot \"{}\" ({}): reads iChannel{n}, but channels has nothing \
                         at {n} (it reads black)",
                        s.id,
                        s.scene
                            .strip_prefix(&m.root)
                            .unwrap_or(&s.scene)
                            .to_string_lossy()
                            .replace('\\', "/")
                    );
                }
            }
        }
    }
    if let Some((b, id)) = largest {
        println!(
            "textures: {} in all; the most one shot needs is {} (shot \"{id}\"; budget {})",
            sizes.len(),
            texture::describe_bytes(b),
            texture::describe_bytes(texture::BUDGET)
        );
    }
    Ok(())
}

/// `check`'s footage warnings: a shot or layer that runs past the end of its sequence
/// (`end = "hold"` freezes the last frame), and a loop film's `end = "loop"` channel
/// whose clip doesn't fit the period a whole number of times (a jump at the seam).
fn footage_warnings(m: &Movie) {
    use movie::{End, FootageClock, ItemRef};
    let items = (0..m.shots.len())
        .map(|i| {
            (
                ItemRef::Shot(i),
                format!("shot \"{}\"", m.shots[i].id),
                m.shots[i].start,
                m.shots[i].end(),
            )
        })
        .chain((0..m.layers.len()).map(|j| {
            let l = &m.layers[j];
            (
                ItemRef::Layer(j),
                format!("layer \"{}\"", l.id),
                l.start,
                l.end,
            )
        }));
    for (item, who, start, end) in items {
        for c in 0..movie::N_CHANNELS {
            let Some(map) = m.frame_map(item, c) else {
                continue;
            };
            let label = match item {
                ItemRef::Shot(i) => m.shots[i].channels[c].as_ref(),
                ItemRef::Layer(j) => m.layers[j].channels[c].as_ref(),
            }
            .map(|c| c.label())
            .unwrap_or_default();
            let last = (m.frame_at(end) - 1).max(m.frame_at(start)) as u64;
            if map.timing.end == End::Hold && map.timing.speed > 0.0 {
                let open = movie::FrameMap {
                    timing: movie::Timing {
                        end: End::Black,
                        ..map.timing
                    },
                    ..map
                };
                if open.source_frame(last).is_none() {
                    println!(
                        "warning: {who} ({:.2} s) runs past the end of {label} ({} frames); \
                         end = \"hold\" freezes its last frame",
                        end - start,
                        map.count
                    );
                }
            }
            if let Some(period) = m.loop_period()
                && map.timing.end == End::Loop
                && map.timing.clock == FootageClock::Movie
            {
                let frames = period * map.timing.speed * map.src.0 as f64 / map.src.1 as f64;
                let laps = frames / map.count as f64;
                if (laps - laps.round()).abs() > 1e-6 {
                    println!(
                        "warning: {who}: {label} ({} frames) doesn't fit the loop period \
                         ({period:.2} s) a whole number of times: a visible jump at the seam",
                        map.count
                    );
                }
            }
        }
    }
}

fn check(
    movie: &Path,
    size: &str,
    frames: usize,
    web: bool,
    strict: bool,
    no_gpu: bool,
    deep: bool,
) -> Result<()> {
    let m = load_movie(movie)?;
    let (w, h) = parse_size(size)?;
    println!(
        "{} — {:.2}s, {} shots{}{}",
        m.title,
        m.duration,
        m.shots.len(),
        m.loop_frames
            .map_or(String::new(), |n| format!(", loops every {n} frames")),
        m.shotglass
            .as_ref()
            .map_or(String::new(), |v| format!(", made for shotglass {v}"))
    );
    for l in &m.libraries {
        let from = match &l.origin {
            library::Origin::Path if l.root.starts_with(&m.root) => "the film".to_string(),
            library::Origin::Path => "the project".to_string(),
            library::Origin::Env(v) => format!("override {v}"),
            library::Origin::Local => format!("override in {}", library::LOCAL_FILE),
            library::Origin::Locked => library::LOCK_FILE.to_string(),
            library::Origin::Bundled(p) => format!("{} (frozen in {p})", library::LOCK_FILE),
        };
        println!(
            "library {}{}  [{}]  from {from}: {}",
            l.describe(),
            l.glsl.map_or(String::new(), |g| format!(", GLSL {g}")),
            if l.licences.is_empty() {
                "no licence declared".to_string()
            } else {
                l.licences.join(", ")
            },
            l.root.display()
        );
    }
    strict_check(&m, web, strict)?;
    check_textures(&m, web, deep)?;
    if no_gpu {
        println!(
            "
ok"
        );
        return Ok(());
    }
    headless::with_gl(move |gl| {
        let t0 = Instant::now();
        let mut r = Renderer::new(gl)?;
        println!("GPU: {}", r.gl_info);
        let dialect = r.dialect_for(&m).map_err(|e| anyhow!(e))?;
        println!("compiling as {dialect}");
        println!(
            "parallel shader compile: {}  |  program cache: {}",
            if r.parallel { "yes" } else { "no" },
            r.shader_cache_dir()
                .map_or("off".into(), |d| d.display().to_string())
        );
        let compiled = match r.load_all(&m) {
            Ok(n) => n,
            Err(errors) => {
                for e in &errors {
                    eprint!("{e}");
                }
                bail!("{} shader(s) failed to compile", errors.len());
            }
        };
        println!(
            "compiled {compiled} programs in {:.0} ms",
            t0.elapsed().as_secs_f32() * 1000.0
        );
        for w in &r.warnings {
            println!("warning: {w}");
        }
        if m.has_sound() {
            let t = Instant::now();
            let mut st = sound::Soundtrack::new();
            if let Err(errors) = st.set_movie(&mut r, &m, 48000) {
                for e in &errors {
                    eprint!("{e}");
                }
                bail!("{} sound shader(s) failed", errors.len());
            }
            // Stream: keep only running stats, not the whole soundtrack.
            let (mut peak, mut sum_sq, mut n) = (0f32, 0f64, 0usize);
            let mut seam = SeamCheck::default();
            st.render_all(&mut r, |d| {
                for x in d {
                    peak = peak.max(x.abs());
                    sum_sq += (*x as f64) * (*x as f64);
                }
                n += d.len();
                seam.feed(d);
            })
            .map_err(|e| anyhow!(e))?;
            if m.loop_frames.is_some()
                && let Some(w) = seam.warning()
            {
                println!("warning: {w}");
            }
            println!("sound tracks: {}", st.take_rendered().join(", "));
            let rms = (sum_sq / n.max(1) as f64).sqrt();
            println!(
                "sound: {:.1}s synthesized in {:.0} ms, peak {:.2}{}, rms {:.3}",
                n as f32 / 2.0 / 48000.0,
                t.elapsed().as_secs_f32() * 1000.0,
                peak,
                if peak >= 0.999 { " (clipping!)" } else { "" },
                rms
            );
        }
        println!(
            "\ntiming at {w}x{h}, {frames} frames each (software GL is much slower than a real GPU):"
        );
        let mut probes: Vec<(String, f32)> = vec![];
        for s in &m.shots {
            if let Some(tr) = &s.transition_in {
                probes.push((
                    format!("  {:<28}", format!("→ {} ({})", s.id, tr.kind)),
                    s.start + tr.duration * 0.5,
                ));
            }
            probes.push((
                format!("  {:<28}", format!("shot {}", s.id)),
                s.settled() + (s.end() - s.settled()) * 0.5,
            ));
        }
        for (name, t) in probes {
            r.render(&m, w, h, &frame_info(&m, t), true);
            r.finish();
            let start = Instant::now();
            for i in 0..frames {
                r.render(&m, w, h, &frame_info(&m, t + i as f32 / m.fps), true);
            }
            r.finish();
            let ms = start.elapsed().as_secs_f32() * 1000.0 / frames as f32;
            let flag = if ms > 16.7 {
                "  over 60 fps budget"
            } else {
                ""
            };
            println!("{name} {ms:8.2} ms{flag}");
        }
        println!("\nok");
        Ok(())
    })
}

/// Does a loop film's sound jump at the seam (last sample → first) much more than
/// anywhere else? Music shaders must be periodic themselves for a click-free loop.
#[derive(Default)]
struct SeamCheck {
    first: Option<[f32; 2]>,
    last: Option<[f32; 2]>,
    largest_step: f32,
}

impl SeamCheck {
    fn feed(&mut self, interleaved: &[f32]) {
        for &f in interleaved.as_chunks::<2>().0 {
            match self.last {
                Some(l) => {
                    let step = (f[0] - l[0]).abs().max((f[1] - l[1]).abs());
                    self.largest_step = self.largest_step.max(step);
                }
                None => self.first = Some(f),
            }
            self.last = Some(f);
        }
    }

    fn seam_step(&self) -> f32 {
        match (self.first, self.last) {
            (Some(a), Some(b)) => (a[0] - b[0]).abs().max((a[1] - b[1]).abs()),
            _ => 0.0,
        }
    }

    fn warning(&self) -> Option<String> {
        let seam = self.seam_step();
        (seam > 0.05 && seam > 2.0 * self.largest_step).then(|| {
            format!(
                "the sound jumps by {seam:.3} at the loop seam (end → start), more than anywhere \
                 else in the film (at most {:.3}), so it clicks every time the film loops; make the \
                 music periodic in the film's duration, or fade it at both ends",
                self.largest_step
            )
        })
    }
}

/// `wav --compare`: the reference soundtrack and how close the new one must be.
struct GoldenSound {
    reference: PathBuf,
    bless: bool,
    tol: golden::SoundTolerance,
}

fn wav(
    movie: &Path,
    out: &Path,
    rate: u32,
    allow_silence: bool,
    golden: Option<GoldenSound>,
) -> Result<()> {
    let m = load_movie(movie)?;
    let frames = if m.has_sound() {
        let path = out.to_path_buf();
        headless::with_gl(move |gl| {
            let mut r = Renderer::new(gl)?;
            export::write_wav(&mut r, &m, rate, &path, 1)
        })?
    } else if allow_silence {
        let frames = m.sound_samples(rate);
        let mut writer = audio::WavWriter::create(out, frames, rate)
            .with_context(|| format!("couldn't create {}", out.display()))?;
        writer.write_silence(frames)?;
        writer.finish()?;
        frames
    } else {
        bail!("this movie has no sound (no [sound] music and no shot sounds)");
    };
    println!(
        "wrote {} ({:.1}s at {rate} Hz)",
        out.display(),
        frames as f32 / rate as f32
    );
    match golden {
        Some(g) => compare_golden_sound(out, &g),
        None => Ok(()),
    }
}

/// Compares the WAV as written (16-bit), so a blessed reference compares equal to itself.
fn compare_golden_sound(out: &Path, g: &GoldenSound) -> Result<()> {
    if g.bless {
        if let Some(dir) = g.reference.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::copy(out, &g.reference)
            .with_context(|| format!("writing {}", g.reference.display()))?;
        println!("blessed: wrote the new reference {}", g.reference.display());
        return Ok(());
    }
    if !g.reference.exists() {
        bail!(
            "no reference at {} (add --bless to make this soundtrack the reference)",
            g.reference.display()
        );
    }
    let (rate, actual) = golden::load_wav(out)?;
    let (ref_rate, reference) = golden::load_wav(&g.reference)?;
    if rate != ref_rate {
        bail!(
            "the soundtrack is at {rate} Hz but the reference {} is at {ref_rate} Hz (pass --rate {ref_rate})",
            g.reference.display()
        );
    }
    let c = golden::compare_sound(&actual, &reference, rate, g.tol)
        .with_context(|| format!("comparing with {}", g.reference.display()))?;
    let diff_path = out.with_extension("diff.wav");
    if c.passes() {
        let _ = std::fs::remove_file(&diff_path); // don't leave a stale one behind
        println!("matches {}: {}", g.reference.display(), c.describe(g.tol));
        return Ok(());
    }
    let mut w = audio::WavWriter::create(&diff_path, c.diff.len() / 2, rate)?;
    w.write(&c.diff)?;
    w.finish()?;
    bail!(
        "the soundtrack doesn't match {}: {}\n  actual: {}\n  diff:   {} (actual - reference)\n  \
         if the change is intended, re-run with --bless and commit the new reference",
        g.reference.display(),
        c.describe(g.tol),
        out.display(),
        diff_path.display()
    )
}

/// `export`'s built-in encoder flags, as given (None = not given).
struct EncoderFlags {
    quality: Option<u8>,
    bitrate: Option<u32>,
    speed: Option<u8>,
    depth: Option<u8>,
    grain: Option<u8>,
}

impl EncoderFlags {
    fn any(&self) -> bool {
        self.quality.is_some()
            || self.bitrate.is_some()
            || self.speed.is_some()
            || self.depth.is_some()
            || self.grain.is_some()
    }

    fn options(&self) -> export::Av1Options {
        let d = export::Av1Options::default();
        export::Av1Options {
            quality: self.quality.unwrap_or(d.quality),
            bitrate: self.bitrate,
            speed: self.speed.unwrap_or(d.speed),
            depth: self.depth.unwrap_or(d.depth),
            grain: self.grain.unwrap_or(d.grain),
            keyint: d.keyint,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn export(
    movie: &Path,
    out: PathBuf,
    size: &str,
    format: Option<&str>,
    ffmpeg_args: Option<&str>,
    rate: u32,
    loops: u64,
    flags: EncoderFlags,
) -> Result<()> {
    use std::io::IsTerminal;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let m = load_movie(movie)?;
    if loops > 1 && m.loop_frames.is_none() {
        bail!("--loops needs a film with loop = true in movie.toml");
    }
    let (w, h) = parse_size(size)?;
    let format = export::Format::choose(format, &out, ffmpeg_args)?;
    format.check_size(w as usize, h as usize)?;
    if flags.any() && !format.builtin() {
        if matches!(format, export::Format::Webm | export::Format::Av1) {
            bail!(
                "--quality, --bitrate, --speed, --depth and --grain set the built-in AV1 encoder,                  and this shotglass was built without it (the webm feature);                  use --format ffmpeg --ffmpeg-args for your own settings"
            );
        }
        bail!(
            "--quality, --bitrate, --speed, --depth and --grain only go with --format webm or av1;              for other formats use --format ffmpeg --ffmpeg-args"
        );
    }
    if format == export::Format::Webm && !format.builtin() {
        println!(
            "note: this shotglass was built without the webm feature, so WebM goes through              ffmpeg as VP9 + Opus"
        );
    }
    if format.builtin() && rate != 48_000 && m.has_sound() {
        println!("note: Opus is always 48 kHz, so --rate {rate} is ignored");
    }
    if format.needs_ffmpeg() {
        export::ffmpeg_encoders()?; // a clear message before any rendering
    }
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let cancel = cancel.clone();
        ctrlc::set_handler(move || {
            if cancel.swap(true, Ordering::Relaxed) {
                std::process::exit(130); // second Ctrl-C: stop now
            }
        })
        .context("couldn't install the Ctrl-C handler")?;
    }
    let opts = export::Options {
        out,
        w: w as usize,
        h: h as usize,
        format,
        rate,
        loops,
        av1: flags.options(),
    };
    println!(
        "{} — {:.2}s{} at {}/{} fps, {w}x{h} → {} ({:?})",
        m.title,
        m.duration,
        if loops > 1 {
            format!(" × {loops} loops")
        } else {
            String::new()
        },
        m.fps_num,
        m.fps_den,
        opts.out.display(),
        opts.format
    );
    headless::with_gl(move |gl| {
        let mut r = load_renderer(gl, &m)?;
        r.set_readback(true);
        println!("GPU: {}", r.gl_info);
        #[cfg(feature = "webm")]
        if opts.format.builtin() {
            let a = &opts.av1;
            let rate = match a.bitrate {
                Some(k) => format!("{k} kbit/s"),
                None => format!("quality {}", a.quality),
            };
            println!(
                "encoder: SVT-AV1 {} ({}-bit, {rate}, speed {}{}), libopus {} ({} kbit/s)",
                webm::encode::svt_version(),
                a.depth,
                a.speed,
                if a.grain > 0 {
                    format!(", grain {}", a.grain)
                } else {
                    String::new()
                },
                webm::encode::opus_version(),
                webm::export::OPUS_KBPS
            );
        }
        let start = Instant::now();
        let tty = std::io::stderr().is_terminal();
        let mut last = Instant::now();
        let mut last_pct = 0;
        let res = export::export(&mut r, &m, &opts, &cancel, |done, total| {
            let pct = done * 100 / total.max(1);
            let now = Instant::now();
            let show = if tty {
                done == total || now - last >= std::time::Duration::from_millis(200)
            } else {
                pct / 10 != last_pct / 10 || done == total
            };
            if !show {
                return;
            }
            last = now;
            last_pct = pct;
            let secs = start.elapsed().as_secs_f64();
            let fps = done as f64 / secs.max(1e-6);
            let eta = (total - done) as f64 / fps.max(1e-6);
            let line = format!(
                "frame {done}/{total} ({pct}%)  {fps:.1} fps  {}s left",
                eta.round()
            );
            if tty {
                eprint!("\r{line}    ");
                if done == total {
                    eprintln!();
                }
            } else {
                eprintln!("{line}");
            }
        });
        if tty && res.is_err() {
            eprintln!();
        }
        let s = res?;
        println!(
            "wrote {}: {} frames, {} audio samples, in {:.1}s",
            opts.out.display(),
            s.frames,
            s.audio_frames,
            start.elapsed().as_secs_f32()
        );
        Ok(())
    })
}

const TEMPLATE: &[(&str, &str)] = &[
    ("movie.toml", include_str!("../templates/new/movie.toml")),
    (
        "shotglass.toml",
        include_str!("../templates/new/shotglass.toml"),
    ),
    (".gitignore", include_str!("../templates/new/gitignore")),
    (
        ".gitattributes",
        include_str!("../templates/new/gitattributes"),
    ),
    (
        "scenes/first.glsl",
        include_str!("../templates/new/scenes/first.glsl"),
    ),
    (
        "scenes/second.glsl",
        include_str!("../templates/new/scenes/second.glsl"),
    ),
    ("music.glsl", include_str!("../templates/new/music.glsl")),
    (
        "sfx/chime.glsl",
        include_str!("../templates/new/sfx/chime.glsl"),
    ),
];

/// The template's `shotglass = "…"` names this engine's contract.
fn template_text(name: &str, text: &str) -> String {
    if name != project::PROJECT_FILE {
        return text.to_string();
    }
    let v = library::contract();
    text.lines()
        .map(|l| match l.strip_prefix("shotglass = \"") {
            Some(rest) => {
                let comment = rest.split_once('"').map_or("", |(_, c)| c);
                format!("shotglass = \"{}.{}\"{comment}\n", v.major, v.minor)
            }
            None => format!("{l}\n"),
        })
        .collect()
}

/// Write the `shotglass new` template into `dir`.
fn write_template(dir: &Path) -> Result<()> {
    for (name, text) in TEMPLATE {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, template_text(name, text))?;
    }
    Ok(())
}

fn new_movie(dir: &Path) -> Result<()> {
    if dir.exists() && std::fs::read_dir(dir)?.next().is_some() {
        bail!("{} already exists and isn't empty", dir.display());
    }
    write_template(dir)?;
    println!(
        "created {}\nnext: shotglass play {}",
        dir.display(),
        dir.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_wav_can_be_blessed_and_compared() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("shotglass-silent-wav-{}", std::process::id()));
        project::write_fixture(
            &dir,
            r#"fps = 24
[[shot]]
id = "silent"
scene = "scene.glsl"
duration = 0.375
"#,
        )?;
        std::fs::write(
            dir.join("scene.glsl"),
            "void mainImage(out vec4 c, in vec2 p) { c = vec4(0.0); }",
        )?;
        let out = dir.join("actual.wav");
        let reference = dir.join("reference.wav");
        let rate = 11025;
        assert_eq!(
            wav(&dir, &out, rate, false, None).unwrap_err().to_string(),
            "this movie has no sound (no [sound] music and no shot sounds)"
        );
        let golden = |bless| GoldenSound {
            reference: reference.clone(),
            bless,
            tol: golden::SoundTolerance::default(),
        };
        wav(&dir, &out, rate, true, Some(golden(true)))?;
        let (actual_rate, samples) = golden::load_wav(&reference)?;
        assert_eq!(actual_rate, rate);
        assert_eq!(samples.len(), 4135 * 2); // ceil(0.375 * 11025) stereo frames
        assert!(samples.iter().all(|&sample| sample == 0.0));
        wav(&dir, &out, rate, true, Some(golden(false)))?;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn template_parses_and_names_this_engine() {
        let file = |name: &str| {
            let (_, text) = TEMPLATE.iter().find(|(n, _)| *n == name).unwrap();
            template_text(name, text)
        };
        let root = Path::new(".");
        let proj = file(project::PROJECT_FILE);
        let p = project::Project::parse(&proj, root).unwrap();
        let m = Movie::parse_in(&file("movie.toml"), root, &p).unwrap();
        let v = library::contract();
        assert_eq!(m.shotglass, Some(format!("{}.{}", v.major, v.minor)));
        assert!(proj.contains("# engine contract"));
        assert!(m.library_specs.iter().any(|s| s.name == "transitions"));
    }

    #[test]
    fn seam_check_warns_only_about_a_jump_at_the_seam() {
        // A sine with a whole number of cycles per period: no jump at the seam.
        let sine = |cycles: f32, n: usize| -> Vec<f32> {
            (0..n)
                .flat_map(|i| {
                    let x = (std::f32::consts::TAU * cycles * i as f32 / n as f32).sin() * 0.5;
                    [x, x]
                })
                .collect()
        };
        let mut s = SeamCheck::default();
        for c in sine(20.0, 4800).chunks(1000) {
            s.feed(c);
        }
        assert_eq!(s.warning(), None);
        // A ramp: smooth everywhere, then it drops from the top to 0 at the seam.
        let ramp: Vec<f32> = (0..4800).flat_map(|i| [i as f32 / 4800.0; 2]).collect();
        let mut s = SeamCheck::default();
        s.feed(&ramp);
        let w = s.warning().unwrap();
        assert!(w.contains("loop seam"), "{w}");
        // Noise jumps everywhere: the seam is nothing special.
        let noise: Vec<f32> = (0..4800u32)
            .flat_map(|i| [((i.wrapping_mul(2654435761) >> 16) as f32 / 65536.0) - 0.5; 2])
            .collect();
        let mut s = SeamCheck::default();
        s.feed(&noise);
        assert_eq!(s.warning(), None);
    }
}
