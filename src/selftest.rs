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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use toodee::{TooDeeOps, TooDeeOpsMut};

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
        name: "playback doesn't stall on driver error checks",
        run: error_check_rate,
    },
    Check {
        name: "compile errors point at the right file and line",
        run: error_mapping,
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
        name: "a movie downloaded file by file plays like the one on disk",
        run: downloaded_movie,
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
        name: "sound shader errors point at the right file and line",
        run: sound_errors,
    },
];

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
                errors.iter().map(|e| e.to_string()).collect::<String>()
            );
        }
    }
    Ok(())
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

fn movie(dir: &Path, toml: &str) -> Result<Movie> {
    std::fs::write(dir.join("movie.toml"), toml)?;
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

/// The web player's path: find the files by following `movie.toml` and the
/// `#include`s one fetch at a time (here from disk instead of HTTP), then play
/// from memory. Pictures and sound must be identical to loading from disk.
fn downloaded_movie(gl: &Gl, dir: &Path) -> Result<String> {
    use crate::files::Preloader;
    write(
        dir,
        &[
            (
                "common.glsl",
                "#include \"lib/tint.glsl\"\nvec3 base(vec2 uv) { return tint(vec3(uv, 0.3)); }\n",
            ),
            (
                "lib/tint.glsl",
                "vec3 tint(vec3 c) { return c * vec3(1.0, 0.8, 0.9); }\n",
            ),
            (
                "scenes/a.glsl",
                &format!(
                    "#include \"../common.glsl\"\n{}",
                    scene("fragColor = vec4(base(uv), 1.0);")
                ),
            ),
            (
                "scenes/b.glsl",
                &format!(
                    "#include \"../common.glsl\"\n{}",
                    scene("fragColor = vec4(base(uv.yx).bgr, 1.0);")
                ),
            ),
            (
                "fx/push.glsl",
                "vec4 transition(vec2 uv) {\n  return uv.x < progress ? getToColor(uv) : getFromColor(uv);\n}\n",
            ),
            (
                "music.glsl",
                "#include \"lib/osc.glsl\"\nvec2 mainSound(int samp, float time) { return vec2(osc(220.0, time)); }\n",
            ),
            (
                "lib/osc.glsl",
                "float osc(float f, float t) { return 0.2 * sin(6.2831853 * f * t); }\n",
            ),
        ],
    )?;
    let toml = "[sound]\nmusic = \"music.glsl\"\n\
        [[shot]]\nid = \"a\"\nscene = \"scenes/a.glsl\"\nduration = 1\n\
        [[shot]]\nid = \"b\"\nscene = \"scenes/b.glsl\"\nduration = 1\n\
        transition = { type = \"fx/push.glsl\", duration = 0.5 }\n";
    let on_disk = movie(dir, toml)?;

    let file = Path::new("movie.toml");
    let mut first = crate::files::Memory::default();
    first.insert(file, toml.into());
    let named = Movie::load_from(Arc::new(first), file)?;
    let mut pre = Preloader::new(&named.root, named.referenced_files());
    pre.insert(file, toml.into());
    let mut fetches = 0;
    loop {
        let want = pre.wanted();
        if want.is_empty() {
            break;
        }
        for p in want {
            fetches += 1;
            pre.provide(&p, std::fs::read_to_string(dir.join(&p)).ok());
        }
    }
    let downloaded = Movie::load_from(Arc::new(pre.finish()), file)?;

    let mut rd = renderer(gl, &on_disk)?;
    let mut rm = renderer(gl, &downloaded)?;
    for t in [0.25, 0.75, 1.25] {
        let a = shot(&mut rd, &on_disk, on_disk.at(t), 1.0)?;
        let b = shot(&mut rm, &downloaded, downloaded.at(t), 1.0)?;
        expect_same(&format!("frame at {t}s"), &b, &a, 0, 0.0)?;
    }
    if render_sound(gl, &on_disk, 32768)? != render_sound(gl, &downloaded, 32768)? {
        bail!("the downloaded movie's sound differs");
    }
    Ok(format!("{fetches} fetches"))
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
        ("crossfade", ""),
        ("fade", ""),
        ("fade", "params = { hold = 0.3, color = [0.2, 0.4, 0.6] }, "),
        ("iris", ""),
        ("iris", "params = { closing = 1.0, center = [0.3, 0.6] }, "),
        ("dissolve", ""),
        ("wipe", "params = { direction = [-0.4, 1.0] }, "),
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
        // An iris centre pixel can sit exactly on the edge: allow a handful of pixels.
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
                 transition = {{ type = \"crossfade\", duration = 1, ease = \"linear\", freeze = {freeze} }}\n"
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
         transition = { type = \"crossfade\", duration = 1, ease = \"linear\" }\n",
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
    write(
        dir,
        &[(
            "s.glsl",
            &scene("fragColor = vec4(uv, fract(uv.x * 7.0 + uv.y * 3.0), 1.0);"),
        )],
    )?;
    let m = movie(dir, "[[shot]]\nid=\"a\"\nscene=\"s.glsl\"\nduration=2\n")?;
    let mut r = renderer(gl, &m)?;
    let one = shot(
        &mut r,
        &m,
        At::Shot {
            shot: 0,
            local: 0.5,
        },
        1.0,
    )?;
    for tiles in [2, 7, 64] {
        r.set_tiles(tiles);
        for scale in [1.0, 0.6] {
            let got = shot(
                &mut r,
                &m,
                At::Shot {
                    shot: 0,
                    local: 0.5,
                },
                scale,
            )?;
            let want = if scale == 1.0 {
                one.clone()
            } else {
                r.set_tiles(1);
                let w = shot(
                    &mut r,
                    &m,
                    At::Shot {
                        shot: 0,
                        local: 0.5,
                    },
                    scale,
                )?;
                r.set_tiles(tiles);
                w
            };
            expect_same(
                &format!("{tiles} strips at scale {scale}"),
                &got,
                &want,
                0,
                0.0,
            )?;
        }
    }
    Ok(String::new())
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
transition = { type = "crossfade", duration = 0.4 }

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
        if d > 1e-5 {
            bail!(
                "chunk {chunk}: differs by {d} at {:.4}s",
                (i / 2) as f32 / 48000.0
            );
        }
        notes.push(format!("{chunk}"));
    }
    if whole.iter().all(|x| x.abs() < 1e-4) {
        bail!("the fixture rendered silence");
    }
    Ok(format!("chunks of {} samples", notes.join(" and ")))
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
    if plain.len() != banded.len() || d > 1e-6 {
        bail!("banded synthesis differs by {d}");
    }
    Ok(String::new())
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
            bail!(
                "chunk {chunk}: streamed sound differs at {:.4}s ({} vs {})",
                (i / 2) as f32 / 48000.0,
                streamed[i],
                blocking[i]
            );
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
    std::fs::write(
        dir.join("music.glsl"),
        "// music\n\nvec2 mainSound(int samp, float time) {\n  return vec2(nope);\n}\n",
    )?;
    let mut r = Renderer::new(gl.clone())?;
    let mut st = Soundtrack::new();
    match st.set_movie(&mut r, &m, 48000) {
        Ok(()) => bail!("a broken music shader was accepted"),
        Err(e) => {
            let text: String = e.iter().map(|e| e.to_string()).collect();
            if !text.contains("music.glsl:4:") {
                bail!("expected an error at music.glsl:4, got:\n{text}");
            }
        }
    }
    Ok(String::new())
}
