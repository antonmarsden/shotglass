//! OpenGL rendering: scene programs, transitions, off-screen targets, sound synthesis.
//!
//! Every frame: the shot (or both shots of a transition) renders into an off-screen
//! texture at `scale` × output size, then a compositor pass (a transition shader, or a
//! plain copy) draws the result into the output texture at full size.

use crate::anim::Anim;
use crate::movie::{At, Layer, LayerScene, Movie, N_CHANNELS, Shot};
use crate::shader::{Assembled, Builder, Dialect, DriverGlsl, remap_log};
use crate::texture::{DecodedImage, Filter, Wrap};
use anyhow::{Result, anyhow};
use eframe::glow::{self, HasContext};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use toodee::{TooDeeOps, TooDeeOpsMut, TranslateOps};

mod audio;
mod draw;
mod fold;
mod footage;
pub mod gldebug;
pub mod glstate;
pub mod ledger;
mod plan;
mod programs;
mod shaders;
mod textures;
pub use ledger::{GlObjects, Tracked};
pub use plan::seq_cues;
use plan::*;
pub use shaders::{
    blend_source, buffer_source, channels_read, layer_source, over_matte_source, scene_source,
    transition_source, vertex_source,
};

/// The engine's own pass that shows one shot unchanged (`transition_source(.., "copy", ..)`).
/// Not a transition films can name: their transitions come from libraries or files.
const COPY: &str = include_str!("../glsl/copy.glsl");
/// The engine's layer composite: premultiplied source-over (#69).
const OVER: &str = include_str!("../glsl/over.glsl");

/// A layer's footer (#69): `mainImage` writes straight alpha (alpha 1 unless it writes
/// one); the stored layer is premultiplied. Its opacity is applied when it's
/// composited (the over pass, or `getLayer` in a blend), so a later layer reading it
/// with `@<id>` sees the layer itself, whatever its opacity (#72). Engine GLSL by the
/// owner's ruling (#67): with the over pass, it defines what a layer at opacity o is.
const LAYER_FOOTER: &str = "void main(){\n  vec4 c = vec4(0.0, 0.0, 0.0, 1.0);\n  mainImage(c, gl_FragCoord.xy);\n  float a = clamp(c.a, 0.0, 1.0);\n  shotglass_out = vec4(c.rgb * a, a);\n}";

/// `ChanKey::tex` values for another pass's picture, bound by the frame rather than
/// taken from the texture store: `@input` (the working picture below the layer),
/// `@picture` (the picture track) and `@<layer>` (`LAYER_TEX - index`).
const INPUT_TEX: u64 = u64::MAX;
const PICTURE_TEX: u64 = u64::MAX - 1;
const LAYER_TEX: u64 = u64::MAX - 2;

/// `@<comp>` on channel n (#74): `COMP_TEX - n`, bound to that channel's comp instance.
const COMP_TEX: u64 = LAYER_TEX - 65000;

/// A pass key rather than a texture in the store?
fn is_pass_key(k: u64) -> bool {
    k > LAYER_TEX - 65536
}

/// `ChanKey::tex` of a footage channel (#70). In the keys a version stores, the frame
/// isn't known yet (`FOOTAGE_UNRESOLVED`: readiness is decided per frame); `resolve`
/// replaces it with that frame's texture, or `FOOTAGE_BLACK` (`end = "black"` past the
/// last frame: always ready, reads black with the clip's resolution).
const FOOTAGE_BLACK: u64 = LAYER_TEX - 65536;
const FOOTAGE_UNRESOLVED: u64 = LAYER_TEX - 65537;

fn is_footage_marker(k: u64) -> bool {
    k == FOOTAGE_BLACK || k == FOOTAGE_UNRESOLVED
}

/// A layer's blend pass (#72) sees the picture below as `getBase`, the layer
/// (premultiplied, its opacity applied: alpha = coverage × opacity) as `getLayer`, and
/// the opacity as `opacity`; it writes `vec4 blend(vec2 uv)` (alpha ignored). The
/// composite passes the opacity in `progress`.
const BLEND_HEADER: &str = "#define getBase getFromColor\n#define getLayer(uv) (getToColor(uv) * progress)\n#define opacity progress\n";
const BLEND_FOOTER: &str = "vec4 transition(vec2 uv) { return blend(uv); }\n";

/// Mattes (#74), engine contract like the over pass: `sg_matte_value(uv)` is the matte
/// source's alpha, or its Rec. 709 luma, or one minus that; a matted layer is multiplied
/// by it where it's composited (the matted over pass, `getLayer` in a blend).
const MATTE_HEADER: &str = "uniform sampler2D sg_matte;\nuniform float sg_matte_luma;\nuniform float sg_matte_invert;\nfloat sg_matte_value(vec2 uv) {\n  vec4 m = texture(sg_matte, clamp(uv, 0.0, 1.0));\n  float v = mix(m.a, dot(m.rgb, vec3(0.2126, 0.7152, 0.0722)), sg_matte_luma);\n  return mix(v, 1.0 - v, sg_matte_invert);\n}\n";
const BLEND_MATTE_HEADER: &str = "#define getBase getFromColor\n#define getLayer(uv) (getToColor(uv) * (progress * sg_matte_value(uv)))\n#define opacity progress\n";
/// The matted over pass (#74).
const OVER_MATTE: &str = include_str!("../glsl/over_matte.glsl");

const VERTEX: &str = "out vec2 sg_uv;\nvoid main(){\n  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));\n  sg_uv = p;\n  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);\n}\n";

const SCENE_HEADER: &str = "\
uniform vec3 iResolution;
uniform float iTime;
uniform float iTimeDelta;
uniform float iFrameRate;
uniform int iFrame;
uniform vec4 iMouse;
uniform vec4 iDate;
uniform float iSampleRate;
uniform sampler2D iChannel0;
uniform sampler2D iChannel1;
uniform sampler2D iChannel2;
uniform sampler2D iChannel3;
uniform sampler2D iChannel4;
uniform sampler2D iChannel5;
uniform sampler2D iChannel6;
uniform sampler2D iChannel7;
uniform vec3 iChannelResolution[8];
uniform float iChannelTime[8];
uniform vec4 iChannelMeta[8];
uniform float iMovieTime;
uniform float iShotTime;
uniform float iShotDuration;
uniform float iShotProgress;
out vec4 shotglass_out;
";

/// The scene header's samplers, one per channel (`movie::N_CHANNELS`).
const CHANNEL_UNIFORMS: [&str; N_CHANNELS] = [
    "iChannel0",
    "iChannel1",
    "iChannel2",
    "iChannel3",
    "iChannel4",
    "iChannel5",
    "iChannel6",
    "iChannel7",
];

const SCENE_FOOTER: &str = "void main(){\n  vec4 c = vec4(0.0, 0.0, 0.0, 1.0);\n  mainImage(c, gl_FragCoord.xy);\n  shotglass_out = vec4(c.rgb, 1.0);\n}";

/// A buffer's footer (#134): the raw state, any values (no premultiply, no clamp).
const BUFFER_FOOTER: &str = "void main(){\n  vec4 c = vec4(0.0, 0.0, 0.0, 1.0);\n  mainImage(c, gl_FragCoord.xy);\n  shotglass_out = c;\n}";

const TRANSITION_HEADER: &str = "\
uniform sampler2D uFrom;
uniform sampler2D uTo;
uniform vec2 uFromScale;
uniform vec2 uToScale;
uniform vec2 uFromMax;
uniform vec2 uToMax;
uniform vec2 uOutRes;
uniform float progress;
uniform float ratio;
uniform bool sg_flip;
out vec4 shotglass_out;
vec4 getFromColor(vec2 uv) { return texture(uFrom, min(clamp(uv, 0.0, 1.0) * uFromScale, uFromMax)); }
vec4 getToColor(vec2 uv) { return texture(uTo, min(clamp(uv, 0.0, 1.0) * uToScale, uToMax)); }
";

// With `sg_flip` the output is drawn upside down, so a readback (bottom row first)
// comes out top row first. Mirrored pixel centres are exact: H - (y + 0.5).
const TRANSITION_FOOTER: &str = "void main(){\n  vec2 fc = gl_FragCoord.xy;\n  if (sg_flip) fc.y = uOutRes.y - fc.y;\n  shotglass_out = vec4(transition(fc / uOutRes).rgb, 1.0);\n}";

/// An RGBA8 image, one `[r, g, b, a]` cell per pixel, top row first.
pub type Frame = toodee::TooDee<[u8; 4]>;

/// CPU readback formats. The GL format, type and byte size must agree.
pub(crate) enum ReadFormat {
    Rgba8,
    RgbaF32,
    RgbaU32,
}

/// Read a rectangle into a byte slice with `row_length` pixels per row at `offset`.
/// Leaves the pack buffer unbound and restores the default pack layout.
pub(crate) fn read_pixels_into(
    gl: &glow::Context,
    (fbo, attachment): (glow::Framebuffer, u32),
    [x, y, w, h]: [i32; 4],
    format: ReadFormat,
    dst: &mut [u8],
    (row_length, (dx, dy)): (usize, (usize, usize)),
) {
    let (format, ty, pixel_bytes) = match format {
        ReadFormat::Rgba8 => (glow::RGBA, glow::UNSIGNED_BYTE, 4),
        ReadFormat::RgbaF32 => (glow::RGBA, glow::FLOAT, 16),
        ReadFormat::RgbaU32 => (glow::RGBA_INTEGER, glow::UNSIGNED_INT, 16),
    };
    assert!(w > 0 && h > 0, "readback rectangle must be nonempty");
    let right = dx
        .checked_add(w as usize)
        .filter(|&right| right <= row_length)
        .expect("readback extends past the destination row");
    let end = dy
        .checked_add(h as usize - 1)
        .and_then(|row| row.checked_mul(row_length))
        .and_then(|row| row.checked_add(right))
        .and_then(|pixels| pixels.checked_mul(pixel_bytes));
    assert!(
        end.is_some_and(|end| end <= dst.len()),
        "readback extends past the destination slice"
    );
    let row_length = i32::try_from(row_length).expect("readback row length exceeds GL limits");
    let dx = i32::try_from(dx).expect("readback column offset exceeds GL limits");
    let dy = i32::try_from(dy).expect("readback row offset exceeds GL limits");

    let pack = [
        (glow::PACK_ALIGNMENT, 1, 4),
        (glow::PACK_ROW_LENGTH, row_length, 0),
        (glow::PACK_SKIP_PIXELS, dx, 0),
        (glow::PACK_SKIP_ROWS, dy, 0),
    ];
    // SAFETY: The layout above bounds every byte GL can write, and unbinding
    // the pack buffer makes GL interpret the slice pointer as client memory.
    unsafe {
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.read_buffer(attachment);
        gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
        if !gl.version().is_embedded {
            gl.pixel_store_i32(glow::PACK_SWAP_BYTES, 0);
        }
        for (name, value, _) in pack {
            gl.pixel_store_i32(name, value);
        }
        gl.read_pixels(
            x,
            y,
            w,
            h,
            format,
            ty,
            glow::PixelPackData::Slice(Some(dst)),
        );
        for (name, _, default) in pack {
            gl.pixel_store_i32(name, default);
        }
        gl.read_buffer(glow::COLOR_ATTACHMENT0);
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
    }
}

/// Error messages one after another (each `Display` ends with its own newline).
pub fn join_errors<E: std::fmt::Display>(errors: &[E]) -> String {
    errors.iter().map(|e| e.to_string()).collect()
}

/// A shader that failed to compile, with messages mapped to file:line.
#[derive(Clone, Debug)]
pub struct ShaderError {
    pub what: String,
    pub messages: Vec<String>,
}

impl std::fmt::Display for ShaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{}:", self.what)?;
        for m in &self.messages {
            writeln!(f, "  {m}")?;
        }
        Ok(())
    }
}

struct Program {
    prog: glow::Program,
    locs: HashMap<String, glow::UniformLocation>,
}

impl Program {
    fn loc(&self, name: &str) -> Option<&glow::UniformLocation> {
        self.locs.get(name)
    }
}

/// An off-screen colour target. Allocated at `alloc` size; a frame may use only the
/// bottom-left `used` part (that's how render scale works without reallocating).
struct Target {
    fbo: glow::Framebuffer,
    tex: glow::Texture,
    alloc: (i32, i32),
    used: (i32, i32),
}

/// A program's compile state. Programs compile lazily, nearest the playhead first.
enum State {
    Idle,
    Pending(Pending),
    Ready(Program),
    Failed(Vec<String>),
}

struct Pending {
    prog: glow::Program,
    shaders: Vec<glow::Shader>,
    from_binary: bool,
}

struct Entry {
    src: Assembled,
    what: String,
    state: State,
    last_used: u64,
}

/// Program keys for one shot: its scene and the transition into it, and the textures
/// on its channels.
#[derive(Clone, Copy, PartialEq)]
struct ShotKeys {
    scene: u64,
    trans: Option<u64>,
    channels: [Option<ChanKey>; N_CHANNELS],
}

/// A texture on a channel: what's uploaded (`tex`, a `TexKey`) and how it's sampled.
#[derive(Clone, Copy, PartialEq)]
struct ChanKey {
    tex: u64,
    filter: Filter,
    wrap: Wrap,
    /// A footage channel's `FootageEntry` (#70); `tex` is then per frame (`resolve`).
    footage: Option<u64>,
    /// `iChannelTime`: the source time of the footage frame shown (0 otherwise).
    time: f32,
}

/// One footage channel of one shot or layer in one movie version (#70): where its
/// frames are and which one shows on each film frame. Frames are ordinary textures in
/// the store, keyed by their file's path, size and mtime (and how texels are made), so
/// versions and channels showing the same file share one upload.
struct FootageEntry {
    scan: std::sync::Arc<crate::footage::Scan>,
    map: crate::movie::FrameMap,
    /// "renders/" (the pattern's folder as written), for frame labels.
    folder: String,
    vflip: bool,
    srgb: bool,
    mipmap: bool,
    /// Source frame → texture key, filled as frames are first needed (one `stat` each).
    frames: HashMap<u64, u64>,
    /// The movie version (`Renderer::generation`) this entry belongs to.
    generation: u64,
}

/// A texture's load state. Textures load lazily, nearest the playhead first, like
/// programs: decoded on a worker thread, then uploaded in bands of rows in `pump`.
enum TexState {
    Idle,
    /// The worker is reading and decoding the file.
    Decoding,
    /// RGBA8 on the CPU, waiting for its upload to start.
    Decoded(DecodedImage),
    /// Level 0 partly uploaded: rows below `row` are submitted.
    Uploading {
        tex: glow::Texture,
        img: DecodedImage,
        row: usize,
    },
    /// Every band and the mipmaps are submitted (GL runs commands in order, so a draw
    /// issued now sees them: nothing to wait for).
    Ready {
        tex: glow::Texture,
        w: usize,
        h: usize,
        bytes: usize,
        /// `iChannelMeta` (a text block's metrics; zeros for images).
        meta: [f32; 4],
    },
    Failed(String),
}

/// What a texture is made from.
#[derive(Clone)]
enum TexSource {
    /// A PNG or JPEG file.
    File(PathBuf),
    /// A text block, laid out and rasterised (#68).
    Text(Box<crate::movie::TextSource>),
    /// A sequence frame (#70): a file that must have the sequence's size.
    Frame(PathBuf, Option<(usize, usize)>),
}

impl TexSource {
    fn load(&self, label: &str, vflip: bool) -> std::result::Result<DecodedImage, String> {
        match self {
            TexSource::File(path) => crate::texture::load(path, label, vflip),
            TexSource::Text(t) => crate::texture::load_text(t, vflip),
            TexSource::Frame(path, size) => {
                let img = crate::texture::load(path, label, vflip)?;
                match size {
                    Some((w, h)) if (img.width, img.height) != (*w, *h) => Err(format!(
                        "{label}: {}×{}, but the sequence is {w}×{h} (its first frame)",
                        img.width, img.height
                    )),
                    _ => Ok(img),
                }
            }
        }
    }
}

struct TexEntry {
    source: TexSource,
    label: String,
    vflip: bool,
    srgb: bool,
    /// Make a mipmap chain after the last band (images always; footage only with
    /// `filter = "mipmap"`).
    mipmap: bool,
    /// A footage frame (#70): its own upload budget and in-flight cap, dropped from the
    /// store when evicted, and counted in `blocking_footage_loads`.
    footage: bool,
    /// Footage: the newest movie version that needed this frame (`errors` reports the
    /// current version's failed frames without walking every footage entry).
    generation: u64,
    state: TexState,
    last_used: u64,
}

type DecodeJob = (u64, TexSource, String, bool);
type DecodeResult = (u64, std::result::Result<DecodedImage, String>);

/// The decode workers (#70: a small pool shared by images, text and footage): jobs
/// (key, source, label, vflip) in, decoded images (or rasterised text) out. Jobs are
/// sent nearest the playhead first and few are in flight, so the queue's order is
/// the priority order.
struct Decoder {
    jobs: std::sync::mpsc::Sender<DecodeJob>,
    results: std::sync::mpsc::Receiver<DecodeResult>,
    workers: usize,
}

impl Decoder {
    fn new() -> Decoder {
        let (jobs, rx) = std::sync::mpsc::channel::<DecodeJob>();
        let rx = Arc::new(std::sync::Mutex::new(rx));
        let (tx, results) = std::sync::mpsc::channel();
        // min(4, cores − 2), at least 1: footage at 30 fps needs a few PNG decodes at once.
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        let workers = cores.saturating_sub(2).clamp(1, 4);
        for i in 0..workers {
            let (rx, tx) = (rx.clone(), tx.clone());
            std::thread::Builder::new()
                .name(format!("shotglass-texture-decode-{i}"))
                .spawn(move || {
                    loop {
                        // Ends when the renderer (the sender) is dropped.
                        let job = rx
                            .lock()
                            .map_err(|_| ())
                            .and_then(|r| r.recv().map_err(|_| ()));
                        let Ok((k, source, label, vflip)) = job else {
                            break;
                        };
                        if tx.send((k, source.load(&label, vflip))).is_err() {
                            break;
                        }
                    }
                })
                .expect("spawn a texture decode thread");
        }
        Decoder {
            jobs,
            results,
            workers,
        }
    }
}

/// Texel bytes uploaded per `pump` (about 1–3 ms of driver memcpy on a desktop GPU).
pub const UPLOAD_BUDGET: usize = 8 << 20;
/// Decoded images waiting for (or in) upload at once: bounds CPU memory.
const TEXTURES_IN_FLIGHT: usize = 2;
/// Decoded footage frames waiting for (or in) upload at once, per decode worker, and
/// their CPU memory cap (at least `FOOTAGE_MIN_IN_FLIGHT` frames whatever their size).
const FOOTAGE_IN_FLIGHT_PER_WORKER: usize = 3;
const FOOTAGE_MIN_IN_FLIGHT: usize = 2;
const FOOTAGE_CPU_BYTES: usize = 64 << 20;
/// Footage prefetch: the window ahead of the playhead (seconds, at least
/// `footage::MIN_FRAMES` frames), frames kept behind it (for stepping back), and how
/// soon a shot or layer must start for its first frames (its head) to load.
const FOOTAGE_WINDOW: f64 = 0.5;
const FOOTAGE_BEHIND: u64 = 2;
const FOOTAGE_HEADS: f32 = 2.0;
/// Spare GL textures kept for reuse by later frames of the same size.
const SPARE_TEXTURES: usize = 8;

/// How far ahead of the playhead programs are compiled.
pub const LOOKAHEAD: f32 = 10.0;
/// Compiled programs kept before the least recently used ones are dropped.
const MAX_READY: usize = 64;

pub struct Renderer {
    gl: Arc<glow::Context>,
    /// What the driver can compile.
    pub driver: DriverGlsl,
    /// The language the current movie's shaders are assembled in (see `dialect_for`).
    pub dialect: Dialect,
    pub gl_info: String,
    vao: glow::VertexArray,
    dummy: glow::Texture,
    /// 1×1 transparent black: a matte source layer that isn't drawn (#74).
    clear: glow::Texture,
    /// Every program source we know about, keyed by a hash of its full text.
    programs: HashMap<u64, Entry>,
    copy: Option<u64>,
    over: Option<u64>,
    /// Programs for each shot of the current movie version, then each layer.
    wanted: Vec<ShotKeys>,
    wanted_ids: Vec<String>,
    /// Last working programs per shot id, shown while a new version compiles or fails.
    fallback: HashMap<String, ShotKeys>,
    /// KHR_parallel_shader_compile: compiles run on driver threads.
    pub parallel: bool,
    binary_dir: Option<PathBuf>,
    /// GL_PROGRAM_BINARY_FORMATS (asked once at startup).
    binary_formats: Vec<u32>,
    frame_no: u64,
    /// Programs loaded from the on-disk cache instead of compiled.
    pub binary_hits: usize,
    /// Scene and transition compiles started (tests: a texture edit compiles nothing).
    pub compiles_started: usize,
    /// Strips a scene is drawn in (more when frames are very slow; see draw_scene).
    tiles: u32,
    /// `set_tiles` fixed the strip count: the GPU timer doesn't change it.
    tiles_pinned: bool,
    /// When `context_lost` last asked the driver, and how often it has (tests).
    last_error_check: Option<std::time::Instant>,
    pub error_checks: usize,
    /// The last audio block was slow: draw the next ones in flushed bands.
    audio_banded: bool,
    /// Submitted sound blocks (see `submit_audio`), keyed by ticket.
    audio_jobs: HashMap<u64, AudioJob>,
    audio_ticket: u64,
    /// Idle readback and staging buffers for sound blocks.
    audio_pbos: Vec<GpuBuf>,
    audio_staging: Vec<GpuBuf>,
    /// Reused mix input textures (texture, rows), persistent path only.
    audio_inputs: [Option<(glow::Texture, usize)>; 2],
    /// GL 4.4 / ARB_buffer_storage: buffers can stay mapped (and are, unless turned off).
    can_persist: bool,
    persistent: bool,
    /// Where sound streaming's GL calls spent their time (the caller resets it).
    pub audio_timing: AudioTiming,
    /// Sound readbacks that waited for the GPU (`wait_audio`): playback must do none.
    pub blocking_audio_reads: usize,
    max_texture_size: usize,
    /// GPU frame times not yet handed to `gpu_ms`.
    gpu_samples: std::collections::VecDeque<f32>,
    /// Compiled audio programs and the float target they render into.
    audio: HashMap<u64, Program>,
    audio_target: Option<(glow::Framebuffer, glow::Texture)>,
    out: Option<Target>,
    /// Scratch targets per sequence (#74): `[0]` the film's, `[1 + c]` comp c's.
    seqs: Vec<SeqTargets>,
    /// The sequence being drawn (whose `seqs` entry `Which` names).
    cur: usize,
    /// Comp instances' outputs this frame (`Which::Inst`), reused across frames.
    inst: Vec<Option<Target>>,
    /// The comp instances drawn last frame (comp, comp-local time): footage prefetch.
    comp_seen: Vec<(usize, f32)>,
    /// Comp times asked for by this frame's plan, ready or not: they become `comp_seen`
    /// even when the frame stalls, so prefetch queues what a stalled frame waits for.
    comp_req: Vec<(usize, f32)>,
    /// Where each sequence's items start in `wanted` (`[0]` = 0: the film's shots,
    /// then its layers; then each comp's shots and layers).
    seq_base: Vec<usize>,
    /// The matted over pass (#74), when any layer has a matte.
    over_matte: Option<u64>,
    /// Layers left out of the picture (the player's mute/solo); export never sets it.
    hidden: std::collections::HashSet<String>,
    /// Draw the output upside down for readback (`set_readback`), and whether the
    /// current output was drawn that way.
    flip_output: bool,
    out_flipped: bool,
    generation: u64,
    timer: Option<GpuTimer>,
    /// Portability warnings from assembling the current movie's shaders.
    pub warnings: Vec<String>,
    /// Every texture we know about, keyed by `TexKey` (file, length, mtime, vflip, srgb).
    textures: HashMap<u64, TexEntry>,
    /// One sampler object per (filter, wrap), made once.
    samplers: Vec<((Filter, Wrap), glow::Sampler)>,
    decoder: Option<Decoder>,
    /// Textures loaded on the spot (`usable` with `blocking`): playback must do none.
    pub blocking_texture_loads: usize,
    /// Bands of rows uploaded (tests: an upload is spread over frames).
    pub texture_bands: usize,
    /// Resident texture memory before `pump` evicts unused textures.
    texture_budget: usize,
    upload_budget: usize,
    /// `set_upload_budget` was called (tests): footage uses that budget too.
    upload_pinned: bool,
    /// Time the last `pump` spent on textures (collecting decodes, uploading).
    pub texture_ms: f32,
    /// Footage channels of the current and fallback versions (#70), by key.
    footage: HashMap<u64, FootageEntry>,
    /// Footage bytes `pump` may upload this time: two frames per footage channel on
    /// screen (one per film frame, plus catch-up), at least `upload_budget`.
    footage_budget: usize,
    /// Footage frames loaded on the spot (`usable` with `blocking`): playback must do none.
    pub blocking_footage_loads: usize,
    /// Non-blocking renders that waited for a footage frame (the Performance panel).
    pub footage_misses: usize,
    /// GL textures created for channels (tests: footage reuses evicted ones).
    pub texture_creates: usize,
    /// Why the last rendered frame drew a footage channel black: its frame failed.
    footage_failure: Option<String>,
    /// Evicted textures kept for reuse: ((w, h, srgb, mipmap), texture).
    spare: Vec<((usize, usize, bool, bool), glow::Texture)>,
    /// Feedback buffers (#134): rings, cursor, checkpoints (`fold.rs`), one fold per
    /// sequence (`[0]` the film's, `[1 + c]` comp c's, #138).
    folds: Vec<fold::Fold>,
    /// Where each sequence's buffers start in `wanted` (after every sequence's items).
    buf_base: Vec<usize>,
    /// Plan a buffer step's picture (#134, #138): (sequence, None) is its picture track
    /// without layers (`@picture`); (sequence, Some(j)) the composite up to layer j
    /// (`after`, `@input`). Either ignores the player's hidden layers.
    plan_layers: Option<(usize, Option<usize>)>,
    /// The film's output size for the fold being advanced: comps without a `size` size
    /// their buffers against it (#138), whoever reads them.
    fold_out: (i32, i32),
    /// Buffer steps drawn (tests).
    pub buffer_steps: usize,
    /// Blocking renders that had to catch a buffer up (more than one step): playback
    /// must do none.
    pub blocking_catchups: usize,
    /// Checkpoints restored (tests).
    pub buffer_restores: usize,
    /// Buffer steps the background fill drew (tests).
    pub fill_steps: usize,
    /// `preview_lookback` walks (tests).
    pub lookback_walks: usize,
}

/// A sound block submitted with `Renderer::submit_audio`, not yet collected.
/// Tickets order like submissions, and fences pass in that order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AudioTicket(u64);

struct AudioJob {
    pbo: GpuBuf,
    /// Mix inputs written for the GPU to copy from (persistent buffers only).
    staging: Option<GpuBuf>,
    fence: AudioFence,
    count: usize,
    submitted: std::time::Instant,
}

/// A pixel pack (readback) or unpack (staging) buffer for sound blocks. With
/// persistent mapping (`ptr` non-null) it stays mapped for its whole life, so reading
/// or writing it needs no GL call at all; it's only touched while no GPU command that
/// uses it is pending (its block's fence has passed, or it was just taken from the pool).
struct GpuBuf {
    gl: Arc<glow::Context>,
    buf: glow::Buffer,
    bytes: i32,
    ptr: *mut u8,
}

impl Drop for GpuBuf {
    fn drop(&mut self) {
        // Deletion unmaps the buffer. GL keeps storage alive for queued commands.
        unsafe { self.gl.free_buffer(self.buf) };
    }
}

/// A temporary pack mapping borrows its buffer and owns the pack binding.
struct AudioReadMapping<'a> {
    buffer: &'a GpuBuf,
    ptr: *mut u8,
}

impl Drop for AudioReadMapping<'_> {
    fn drop(&mut self) {
        let gl = &self.buffer.gl;
        unsafe {
            if !self.ptr.is_null() {
                gl.unmap_buffer(glow::PIXEL_PACK_BUFFER);
            }
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
        }
    }
}

struct AudioFence {
    gl: Arc<glow::Context>,
    sync: glow::Fence,
}

impl Drop for AudioFence {
    fn drop(&mut self) {
        unsafe { self.gl.free_fence(self.sync) };
    }
}

/// Restores the bindings used by submission, including on an early return.
struct AudioSubmit<'a> {
    gl: &'a glow::Context,
    input_tex: Vec<glow::Texture>,
}

impl Drop for AudioSubmit<'_> {
    fn drop(&mut self) {
        let gl = self.gl;
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            for (unit, tex) in self.input_tex.drain(..).enumerate() {
                gl.active_texture(glow::TEXTURE0 + unit as u32);
                gl.bind_texture(glow::TEXTURE_2D, None);
                gl.free_texture(tex);
            }
            gl.active_texture(glow::TEXTURE0);
            gl.use_program(None);
            gl.bind_vertex_array(None);
        }
    }
}

/// Where the GL calls of sound streaming spent their time (reset by the caller).
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioTiming {
    /// glClientWaitSync(.., 0) polls.
    pub poll_ms: f32,
    /// Reading finished blocks (a map, or plain memory when persistently mapped).
    pub read_ms: f32,
    /// Mix input uploads.
    pub upload_ms: f32,
    /// Draws and the readback into a buffer.
    pub draw_ms: f32,
    /// glFenceSync + glFlush.
    pub fence_ms: f32,
}

fn ms_since(t: std::time::Instant) -> f32 {
    t.elapsed().as_secs_f32() * 1000.0
}

/// Inputs for one frame.
pub struct FrameInfo {
    pub t: f32,
    pub dt: f32,
    pub frame: i32,
    pub scale: f32,
}

impl FrameInfo {
    /// Full-resolution frame at `t`: iFrame is the film frame on screen at `t`, so
    /// `still`, `strip` and `export` give the same picture for the same time. A loop
    /// film's `t` is taken within one period (t + k·period is the frame at t).
    pub fn at(m: &Movie, t: f32) -> FrameInfo {
        let t = m.wrap(t);
        FrameInfo {
            t,
            dt: m.fps_den as f32 / m.fps_num as f32,
            frame: m.frame_at(t).min(i32::MAX as i64) as i32,
            scale: 1.0,
        }
    }
}

impl FrameInfo {
    /// Full-resolution film frame `n` (of a loop film: frame `n` mod the period, exactly).
    pub fn frame(m: &Movie, n: u64) -> FrameInfo {
        FrameInfo::at(m, m.frame_time(m.wrap_frame(n)))
    }
}

impl Renderer {
    pub fn new(gl: Arc<glow::Context>) -> Result<Renderer> {
        unsafe {
            let glsl = gl.get_parameter_string(glow::SHADING_LANGUAGE_VERSION);
            let version = gl.get_parameter_string(glow::VERSION);
            let renderer = gl.get_parameter_string(glow::RENDERER);
            let driver = DriverGlsl::detect(&glsl, &version);
            let dialect = driver.dialect(None).map_err(|e| anyhow!(e))?;
            let vao = gl.new_vertex_array().map_err(|e| anyhow!(e))?;
            let dummy = gl.new_texture().map_err(|e| anyhow!(e))?;
            gl.bind_texture(glow::TEXTURE_2D, Some(dummy));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                1,
                1,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&[0, 0, 0, 255])),
            );
            // One level, so no mipmap filter: the default NEAREST_MIPMAP_LINEAR would
            // leave it incomplete (macOS reads it as zero, not opaque black).
            for p in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, p, glow::NEAREST as i32);
            }
            // A matte whose source layer isn't drawn this frame (#74): nothing.
            let clear = gl.new_texture().map_err(|e| anyhow!(e))?;
            gl.bind_texture(glow::TEXTURE_2D, Some(clear));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                1,
                1,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&[0, 0, 0, 0])),
            );
            for p in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, p, glow::NEAREST as i32);
            }
            gl.bind_texture(glow::TEXTURE_2D, None);
            let timer = if driver.es { None } else { GpuTimer::new(&gl) };
            let ext = gl.supported_extensions();
            let parallel = ext.contains("GL_KHR_parallel_shader_compile")
                || ext.contains("GL_ARB_parallel_shader_compile");
            if parallel {
                gl.max_shader_compiler_threads(0xFFFF_FFFF);
            }
            let formats = gl.get_parameter_i32(glow::NUM_PROGRAM_BINARY_FORMATS);
            // The formats themselves, asked once: a cached binary in any other format
            // (a corrupt file, another driver's) is stale, never handed to GL (an error).
            let mut binary_formats = vec![0i32; formats.max(0) as usize];
            if formats > 0 {
                gl.get_parameter_i32_slice(glow::PROGRAM_BINARY_FORMATS, &mut binary_formats);
            }
            let binary_formats: Vec<u32> = binary_formats.into_iter().map(|f| f as u32).collect();
            let v = gl.version();
            let can_persist = !driver.es
                && ((v.major, v.minor) >= (4, 4) || ext.contains("GL_ARB_buffer_storage"))
                && std::env::var_os("SHOTGLASS_NO_PERSISTENT").is_none();
            // Asked once: on threaded drivers every glGet is a round trip.
            let max_texture_size = gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) as usize;
            let gl_info = format!("{renderer} · OpenGL {version} · GLSL {glsl}");
            let mut samplers = vec![];
            for filter in [Filter::Mipmap, Filter::Linear, Filter::Nearest] {
                for wrap in [Wrap::Repeat, Wrap::Clamp, Wrap::Mirror] {
                    let s = gl.new_sampler().map_err(|e| anyhow!(e))?;
                    let (min, mag) = match filter {
                        Filter::Mipmap => (glow::LINEAR_MIPMAP_LINEAR, glow::LINEAR),
                        Filter::Linear => (glow::LINEAR, glow::LINEAR),
                        Filter::Nearest => (glow::NEAREST, glow::NEAREST),
                    };
                    let w = match wrap {
                        Wrap::Repeat => glow::REPEAT,
                        Wrap::Clamp => glow::CLAMP_TO_EDGE,
                        Wrap::Mirror => glow::MIRRORED_REPEAT,
                    };
                    gl.sampler_parameter_i32(s, glow::TEXTURE_MIN_FILTER, min as i32);
                    gl.sampler_parameter_i32(s, glow::TEXTURE_MAG_FILTER, mag as i32);
                    gl.sampler_parameter_i32(s, glow::TEXTURE_WRAP_S, w as i32);
                    gl.sampler_parameter_i32(s, glow::TEXTURE_WRAP_T, w as i32);
                    samplers.push(((filter, wrap), s));
                }
            }
            let binary_dir = if formats > 0 && std::env::var_os("SHOTGLASS_NO_CACHE").is_none() {
                cache_dir().map(|d| d.join(format!("{:016x}", hash(&gl_info))))
            } else {
                None
            };
            let fold_state = fold::Fold::new(&gl, ext);
            Ok(Renderer {
                warnings: vec![],
                gl_info,
                gl,
                driver,
                dialect,
                vao,
                dummy,
                clear,
                programs: HashMap::new(),
                copy: None,
                over: None,
                wanted: vec![],
                wanted_ids: vec![],
                fallback: HashMap::new(),
                parallel,
                binary_dir,
                binary_formats,
                frame_no: 0,
                binary_hits: 0,
                compiles_started: 0,
                tiles: 1,
                tiles_pinned: false,
                last_error_check: None,
                error_checks: 0,
                audio_banded: false,
                audio_jobs: HashMap::new(),
                audio_ticket: 0,
                audio_pbos: vec![],
                audio_staging: vec![],
                audio_inputs: [None, None],
                can_persist,
                persistent: can_persist,
                audio_timing: AudioTiming::default(),
                blocking_audio_reads: 0,
                max_texture_size,
                gpu_samples: Default::default(),
                audio: HashMap::new(),
                audio_target: None,
                out: None,
                seqs: vec![SeqTargets::default()],
                cur: 0,
                inst: vec![],
                comp_seen: vec![],
                comp_req: vec![],
                seq_base: vec![0],
                over_matte: None,
                hidden: Default::default(),
                flip_output: false,
                out_flipped: false,
                generation: 0,
                timer,
                textures: HashMap::new(),
                samplers,
                decoder: None,
                blocking_texture_loads: 0,
                texture_bands: 0,
                texture_budget: crate::texture::BUDGET,
                upload_budget: UPLOAD_BUDGET,
                upload_pinned: false,
                footage: HashMap::new(),
                footage_budget: UPLOAD_BUDGET,
                blocking_footage_loads: 0,
                footage_misses: 0,
                texture_creates: 0,
                footage_failure: None,
                spare: vec![],
                texture_ms: 0.0,
                folds: vec![fold_state],
                buf_base: vec![0],
                plan_layers: None,
                fold_out: (1, 1),
                buffer_steps: 0,
                blocking_catchups: 0,
                buffer_restores: 0,
                fill_steps: 0,
                lookback_walks: 0,
            })
        }
    }

    pub fn shader_cache_dir(&self) -> Option<&Path> {
        self.binary_dir.as_deref()
    }

    /// Use a different on-disk program cache (None turns it off).
    pub fn set_cache_dir(&mut self, dir: Option<PathBuf>) {
        if self.binary_dir.is_some() || dir.is_none() {
            self.binary_dir = dir;
        }
    }

    /// For renderers whose output is read back rather than shown (still, strip,
    /// export): draw the final pass upside down so `read_output_into` needs no
    /// row flip. Pixel-identical either way; the player leaves this off.
    pub fn set_readback(&mut self, on: bool) {
        self.flip_output = on;
    }

    /// Force audio blocks to be drawn in flushed bands, or not (tests).
    pub fn set_audio_banded(&mut self, on: bool) {
        self.audio_banded = on;
    }

    /// Fix the number of strips scenes are drawn in (tests); `None` goes back to
    /// choosing it from the GPU timer. Without the pin, the next frame's timer
    /// reading would put a forced count straight back to 1 on a fast driver.
    pub fn set_tiles(&mut self, n: Option<u32>) {
        self.tiles_pinned = n.is_some();
        self.tiles = n.unwrap_or(1).max(1);
    }

    /// Resident texture memory before unused textures are evicted (tests force a tiny one).
    pub fn set_texture_budget(&mut self, bytes: usize) {
        self.texture_budget = bytes;
    }

    /// Texel bytes uploaded per `pump` (tests use a small one).
    pub fn set_upload_budget(&mut self, bytes: usize) {
        self.upload_budget = bytes.max(1);
        self.upload_pinned = true;
    }

    /// The unpack state (alignment, row length, skip rows, skip pixels, unpack buffer
    /// binding), for tests: uploads must leave it at the defaults.
    pub fn unpack_state(&self) -> [i32; 5] {
        unsafe {
            [
                glow::UNPACK_ALIGNMENT,
                glow::UNPACK_ROW_LENGTH,
                glow::UNPACK_SKIP_ROWS,
                glow::UNPACK_SKIP_PIXELS,
                glow::PIXEL_UNPACK_BUFFER_BINDING,
            ]
            .map(|p| self.gl.get_parameter_i32(p))
        }
    }

    /// Compiled (ready) programs currently held.
    pub fn ready_count(&self) -> usize {
        self.programs
            .values()
            .filter(|e| matches!(e.state, State::Ready(_)))
            .count()
    }

    /// Switch to a new version of the movie. Sources are assembled now (so a missing
    /// file is reported at once); compiling happens later, nearest the playhead first.
    /// On error nothing changes.
    /// Texture keys for a shot's or layer's channels, adding new textures to the store
    /// (`Idle`). A missing file is an error for `what`; `@input` gets `INPUT_TEX`.
    fn channel_keys(
        &mut self,
        what: &str,
        item: crate::movie::ItemRef,
        movie: &Movie,
        tex_errors: &mut Vec<ShaderError>,
    ) -> [Option<ChanKey>; N_CHANNELS] {
        let channels = movie.item_channels(item);
        let mut keys = [None; N_CHANNELS];
        for (n, c) in channels.iter().enumerate() {
            let Some(c) = c else { continue };
            let pass = match &c.source {
                crate::movie::Source::Input => Some(INPUT_TEX),
                crate::movie::Source::Picture => Some(PICTURE_TEX),
                crate::movie::Source::Layer { index, .. } => Some(LAYER_TEX - *index as u64),
                crate::movie::Source::Comp { .. } => Some(COMP_TEX - n as u64),
                crate::movie::Source::Buffer { index, back, .. } => {
                    Some(fold::buf_key(*index, *back))
                }
                _ => None,
            };
            if let Some(tex) = pass {
                keys[n] = Some(ChanKey {
                    tex,
                    filter: c.filter,
                    wrap: c.wrap,
                    footage: None,
                    time: 0.0,
                });
                continue;
            }
            if let crate::movie::Source::Frames(seq) = &c.source {
                // A footage entry per version (`generation` in the key): its frame keys
                // are stat'ed afresh, so a frame re-rendered in place reloads, while the
                // previous version's entry keeps its frames for the fallback.
                let (Some(scan), Some(map)) = (&seq.scan, movie.frame_map(item, n)) else {
                    tex_errors.push(ShaderError {
                        what: "movie.toml".into(),
                        messages: vec![format!(
                            "{what}: channels[{n}]: {}: the sequence wasn't scanned",
                            c.label()
                        )],
                    });
                    continue;
                };
                let mipmap = c.filter == Filter::Mipmap;
                let fk = hash(&format!(
                    "footage\0{}\0{scan:?}\0{map:?}\0{}\0{}\0{mipmap}\0{}",
                    seq.pattern.file.display(),
                    c.vflip,
                    c.srgb,
                    self.generation + 1
                ));
                self.footage.entry(fk).or_insert(FootageEntry {
                    scan: scan.clone(),
                    map,
                    folder: seq
                        .pattern
                        .label
                        .rsplit_once('/')
                        .map_or(String::new(), |(d, _)| format!("{d}/")),
                    vflip: c.vflip,
                    srgb: c.srgb,
                    mipmap,
                    frames: HashMap::new(),
                    generation: self.generation + 1,
                });
                keys[n] = Some(ChanKey {
                    tex: FOOTAGE_UNRESOLVED,
                    filter: c.filter,
                    wrap: c.wrap,
                    footage: Some(fk),
                    time: 0.0,
                });
                continue;
            }
            // The key covers every file the texture reads (path, size, mtime) and
            // how its texels are made, so an edit to any of them reloads it.
            let mut stamp = String::new();
            let mut missing = None;
            for f in c.files() {
                match std::fs::metadata(&f.file) {
                    Ok(md) => {
                        let mtime = md
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map_or(0, |d| d.as_nanos());
                        stamp += &format!("{}\0{}\0{mtime}\0", f.file.display(), md.len());
                    }
                    Err(e) => {
                        missing = Some(format!("\"{}\": {e}", f.label));
                        break;
                    }
                }
            }
            if let Some(m) = missing {
                tex_errors.push(ShaderError {
                    what: "movie.toml".into(),
                    messages: vec![format!("{what}: channels[{n}]: {m}")],
                });
                continue;
            }
            let source = match &c.source {
                crate::movie::Source::Image(f) => TexSource::File(f.file.clone()),
                crate::movie::Source::Text(t) => TexSource::Text(t.clone()),
                _ => unreachable!("pass outputs and footage are handled above"),
            };
            let made = match &c.source {
                crate::movie::Source::Text(t) => format!("{t:?}"),
                _ => String::new(),
            };
            let tex = hash(&format!("{stamp}{made}\0{}\0{}", c.vflip, c.srgb));
            self.textures.entry(tex).or_insert(TexEntry {
                source,
                label: c.label(),
                vflip: c.vflip,
                srgb: c.srgb,
                mipmap: true,
                footage: false,
                generation: 0,
                state: TexState::Idle,
                last_used: 0,
            });
            keys[n] = Some(ChanKey {
                tex,
                filter: c.filter,
                wrap: c.wrap,
                footage: None,
                time: 0.0,
            });
        }
        keys
    }

    pub fn set_movie(&mut self, movie: &Movie) -> std::result::Result<(), Vec<ShaderError>> {
        let dialect = self.dialect_for(movie).map_err(|e| {
            vec![ShaderError {
                what: "movie.toml".into(),
                messages: vec![e],
            }]
        })?;
        let cues = movie.cue_defines();
        let mut errors = vec![];
        let mut warnings: Vec<String> = vec![];
        let mut add = |r: &mut Renderer, what: String, src: Result<Assembled>| -> Option<u64> {
            match src {
                Ok(src) => {
                    warnings.extend(src.warnings.iter().map(|w| format!("{what}: {w}")));
                    let k = hash(&src.text);
                    r.programs.entry(k).or_insert(Entry {
                        src,
                        what,
                        state: State::Idle,
                        last_used: 0,
                    });
                    Some(k)
                }
                Err(e) => {
                    errors.push(ShaderError {
                        what,
                        messages: vec![format!("{e:#}")],
                    });
                    None
                }
            }
        };
        let src = transition_source(dialect, "copy", &[], &cues, movie);
        let copy = add(self, "copy pass".into(), src);
        let mut wanted = vec![];
        let mut ids: Vec<String> = vec![];
        let mut seq_base = vec![];
        let mut tex_errors = vec![];
        // The film, then each comp (#74): a comp's items come after the film's in
        // `wanted`, its ids are "<comp>/<id>" (fallback keys), and its shaders get the
        // film's cues plus its own (`COMP_*`, comp-local seconds).
        let seqs: Vec<(String, &Movie, String)> =
            std::iter::once((String::new(), movie, cues.clone()))
                .chain(movie.comps.iter().map(|c| {
                    (
                        format!("{}/", c.name),
                        &*c.movie,
                        seq_cues(movie, Some(&c.movie)),
                    )
                }))
                .collect();
        for (pre, seq, cues) in &seqs {
            seq_base.push(wanted.len());
            let label = if pre.is_empty() {
                String::new()
            } else {
                format!("comp \"{}\": ", pre.trim_end_matches('/'))
            };
            for (i, shot) in seq.shots.iter().enumerate() {
                let scene = match shot.comp {
                    // A comp shot draws no program of its own: the copy pass stands in, so
                    // its keys are ready as soon as its transition and the comp are.
                    Some(_) => copy,
                    None => {
                        let src = scene_source(dialect, shot, cues, seq);
                        add(
                            self,
                            format!(
                                "{label}shot \"{}\" ({})",
                                shot.id,
                                rel(&shot.scene, &movie.root)
                            ),
                            src,
                        )
                    }
                };
                let trans = match &shot.transition_in {
                    Some(t) => {
                        let src = transition_source(dialect, &t.kind, &t.params, cues, seq);
                        add(
                            self,
                            format!("{label}transition into \"{}\" ({})", shot.id, t.kind),
                            src,
                        )
                        .map(Some)
                    }
                    None => Some(None),
                };
                let channels = self.channel_keys(
                    &format!("{label}shot \"{}\"", shot.id),
                    crate::movie::ItemRef::Shot(i),
                    seq,
                    &mut tex_errors,
                );
                if let (Some(scene), Some(trans)) = (scene, trans) {
                    wanted.push(ShotKeys {
                        scene,
                        trans,
                        channels,
                    });
                    ids.push(format!("{pre}{}", shot.id));
                }
            }
            // Layers (#69): their programs and channels follow the shots in `wanted`.
            for (j, layer) in seq.layers.iter().enumerate() {
                let scene = match &layer.scene {
                    LayerScene::Comp(_) => copy,
                    LayerScene::File(p) => add(
                        self,
                        format!("{label}layer \"{}\" ({})", layer.id, rel(p, &movie.root)),
                        layer_source(dialect, layer, cues, seq),
                    ),
                    LayerScene::Pass(k) => add(
                        self,
                        format!("{label}layer \"{}\" ({k})", layer.id),
                        layer_source(dialect, layer, cues, seq),
                    ),
                };
                let blend = match &layer.blend {
                    Some(b) => add(
                        self,
                        format!("{label}blend of layer \"{}\" ({b})", layer.id),
                        blend_source(dialect, layer, b, cues, seq),
                    )
                    .map(Some),
                    None => Some(None),
                };
                let channels = self.channel_keys(
                    &format!("{label}layer \"{}\"", layer.id),
                    crate::movie::ItemRef::Layer(j),
                    seq,
                    &mut tex_errors,
                );
                if let (Some(scene), Some(blend)) = (scene, blend) {
                    // A layer's blend program rides in `trans`: keys_ready needs it too.
                    wanted.push(ShotKeys {
                        scene,
                        trans: blend,
                        channels,
                    });
                    ids.push(format!("{pre}{}", layer.id));
                }
            }
        }
        // Buffers (#134) after every sequence's items, the film's then each comp's
        // (#138): the scene, its init pass in `trans` (keys_ready needs both), its
        // channels.
        let mut buf_base = vec![];
        for (pre, seq, cues) in &seqs {
            buf_base.push(wanted.len());
            let label = if pre.is_empty() {
                String::new()
            } else {
                format!("comp \"{}\": ", pre.trim_end_matches('/'))
            };
            for (b, buf) in seq.buffers.iter().enumerate() {
                let what = format!("{label}buffer \"{}\"", buf.id);
                let scene = add(
                    self,
                    format!("{what} ({})", rel(&buf.scene, &movie.root)),
                    buffer_source(dialect, buf, &buf.scene, cues, seq),
                );
                let init = match &buf.init {
                    Some(p) => add(
                        self,
                        format!("init of {what} ({})", rel(p, &movie.root)),
                        buffer_source(dialect, buf, p, cues, seq),
                    )
                    .map(Some),
                    None => Some(None),
                };
                let channels = self.channel_keys(
                    &what,
                    crate::movie::ItemRef::Buffer(b),
                    seq,
                    &mut tex_errors,
                );
                if let (Some(scene), Some(init)) = (scene, init) {
                    wanted.push(ShotKeys {
                        scene,
                        trans: init,
                        channels,
                    });
                    ids.push(format!("[buffer]{pre}{}", buf.id));
                }
            }
        }
        let over = if seqs.iter().all(|(_, s, _)| s.layers.is_empty()) {
            None
        } else {
            let src = transition_source(dialect, "over", &[], &cues, movie);
            add(self, "layer composite".into(), src)
        };
        let over_matte = if seqs
            .iter()
            .any(|(_, s, _)| s.layers.iter().any(|l| l.matte.is_some()))
        {
            let src = over_matte_source(dialect, &cues, movie);
            add(self, "matted layer composite".into(), src)
        } else {
            None
        };
        errors.extend(tex_errors);
        warnings.dedup();
        self.warnings = warnings;
        if !errors.is_empty() {
            self.collect_garbage();
            return Err(errors);
        }
        // Remember what currently works, per item id, for while the new version compiles.
        for (id, k) in self.wanted_ids.clone().iter().zip(self.wanted.clone()) {
            if self.keys_ready(k) {
                self.fallback.insert(id.clone(), k);
            }
        }
        self.dialect = dialect;
        self.wanted = wanted;
        self.fallback.retain(|id, _| ids.contains(id));
        self.wanted_ids = ids;
        self.seq_base = seq_base;
        self.buf_base = buf_base;
        // One fold per sequence (#138): comps removed take theirs (and its targets).
        for mut f in self
            .folds
            .drain(seqs.len().min(self.folds.len())..)
            .collect::<Vec<_>>()
        {
            for t in f.targets() {
                unsafe {
                    self.gl.free_framebuffer(t.fbo);
                    self.gl.free_texture(t.tex);
                }
            }
        }
        while self.folds.len() < seqs.len() {
            let f = fold::Fold::like(&self.folds[0]);
            self.folds.push(f);
        }
        // What a fold reads besides program and texture keys (#134, "Invalidation").
        for (q, (_, seq, _)) in seqs.iter().enumerate() {
            // A buffer reading the layer stack (#138) reads the picture track too.
            let reach = crate::movie::stack_reach(seq);
            let reads_film = reach.is_some()
                || seq.buffers.iter().any(|b| {
                    b.channels.iter().flatten().any(|c| {
                        matches!(
                            c.source,
                            crate::movie::Source::Picture | crate::movie::Source::Comp { .. }
                        )
                    })
                });
            let size = match q {
                0 => None,
                _ => movie.comps[q - 1].size,
            };
            // A sequence whose buffers were all removed frees its rings and checkpoints
            // now (its fold isn't advanced any more, so nothing else would).
            if seq.buffers.is_empty() && self.folds[q].holds_targets() {
                for t in self.folds[q].release() {
                    unsafe {
                        self.gl.free_framebuffer(t.fbo);
                        self.gl.free_texture(t.tex);
                    }
                }
            }
            let f = &mut self.folds[q];
            f.reads_film = reads_film;
            f.decl = hash(&format!(
                "{:?}\0{}/{}\0{:?}/{}\0{}\0{:?}\0{}",
                seq.buffers,
                seq.fps_num,
                seq.fps_den,
                seq.loop_frames,
                seq.warm,
                seq.duration,
                size,
                if reads_film {
                    format!(
                        "{:?}{:?}{:?}",
                        seq.shots,
                        movie.comps,
                        &seq.layers[..reach.map_or(0, |r| r + 1)]
                    )
                } else {
                    String::new()
                }
            ));
        }
        // Sequences gone with a removed comp: free their targets, not just the handles.
        for s in self
            .seqs
            .drain(seqs.len().min(self.seqs.len())..)
            .collect::<Vec<_>>()
        {
            for x in seq_targets(s) {
                unsafe {
                    self.gl.free_framebuffer(x.fbo);
                    self.gl.free_texture(x.tex);
                }
            }
        }
        self.seqs.resize_with(seqs.len(), SeqTargets::default);
        // A sequence that lost its layers frees their targets; held outputs past the
        // last layer go too (they'd stay full size until the next resize otherwise).
        let mut gone = vec![];
        for (s, (_, seq, _)) in self.seqs.iter_mut().zip(&seqs) {
            if seq.layers.is_empty() {
                gone.extend(s.work.iter_mut().map(Option::take));
                gone.extend([s.layer.take(), s.picture.take(), s.input.take()]);
                gone.append(&mut s.held);
            } else if s.held.len() > seq.layers.len() {
                gone.extend(s.held.drain(seq.layers.len()..));
            }
        }
        // Comp instance targets are sized per frame plan: an edit starts them over, so
        // instances of a removed or resized comp don't stay allocated.
        gone.append(&mut self.inst);
        for x in gone.into_iter().flatten() {
            unsafe {
                self.gl.free_framebuffer(x.fbo);
                self.gl.free_texture(x.tex);
            }
        }
        self.over = over;
        self.over_matte = over_matte;
        self.copy = copy;
        if let Some(c) = copy {
            self.compile_now(c);
        }
        self.generation += 1;
        for s in &mut self.seqs {
            s.freeze_key = None;
        }
        self.collect_garbage();
        Ok(())
    }

    /// Compile everything now (for headless commands). Returns compile errors.
    pub fn load_all(&mut self, movie: &Movie) -> std::result::Result<usize, Vec<ShaderError>> {
        self.set_movie(movie)?;
        let mut keys: Vec<u64> = self
            .wanted
            .iter()
            .flat_map(|k| [Some(k.scene), k.trans])
            .flatten()
            .collect();
        keys.sort_unstable();
        keys.dedup();
        let fresh = keys.iter().filter(|k| self.is_idle(**k)).count();
        // Start them all first so a parallel-compiling driver can overlap them.
        for k in &keys {
            self.start(*k);
        }
        for k in &keys {
            self.compile_now(*k);
        }
        let mut texs: Vec<u64> = self
            .wanted
            .iter()
            .flat_map(|k| k.channels.iter().flatten().map(|c| c.tex))
            .collect();
        texs.sort_unstable();
        texs.dedup();
        for k in texs {
            self.load_texture_now(k);
        }
        let errors = self.errors();
        if errors.is_empty() {
            Ok(fresh)
        } else {
            Err(errors)
        }
    }

    /// Errors of the current version's programs.
    /// The GL textures of sequence `s`'s scratch targets (selftest: removed comps
    /// free theirs).
    pub fn seq_textures(&self, s: usize) -> Vec<glow::Texture> {
        let Some(q) = self.seqs.get(s) else {
            return vec![];
        };
        [&q.a, &q.b, &q.freeze, &q.layer, &q.picture]
            .into_iter()
            .chain(q.work.iter())
            .chain(q.held.iter())
            .flatten()
            .map(|x| x.tex)
            .collect()
    }

    pub fn errors(&self) -> Vec<ShaderError> {
        let mut seen = std::collections::HashSet::new();
        let mut out = vec![];
        for k in self
            .wanted
            .iter()
            .flat_map(|k| [Some(k.scene), k.trans])
            .flatten()
            .chain(self.copy)
            .chain(self.over)
            .chain(self.over_matte)
        {
            if let Some(Entry {
                state: State::Failed(m),
                what,
                ..
            }) = self.programs.get(&k)
                && seen.insert(k)
            {
                out.push(ShaderError {
                    what: what.clone(),
                    messages: m.clone(),
                });
            }
        }
        // Still textures, then the footage frames this version needed that failed.
        let mut frames: Vec<(&str, u64)> = self
            .textures
            .iter()
            .filter(|(_, e)| {
                e.footage
                    && e.generation == self.generation
                    && matches!(e.state, TexState::Failed(_))
            })
            .map(|(k, e)| (e.label.as_str(), *k))
            .collect();
        frames.sort_unstable();
        let frames = frames.into_iter().map(|(_, k)| k);
        for k in self
            .wanted
            .iter()
            .flat_map(|k| k.channels.iter().flatten().map(|c| c.tex))
            .chain(frames)
        {
            if let Some(TexEntry {
                state: TexState::Failed(m),
                ..
            }) = self.textures.get(&k)
                && seen.insert(k)
            {
                out.push(ShaderError {
                    what: "texture".into(),
                    messages: vec![m.clone()],
                });
            }
        }
        out
    }

    /// (ready, total) programs for the current version.
    pub fn progress(&self) -> (usize, usize) {
        let keys: std::collections::HashSet<u64> = self
            .wanted
            .iter()
            .flat_map(|k| [Some(k.scene), k.trans])
            .flatten()
            .collect();
        let ready = keys
            .iter()
            .filter(|k| matches!(self.state(**k), Some(State::Ready(_) | State::Failed(_))))
            .count();
        (ready, keys.len())
    }

    /// Per-frame housekeeping: finish compiles that are done, start the ones the
    /// playhead will need next, compile the rest in the background, evict old ones.
    /// Tests (gfx::glstate::set_audit): record GL state a public entry point left
    /// behind. Off, this is one relaxed atomic load.
    fn audit(&self, what: &str) {
        glstate::audit(&self.gl, what);
    }

    pub fn pump(&mut self, movie: &Movie, t: f32, background: bool) {
        self.pump_inner(movie, t, background);
        self.audit("pump");
    }

    fn pump_inner(&mut self, movie: &Movie, t: f32, background: bool) {
        self.frame_no += 1;
        let pending: Vec<u64> = self
            .programs
            .iter()
            .filter(|(_, e)| matches!(e.state, State::Pending(_)))
            .map(|(k, _)| *k)
            .collect();
        for k in pending {
            let done = match self.state(k) {
                Some(State::Pending(p)) => {
                    !self.parallel || unsafe { self.gl.get_program_completion_status(p.prog) }
                }
                _ => false,
            };
            if done {
                self.compile_now(k);
            }
        }
        let comp_dist = comp_distances(movie, t);
        let order = self.priority(movie, t, &comp_dist);
        let in_flight = |r: &Renderer| {
            r.programs
                .values()
                .filter(|e| matches!(e.state, State::Pending(_)))
                .count()
        };
        'compile: {
            for (k, dist) in &order {
                if *dist > LOOKAHEAD {
                    break;
                }
                if self.is_idle(*k) {
                    if self.parallel {
                        self.start(*k);
                    } else {
                        // Without parallel compiling a compile blocks: do one per frame.
                        self.compile_now(*k);
                        break 'compile;
                    }
                }
            }
            if background {
                // Everything else, so errors show up early. Few at a time.
                for (k, _) in &order {
                    if in_flight(self) >= 2 {
                        break;
                    }
                    if self.is_idle(*k) {
                        if self.parallel {
                            self.start(*k);
                        } else {
                            self.compile_now(*k);
                            break;
                        }
                    }
                }
            }
        }
        let t_tex = std::time::Instant::now();
        let mut tex_order = self.texture_order(movie, t, &comp_dist);
        let footage = !self.footage.is_empty();
        if footage {
            tex_order.extend(self.footage_order(movie, t, &comp_dist));
            tex_order.sort_by(|a, b| a.1.total_cmp(&b.1));
            let mut seen = std::collections::HashSet::new();
            tex_order.retain(|(k, _)| seen.insert(*k));
        }
        self.pump_textures(&tex_order, background);
        // Footage turns over every film frame: keep it within the budget every pump.
        if footage {
            self.evict_textures(&tex_order);
        }
        self.texture_ms = ms_since(t_tex);
        // A ready frame only proves that one frame of a sequence works. Keep
        // the previous version available for other frames, subject to texture eviction.
        if !self.fallback.is_empty() {
            let ready: Vec<_> = self
                .wanted_ids
                .iter()
                .zip(&self.wanted)
                .filter(|(id, k)| {
                    self.fallback.contains_key(*id)
                        && k.channels.iter().flatten().all(|c| c.footage.is_none())
                        && self.keys_ready(**k)
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in ready {
                self.fallback.remove(&id);
            }
        }
        if self.frame_no.is_multiple_of(30) {
            self.collect_garbage();
            self.evict(&order);
            if !footage {
                self.evict_textures(&tex_order);
            }
        }
    }

    /// Leave these layers out of the picture (the player's mute/solo). Their `@<id>`
    /// readers see black. Export and the CLI never call it.
    pub fn set_hidden_layers(&mut self, ids: std::collections::HashSet<String>) {
        self.hidden = ids;
    }

    pub fn has_gpu_timer(&self) -> bool {
        self.timer.is_some()
    }

    /// The newest GPU frame time measured since the last call, if any.
    pub fn gpu_ms(&mut self) -> Option<f32> {
        self.poll_timer();
        let last = self.gpu_samples.back().copied();
        self.gpu_samples.clear();
        last
    }

    fn poll_timer(&mut self) {
        let gl = self.gl.clone();
        if let Some(ms) = self.timer.as_mut().and_then(|t| t.poll(&gl)) {
            // Keep each strip under ~100 ms of GPU time.
            let per_tile = ms / self.tiles as f32;
            let off = per_tile > 100.0 || (self.tiles > 1 && ms < 50.0 * self.tiles as f32);
            if off && !self.tiles_pinned {
                self.tiles = ((ms / 100.0).ceil() as u32).clamp(1, 64);
            }
            self.gpu_samples.push_back(ms);
            if self.gpu_samples.len() > 16 {
                self.gpu_samples.pop_front();
            }
        }
    }

    /// True if the driver reset the GPU (a shader ran too long) or lost the context.
    ///
    /// `glGetError` makes the CPU wait for drivers that run GL on their own thread
    /// (NVIDIA's "threaded optimization"), often for a whole display refresh, so it is
    /// only asked when there's a reason: `suspicious` (the caller saw a long frame
    /// gap: a driver reset hangs the GPU for seconds first), a frame that took over
    /// 500 ms on the GPU, or, if `periodic` (not playing, where a hitch is harmless),
    /// about once a second.
    pub fn context_lost(&mut self, periodic: bool, suspicious: bool) -> bool {
        let slow_gpu = self.gpu_samples.back().is_some_and(|ms| *ms > 500.0);
        let due = periodic
            && self
                .last_error_check
                .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1));
        if !suspicious && !slow_gpu && !due {
            return false;
        }
        self.last_error_check = Some(std::time::Instant::now());
        self.error_checks += 1;
        unsafe { self.gl.get_error() == glow::CONTEXT_LOST }
    }

    /// Size of the output texture (the last render's w × h), if anything was rendered.
    pub fn output_size(&self) -> Option<(usize, usize)> {
        self.out
            .as_ref()
            .map(|t| (t.alloc.0 as usize, t.alloc.1 as usize))
    }

    /// The GL context, for engine passes that live outside the renderer (export's
    /// YUV pass, `yuv.rs`).
    pub(crate) fn gl(&self) -> &Arc<glow::Context> {
        &self.gl
    }

    /// Whether the last render's output was drawn upside down (`set_readback`):
    /// then its texture row 0 is the image's top row.
    pub(crate) fn output_flipped(&self) -> bool {
        self.out_flipped
    }

    /// Read the output texture as an RGBA8 frame, top row first.
    pub fn read_output(&self) -> Option<Frame> {
        let (w, h) = self.output_size()?;
        let mut frame = Frame::init(w, h, [0; 4]);
        self.read_output_into(&mut frame, (0, 0));
        Some(frame)
    }

    /// Read the output into `dst` with its top-left corner at (x, y), top row
    /// first, straight into place: no intermediate frame, and no row flip when the
    /// output was drawn for readback (`set_readback`). Pixels of `dst` outside that
    /// window are untouched. Panics if the window doesn't fit, or if nothing has
    /// been rendered yet.
    pub fn read_output_into(&self, dst: &mut Frame, (x, y): (usize, usize)) {
        self.read_output_into_inner(dst, (x, y));
        self.audit("read_output_into");
    }

    fn read_output_into_inner(&self, dst: &mut Frame, (x, y): (usize, usize)) {
        let t = self
            .out
            .as_ref()
            .expect("read_output_into before any render");
        let (w, h) = t.alloc;
        let dw = dst.num_cols();
        // GL fills rows bottom-up; with a flipped output, GL's first row is the
        // image's top row, so it goes at row y. Otherwise we flip afterwards.
        read_pixels_into(
            &self.gl,
            (t.fbo, glow::COLOR_ATTACHMENT0),
            [0, 0, w, h],
            ReadFormat::Rgba8,
            dst.data_mut().as_flattened_mut(),
            (dw, (x, y)),
        );
        if !self.out_flipped {
            dst.view_mut((x, y), (x + w as usize, y + h as usize))
                .flip_rows();
        }
    }

    /// The pack state (alignment, row length, skips, buffer), for tests.
    pub fn pack_state(&self) -> [i32; 5] {
        unsafe {
            [
                glow::PACK_ALIGNMENT,
                glow::PACK_ROW_LENGTH,
                glow::PACK_SKIP_PIXELS,
                glow::PACK_SKIP_ROWS,
                glow::PIXEL_PACK_BUFFER_BINDING,
            ]
            .map(|p| self.gl.get_parameter_i32(p))
        }
    }

    pub fn finish(&self) {
        unsafe { self.gl.finish() };
    }

    /// The language `m`'s shaders compile in on this driver: the portable baseline,
    /// or the film's `glsl` opt-in. Err (a message for the user) if the driver can't
    /// provide the opt-in.
    pub fn dialect_for(&self, m: &Movie) -> std::result::Result<Dialect, String> {
        self.driver.dialect(m.glsl)
    }
}

/// A readback (`pack`) or staging buffer of at least `bytes`, from the pool or new.
/// New ones are persistently mapped when the driver can.
unsafe fn take_buf(
    gl: &Arc<glow::Context>,
    pool: &mut Vec<GpuBuf>,
    persistent: bool,
    pack: bool,
    bytes: i32,
) -> std::result::Result<GpuBuf, String> {
    if let Some(i) = pool
        .iter()
        .position(|b| b.bytes >= bytes && b.ptr.is_null() != persistent)
    {
        return Ok(pool.swap_remove(i));
    }
    let target = if pack {
        glow::PIXEL_PACK_BUFFER
    } else {
        glow::PIXEL_UNPACK_BUFFER
    };
    unsafe {
        let buf = gl.new_buffer()?;
        gl.bind_buffer(target, Some(buf));
        let mut ptr = std::ptr::null_mut();
        if persistent {
            let access = if pack {
                glow::MAP_READ_BIT
            } else {
                glow::MAP_WRITE_BIT
            };
            let flags = access | glow::MAP_PERSISTENT_BIT | glow::MAP_COHERENT_BIT;
            gl.buffer_storage(target, bytes, None, flags);
            ptr = gl.map_buffer_range(target, 0, bytes, flags);
        } else {
            let usage = if pack {
                glow::STREAM_READ
            } else {
                glow::STREAM_DRAW
            };
            gl.buffer_data_size(target, bytes, usage);
        }
        gl.bind_buffer(target, None);
        let buffer = GpuBuf {
            gl: gl.clone(),
            buf,
            bytes,
            ptr,
        };
        if persistent && ptr.is_null() {
            return Err("mapping a persistent sound buffer failed".into());
        }
        Ok(buffer)
    }
}

/// An empty RG32F track texture of `rows` rows (filled with tex_sub_image_2d).
unsafe fn new_track_texture(
    gl: &glow::Context,
    rows: usize,
) -> std::result::Result<glow::Texture, String> {
    unsafe {
        let tex = gl.new_texture()?;
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RG32F as i32,
            TRACK_WIDTH as i32,
            rows as i32,
            0,
            glow::RG,
            glow::FLOAT,
            glow::PixelUnpackData::Slice(None),
        );
        for p in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, p, glow::NEAREST as i32);
        }
        Ok(tex)
    }
}

/// Upload interleaved stereo as an RG32F texture, `TRACK_WIDTH` samples per row.
unsafe fn upload_track(
    gl: &glow::Context,
    data: &[f32],
    max: usize,
) -> std::result::Result<glow::Texture, String> {
    let n = (data.len() / 2).max(1);
    let rows = n.div_ceil(TRACK_WIDTH);
    if rows > max {
        return Err(format!(
            "the soundtrack is too long for one texture ({rows} rows, GPU max {max})"
        ));
    }
    let mut padded = vec![0f32; rows * TRACK_WIDTH * 2];
    padded[..data.len()].copy_from_slice(data);
    let bytes =
        unsafe { std::slice::from_raw_parts(padded.as_ptr() as *const u8, padded.len() * 4) };
    unsafe {
        let tex = gl.new_texture()?;
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RG32F as i32,
            TRACK_WIDTH as i32,
            rows as i32,
            0,
            glow::RG,
            glow::FLOAT,
            glow::PixelUnpackData::Slice(Some(bytes)),
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::NEAREST as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::NEAREST as i32,
        );
        Ok(tex)
    }
}

/// Samples per row of a track texture (must match `sg_fetch` in sound.rs).
pub const TRACK_WIDTH: usize = 4096;
/// Most samples one `render_audio` call can produce.
pub const AUDIO_BLOCK: usize = 1024 * 128;

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            for (_, e) in std::mem::take(&mut self.programs) {
                self.delete_state(e.state);
            }
            for (_, p) in std::mem::take(&mut self.audio) {
                self.gl.free_program(p.prog);
            }
            self.audio_jobs.clear();
            self.audio_pbos.clear();
            self.audio_staging.clear();
            for (t, _) in self.audio_inputs.iter().flatten() {
                self.gl.free_texture(*t);
            }
            if let Some((fbo, tex)) = self.audio_target.take() {
                self.gl.free_framebuffer(fbo);
                self.gl.free_texture(tex);
            }
            let mut all: Vec<Option<Target>> = vec![self.out.take()];
            for s in std::mem::take(&mut self.seqs) {
                all.extend(seq_targets(s).into_iter().map(Some));
            }
            all.extend(std::mem::take(&mut self.inst));
            for f in &mut self.folds {
                all.extend(f.targets().into_iter().map(Some));
            }
            for t in all.into_iter().flatten() {
                self.gl.free_framebuffer(t.fbo);
                self.gl.free_texture(t.tex);
            }
            for (_, e) in std::mem::take(&mut self.textures) {
                self.delete_texture_state(e.state);
            }
            for (_, t) in std::mem::take(&mut self.spare) {
                self.gl.free_texture(t);
            }
            for (_, s) in std::mem::take(&mut self.samplers) {
                self.gl.free_sampler(s);
            }
            if let Some(t) = self.timer.take() {
                if t.active.is_some() {
                    self.gl.end_query(glow::TIME_ELAPSED);
                }
                for q in t.free.into_iter().chain(t.pending).chain(t.active) {
                    self.gl.free_query(q);
                }
            }
            self.gl.free_vertex_array(self.vao);
            self.gl.free_texture(self.dummy);
            self.gl.free_texture(self.clear);
        }
    }
}

#[derive(Clone, Copy)]
enum Which {
    A,
    B,
    Freeze,
    /// A layer working picture (#69): the picture so far, or the next one.
    Work(usize),
    /// Where a layer draws, at its scale.
    Layer,
    /// The picture track, kept for `@picture` (#72).
    Picture,
    /// A layer that later layers read (`@<id>`), at exactly its own size.
    Held(usize),
    /// A comp instance's output this frame (#74), at exactly its size.
    Inst(usize),
    /// Slot `s` of buffer `b`'s ring (#134) in sequence `q`'s fold: Ring(q, b, s).
    Ring(usize, usize, usize),
    /// The composite a buffer step reads as `@input` (#138, `after`).
    Input,
}

/// One sequence's scratch targets (#74: the film's, and each comp's).
#[derive(Default)]
struct SeqTargets {
    a: Option<Target>,
    b: Option<Target>,
    freeze: Option<Target>,
    /// Layers (#69): two full-size working pictures (the picture so far, and the next)
    /// and the scratch target a layer draws into at its scale.
    work: [Option<Target>; 2],
    layer: Option<Target>,
    /// The picture track while layers are active (for `@picture` and the first layer).
    picture: Option<Target>,
    /// Outputs of layers that later layers read, per layer index.
    held: Vec<Option<Target>>,
    /// The composite up to a layer that a buffer step reads (#138, `after`).
    input: Option<Target>,
    /// (shot, movie generation, outgoing shot's keys) of the frozen frame.
    freeze_key: Option<(usize, u64, ShotKeys)>,
}

/// Every target of a sequence's scratch set (to delete).
fn seq_targets(s: SeqTargets) -> Vec<Target> {
    let mut all = vec![s.a, s.b, s.freeze, s.layer, s.picture, s.input];
    all.extend(s.work);
    all.extend(s.held);
    all.into_iter().flatten().collect()
}

/// A comp drawn this frame (#74): comp `seq - 1` at its local time, drawn at
/// `full` × `scale` and resolved into an exactly `dest`-sized target.
#[derive(Clone, PartialEq)]
struct InstKey {
    comp: usize,
    t: u32,
    full: (i32, i32),
    scale: u32,
    dest: (i32, i32),
}

/// A planned active layer: (index, local time, keys, comp instance, matte instance).
type PlannedLayer = (usize, f32, ShotKeys, Option<usize>, Option<usize>);

/// What a sequence draws this frame, decided before any GL work (#74): the shots and
/// layers it needs, ready, and the comp instances they show.
struct SeqPlan {
    seq: usize,
    at: At,
    /// Sequence-local time and frame (the film's, or the comp's).
    t: f32,
    frame: i32,
    full: (i32, i32),
    scaled: (i32, i32),
    /// Per needed shot: its keys, its film frame (footage), and the instance it shows
    /// if it's a comp shot.
    shots: Vec<(ShotKeys, Option<usize>)>,
    /// Per item (needed shots, then active layers): `(channel pass key, instance)`.
    chans: Vec<Vec<(u64, usize)>>,
    /// Active layers.
    layers: Vec<PlannedLayer>,
    over: Option<u64>,
    /// A buffer step's picture (#138): the (virtual) fold frame its layers read buffer
    /// states at, and layers buffers read (`@<layer>`) are held. None: the frame shown
    /// (`Fold::n`).
    step: Option<u64>,
}

/// What `draw_scene` needs from a shot or a layer: its clock, uniforms and, for a
/// layer, its opacity and the picture below it (for `@input`).
struct Item<'a> {
    /// Movie time at local 0 (`iMovieTime = start + local`).
    start: f32,
    duration: f32,
    /// iTime at this frame.
    itime: f32,
    uniforms: &'a [(String, Anim)],
    /// Some for a layer (its opacity; the composite applies it). A layer whose program
    /// failed with no working version draws nothing rather than black.
    opacity: Option<f32>,
    /// Other passes' pictures a layer's channels can read (`@input`, `@picture`,
    /// `@<layer>`): (pass key, (texture, width, height)).
    passes: Vec<(u64, (glow::Texture, usize, usize))>,
}

impl<'a> Item<'a> {
    fn shot(shot: &'a Shot, local: f32) -> Item<'a> {
        Item {
            start: shot.start,
            duration: shot.duration,
            itime: shot.scene_time(local),
            uniforms: &shot.uniforms,
            opacity: None,
            passes: vec![],
        }
    }
}

fn sanitize(x: f32) -> f32 {
    if x.is_finite() {
        x.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

fn ensure_target<'a>(
    gl: &glow::Context,
    slot: &'a mut Option<Target>,
    w: i32,
    h: i32,
) -> &'a Target {
    if slot.as_ref().is_some_and(|t| t.alloc == (w, h)) {
        return slot.as_ref().unwrap();
    }
    unsafe {
        if let Some(old) = slot.take() {
            gl.free_framebuffer(old.fbo);
            gl.free_texture(old.tex);
        }
        let tex = gl.new_texture().expect("create texture");
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            w,
            h,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.bind_texture(glow::TEXTURE_2D, None);
        let fbo = gl.new_framebuffer().expect("create framebuffer");
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(tex),
            0,
        );
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        *slot = Some(Target {
            fbo,
            tex,
            alloc: (w, h),
            used: (w, h),
        });
    }
    slot.as_ref().unwrap()
}

unsafe fn set_anim(gl: &glow::Context, l: &glow::UniformLocation, a: &Anim, t: f32) {
    let v = a.eval(t);
    unsafe {
        match a.dim() {
            1 => gl.uniform_1_f32(Some(l), v[0]),
            2 => gl.uniform_2_f32(Some(l), v[0], v[1]),
            3 => gl.uniform_3_f32(Some(l), v[0], v[1], v[2]),
            _ => gl.uniform_4_f32(Some(l), v[0], v[1], v[2], v[3]),
        }
    }
}

fn hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// How soon the playhead at `t` needs `shot` (seconds); behind it counts double (only
/// needed when scrubbing back).
/// How soon the playhead at `t` needs `shot` (seconds). A loop film's first shots
/// are also near its end: the playhead reaches them again across the wrap.
fn distance(movie: &Movie, shot: &Shot, t: f32) -> f32 {
    let d = distance_once(shot, t);
    match movie.loop_period() {
        Some(p) => d.min(distance_once(shot, t - p as f32)),
        None => d,
    }
}

/// How far `t` is from a layer's span (0 inside it), like `distance` for shots.
fn layer_distance(movie: &Movie, layer: &Layer, t: f32) -> f32 {
    span_distance(movie, layer.start, layer.end, t)
}

/// How soon the playhead at `t` needs something active on `[start, end)`; behind it
/// counts double.
fn span_distance(movie: &Movie, start: f32, end: f32, t: f32) -> f32 {
    let once = |t: f32| {
        if t < start {
            start - t
        } else if t >= end {
            (t - end) * 2.0
        } else {
            0.0
        }
    };
    match movie.loop_period() {
        Some(p) => once(t).min(once(t - p as f32)),
        None => once(t),
    }
}

fn distance_once(shot: &Shot, t: f32) -> f32 {
    if t < shot.start {
        shot.start - t
    } else if t > shot.end() {
        (t - shot.end()) * 2.0
    } else {
        0.0
    }
}

/// The film frame on screen at `t` (a loop film's within one period): `frame_at`, as
/// `FrameInfo::at` derives `iFrame`, so playback, `still` and `export` agree.
fn film_frame(movie: &Movie, t: f32) -> u64 {
    movie.frame_at(movie.wrap(t)).max(0) as u64
}

fn invalid_at(movie: &Movie, at: At) -> bool {
    match at {
        At::Shot { shot, .. } => shot >= movie.shots.len(),
        At::Transition { from, to, .. } => {
            from >= movie.shots.len()
                || to >= movie.shots.len()
                || movie.shots[to].transition_in.is_none()
        }
    }
}

/// Where compiled program binaries are kept between runs.
fn cache_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("SHOTGLASS_CACHE_DIR") {
        return Some(PathBuf::from(d));
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("shotglass").join("programs"))
}

fn rel(p: &Path, root: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Read a query object's value. glow's `get_query_parameter_u32`/`_u64` call the
/// EXT entry points unless GL 4.5 is present, and panic on macOS (GL 4.1), which
/// has only the core ones; the `_with_offset` getter checks the core function.
/// No GL_QUERY_BUFFER is ever bound, so the "offset" is a pointer to `v`.
fn query_u64(gl: &glow::Context, q: glow::Query, pname: u32) -> u64 {
    let mut v: u64 = 0;
    unsafe { gl.get_query_parameter_u64_with_offset(q, pname, &mut v as *mut u64 as usize) };
    v
}

/// GPU time per frame from GL_TIME_ELAPSED queries (results arrive a few frames late).
struct GpuTimer {
    pending: std::collections::VecDeque<glow::Query>,
    free: Vec<glow::Query>,
    active: Option<glow::Query>,
}

impl GpuTimer {
    fn new(gl: &glow::Context) -> Option<GpuTimer> {
        let mut queries = vec![];
        for _ in 0..6 {
            queries.push(unsafe { gl.new_query().ok()? });
        }
        Some(GpuTimer {
            free: queries,
            pending: Default::default(),
            active: None,
        })
    }
    fn begin(&mut self, gl: &glow::Context) {
        if let Some(q) = self.free.pop() {
            unsafe { gl.begin_query(glow::TIME_ELAPSED, q) };
            self.active = Some(q);
        }
    }
    fn end(&mut self, gl: &glow::Context) {
        if let Some(q) = self.active.take() {
            unsafe { gl.end_query(glow::TIME_ELAPSED) };
            self.pending.push_back(q);
        }
    }
    /// The newest result that arrived since the last poll, if any.
    fn poll(&mut self, gl: &glow::Context) -> Option<f32> {
        let mut fresh = None;
        while let Some(&q) = self.pending.front() {
            let ready = query_u64(gl, q, glow::QUERY_RESULT_AVAILABLE) != 0;
            if !ready {
                break;
            }
            let ns = query_u64(gl, q, glow::QUERY_RESULT);
            fresh = Some(ns as f32 / 1.0e6);
            self.pending.pop_front();
            self.free.push(q);
        }
        fresh
    }
}
