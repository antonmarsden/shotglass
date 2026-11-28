//! Shared helpers: fixture writing, renderers, frame comparisons, inexact-renderer notes.

use super::*;

pub(super) fn write(dir: &Path, files: &[(&str, &str)]) -> Result<()> {
    for (name, text) in files {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(p, text)?;
    }
    Ok(())
}

pub(super) fn scene(body: &str) -> String {
    format!(
        "void mainImage(out vec4 fragColor, in vec2 fragCoord) {{\n  vec2 uv = fragCoord / iResolution.xy;\n  {body}\n}}\n"
    )
}

pub(super) fn info(t: f32, scale: f32) -> FrameInfo {
    FrameInfo {
        t,
        dt: 1.0 / 30.0,
        frame: (t * 30.0) as i32,
        scale,
    }
}

/// A fixture film's metric shaders (`quality.toml`) must pass the strict check, at the
/// baseline and as GLSL ES, like its other shaders ([`strict`]).
pub(super) fn strict_metrics(m: &Movie) -> Result<()> {
    if !crate::validate::AVAILABLE {
        return Ok(());
    }
    for d in [Dialect::BASELINE, Dialect::WEB] {
        let errors = crate::validate::validate_sources(&crate::quality::sources(m, d)?);
        if !errors.is_empty() {
            bail!(
                "metric fixtures aren't strictly valid {d}:\n{}",
                crate::gfx::join_errors(&errors)
            );
        }
    }
    Ok(())
}

/// Fixture films must pass the strict check too, at the baseline and as GLSL ES, so
/// the selftest doubles as the strict check's test corpus (and CI runs it).
pub(super) fn strict(m: &Movie) -> Result<()> {
    if !crate::validate::AVAILABLE {
        return Ok(());
    }
    for d in [Dialect::desktop(m.glsl), Dialect::WEB] {
        let errors = crate::validate::validate_movie(m, d);
        if !errors.is_empty() {
            bail!(
                "fixture isn't strictly valid {d}:\n{}",
                crate::gfx::join_errors(&errors)
            );
        }
    }
    Ok(())
}

/// Renderers whose sound isn't bit-exact across draw sizes, so "chunked / banded /
/// streamed / exported sound matches exactly" can't hold there. Apple's software
/// renderer (macOS CI) differs by about 1e-5 depending on the block drawn; those
/// comparisons are reported, not failed. Real GPUs and llvmpipe must match exactly.
pub(super) const INEXACT_SOUND_RENDERERS: &[&str] = &["Apple Software Renderer"];

/// None if sound must match exactly on this driver, else the renderer's name.
pub(super) fn inexact_sound(gl: &Gl) -> Option<String> {
    use eframe::glow::{self, HasContext};
    let name = unsafe { gl.get_parameter_string(glow::RENDERER) };
    INEXACT_SOUND_RENDERERS
        .contains(&name.as_str())
        .then_some(name)
}

/// Fail with `msg`, or on a renderer listed in `INEXACT_SOUND_RENDERERS` note it.
pub(super) fn sound_mismatch(gl: &Gl, notes: &mut Vec<String>, msg: String) -> Result<()> {
    match inexact_sound(gl) {
        Some(name) => {
            notes.push(format!("exact match skipped on {name}: {msg}"));
            Ok(())
        }
        None => bail!(msg),
    }
}

thread_local! {
    /// Buffer comparisons skipped on an inexact renderer during the current check.
    pub(super) static FOLD_NOTES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(vec![]) };
}

/// `expect_same` for buffer states. On the renderers in `INEXACT_SOUND_RENDERERS` (the
/// Apple Software Renderer) the same fold reached by different paths differs by 1/255
/// from the first frames, and float state compounds it over a long fold (cause unknown;
/// not the slow-frame strips), so a mismatch is noted, not failed, as for sound. Real
/// GPUs and llvmpipe must match exactly.
pub(super) fn expect_fold(
    gl: &Gl,
    what: &str,
    a: &Frame,
    b: &Frame,
    tol: u8,
    frac: f32,
) -> Result<()> {
    match expect_same(what, a, b, tol, frac) {
        Err(e) if inexact_sound(gl).is_some() => {
            FOLD_NOTES.with(|n| n.borrow_mut().push(e.to_string()));
            Ok(())
        }
        r => r,
    }
}

pub(super) fn renderer(gl: &Gl, m: &Movie) -> Result<Renderer> {
    strict(m)?;
    let mut r = Renderer::new(gl.clone())?;
    r.set_cache_dir(None); // checks must not depend on (or pollute) the user's cache
    r.load_all(m)
        .map_err(|e| anyhow!("{}", e.iter().map(|e| e.to_string()).collect::<String>()))?;
    Ok(r)
}

/// Render and read back RGBA8, top row first.
pub(super) fn shot(r: &mut Renderer, m: &Movie, at: At, scale: f32) -> Result<Frame> {
    let t = match at {
        At::Shot { shot, local } => m.shots[shot].start + local,
        At::Transition { to, to_local, .. } => m.shots[to].start + to_local,
    };
    r.render_at(m, W, H, &info(t, scale), at, true)
        .ok_or_else(|| anyhow!("nothing rendered"))?;
    r.read_output().ok_or_else(|| anyhow!("no output"))
}

/// Plays from where `r` is, not blocking, at film time `t` (pumping between frames)
/// until a frame draws; returns it and how many frames were held before it.
pub(super) fn play_until_ready(
    r: &mut Renderer,
    m: &Movie,
    t: f32,
    what: &str,
) -> Result<(Frame, usize)> {
    let mut frames = 0;
    loop {
        if r.render(m, W, H, &info(t, 1.0), false).is_some() {
            return Ok((r.read_output().unwrap(), frames));
        }
        frames += 1;
        if frames > 5000 {
            bail!("the {what} never became ready without blocking");
        }
        r.pump(m, t, false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// (largest channel difference, fraction of pixels differing by more than `tol`).
pub(super) fn diff(a: &Frame, b: &Frame, tol: u8) -> (u8, f32) {
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
pub(super) fn px(f: &Frame, col: usize, row: usize) -> [u8; 4] {
    f[(col, row)]
}

pub(super) fn expect_same(what: &str, a: &Frame, b: &Frame, tol: u8, frac: f32) -> Result<()> {
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

/// Transitions the fixture films use, as files in the film (`type = "fx/wipe.glsl"`):
/// shotglass has no built-in looks, and the selftest must not need the network.
pub(super) const FX: &[(&str, &str)] = &[
    (
        "fx/crossfade.glsl",
        "vec4 transition(vec2 uv) { return mix(getFromColor(uv), getToColor(uv), progress); }\n",
    ),
    (
        "fx/wipe.glsl",
        "#ifndef direction\n#define direction vec2(1.0, 0.0)\n#endif\n\
         #ifndef softness\n#define softness 0.01\n#endif\n\
         vec4 transition(vec2 uv) {\n\
         \x20   vec2 d = normalize(direction);\n\
         \x20   float lo = min(min(0.0, d.x), min(d.y, d.x + d.y));\n\
         \x20   float hi = max(max(0.0, d.x), max(d.y, d.x + d.y));\n\
         \x20   float edge = mix(lo - softness, hi + softness, progress);\n\
         \x20   float m = smoothstep(edge - softness, edge + softness, dot(uv, d));\n\
         \x20   return mix(getToColor(uv), getFromColor(uv), m);\n\
         }\n",
    ),
];

/// Write a fixture film (one text: `shotglass = …` and `[libraries]` go to its
/// shotglass.toml) and load it.
pub(super) fn movie(dir: &Path, toml: &str) -> Result<Movie> {
    write(dir, FX)?;
    crate::project::write_fixture(dir, toml)?;
    Movie::load(dir)
}

pub(super) fn dirty_pack_state(gl: &Gl) -> Result<glow::Buffer> {
    unsafe {
        let buf = gl.new_buffer().map_err(|e| anyhow!(e))?;
        gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(buf));
        gl.buffer_data_size(glow::PIXEL_PACK_BUFFER, 4096, glow::STREAM_READ);
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 8);
        gl.pixel_store_i32(glow::PACK_ROW_LENGTH, 1);
        gl.pixel_store_i32(glow::PACK_SKIP_PIXELS, 1);
        gl.pixel_store_i32(glow::PACK_SKIP_ROWS, 1);
        if !gl.version().is_embedded {
            gl.pixel_store_i32(glow::PACK_SWAP_BYTES, 1);
        }
        Ok(buf)
    }
}

pub(super) fn at_time(m: &Movie, at: At) -> f32 {
    match at {
        At::Shot { shot, local } => m.shots[shot].start + local,
        At::Transition { to, to_local, .. } => m.shots[to].start + to_local,
    }
}

/// Write a PNG of `w` × `h` pixels, `f(x, y)` = RGBA with y = 0 the top row.
pub(super) fn png(
    dir: &Path,
    name: &str,
    w: u32,
    h: u32,
    f: impl Fn(u32, u32) -> [u8; 4],
) -> Result<()> {
    let p = dir.join(name);
    std::fs::create_dir_all(p.parent().unwrap())?;
    image::RgbaImage::from_fn(w, h, |x, y| image::Rgba(f(x, y))).save(p)?;
    Ok(())
}

pub(super) fn near(got: [u8; 4], want: [u8; 3], tol: u8) -> bool {
    (0..3).all(|i| got[i].abs_diff(want[i]) <= tol)
}

pub(super) fn expect_px(
    what: &str,
    f: &Frame,
    col: usize,
    row: usize,
    want: [u8; 3],
    tol: u8,
) -> Result<()> {
    let got = px(f, col, row);
    if !near(got, want, tol) {
        bail!(
            "{what}: pixel ({col}, {row}) is {:?}, wanted {want:?} (±{tol})",
            &got[..3]
        );
    }
    Ok(())
}

pub(super) fn at_shot(i: usize, local: f32) -> At {
    At::Shot { shot: i, local }
}

/// Quadrants of a 2×2 image, top row first: red, green / blue, white.
pub(super) fn quadrants(x: u32, y: u32) -> [u8; 4] {
    match (x, y) {
        (0, 0) => [255, 0, 0, 255],
        (1, 0) => [0, 255, 0, 255],
        (0, 1) => [0, 0, 255, 255],
        _ => [255, 255, 255, 255],
    }
}
