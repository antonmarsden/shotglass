//! shotglass — compose short films from GLSL shaders.

mod anim;
mod app;
mod audio;
mod gfx;
mod headless;
mod movie;
mod selftest;
mod shader;
mod sound;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use gfx::{Frame, FrameInfo, Renderer};
use movie::{At, Movie};
use std::path::{Path, PathBuf};
use std::time::Instant;
use toodee::{CopyOps, TooDeeOps, TooDeeOpsMut};

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
    },
    /// Validate the movie, compile every shader, synthesize the sound, and time each shot.
    Check {
        movie: PathBuf,
        /// Resolution to time frames at.
        #[arg(long, default_value = "1280x720")]
        size: String,
        /// Frames rendered per measurement.
        #[arg(long, default_value_t = 8)]
        frames: usize,
    },
    /// Write the GLSL soundtrack to a WAV file.
    Wav {
        movie: PathBuf,
        #[arg(long, default_value = "sound.wav")]
        out: PathBuf,
        #[arg(long, default_value_t = 48000)]
        rate: u32,
    },
    /// Create a new movie folder from a template.
    New { dir: PathBuf },
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
            let m = Movie::load(&movie)?;
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
        } => strip(&movie, count, at.as_deref(), &size, cols, &out),
        Cmd::Check {
            movie,
            size,
            frames,
        } => check(&movie, &size, frames),
        Cmd::Wav { movie, out, rate } => wav(&movie, &out, rate),
        Cmd::New { dir } => new_movie(&dir),
        Cmd::Selftest { filter } => selftest::run(filter),
    }
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
    FrameInfo {
        t,
        dt: 1.0 / m.fps,
        frame: (t * m.fps) as i32,
        scale: 1.0,
    }
}

fn still(movie: &Path, at: &str, size: &str, out: &Path) -> Result<()> {
    let m = Movie::load(movie)?;
    let times = parse_times(at, &m)?;
    let (w, h) = parse_size(size)?;
    std::fs::create_dir_all(out)?;
    let out = out.to_path_buf();
    headless::with_gl(move |gl| {
        let mut r = load_renderer(gl, &m)?;
        for t in times {
            r.render(&m, w, h, &frame_info(&m, t), true);
            let frame = r.read_output().ok_or_else(|| anyhow!("nothing rendered"))?;
            let path = out.join(format!("t{:08.3}.png", t));
            save_png(&path, &frame)?;
            println!("{}  {}", path.display(), describe(&m, t));
        }
        Ok(())
    })
}

fn strip(
    movie: &Path,
    count: usize,
    at: Option<&str>,
    size: &str,
    cols: usize,
    out: &Path,
) -> Result<()> {
    let m = Movie::load(movie)?;
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
        let (w, h) = (w as usize, h as usize);
        let mut sheet = Frame::init(
            cols * (w + gap) + gap,
            rows * (h + gap) + gap,
            [24, 24, 24, 255],
        );
        for (i, t) in times.iter().enumerate() {
            r.render(&m, w as i32, h as i32, &frame_info(&m, *t), true);
            let frame = r.read_output().ok_or_else(|| anyhow!("nothing rendered"))?;
            let (x, y) = ((i % cols) * (w + gap) + gap, (i / cols) * (h + gap) + gap);
            sheet
                .view_mut((x, y), (x + w, y + h))
                .copy_from_toodee(&frame);
            println!("#{i:<3} {:>7.2}s  {}", t, describe(&m, *t));
        }
        save_png(&out, &sheet)?;
        println!("wrote {}", out.display());
        Ok(())
    })
}

fn save_png(path: &Path, frame: &Frame) -> Result<()> {
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

fn check(movie: &Path, size: &str, frames: usize) -> Result<()> {
    let m = Movie::load(movie)?;
    let (w, h) = parse_size(size)?;
    println!("{} — {:.2}s, {} shots", m.title, m.duration, m.shots.len());
    headless::with_gl(move |gl| {
        let t0 = Instant::now();
        let mut r = Renderer::new(gl)?;
        println!("GPU: {}", r.gl_info);
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

fn wav(movie: &Path, out: &Path, rate: u32) -> Result<()> {
    let m = Movie::load(movie)?;
    if !m.has_sound() {
        bail!("this movie has no sound (no [sound] music and no shot sounds)");
    }
    let out = out.to_path_buf();
    headless::with_gl(move |gl| {
        let mut r = Renderer::new(gl)?;
        let mut st = sound::Soundtrack::new();
        st.set_movie(&mut r, &m, rate).map_err(|errors| {
            anyhow!(
                "{}",
                errors.iter().map(|e| e.to_string()).collect::<String>()
            )
        })?;
        let frames = (m.duration * rate as f32).ceil() as usize;
        let mut w = audio::WavWriter::create(&out, frames, rate)?;
        let mut err = None;
        st.render_all(&mut r, |d| {
            if let Err(e) = w.write(d) {
                err.get_or_insert(e);
            }
        })
        .map_err(|e| anyhow!(e))?;
        if let Some(e) = err {
            return Err(e.into());
        }
        w.finish()?;
        println!(
            "wrote {} ({:.1}s at {rate} Hz)",
            out.display(),
            frames as f32 / rate as f32
        );
        Ok(())
    })
}

const TEMPLATE: &[(&str, &str)] = &[
    ("movie.toml", include_str!("../templates/new/movie.toml")),
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

fn new_movie(dir: &Path) -> Result<()> {
    if dir.exists() && std::fs::read_dir(dir)?.next().is_some() {
        bail!("{} already exists and isn't empty", dir.display());
    }
    for (name, text) in TEMPLATE {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, text)?;
    }
    println!(
        "created {}\nnext: shotglass play {}",
        dir.display(),
        dir.display()
    );
    Ok(())
}
