//! shotglass — compose short films from GLSL shaders.

mod anim;
mod app;
mod audio;
mod export;
mod gfx;
mod golden;
mod headless;
mod library;
mod movie;
mod selftest;
mod shader;
mod sound;
mod texture;
mod validate;

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
        /// Only validate (movie.toml and the strict GLSL check); no GPU needed.
        #[arg(long)]
        no_gpu: bool,
    },
    /// Write the GLSL soundtrack to a WAV file.
    Wav {
        movie: PathBuf,
        #[arg(long, default_value = "sound.wav")]
        out: PathBuf,
        #[arg(long, default_value_t = 48000)]
        rate: u32,
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
    /// Needs ffmpeg on the PATH (or SHOTGLASS_FFMPEG), except for --format png.
    Export {
        movie: PathBuf,
        /// Output file (.webm, .mkv) or folder (PNG sequence + sound.wav).
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value = "1920x1080")]
        size: String,
        /// webm (VP9 + Opus), av1 (AV1 + Opus), ffv1 (lossless, + FLAC), png (frames + WAV),
        /// or ffmpeg (with --ffmpeg-args). Default: from the --out extension.
        #[arg(long)]
        format: Option<String>,
        /// Your own ffmpeg output arguments, e.g. "-c:v libx264 -crf 18 -c:a aac".
        #[arg(long, allow_hyphen_values = true)]
        ffmpeg_args: Option<String>,
        /// Audio sample rate.
        #[arg(long, default_value_t = 48000)]
        rate: u32,
    },
    /// Create a new movie folder from a template.
    New { dir: PathBuf },
    /// Download the film's libraries and pin them in shotglass.lock (keeps existing pins).
    Fetch { movie: PathBuf },
    /// Move libraries in shotglass.lock to the newest version movie.toml allows.
    Update {
        movie: PathBuf,
        /// Library names (local names from movie.toml); all if none.
        names: Vec<String>,
    },
    /// Check the renderer and soundtrack on this GPU and driver with built-in test films.
    Selftest {
        /// Only run checks whose name contains this text.
        filter: Option<String>,
    },
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
            no_gpu,
        } => check(&movie, &size, frames, web, no_gpu),
        Cmd::Wav {
            movie,
            out,
            rate,
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
            wav(&movie, &out, rate, golden)
        }
        Cmd::Export {
            movie,
            out,
            size,
            format,
            ffmpeg_args,
            rate,
        } => export(
            &movie,
            out,
            &size,
            format.as_deref(),
            ffmpeg_args.as_deref(),
            rate,
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
        Cmd::Selftest { filter } => selftest::run(filter),
    }
}

/// Load a film for a command, first fetching libraries that aren't locked or downloaded
/// yet (never moving an existing pin).
fn load_movie(path: &Path) -> Result<Movie> {
    let m = Movie::read(path)?;
    if m.library_specs.iter().any(|s| s.source.is_some()) {
        let env = |k: &str| std::env::var(k).ok();
        let skip = library::overrides(&m.root, &m.library_specs, &env)?
            .into_keys()
            .collect();
        fetch_libraries(path, library::FetchMode::Missing { skip }, false)?;
    }
    let m = Movie::load(path)?;
    for w in &m.warnings {
        eprintln!("warning: {w}");
    }
    Ok(m)
}

/// `fetch`, `update`, and the automatic fetch before other commands (`verbose` = false:
/// only says something when it downloads or changes the lock).
fn fetch_libraries(path: &Path, mode: library::FetchMode, verbose: bool) -> Result<()> {
    let m = Movie::read(path)?;
    if !m.library_specs.iter().any(|s| s.source.is_some()) {
        if verbose {
            println!(
                "{} has no libraries with a source; nothing to fetch",
                m.title
            );
        }
        return Ok(());
    }
    let old = library::Lock::load(&m.root)?;
    let (lock, log) = library::fetch(
        &m.library_specs,
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
        let p = m.root.join(library::LOCK_FILE);
        std::fs::write(&p, lock.to_text()).with_context(|| format!("writing {}", p.display()))?;
        println!("wrote {}", p.display());
    } else if verbose {
        println!("{} is up to date", library::LOCK_FILE);
    }
    Ok(())
}

fn parse_size(s: &str) -> Result<(i32, i32)> {
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
            let frame = r.read_output().ok_or_else(|| anyhow!("nothing rendered"))?;
            let path = out.join(format!("t{:08.3}.png", t));
            save_png(&path, &frame)?;
            println!("{}  {}", path.display(), describe(&m, t));
        }
        Ok(())
    })
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
    let cols = cols.clamp(1, times.len().max(1));
    let rows = times.len().div_ceil(cols);
    let gap = 4usize;
    let out = out.to_path_buf();
    headless::with_gl(move |gl| {
        let mut r = load_renderer(gl, &m)?;
        r.set_readback(true);
        let (w, h) = (w as usize, h as usize);
        let mut sheet = Frame::init(
            cols * (w + gap) + gap,
            rows * (h + gap) + gap,
            [24, 24, 24, 255],
        );
        for (i, t) in times.iter().enumerate() {
            r.render(&m, w as i32, h as i32, &frame_info(&m, *t), true)
                .ok_or_else(|| anyhow!("nothing rendered"))?;
            let (x, y) = ((i % cols) * (w + gap) + gap, (i / cols) * (h + gap) + gap);
            r.read_output_into(&mut sheet, (x, y));
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
    match m.at(t) {
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
fn strict_check(m: &Movie, web: bool) -> Result<()> {
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
    let sources = validate::movie_sources(m, desktop);
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
            let errors = validate::validate_movie(m, Dialect::WEB);
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
        }
    }
    Ok(())
}

/// The texture part of `check` (no GPU needed): decode every texture, check the limits
/// and each shot's memory, and point out channels a scene reads but nothing is bound to.
fn check_textures(m: &Movie, web: bool) -> Result<()> {
    if let Some(w) = m.contract_warning() {
        println!("warning: {w}");
    }
    let mut sizes: std::collections::HashMap<PathBuf, (usize, usize)> = Default::default();
    let mut failed = 0;
    for c in m.textures() {
        match texture::load(&c.file, &c.label, c.vflip) {
            Ok(img) => {
                let (w, h) = (img.width, img.height);
                println!(
                    "texture {}: {w}×{h}, {} on the GPU{}",
                    c.label,
                    texture::describe_bytes(texture::gpu_bytes(w, h)),
                    match (c.srgb, c.vflip) {
                        (true, true) => ", srgb",
                        (true, false) => ", srgb, vflip = false",
                        (false, false) => ", vflip = false",
                        (false, true) => "",
                    }
                );
                for warning in &img.warnings {
                    println!("warning: {warning}");
                }
                if w.max(h) > texture::WEB_MAX_SIZE {
                    let max = texture::WEB_MAX_SIZE;
                    println!(
                        "warning: {}: {w}×{h} is over {max}×{max}, the most some phones' browsers \
                         support{}",
                        c.label,
                        if web {
                            ""
                        } else {
                            " (matters for the web player)"
                        }
                    );
                }
                sizes.insert(c.file.clone(), (w, h));
            }
            Err(e) => {
                eprintln!("{e}");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        bail!("{failed} texture(s) can't be used");
    }
    let mut largest: Option<(usize, &str)> = None;
    let cues = m.cue_defines();
    for s in &m.shots {
        let mut files: Vec<&PathBuf> = s.channels.iter().flatten().map(|c| &c.file).collect();
        files.sort();
        files.dedup();
        let total: usize = files
            .iter()
            .filter_map(|f| sizes.get(*f))
            .map(|(w, h)| texture::gpu_bytes(*w, *h))
            .sum();
        if total > 0 && largest.is_none_or(|(b, _)| total > b) {
            largest = Some((total, &s.id));
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
            "textures: {} file(s); the most one shot needs is {} (shot \"{id}\"; budget {})",
            sizes.len(),
            texture::describe_bytes(b),
            texture::describe_bytes(texture::BUDGET)
        );
    }
    Ok(())
}

fn check(movie: &Path, size: &str, frames: usize, web: bool, no_gpu: bool) -> Result<()> {
    let m = load_movie(movie)?;
    let (w, h) = parse_size(size)?;
    println!(
        "{} — {:.2}s, {} shots{}",
        m.title,
        m.duration,
        m.shots.len(),
        m.shotglass
            .as_ref()
            .map_or(String::new(), |v| format!(", made for shotglass {v}"))
    );
    for l in &m.libraries {
        let from = match &l.origin {
            library::Origin::Path => "in the film".to_string(),
            library::Origin::Env(v) => format!("override {v}"),
            library::Origin::Local => format!("override in {}", library::LOCAL_FILE),
            library::Origin::Locked => library::LOCK_FILE.to_string(),
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
    strict_check(&m, web)?;
    check_textures(&m, web)?;
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
            st.render_all(&mut r, |d| {
                for x in d {
                    peak = peak.max(x.abs());
                    sum_sq += (*x as f64) * (*x as f64);
                }
                n += d.len();
            })
            .map_err(|e| anyhow!(e))?;
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

/// `wav --compare`: the reference soundtrack and how close the new one must be.
struct GoldenSound {
    reference: PathBuf,
    bless: bool,
    tol: golden::SoundTolerance,
}

fn wav(movie: &Path, out: &Path, rate: u32, golden: Option<GoldenSound>) -> Result<()> {
    let m = load_movie(movie)?;
    if !m.has_sound() {
        bail!("this movie has no sound (no [sound] music and no shot sounds)");
    }
    let path = out.to_path_buf();
    headless::with_gl(move |gl| {
        let mut r = Renderer::new(gl)?;
        let frames = export::write_wav(&mut r, &m, rate, &path)?;
        println!(
            "wrote {} ({:.1}s at {rate} Hz)",
            path.display(),
            frames as f32 / rate as f32
        );
        Ok(())
    })?;
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

fn export(
    movie: &Path,
    out: PathBuf,
    size: &str,
    format: Option<&str>,
    ffmpeg_args: Option<&str>,
    rate: u32,
) -> Result<()> {
    use std::io::IsTerminal;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let m = load_movie(movie)?;
    let (w, h) = parse_size(size)?;
    let format = export::Format::choose(format, &out, ffmpeg_args)?;
    format.check_size(w as usize, h as usize)?;
    if format != export::Format::Png {
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
    };
    println!(
        "{} — {:.2}s at {}/{} fps, {w}x{h} → {} ({:?})",
        m.title,
        m.duration,
        m.fps_num,
        m.fps_den,
        opts.out.display(),
        opts.format
    );
    headless::with_gl(move |gl| {
        let mut r = load_renderer(gl, &m)?;
        r.set_readback(true);
        println!("GPU: {}", r.gl_info);
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
    if name != "movie.toml" {
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
    fn template_parses_and_names_this_engine() {
        let (_, text) = TEMPLATE.iter().find(|(n, _)| *n == "movie.toml").unwrap();
        let text = template_text("movie.toml", text);
        let m = Movie::parse(&text, Path::new(".")).unwrap();
        let v = library::contract();
        assert_eq!(m.shotglass, Some(format!("{}.{}", v.major, v.minor)));
        assert!(text.contains("# engine contract"));
    }
}
