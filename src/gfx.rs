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

/// The engine's own pass that shows one shot unchanged (`transition_source(.., "copy", ..)`).
/// Not a transition films can name: their transitions come from libraries or files.
const COPY: &str = include_str!("glsl/copy.glsl");
/// The engine's layer composite: premultiplied source-over (#69).
const OVER: &str = include_str!("glsl/over.glsl");

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
    a: Option<Target>,
    b: Option<Target>,
    freeze: Option<Target>,
    out: Option<Target>,
    /// Layers (#69): two full-size working pictures (the picture so far, and the next)
    /// and the scratch target a layer draws into at its scale.
    work: [Option<Target>; 2],
    layer: Option<Target>,
    /// The picture track while layers are active (for `@picture` and the first layer).
    picture: Option<Target>,
    /// Outputs of layers that later layers read, per layer index.
    held: Vec<Option<Target>>,
    /// Layers left out of the picture (the player's mute/solo); export never sets it.
    hidden: std::collections::HashSet<String>,
    /// Draw the output upside down for readback (`set_readback`), and whether the
    /// current output was drawn that way.
    flip_output: bool,
    out_flipped: bool,
    /// (shot, movie generation, outgoing shot's keys) of the frozen frame.
    freeze_key: Option<(usize, u64, ShotKeys)>,
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
        unsafe { self.gl.delete_buffer(self.buf) };
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
        unsafe { self.gl.delete_sync(self.sync) };
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
                gl.delete_texture(tex);
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
            let vao = gl.create_vertex_array().map_err(|e| anyhow!(e))?;
            let dummy = gl.create_texture().map_err(|e| anyhow!(e))?;
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
            gl.bind_texture(glow::TEXTURE_2D, None);
            let timer = if driver.es { None } else { GpuTimer::new(&gl) };
            let ext = gl.supported_extensions();
            let parallel = ext.contains("GL_KHR_parallel_shader_compile")
                || ext.contains("GL_ARB_parallel_shader_compile");
            if parallel {
                gl.max_shader_compiler_threads(0xFFFF_FFFF);
            }
            let formats = gl.get_parameter_i32(glow::NUM_PROGRAM_BINARY_FORMATS);
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
                    let s = gl.create_sampler().map_err(|e| anyhow!(e))?;
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
            Ok(Renderer {
                warnings: vec![],
                gl_info,
                gl,
                driver,
                dialect,
                vao,
                dummy,
                programs: HashMap::new(),
                copy: None,
                over: None,
                wanted: vec![],
                wanted_ids: vec![],
                fallback: HashMap::new(),
                parallel,
                binary_dir,
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
                a: None,
                b: None,
                freeze: None,
                out: None,
                work: [None, None],
                layer: None,
                picture: None,
                held: vec![],
                hidden: Default::default(),
                flip_output: false,
                out_flipped: false,
                freeze_key: None,
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

    /// The error of a footage frame the last render needed and couldn't decode (it
    /// read black). Headless commands fail on it, as `load_all` does for textures.
    pub fn footage_failure(&self) -> Option<&str> {
        self.footage_failure.as_deref()
    }

    /// Footage on the GPU (#70): (frames resident, their bytes).
    pub fn footage_resident(&self) -> (usize, usize) {
        self.textures
            .values()
            .filter(|e| e.footage)
            .filter_map(|e| match e.state {
                TexState::Ready { bytes, .. } => Some(bytes),
                _ => None,
            })
            .fold((0, 0), |(n, b), x| (n + 1, b + x))
    }

    /// Resident texture bytes (images, text and footage), for tests.
    pub fn texture_bytes(&self) -> usize {
        self.textures
            .values()
            .filter_map(|e| match e.state {
                TexState::Ready { bytes, .. } => Some(bytes),
                _ => None,
            })
            .sum()
    }

    /// Textures fully uploaded and held on the GPU.
    pub fn textures_ready(&self) -> usize {
        self.textures
            .values()
            .filter(|e| matches!(e.state, TexState::Ready { .. }))
            .count()
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
        let channels = match item {
            crate::movie::ItemRef::Shot(i) => &movie.shots[i].channels,
            crate::movie::ItemRef::Layer(j) => &movie.layers[j].channels,
        };
        let mut keys = [None; N_CHANNELS];
        for (n, c) in channels.iter().enumerate() {
            let Some(c) = c else { continue };
            let pass = match &c.source {
                crate::movie::Source::Input => Some(INPUT_TEX),
                crate::movie::Source::Picture => Some(PICTURE_TEX),
                crate::movie::Source::Layer { index, .. } => Some(LAYER_TEX - *index as u64),
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
        let mut tex_errors = vec![];
        for (i, shot) in movie.shots.iter().enumerate() {
            let src = scene_source(dialect, shot, &cues, movie);
            let scene = add(
                self,
                format!("shot \"{}\" ({})", shot.id, rel(&shot.scene, &movie.root)),
                src,
            );
            let trans = match &shot.transition_in {
                Some(t) => {
                    let src = transition_source(dialect, &t.kind, &t.params, &cues, movie);
                    add(
                        self,
                        format!("transition into \"{}\" ({})", shot.id, t.kind),
                        src,
                    )
                    .map(Some)
                }
                None => Some(None),
            };
            let channels = self.channel_keys(
                &format!("shot \"{}\"", shot.id),
                crate::movie::ItemRef::Shot(i),
                movie,
                &mut tex_errors,
            );
            if let (Some(scene), Some(trans)) = (scene, trans) {
                wanted.push(ShotKeys {
                    scene,
                    trans,
                    channels,
                });
            }
        }
        // Layers (#69): their programs and channels follow the shots in `wanted`.
        for (j, layer) in movie.layers.iter().enumerate() {
            let src = layer_source(dialect, layer, &cues, movie);
            let what = match &layer.scene {
                LayerScene::File(p) => format!("layer \"{}\" ({})", layer.id, rel(p, &movie.root)),
                LayerScene::Pass(k) => format!("layer \"{}\" ({k})", layer.id),
            };
            let scene = add(self, what, src);
            let blend = match &layer.blend {
                Some(b) => add(
                    self,
                    format!("blend of layer \"{}\" ({b})", layer.id),
                    blend_source(dialect, layer, b, &cues, movie),
                )
                .map(Some),
                None => Some(None),
            };
            let channels = self.channel_keys(
                &format!("layer \"{}\"", layer.id),
                crate::movie::ItemRef::Layer(j),
                movie,
                &mut tex_errors,
            );
            if let (Some(scene), Some(blend)) = (scene, blend) {
                // A layer's blend program rides in `trans`: keys_ready needs it too.
                wanted.push(ShotKeys {
                    scene,
                    trans: blend,
                    channels,
                });
            }
        }
        let over = if movie.layers.is_empty() {
            None
        } else {
            let src = transition_source(dialect, "over", &[], &cues, movie);
            add(self, "layer composite".into(), src)
        };
        errors.extend(tex_errors);
        warnings.dedup();
        self.warnings = warnings;
        if !errors.is_empty() {
            self.collect_garbage();
            return Err(errors);
        }
        // Remember what currently works, per shot id, for while the new version compiles.
        for (id, k) in self.wanted_ids.clone().iter().zip(self.wanted.clone()) {
            if self.keys_ready(k) {
                self.fallback.insert(id.clone(), k);
            }
        }
        self.dialect = dialect;
        self.wanted = wanted;
        self.wanted_ids = movie
            .shots
            .iter()
            .map(|s| s.id.clone())
            .chain(movie.layers.iter().map(|l| l.id.clone()))
            .collect();
        self.fallback.retain(|id, _| {
            movie.shots.iter().any(|s| &s.id == id) || movie.layers.iter().any(|l| &l.id == id)
        });
        self.over = over;
        self.copy = copy;
        if let Some(c) = copy {
            self.compile_now(c);
        }
        self.generation += 1;
        self.freeze_key = None;
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

    /// (loaded, total) textures for the current version (failed ones count as loaded).
    pub fn texture_progress(&self) -> (usize, usize) {
        let keys: std::collections::HashSet<u64> = self
            .wanted
            .iter()
            .flat_map(|k| k.channels.iter().flatten().map(|c| c.tex))
            // Another pass's picture or a footage channel (loaded per frame): not a
            // texture to load.
            .filter(|k| !is_pass_key(*k) && !is_footage_marker(*k))
            .collect();
        let done = keys
            .iter()
            .filter(|k| {
                matches!(
                    self.textures.get(*k).map(|e| &e.state),
                    Some(TexState::Ready { .. } | TexState::Failed(_))
                )
            })
            .count();
        (done, keys.len())
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
    pub fn pump(&mut self, movie: &Movie, t: f32, background: bool) {
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
        let order = self.priority(movie, t);
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
        let mut tex_order = self.texture_order(movie, t);
        let footage = !self.footage.is_empty();
        if footage {
            tex_order.extend(self.footage_order(movie, t));
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

    /// Footage frames to load, by how soon they're needed (seconds), and this pump's
    /// footage upload budget (#70). For each shot and layer on screen, the frames of
    /// film frames `n − FOOTAGE_BEHIND .. n + W` (W: half a second, at least
    /// `footage::MIN_FRAMES`, fewer when frames are large); for each one starting within
    /// `FOOTAGE_HEADS` seconds (a loop film's first shots near the end of the period
    /// too), its first frames: cuts and loop seams then don't stall. Frames are mapped
    /// through `source_frame`, so repeats, speed and `end` count exactly.
    fn footage_order(&mut self, movie: &Movie, t: f32) -> Vec<(u64, f32)> {
        let n_now = film_frame(movie, t);
        let dt = movie.fps_den as f64 / movie.fps_num as f64;
        let n_shots = movie.shots.len();
        // (item index in `wanted`, distance, the film frame its frames start from,
        // how many frames: None = a window or head as below)
        let mut items: Vec<(usize, f32, u64, Option<u64>)> = vec![];
        for (i, shot) in movie.shots.iter().enumerate() {
            let d = distance(movie, shot, t);
            if d <= FOOTAGE_HEADS {
                let from = if d == 0.0 {
                    n_now
                } else {
                    movie.frame_at(shot.start).max(0) as u64
                };
                items.push((i, d, from, None));
            }
            // A frozen transition out of this shot shows its frame at the transition's
            // first frame for the whole transition: keep that one frame too.
            if let Some(next) = movie.shots.get(i + 1)
                && next.transition_in.as_ref().is_some_and(|tr| tr.freeze)
            {
                let d = distance(movie, next, t);
                if d <= FOOTAGE_HEADS {
                    items.push((i, d, movie.frame_at(next.start).max(0) as u64, Some(1)));
                }
            }
        }
        for (j, layer) in movie.layers.iter().enumerate() {
            let d = layer_distance(movie, layer, t);
            if d <= FOOTAGE_HEADS {
                let from = if d == 0.0 {
                    n_now
                } else {
                    movie.frame_at(layer.start).max(0) as u64
                };
                items.push((n_shots + j, d, from, None));
            }
        }
        let uses: Vec<(u64, f32, u64, Option<u64>)> = items
            .iter()
            .filter_map(|&(i, d, from, len)| Some((self.wanted.get(i)?, d, from, len)))
            .flat_map(|(k, d, from, len)| {
                k.channels
                    .iter()
                    .flatten()
                    .filter_map(move |c| c.footage.map(|f| (f, d, from, len)))
            })
            .collect();
        if uses.is_empty() {
            self.footage_budget = self.upload_budget;
            return vec![];
        }
        let half = ((FOOTAGE_WINDOW / dt).ceil() as u64).max(crate::footage::MIN_FRAMES as u64);
        let share = self.texture_budget / 2 / uses.len();
        let mut on_screen = 0;
        let mut out = vec![];
        for (fk, d, from, len) in uses {
            let Some(e) = self.footage.get(&fk) else {
                continue;
            };
            let bytes = e
                .scan
                .size
                .map_or(1, |(w, h)| crate::texture::frame_bytes(w, h, e.mipmap));
            let w = ((share / bytes) as u64).clamp(crate::footage::MIN_FRAMES as u64, half);
            let map = e.map;
            let range: Vec<u64> = if let Some(len) = len {
                (from..from + len).collect()
            } else if d == 0.0 {
                on_screen += bytes;
                (from.saturating_sub(FOOTAGE_BEHIND)..from + w).collect()
            } else {
                (from..from + w.div_ceil(2).max(crate::footage::MIN_FRAMES as u64)).collect()
            };
            for m in range {
                let Some(k) = map.source_frame(m) else {
                    continue;
                };
                let ahead = if m >= from {
                    (m - from) as f64 * dt
                } else {
                    (from - m) as f64 * dt * 2.0
                };
                let tex = self.frame_tex(fk, k);
                out.push((tex, d + ahead as f32));
            }
        }
        self.footage_budget = if self.upload_pinned {
            self.upload_budget
        } else {
            self.upload_budget.max(2 * on_screen)
        };
        out
    }

    /// The texture key of source frame `k` of footage `fk`, adding the frame to the
    /// store (`Idle`) if it isn't there: keyed by its file's path, size and mtime (one
    /// `stat` per version and frame) and how its texels are made. A missing file is a
    /// `Failed` frame.
    fn frame_tex(&mut self, fk: u64, k: u64) -> u64 {
        let Some(e) = self.footage.get_mut(&fk) else {
            return FOOTAGE_BLACK;
        };
        let generation = e.generation;
        // Fast path (every render and pump): known, and still in the store.
        if let Some(&t) = e.frames.get(&k)
            && let Some(entry) = self.textures.get_mut(&t)
        {
            entry.generation = entry.generation.max(generation);
            return t;
        }
        let path = e.scan.path(k);
        let tex = match e.frames.get(&k) {
            Some(&t) => t,
            None => {
                let stamp = match std::fs::metadata(&path) {
                    Ok(md) => {
                        let mtime = md
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map_or(0, |d| d.as_nanos());
                        format!("{}\0{mtime}", md.len())
                    }
                    Err(_) => "missing".to_string(),
                };
                let t = hash(&format!(
                    "frame\0{}\0{stamp}\0{:?}\0{}\0{}\0{}",
                    path.display(),
                    e.scan.size,
                    e.vflip,
                    e.srgb,
                    e.mipmap
                ));
                e.frames.insert(k, t);
                t
            }
        };
        // Another channel (or version) may have this very frame already: share it.
        let entry = self.textures.entry(tex).or_insert_with(|| {
            let label = format!("{}{}", e.folder, e.scan.pattern.name(e.scan.first + k));
            let state = if path.is_file() {
                TexState::Idle
            } else {
                TexState::Failed(format!(
                    "{label}: not found (it was there when the film loaded)"
                ))
            };
            TexEntry {
                source: TexSource::Frame(path, e.scan.size),
                label,
                vflip: e.vflip,
                srgb: e.srgb,
                mipmap: e.mipmap,
                footage: true,
                generation,
                state,
                last_used: 0,
            }
        });
        entry.generation = entry.generation.max(generation);
        tex
    }

    /// `k` with each footage channel's frame for film frame `n` filled in (`tex` = that
    /// frame's texture, or `FOOTAGE_BLACK`; `time` = its source time).
    fn resolve(&mut self, mut k: ShotKeys, n: u64) -> ShotKeys {
        for c in k.channels.iter_mut().flatten() {
            let Some(fk) = c.footage else { continue };
            let Some(map) = self.footage.get(&fk).map(|e| e.map) else {
                c.tex = FOOTAGE_BLACK;
                continue;
            };
            match map.source_frame(n) {
                Some(frame) => {
                    c.tex = self.frame_tex(fk, frame);
                    c.time = map.source_time(frame);
                }
                None => {
                    c.tex = FOOTAGE_BLACK;
                    c.time = 0.0;
                }
            }
        }
        k
    }

    /// Textures sorted by how soon the playhead needs them (seconds).
    fn texture_order(&self, movie: &Movie, t: f32) -> Vec<(u64, f32)> {
        let mut v: Vec<(u64, f32)> = vec![];
        for (i, shot) in movie.shots.iter().enumerate() {
            let Some(k) = self.wanted.get(i) else {
                continue;
            };
            let d = distance(movie, shot, t);
            v.extend(
                k.channels
                    .iter()
                    .flatten()
                    .filter(|c| c.footage.is_none())
                    .map(|c| (c.tex, d)),
            );
        }
        let n = movie.shots.len();
        for (j, layer) in movie.layers.iter().enumerate() {
            let Some(k) = self.wanted.get(n + j) else {
                continue;
            };
            let d = layer_distance(movie, layer, t);
            v.extend(
                k.channels
                    .iter()
                    .flatten()
                    .filter(|c| !is_pass_key(c.tex) && c.footage.is_none())
                    .map(|c| (c.tex, d)),
            );
        }
        v.sort_by(|a, b| a.1.total_cmp(&b.1));
        let mut seen = std::collections::HashSet::new();
        v.retain(|(k, _)| seen.insert(*k));
        v
    }

    /// Texture housekeeping, never blocking: collect finished decodes, upload at most
    /// `upload_budget` bytes of rows (nearest first), start decodes within the
    /// lookahead (everything with `background`), a couple at a time.
    fn pump_textures(&mut self, order: &[(u64, f32)], background: bool) {
        if self.textures.is_empty() {
            return;
        }
        if let Some(d) = &self.decoder {
            while let Ok((k, r)) = d.results.try_recv() {
                if let Some(e) = self.textures.get_mut(&k)
                    && matches!(e.state, TexState::Decoding)
                {
                    e.state = match r {
                        Ok(img) => TexState::Decoded(img),
                        Err(m) => TexState::Failed(m),
                    };
                }
            }
        }
        let wanted = |d: f32| background || d <= LOOKAHEAD;
        // Images and text share one budget, footage has its own (#70), so a big still
        // texture never starves the frames under the playhead, nor they it.
        let mut budget = self.upload_budget;
        let mut footage_budget = self.footage_budget;
        for (k, d) in order {
            if (budget == 0 && footage_budget == 0) || !wanted(*d) {
                break;
            }
            let Some(footage) = self.textures.get(k).map(|e| e.footage) else {
                continue;
            };
            if footage {
                if footage_budget > 0 {
                    footage_budget = self.upload_some(*k, footage_budget);
                }
            } else if budget > 0 {
                budget = self.upload_some(*k, budget);
            }
        }
        // Decodes in flight, (images and text, footage frames and their CPU bytes).
        let in_flight = |r: &Renderer| {
            r.textures
                .values()
                .filter(|e| {
                    matches!(
                        e.state,
                        TexState::Decoding | TexState::Decoded(_) | TexState::Uploading { .. }
                    )
                })
                .fold((0, 0, 0), |(n, f, b), e| {
                    if e.footage {
                        let bytes = match &e.source {
                            TexSource::Frame(_, Some((w, h))) => w * h * 4,
                            _ => 0,
                        };
                        (n, f + 1, b + bytes)
                    } else {
                        (n + 1, f, b)
                    }
                })
        };
        let workers = self.decoder.as_ref().map_or(1, |d| d.workers);
        let footage_cap = (workers * FOOTAGE_IN_FLIGHT_PER_WORKER).max(FOOTAGE_MIN_IN_FLIGHT);
        let (mut stills, mut frames, mut frame_bytes) = in_flight(self);
        for (k, d) in order {
            if !wanted(*d) || (stills >= TEXTURES_IN_FLIGHT && frames >= footage_cap) {
                break;
            }
            let Some(e) = self.textures.get_mut(k) else {
                continue;
            };
            if !matches!(e.state, TexState::Idle) {
                continue;
            }
            if e.footage {
                let bytes = match &e.source {
                    TexSource::Frame(_, Some((w, h))) => w * h * 4,
                    _ => 0,
                };
                let over_cpu =
                    frames >= FOOTAGE_MIN_IN_FLIGHT && frame_bytes + bytes > FOOTAGE_CPU_BYTES;
                if frames >= footage_cap || over_cpu {
                    continue;
                }
                frames += 1;
                frame_bytes += bytes;
            } else {
                if stills >= TEXTURES_IN_FLIGHT {
                    continue;
                }
                stills += 1;
            }
            {
                let job = (*k, e.source.clone(), e.label.clone(), e.vflip);
                e.state = TexState::Decoding;
                let d = self.decoder.get_or_insert_with(Decoder::new);
                if d.jobs.send(job).is_err() {
                    self.textures.get_mut(k).unwrap().state =
                        TexState::Failed("the texture decode thread stopped".into());
                }
            }
        }
    }

    /// Upload up to `budget` bytes of texture `k` (at least one row, so a wide image
    /// still makes progress), starting its upload if it's decoded. Returns the budget left.
    fn upload_some(&mut self, k: u64, budget: usize) -> usize {
        let Some(e) = self.textures.get_mut(&k) else {
            return budget;
        };
        if !matches!(e.state, TexState::Decoded(_) | TexState::Uploading { .. }) {
            return budget;
        }
        let (srgb, mipmap, label) = (e.srgb, e.mipmap, e.label.clone());
        let (tex, img, row) = match std::mem::replace(&mut e.state, TexState::Idle) {
            TexState::Decoded(img) => match self.begin_upload(&img, srgb, mipmap, &label) {
                Ok(tex) => (tex, img, 0),
                Err(m) => {
                    self.textures.get_mut(&k).unwrap().state = TexState::Failed(m);
                    return budget;
                }
            },
            TexState::Uploading { tex, img, row } => (tex, img, row),
            other => {
                self.textures.get_mut(&k).unwrap().state = other;
                return budget;
            }
        };
        let row_bytes = img.width * 4;
        let rows = (budget / row_bytes).clamp(1, img.height - row);
        self.upload_rows(tex, &img, row, rows, mipmap);
        let row = row + rows;
        let state = if row == img.height {
            TexState::Ready {
                tex,
                w: img.width,
                h: img.height,
                bytes: crate::texture::frame_bytes(img.width, img.height, mipmap),
                meta: img.meta,
            }
        } else {
            TexState::Uploading { tex, img, row }
        };
        self.textures.get_mut(&k).unwrap().state = state;
        budget.saturating_sub(rows * row_bytes)
    }

    /// Make texture `k` ready (or failed) now: decode on this thread if needed and
    /// upload the rest at once. Only for paused/scrubbing and headless renders.
    fn load_texture_now(&mut self, k: u64) {
        let Some(e) = self.textures.get_mut(&k) else {
            return;
        };
        match &e.state {
            TexState::Ready { .. } | TexState::Failed(_) => return,
            TexState::Idle | TexState::Decoding => {
                // A worker may be decoding it too; its result is ignored once this is done.
                e.state = match e.source.load(&e.label, e.vflip) {
                    Ok(img) => TexState::Decoded(img),
                    Err(m) => TexState::Failed(m),
                };
            }
            _ => {}
        }
        if e.footage {
            self.blocking_footage_loads += 1;
        } else {
            self.blocking_texture_loads += 1;
        }
        self.upload_some(k, usize::MAX);
    }

    /// Allocate level 0 (`RGBA8`, or `SRGB8_ALPHA8` with `srgb`) for `img`, or reuse a
    /// spare texture of that size and format (footage frames turn over constantly:
    /// `tex_sub_image_2d` into a reused texture, no reallocation; GL orders the new
    /// upload after earlier draws that sampled the old contents, so no fence).
    fn begin_upload(
        &mut self,
        img: &DecodedImage,
        srgb: bool,
        mipmap: bool,
        label: &str,
    ) -> std::result::Result<glow::Texture, String> {
        let max = self.max_texture_size;
        if img.width > max || img.height > max {
            return Err(format!(
                "{label}: {}×{} is larger than this GPU's limit of {max} (GL_MAX_TEXTURE_SIZE)",
                img.width, img.height
            ));
        }
        let slot = (img.width, img.height, srgb, mipmap);
        if let Some(i) = self.spare.iter().position(|(k, _)| *k == slot) {
            return Ok(self.spare.swap_remove(i).1);
        }
        self.texture_creates += 1;
        let gl = &self.gl;
        unsafe {
            let tex = gl.create_texture()?;
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            let internal = if srgb {
                glow::SRGB8_ALPHA8
            } else {
                glow::RGBA8
            };
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                internal as i32,
                img.width as i32,
                img.height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            // Sampling state comes from sampler objects; these only keep the texture
            // complete if one is ever sampled without.
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                if mipmap {
                    glow::LINEAR_MIPMAP_LINEAR
                } else {
                    glow::LINEAR
                } as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
            Ok(tex)
        }
    }

    /// Submit rows `row .. row + rows` of level 0 from client memory (the driver copies
    /// them before returning: nothing to wait for), and the mipmaps after the last band.
    /// The unpack state is set for the band and put back to GL's defaults, and no
    /// unpack buffer is bound (with one bound, the slice would be read as an offset).
    fn upload_rows(
        &mut self,
        tex: glow::Texture,
        img: &DecodedImage,
        row: usize,
        rows: usize,
        mipmap: bool,
    ) {
        let gl = &self.gl;
        let stride = img.width * 4;
        unsafe {
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
            gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 0);
            gl.pixel_store_i32(glow::UNPACK_SKIP_PIXELS, 0);
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                row as i32,
                img.width as i32,
                rows as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&img.rgba[row * stride..(row + rows) * stride])),
            );
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            if mipmap && row + rows == img.height {
                gl.generate_mipmap(glow::TEXTURE_2D);
            }
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
        self.texture_bands += 1;
    }

    fn texture_ready(&self, k: u64) -> bool {
        is_pass_key(k)
            || is_footage_marker(k)
            || matches!(
                self.textures.get(&k).map(|e| &e.state),
                Some(TexState::Ready { .. })
            )
    }

    /// Keep resident texture memory under the budget: drop the least recently used
    /// textures that the playhead won't need soon and no fallback holds. They come
    /// back through the same decode and upload, identically.
    fn evict_textures(&mut self, order: &[(u64, f32)]) {
        // Footage frames the playhead no longer wants (it moved on, or jumped) that
        // aren't uploaded yet leave the store, so they hold neither CPU memory nor an
        // in-flight slot; a decode still running for one is dropped when it returns.
        if self.textures.values().any(|e| e.footage) {
            let wanted: std::collections::HashSet<u64> = order.iter().map(|(k, _)| *k).collect();
            let stale: Vec<u64> = self
                .textures
                .iter()
                .filter(|(k, e)| {
                    e.footage
                        && !wanted.contains(k)
                        && matches!(
                            e.state,
                            TexState::Idle
                                | TexState::Decoding
                                | TexState::Decoded(_)
                                | TexState::Uploading { .. }
                        )
                })
                .map(|(k, _)| *k)
                .collect();
            for k in stale {
                if let Some(e) = self.textures.remove(&k) {
                    self.delete_texture_state(e.state);
                }
            }
        }
        let mut total: usize = self
            .textures
            .values()
            .map(|e| match e.state {
                TexState::Ready { bytes, .. } => bytes,
                _ => 0,
            })
            .sum();
        if total <= self.texture_budget {
            return;
        }
        let near: std::collections::HashSet<u64> = order
            .iter()
            .filter(|(_, d)| *d <= LOOKAHEAD * 1.5)
            .map(|(k, _)| *k)
            .collect();
        let held: std::collections::HashSet<u64> = self
            .fallback
            .values()
            .flat_map(|f| f.channels.iter().flatten().map(|c| c.tex))
            .collect();
        // Footage frames the playhead has passed go first (they turn over anyway),
        // then the least recently used.
        let mut candidates: Vec<(u64, (bool, u64), usize)> = self
            .textures
            .iter()
            .filter_map(|(k, e)| match e.state {
                TexState::Ready { bytes, .. } if !near.contains(k) && !held.contains(k) => {
                    Some((*k, (!e.footage, e.last_used), bytes))
                }
                _ => None,
            })
            .collect();
        candidates.sort_by_key(|(_, used, _)| *used);
        for (k, _, bytes) in candidates {
            if total <= self.texture_budget {
                break;
            }
            let e = self.textures.get_mut(&k).unwrap();
            let footage = e.footage;
            let slot = (e.srgb, e.mipmap);
            let old = std::mem::replace(&mut e.state, TexState::Idle);
            if footage {
                // Footage frames leave the store (their footage entry remembers the key
                // and re-adds them if needed again); their texture waits for the next
                // frame of the same size.
                self.textures.remove(&k);
                if let TexState::Ready { tex, w, h, .. } = old
                    && self.spare.len() < SPARE_TEXTURES
                {
                    self.spare.push(((w, h, slot.0, slot.1), tex));
                    total -= bytes;
                    continue;
                }
            }
            self.delete_texture_state(old);
            total -= bytes;
        }
    }

    fn delete_texture_state(&self, s: TexState) {
        if let TexState::Ready { tex, .. } | TexState::Uploading { tex, .. } = s {
            unsafe { self.gl.delete_texture(tex) };
        }
    }

    /// Program keys sorted by how soon the playhead needs them (seconds).
    fn priority(&self, movie: &Movie, t: f32) -> Vec<(u64, f32)> {
        let mut v: Vec<(u64, f32)> = vec![];
        for (i, shot) in movie.shots.iter().enumerate() {
            let Some(k) = self.wanted.get(i) else {
                continue;
            };
            let d = distance(movie, shot, t);
            v.push((k.scene, d));
            if let Some(tr) = k.trans {
                v.push((tr, d));
            }
        }
        let n = movie.shots.len();
        for (j, layer) in movie.layers.iter().enumerate() {
            if let Some(k) = self.wanted.get(n + j) {
                let d = layer_distance(movie, layer, t);
                v.push((k.scene, d));
                if let Some(b) = k.trans {
                    v.push((b, d));
                }
            }
        }
        // The layer composite is needed as soon as any layer is near.
        if let Some(o) = self.over {
            let d = movie
                .layers
                .iter()
                .map(|l| layer_distance(movie, l, t))
                .fold(f32::INFINITY, f32::min);
            if d.is_finite() {
                v.push((o, d));
            }
        }
        v.sort_by(|a, b| a.1.total_cmp(&b.1));
        let mut seen = std::collections::HashSet::new();
        v.retain(|(k, _)| seen.insert(*k));
        v
    }

    fn state(&self, k: u64) -> Option<&State> {
        self.programs.get(&k).map(|e| &e.state)
    }

    fn is_idle(&self, k: u64) -> bool {
        matches!(self.state(k), Some(State::Idle))
    }

    fn keys_ready(&self, k: ShotKeys) -> bool {
        let ok = |k: u64| {
            matches!(
                self.programs.get(&k).map(|e| &e.state),
                Some(State::Ready(_))
            )
        };
        ok(k.scene)
            && k.trans.is_none_or(ok)
            && k.channels
                .iter()
                .flatten()
                .all(|c| self.texture_ready(c.tex))
    }

    /// The programs to draw shot `i` with: the current version if it compiled,
    /// otherwise the last working one. With `blocking`, the current version is
    /// compiled first, so the decision is made knowing whether it works.
    ///
    /// Footage (#70): the keys are resolved for film frame `n`, and a frame is usable
    /// only when *that* source frame is ready on every footage channel (or the fallback
    /// version has it): never a neighbouring frame, so playback shows only what `still`
    /// would.
    fn usable(&mut self, i: usize, blocking: bool, n: u64) -> Option<ShotKeys> {
        let k = *self.wanted.get(i)?;
        let k = self.resolve(k, n);
        if blocking {
            self.compile_now(k.scene);
            if let Some(t) = k.trans {
                self.compile_now(t);
            }
            for c in k.channels.iter().flatten() {
                self.load_texture_now(c.tex);
            }
        }
        if self.keys_ready(k) {
            return Some(k);
        }
        let fb = self
            .wanted_ids
            .get(i)
            .and_then(|id| self.fallback.get(id))
            .copied();
        if let Some(f) = fb
            .map(|f| self.resolve(f, n))
            .filter(|f| self.keys_ready(*f))
        {
            return Some(f);
        }
        if !blocking
            && k.channels
                .iter()
                .flatten()
                .any(|c| c.footage.is_some() && !self.texture_ready(c.tex))
        {
            self.footage_misses += 1;
        }
        // Nothing working to show: with `blocking`, draw what we have (a failed
        // scene shows black, a failed texture reads black); otherwise wait.
        blocking.then_some(k)
    }

    fn start(&mut self, k: u64) {
        let Some(e) = self.programs.get(&k) else {
            return;
        };
        if !matches!(e.state, State::Idle) {
            return;
        }
        let state = match self.begin_compile(&e.src) {
            Ok(p) => State::Pending(p),
            Err(m) => State::Failed(m),
        };
        self.compiles_started += 1;
        self.programs.get_mut(&k).unwrap().state = state;
    }

    /// Make program `k` ready (or failed) now, blocking if needed.
    fn compile_now(&mut self, k: u64) {
        self.start(k);
        let Some(e) = self.programs.get_mut(&k) else {
            return;
        };
        if let State::Pending(_) = e.state {
            let State::Pending(p) = std::mem::replace(&mut e.state, State::Idle) else {
                unreachable!()
            };
            let from_binary = p.from_binary;
            self.binary_hits += from_binary as usize;
            let src = e.src.clone();
            let state = match self.finish_compile(p, &src) {
                Ok(prog) => {
                    if !from_binary {
                        self.save_binary(&prog, &src);
                    }
                    self.warm_up(&prog);
                    State::Ready(prog)
                }
                Err(m) => State::Failed(m),
            };
            self.programs.get_mut(&k).unwrap().state = state;
        }
    }

    fn collect_garbage(&mut self) {
        let mut keep: std::collections::HashSet<u64> =
            self.copy.into_iter().chain(self.over).collect();
        for k in self.wanted.iter().chain(self.fallback.values()) {
            keep.insert(k.scene);
            if let Some(t) = k.trans {
                keep.insert(t);
            }
        }
        let drop: Vec<u64> = self
            .programs
            .keys()
            .filter(|k| !keep.contains(k))
            .copied()
            .collect();
        for k in drop {
            if let Some(e) = self.programs.remove(&k) {
                self.delete_state(e.state);
            }
        }
        // Footage entries of the current version and the fallbacks, and their frames.
        let live: std::collections::HashSet<u64> = self
            .wanted
            .iter()
            .chain(self.fallback.values())
            .flat_map(|k| k.channels.iter().flatten().filter_map(|c| c.footage))
            .collect();
        self.footage.retain(|k, _| live.contains(k));
        let keep: std::collections::HashSet<u64> = self
            .wanted
            .iter()
            .chain(self.fallback.values())
            .flat_map(|k| k.channels.iter().flatten().map(|c| c.tex))
            .chain(
                self.footage
                    .values()
                    .flat_map(|f| f.frames.values().copied()),
            )
            .collect();
        let drop: Vec<u64> = self
            .textures
            .keys()
            .filter(|k| !keep.contains(k))
            .copied()
            .collect();
        for k in drop {
            if let Some(e) = self.textures.remove(&k) {
                self.delete_texture_state(e.state);
            }
        }
    }

    /// Keep at most MAX_READY compiled programs: drop the least recently used ones
    /// that the playhead won't need soon. They recompile (or reload from the
    /// on-disk cache) when needed again.
    fn evict(&mut self, order: &[(u64, f32)]) {
        let ready: Vec<(u64, u64)> = self
            .programs
            .iter()
            .filter(|(_, e)| matches!(e.state, State::Ready(_)))
            .map(|(k, e)| (*k, e.last_used))
            .collect();
        if ready.len() <= MAX_READY {
            return;
        }
        let near: std::collections::HashSet<u64> = order
            .iter()
            .filter(|(_, d)| *d <= LOOKAHEAD * 1.5)
            .map(|(k, _)| *k)
            .collect();
        let mut candidates: Vec<(u64, u64)> = ready
            .into_iter()
            .filter(|(k, _)| {
                !near.contains(k)
                    && Some(*k) != self.copy
                    && Some(*k) != self.over
                    && !self
                        .fallback
                        .values()
                        .any(|f| f.scene == *k || f.trans == Some(*k))
            })
            .collect();
        candidates.sort_by_key(|(_, used)| *used);
        let excess = self
            .programs
            .values()
            .filter(|e| matches!(e.state, State::Ready(_)))
            .count()
            - MAX_READY;
        for (k, _) in candidates.into_iter().take(excess) {
            let e = self.programs.get_mut(&k).unwrap();
            let old = std::mem::replace(&mut e.state, State::Idle);
            self.delete_state(old);
        }
    }

    fn delete_state(&self, s: State) {
        unsafe {
            match s {
                State::Ready(p) => self.gl.delete_program(p.prog),
                State::Pending(p) => {
                    for sh in p.shaders {
                        self.gl.delete_shader(sh);
                    }
                    self.gl.delete_program(p.prog);
                }
                _ => {}
            }
        }
    }

    fn binary_path(&self, src: &Assembled) -> Option<PathBuf> {
        self.binary_dir
            .as_ref()
            .map(|d| d.join(format!("{:016x}.bin", hash(&src.text))))
    }

    fn save_binary(&self, p: &Program, src: &Assembled) {
        let Some(path) = self.binary_path(src) else {
            return;
        };
        if let Some(b) = unsafe { self.gl.get_program_binary(p.prog) }
            && !b.buffer.is_empty()
        {
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            let mut bytes = b.format.to_le_bytes().to_vec();
            bytes.extend_from_slice(&b.buffer);
            let _ = std::fs::write(path, bytes);
        }
    }

    /// Kick off a compile: from the on-disk binary cache if possible, otherwise from
    /// source. With KHR_parallel_shader_compile this returns before the driver is done.
    fn begin_compile(&self, src: &Assembled) -> std::result::Result<Pending, Vec<String>> {
        let gl = &self.gl;
        unsafe {
            if let Some(bytes) = self.binary_path(src).and_then(|p| std::fs::read(p).ok())
                && bytes.len() > 4
            {
                let format = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                let prog = gl.create_program().map_err(|e| vec![e])?;
                gl.program_binary(
                    prog,
                    &glow::ProgramBinary {
                        buffer: bytes[4..].to_vec(),
                        format,
                    },
                );
                if gl.get_program_link_status(prog) {
                    return Ok(Pending {
                        prog,
                        shaders: vec![],
                        from_binary: true,
                    });
                }
                gl.delete_program(prog); // stale (driver update): compile from source
            }
            let vs_src = vertex_source(src.dialect);
            let vs = gl.create_shader(glow::VERTEX_SHADER).map_err(|e| vec![e])?;
            gl.shader_source(vs, &vs_src);
            gl.compile_shader(vs);
            let fs = gl
                .create_shader(glow::FRAGMENT_SHADER)
                .map_err(|e| vec![e])?;
            gl.shader_source(fs, &src.text);
            gl.compile_shader(fs);
            let prog = gl.create_program().map_err(|e| vec![e])?;
            gl.attach_shader(prog, vs);
            gl.attach_shader(prog, fs);
            if self.binary_dir.is_some() {
                gl.program_binary_retrievable_hint(prog, true);
            }
            gl.link_program(prog);
            Ok(Pending {
                prog,
                shaders: vec![vs, fs],
                from_binary: false,
            })
        }
    }

    /// Wait for a compile to finish and collect errors and uniform locations.
    fn finish_compile(
        &self,
        p: Pending,
        src: &Assembled,
    ) -> std::result::Result<Program, Vec<String>> {
        let gl = &self.gl;
        unsafe {
            let linked = gl.get_program_link_status(p.prog);
            let mut errors = vec![];
            if !linked {
                for (i, sh) in p.shaders.iter().enumerate() {
                    if !gl.get_shader_compile_status(*sh) {
                        let log = gl.get_shader_info_log(*sh);
                        if i == 0 {
                            errors.push(format!("vertex shader: {log}"));
                        } else {
                            errors.extend(remap_log(&log, &src.files));
                        }
                    }
                }
                if errors.is_empty() {
                    errors = remap_log(
                        &format!("link: {}", gl.get_program_info_log(p.prog)),
                        &src.files,
                    );
                }
                if errors.is_empty() {
                    errors.push("compile failed (driver gave no log)".into());
                }
            }
            for sh in &p.shaders {
                gl.detach_shader(p.prog, *sh);
                gl.delete_shader(*sh);
            }
            if !linked {
                gl.delete_program(p.prog);
                return Err(src.explain(errors));
            }
            let mut locs = HashMap::new();
            let n = gl.get_active_uniforms(p.prog);
            for i in 0..n {
                if let Some(u) = gl.get_active_uniform(p.prog, i) {
                    let name = u.name.trim_end_matches("[0]").to_string();
                    if let Some(l) = gl.get_uniform_location(p.prog, &name) {
                        locs.insert(name, l);
                    }
                }
            }
            // The channel samplers read units 0..N_CHANNELS, always: set once here (this
            // also covers programs loaded from the binary cache, whose uniforms reset).
            let units: Vec<(&glow::UniformLocation, i32)> = (0..N_CHANNELS)
                .filter_map(|n| locs.get(CHANNEL_UNIFORMS[n]).map(|l| (l, n as i32)))
                .collect();
            if !units.is_empty() {
                gl.use_program(Some(p.prog));
                for (l, n) in units {
                    gl.uniform_1_i32(Some(l), n);
                }
                gl.use_program(None);
            }
            Ok(Program { prog: p.prog, locs })
        }
    }

    /// Compile synchronously (sound shaders, which run once).
    fn compile(&self, src: &Assembled) -> std::result::Result<Program, Vec<String>> {
        let p = self.begin_compile(src)?;
        self.finish_compile(p, src)
    }

    /// Draw a program once into a tiny viewport so the driver finishes any lazy work
    /// now, not on the first frame the shot appears.
    fn warm_up(&mut self, p: &Program) {
        if self.a.is_none() {
            ensure_target(&self.gl, &mut self.a, 8, 8);
        }
        let t = self.a.as_ref().unwrap();
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(t.fbo));
            self.gl.viewport(0, 0, 4, 4);
            self.gl.bind_vertex_array(Some(self.vao));
            self.gl.use_program(Some(p.prog));
            if let Some(l) = p.loc("iResolution") {
                self.gl.uniform_3_f32(Some(l), 4.0, 4.0, 1.0);
            }
            if let Some(l) = p.loc("uOutRes") {
                self.gl.uniform_2_f32(Some(l), 4.0, 4.0);
            }
            // Samplers need something loadable on their units: with nothing bound,
            // macOS logs "unit 0 GLD_TEXTURE_INDEX_2D is unloadable".
            for n in 0..N_CHANNELS as u32 {
                self.gl.active_texture(glow::TEXTURE0 + n);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(self.dummy));
            }
            self.gl.draw_arrays(glow::TRIANGLES, 0, 3);
            for n in (0..N_CHANNELS as u32).rev() {
                self.gl.active_texture(glow::TEXTURE0 + n);
                self.gl.bind_texture(glow::TEXTURE_2D, None);
            }
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.use_program(None);
            self.gl.bind_vertex_array(None);
        }
    }

    fn prog(&self, k: u64) -> Option<&Program> {
        match &self.programs.get(&k)?.state {
            State::Ready(p) => Some(p),
            _ => None,
        }
    }

    /// Render the movie at `info.t` into the output texture (w × h).
    /// With `blocking = false` it returns None when a shot's program isn't compiled
    /// yet (keep showing the previous frame); with `true` it compiles on the spot.
    pub fn render(
        &mut self,
        movie: &Movie,
        w: i32,
        h: i32,
        info: &FrameInfo,
        blocking: bool,
    ) -> Option<glow::Texture> {
        self.render_at(movie, w, h, info, movie.at(info.t), blocking)
    }

    /// Like `render`, but for an explicit timeline position (tests use this to
    /// render a transition at an exact progress).
    pub fn render_at(
        &mut self,
        movie: &Movie,
        w: i32,
        h: i32,
        info: &FrameInfo,
        at: At,
        blocking: bool,
    ) -> Option<glow::Texture> {
        let copy = self.copy?;
        let (w, h) = (w.max(1), h.max(1));
        let sw = ((w as f32 * info.scale).round() as i32).clamp(1, w);
        let sh = ((h as f32 * info.scale).round() as i32).clamp(1, h);
        self.poll_timer();
        if invalid_at(movie, at) {
            return None;
        }
        // The film frame on screen picks each footage channel's source frame (#70); a
        // frozen transition's outgoing shot shows the frame where the transition starts.
        let n = film_frame(movie, info.t);
        let needed: Vec<(usize, u64)> = match at {
            At::Shot { shot, .. } => vec![(shot, n)],
            At::Transition { from, to, .. } => {
                let freeze = movie.shots[to]
                    .transition_in
                    .as_ref()
                    .is_some_and(|t| t.freeze);
                let from_n = if freeze {
                    movie.frame_at(movie.shots[to].start).max(0) as u64
                } else {
                    n
                };
                vec![(from, from_n), (to, n)]
            }
        };
        let mut keys = vec![];
        for &(i, n) in &needed {
            keys.push(self.usable(i, blocking, n)?);
        }
        // Layers (#69): a frame is usable only when every active layer is, so playback
        // never shows a frame without a layer that `still` and `export` would draw.
        let n_shots = movie.shots.len();
        let active: Vec<(usize, f32)> = movie
            .layers_at(info.t)
            .into_iter()
            .filter(|(j, _)| !self.hidden.contains(&movie.layers[*j].id))
            .collect();
        let mut layer_keys = vec![];
        for &(j, _) in &active {
            layer_keys.push(self.usable(n_shots + j, blocking, n)?);
        }
        let over = if active.is_empty() {
            None
        } else {
            let o = self.over?;
            if blocking {
                self.compile_now(o);
            }
            self.prog(o)?;
            Some(o)
        };
        // A footage frame that failed to decode reads black (a broken still texture
        // too); headless commands refuse such a frame (`footage_failure`).
        self.footage_failure = keys
            .iter()
            .chain(&layer_keys)
            .flat_map(|k| k.channels.iter().flatten())
            .filter(|c| c.footage.is_some())
            .find_map(|c| match &self.textures.get(&c.tex)?.state {
                TexState::Failed(m) => Some(m.clone()),
                _ => None,
            });
        let frame_no = self.frame_no;
        for k in keys.iter().chain(&layer_keys) {
            for x in [Some(k.scene), k.trans].into_iter().flatten() {
                if let Some(e) = self.programs.get_mut(&x) {
                    e.last_used = frame_no;
                }
            }
            for c in k.channels.iter().flatten() {
                if let Some(e) = self.textures.get_mut(&c.tex) {
                    e.last_used = frame_no;
                }
            }
        }
        if let Some(timer) = &mut self.timer {
            timer.begin(&self.gl);
        }
        ensure_target(&self.gl, &mut self.a, w, h);
        ensure_target(&self.gl, &mut self.b, w, h);
        ensure_target(&self.gl, &mut self.out, w, h);
        // With layers, the picture track goes into a working picture first.
        let pic_dest = over.map(|_| Which::Picture);
        if over.is_some() {
            ensure_target(&self.gl, &mut self.picture, w, h);
            ensure_target(&self.gl, &mut self.work[0], w, h);
            ensure_target(&self.gl, &mut self.work[1], w, h);
            ensure_target(&self.gl, &mut self.layer, w, h);
            self.held.resize_with(movie.layers.len(), || None);
        }
        match at {
            At::Shot { shot, local } => {
                self.draw_scene(
                    keys[0],
                    &Item::shot(&movie.shots[shot], local),
                    local,
                    Which::A,
                    sw,
                    sh,
                    info,
                    movie,
                );
                self.composite_into(copy, Which::A, Which::A, 0.0, w, h, pic_dest);
            }
            At::Transition {
                from,
                to,
                from_local,
                to_local,
                progress,
            } => {
                // checked by invalid_at before any GL work
                let Some(tr) = movie.shots[to].transition_in.as_ref() else {
                    unreachable!()
                };
                let from_target = if tr.freeze {
                    let key = (to, self.generation, keys[0]);
                    if self.freeze_key != Some(key)
                        || self.freeze.as_ref().is_none_or(|f| f.alloc != (w, h))
                    {
                        ensure_target(&self.gl, &mut self.freeze, w, h);
                        let freeze_local = movie.shots[to].start - movie.shots[from].start;
                        self.draw_scene(
                            keys[0],
                            &Item::shot(&movie.shots[from], freeze_local),
                            freeze_local,
                            Which::Freeze,
                            sw,
                            sh,
                            info,
                            movie,
                        );
                        self.freeze_key = Some(key);
                    }
                    Which::Freeze
                } else {
                    self.draw_scene(
                        keys[0],
                        &Item::shot(&movie.shots[from], from_local),
                        from_local,
                        Which::A,
                        sw,
                        sh,
                        info,
                        movie,
                    );
                    Which::A
                };
                self.draw_scene(
                    keys[1],
                    &Item::shot(&movie.shots[to], to_local),
                    to_local,
                    Which::B,
                    sw,
                    sh,
                    info,
                    movie,
                );
                let eased = tr.ease.apply(progress);
                let tp = keys[1]
                    .trans
                    .filter(|k| self.prog(*k).is_some())
                    .unwrap_or(copy);
                self.composite_into(tp, from_target, Which::B, eased, w, h, pic_dest);
            }
        }
        if let Some(over) = over {
            // Each layer bottom to top: draw it at its scale (premultiplied, opacity
            // applied), then composite it over the picture so far (the engine's
            // source-over, or its blend pass) into a working picture; the picture
            // track stays in its own target for `@picture`. A layer that later layers
            // read draws into a target of exactly its size and stays there for them.
            // The last working picture is copied to the output.
            let read: std::collections::HashSet<usize> = movie
                .layers
                .iter()
                .flat_map(|l| l.channels.iter().flatten())
                .filter_map(|c| match c.source {
                    crate::movie::Source::Layer { index, .. } => Some(index),
                    _ => None,
                })
                .collect();
            let full = (w as usize, h as usize);
            let picture = self.picture.as_ref().unwrap().tex;
            let mut held: Vec<(u64, (glow::Texture, usize, usize))> = vec![];
            let mut cur = Which::Picture;
            for (&(j, local), k) in active.iter().zip(&layer_keys) {
                let layer = &movie.layers[j];
                let (lw, lh) = match layer.scale {
                    None => (sw, sh),
                    Some(s) => (
                        ((w as f32 * s).round() as i32).clamp(1, w),
                        ((h as f32 * s).round() as i32).clamp(1, h),
                    ),
                };
                let below = self.target(cur).tex;
                let mut passes = vec![
                    (INPUT_TEX, (below, full.0, full.1)),
                    (PICTURE_TEX, (picture, full.0, full.1)),
                ];
                passes.extend(held.iter().copied());
                let item = Item {
                    start: layer.start,
                    duration: layer.duration(),
                    itime: layer.scene_time(local),
                    uniforms: &layer.uniforms,
                    opacity: Some(layer.opacity_at(local)),
                    passes,
                };
                let into = if read.contains(&j) {
                    ensure_target(&self.gl, &mut self.held[j], lw, lh);
                    Which::Held(j)
                } else {
                    Which::Layer
                };
                self.draw_scene(*k, &item, local, into, lw, lh, info, movie);
                if let Which::Held(j) = into {
                    let t = self.target(into);
                    held.push((LAYER_TEX - j as u64, (t.tex, lw as usize, lh as usize)));
                }
                let next = match cur {
                    Which::Work(0) => Which::Work(1),
                    _ => Which::Work(0),
                };
                let prog = k.trans.filter(|b| self.prog(*b).is_some()).unwrap_or(over);
                self.composite_into(prog, cur, into, layer.opacity_at(local), w, h, Some(next));
                cur = next;
            }
            self.composite(copy, cur, cur, 0.0, w, h);
        }
        if let Some(timer) = &mut self.timer {
            timer.end(&self.gl);
        }
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.use_program(None);
            self.gl.bind_vertex_array(None);
        }
        self.out.as_ref().map(|t| t.tex)
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

    fn target(&self, w: Which) -> &Target {
        match w {
            Which::A => self.a.as_ref().unwrap(),
            Which::B => self.b.as_ref().unwrap(),
            Which::Freeze => self.freeze.as_ref().unwrap(),
            Which::Work(i) => self.work[i].as_ref().unwrap(),
            Which::Layer => self.layer.as_ref().unwrap(),
            Which::Picture => self.picture.as_ref().unwrap(),
            Which::Held(j) => self.held[j].as_ref().unwrap(),
        }
    }

    fn target_mut(&mut self, w: Which) -> &mut Target {
        match w {
            Which::A => self.a.as_mut().unwrap(),
            Which::B => self.b.as_mut().unwrap(),
            Which::Freeze => self.freeze.as_mut().unwrap(),
            Which::Work(i) => self.work[i].as_mut().unwrap(),
            Which::Layer => self.layer.as_mut().unwrap(),
            Which::Picture => self.picture.as_mut().unwrap(),
            Which::Held(j) => self.held[j].as_mut().unwrap(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_scene(
        &mut self,
        keys: ShotKeys,
        item: &Item<'_>,
        local: f32,
        into: Which,
        sw: i32,
        sh: i32,
        info: &FrameInfo,
        movie: &Movie,
    ) {
        let gl = self.gl.clone();
        let t = self.target_mut(into);
        t.used = (sw, sh);
        let fbo = t.fbo;
        let Some(p) = self.prog(keys.scene) else {
            // Failed with no working version: a shot shows black, a layer nothing.
            let a = if item.opacity.is_some() { 0.0 } else { 1.0 };
            unsafe {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                gl.viewport(0, 0, sw, sh);
                gl.clear_color(0.0, 0.0, 0.0, a);
                gl.clear(glow::COLOR_BUFFER_BIT);
            }
            return;
        };
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.viewport(0, 0, sw, sh);
            gl.disable(glow::BLEND);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            gl.bind_vertex_array(Some(self.vao));
            gl.use_program(Some(p.prog));
            let f1 = |n: &str, v: f32| {
                if let Some(l) = p.loc(n) {
                    gl.uniform_1_f32(Some(l), v)
                }
            };
            if let Some(l) = p.loc("iResolution") {
                gl.uniform_3_f32(Some(l), sw as f32, sh as f32, 1.0);
            }
            f1("iTime", item.itime);
            f1("iTimeDelta", info.dt);
            f1("iFrameRate", movie.fps);
            f1("iSampleRate", 44100.0);
            f1("iMovieTime", item.start + local);
            f1("iShotTime", local);
            f1("iShotDuration", item.duration);
            f1("iShotProgress", (local / item.duration).clamp(0.0, 1.0));

            if let Some(l) = p.loc("iFrame") {
                gl.uniform_1_i32(Some(l), info.frame);
            }
            if let Some(l) = p.loc("iMouse") {
                gl.uniform_4_f32(Some(l), 0.0, 0.0, 0.0, 0.0);
            }
            if let Some(l) = p.loc("iDate") {
                gl.uniform_4_f32(Some(l), 2026.0, 0.0, 1.0, item.start + local);
            }
            // Channels: the texture and its sampler on unit n (the iChannelN uniforms
            // were set to n at link time), or black with resolution 0.
            let mut res = [0f32; 3 * N_CHANNELS];
            let mut meta = [0f32; 4 * N_CHANNELS];
            let mut times = [0f32; N_CHANNELS];
            let mut sampled = false;
            for (n, c) in keys.channels.iter().enumerate() {
                // Footage: the shown frame's source time; past the end with
                // `end = "black"`, black with the clip's resolution.
                if let Some(c) = c
                    && let Some(fk) = c.footage
                {
                    times[n] = c.time;
                    if c.tex == FOOTAGE_BLACK
                        && let Some((w, h)) = self.footage.get(&fk).and_then(|f| f.scan.size)
                    {
                        res[n * 3..n * 3 + 3].copy_from_slice(&[w as f32, h as f32, 1.0]);
                    }
                }
                let ready = c.and_then(|c| {
                    if is_pass_key(c.tex) {
                        // Another pass's picture, exactly its size; black (resolution 0)
                        // when it isn't drawn this frame (an inactive or hidden layer).
                        let &(_, (tex, w, h)) = item.passes.iter().find(|(k, _)| *k == c.tex)?;
                        return Some((tex, w, h, [0.0; 4], self.sampler(c)));
                    }
                    match self.textures.get(&c.tex)?.state {
                        TexState::Ready {
                            tex, w, h, meta, ..
                        } => Some((tex, w, h, meta, self.sampler(c))),
                        _ => None,
                    }
                });
                if ready.is_none() && p.loc(CHANNEL_UNIFORMS[n]).is_none() {
                    continue;
                }
                gl.active_texture(glow::TEXTURE0 + n as u32);
                match ready {
                    Some((tex, w, h, m, sampler)) => {
                        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                        gl.bind_sampler(n as u32, Some(sampler));
                        res[n * 3..n * 3 + 3].copy_from_slice(&[w as f32, h as f32, 1.0]);
                        meta[n * 4..n * 4 + 4].copy_from_slice(&m);
                        sampled = true;
                    }
                    None => {
                        gl.bind_texture(glow::TEXTURE_2D, Some(self.dummy));
                        gl.bind_sampler(n as u32, None);
                    }
                }
            }
            if let Some(l) = p.loc("iChannelResolution") {
                gl.uniform_3_f32_slice(Some(l), &res);
            }
            if let Some(l) = p.loc("iChannelMeta") {
                gl.uniform_4_f32_slice(Some(l), &meta);
            }
            if let Some(l) = p.loc("iChannelTime") {
                gl.uniform_1_f32_slice(Some(l), &times);
            }
            for (name, a) in item.uniforms {
                if let Some(l) = p.loc(name) {
                    set_anim(&gl, l, a, local);
                }
            }
            // A very slow frame is drawn in horizontal strips, flushed one by one, so
            // no single GPU job runs long enough to trip the driver's watchdog
            // (Windows resets the GPU after ~2 s). Each strip is its own viewport, as
            // for sound blocks: gl_FragCoord stays window-relative, so the picture is
            // the same. Not a scissor: the Apple Software Renderer left parts of
            // scissored strips undrawn (stale rows from the previous frame).
            let tiles = self.tiles.clamp(1, sh.max(1) as u32) as i32;
            if tiles == 1 {
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            } else {
                for i in 0..tiles {
                    let y0 = sh * i / tiles;
                    let y1 = sh * (i + 1) / tiles;
                    gl.viewport(0, y0, sw, y1 - y0);
                    gl.draw_arrays(glow::TRIANGLES, 0, 3);
                    gl.flush();
                }
                gl.viewport(0, 0, sw, sh);
            }
            // Sampler objects override texture state on their unit: unbind them, or
            // the composite pass (and egui) would sample through them.
            if sampled {
                for n in 0..N_CHANNELS as u32 {
                    gl.bind_sampler(n, None);
                }
            }
            gl.active_texture(glow::TEXTURE0);
        }
    }

    fn sampler(&self, c: ChanKey) -> glow::Sampler {
        self.samplers
            .iter()
            .find(|(k, _)| *k == (c.filter, c.wrap))
            .map(|(_, s)| *s)
            .expect("a sampler for every (filter, wrap)")
    }

    fn composite(&mut self, prog: u64, from: Which, to: Which, progress: f32, w: i32, h: i32) {
        self.composite_into(prog, from, to, progress, w, h, None);
    }

    /// `composite`, into a layer working picture (`dest`, never flipped) or, with
    /// None, into the output (flipped for readback when asked).
    #[allow(clippy::too_many_arguments)]
    fn composite_into(
        &mut self,
        prog: u64,
        from: Which,
        to: Which,
        progress: f32,
        w: i32,
        h: i32,
        dest: Option<Which>,
    ) {
        let gl = self.gl.clone();
        let (ft, tt) = (self.target(from), self.target(to));
        let scale = |t: &Target| {
            (
                t.used.0 as f32 / t.alloc.0 as f32,
                t.used.1 as f32 / t.alloc.1 as f32,
            )
        };
        let maxv = |t: &Target| {
            (
                (t.used.0 as f32 - 0.5) / t.alloc.0 as f32,
                (t.used.1 as f32 - 0.5) / t.alloc.1 as f32,
            )
        };
        let (fs, fm, ftex) = (scale(ft), maxv(ft), ft.tex);
        let (ts, tm, ttex) = (scale(tt), maxv(tt), tt.tex);
        let flip = dest.is_none() && self.flip_output;
        let out = match dest {
            None => {
                self.out_flipped = self.flip_output;
                self.out.as_ref().unwrap().fbo
            }
            Some(d) => {
                let t = self.target_mut(d);
                t.used = (w, h);
                t.fbo
            }
        };
        let Some(p) = self.prog(prog) else { return };
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(out));
            gl.viewport(0, 0, w, h);
            gl.bind_vertex_array(Some(self.vao));
            gl.use_program(Some(p.prog));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(ftex));
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(ttex));
            if let Some(l) = p.loc("uFrom") {
                gl.uniform_1_i32(Some(l), 0);
            }
            if let Some(l) = p.loc("uTo") {
                gl.uniform_1_i32(Some(l), 1);
            }
            let v2 = |n: &str, v: (f32, f32)| {
                if let Some(l) = p.loc(n) {
                    gl.uniform_2_f32(Some(l), v.0, v.1)
                }
            };
            v2("uFromScale", fs);
            v2("uToScale", ts);
            v2("uFromMax", fm);
            v2("uToMax", tm);
            v2("uOutRes", (w as f32, h as f32));
            if let Some(l) = p.loc("progress") {
                gl.uniform_1_f32(Some(l), progress);
            }
            if let Some(l) = p.loc("ratio") {
                gl.uniform_1_f32(Some(l), w as f32 / h as f32);
            }
            if let Some(l) = p.loc("sg_flip") {
                gl.uniform_1_i32(Some(l), flip as i32);
            }
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    /// Size of the output texture (the last render's w × h), if anything was rendered.
    pub fn output_size(&self) -> Option<(usize, usize)> {
        self.out
            .as_ref()
            .map(|t| (t.alloc.0 as usize, t.alloc.1 as usize))
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

    /// Compile (or reuse) an audio program. Returns its key for `render_audio`.
    pub fn audio_program(
        &mut self,
        src: &Assembled,
        what: &str,
    ) -> std::result::Result<u64, ShaderError> {
        let k = hash(&src.text);
        if !self.audio.contains_key(&k) {
            let p = self.compile(src).map_err(|m| ShaderError {
                what: what.into(),
                messages: m,
            })?;
            self.audio.insert(k, p);
        }
        Ok(k)
    }

    /// Delete audio programs that aren't in `keep`.
    pub fn retain_audio(&mut self, keep: &[u64]) {
        let drop: Vec<u64> = self
            .audio
            .keys()
            .filter(|k| !keep.contains(k))
            .copied()
            .collect();
        for k in drop {
            if let Some(p) = self.audio.remove(&k) {
                unsafe { self.gl.delete_program(p.prog) };
            }
        }
    }

    /// Queue audio program `key` for global samples `first .. first + count`
    /// (count ≤ AUDIO_BLOCK). `inputs` = (music, sfx, global index of their first sample),
    /// bound for the mix pass as `sg_music` / `sg_sfx`.
    ///
    /// Never waits for the GPU: the block is drawn, read back into a pixel pack buffer
    /// and fenced. Collect it with `poll_audio` (never blocks) or `wait_audio` (blocks).
    /// Only WebGL 2 features are used (fences, PBOs), so a web build can do the same.
    pub fn submit_audio(
        &mut self,
        key: u64,
        first: usize,
        count: usize,
        sample_rate: u32,
        shot: (f32, f32),
        inputs: Option<(&[f32], &[f32], i64)>,
    ) -> std::result::Result<AudioTicket, String> {
        const W: i32 = 1024;
        assert!(count <= AUDIO_BLOCK);
        let rows = (count as i32 + W - 1) / W;
        let gl = self.gl.clone();
        let Some(prog) = self.audio.get(&key) else {
            return Err("audio program not compiled".into());
        };
        unsafe {
            if self.audio_target.is_none() {
                let tex = gl.create_texture()?;
                gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA32F as i32,
                    W,
                    (AUDIO_BLOCK as i32) / W,
                    0,
                    glow::RGBA,
                    glow::FLOAT,
                    glow::PixelUnpackData::Slice(None),
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
                gl.bind_texture(glow::TEXTURE_2D, None);
                let fbo = gl
                    .create_framebuffer()
                    .inspect_err(|_| gl.delete_texture(tex))?;
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                gl.framebuffer_texture_2d(
                    glow::FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0,
                    glow::TEXTURE_2D,
                    Some(tex),
                    0,
                );
                let ok =
                    gl.check_framebuffer_status(glow::FRAMEBUFFER) == glow::FRAMEBUFFER_COMPLETE;
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                if !ok {
                    gl.delete_framebuffer(fbo);
                    gl.delete_texture(tex);
                    return Err(
                        "this GPU can't render to float textures, so GLSL sound is unavailable"
                            .into(),
                    );
                }
                self.audio_target = Some((fbo, tex));
            }
            let (fbo, _) = self.audio_target.unwrap();
            gl.use_program(Some(prog.prog));
            let mut bindings = AudioSubmit {
                gl: &gl,
                input_tex: vec![],
            };
            let mut staging = None;
            let t_upload = std::time::Instant::now();
            if let Some((music, sfx, base)) = inputs {
                let rows = (music.len() / 2).max(1).div_ceil(TRACK_WIDTH);
                if rows > self.max_texture_size {
                    return Err(format!(
                        "the soundtrack is too long for one texture ({rows} rows, GPU max {})",
                        self.max_texture_size
                    ));
                }
                if self.persistent {
                    // Write the inputs into mapped memory and copy them into textures
                    // on the GPU: the driver never reads our memory, so nothing waits.
                    let row = TRACK_WIDTH * 8;
                    let st = take_buf(
                        &gl,
                        &mut self.audio_staging,
                        self.persistent,
                        false,
                        (2 * rows * row) as i32,
                    )?;
                    for (k, data) in [music, sfx].into_iter().enumerate() {
                        let bytes =
                            std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 4);
                        let off = k * rows * row;
                        let dst = std::slice::from_raw_parts_mut(st.ptr.add(off), rows * row);
                        dst[..bytes.len()].copy_from_slice(bytes);
                        dst[bytes.len()..].fill(0);
                    }
                    // Allocate before binding the unpack buffer: tex_image_2d uses a
                    // null client pointer to reserve storage for the texture.
                    for unit in 0..self.audio_inputs.len() {
                        if self.audio_inputs[unit].is_some_and(|(_, r)| r >= rows) {
                            continue;
                        }
                        gl.active_texture(glow::TEXTURE0 + unit as u32);
                        let tex = new_track_texture(&gl, rows)?;
                        if let Some((old, _)) = self.audio_inputs[unit].replace((tex, rows)) {
                            gl.delete_texture(old);
                        }
                    }
                    gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(st.buf));
                    for (unit, name) in ["sg_music", "sg_sfx"].into_iter().enumerate() {
                        gl.active_texture(glow::TEXTURE0 + unit as u32);
                        let (tex, _) = self.audio_inputs[unit].unwrap();
                        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                        gl.tex_sub_image_2d(
                            glow::TEXTURE_2D,
                            0,
                            0,
                            0,
                            TRACK_WIDTH as i32,
                            rows as i32,
                            glow::RG,
                            glow::FLOAT,
                            glow::PixelUnpackData::BufferOffset((unit * rows * row) as u32),
                        );
                        if let Some(l) = prog.loc(name) {
                            gl.uniform_1_i32(Some(l), unit as i32);
                        }
                    }
                    gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
                    staging = Some(st);
                } else {
                    for (unit, (name, data)) in [("sg_music", music), ("sg_sfx", sfx)]
                        .into_iter()
                        .enumerate()
                    {
                        // select the unit first: upload_track binds the new texture to it
                        gl.active_texture(glow::TEXTURE0 + unit as u32);
                        let tex = upload_track(&gl, data, self.max_texture_size)?;
                        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                        if let Some(l) = prog.loc(name) {
                            gl.uniform_1_i32(Some(l), unit as i32);
                        }
                        bindings.input_tex.push(tex);
                    }
                }
                if let Some(l) = prog.loc("sg_len") {
                    gl.uniform_1_i32(Some(l), (music.len() / 2) as i32);
                }
                if let Some(l) = prog.loc("sg_base") {
                    gl.uniform_1_i32(Some(l), base as i32);
                }
            }
            self.audio_timing.upload_ms += ms_since(t_upload);
            let t_draw = std::time::Instant::now();
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.viewport(0, 0, W, rows);
            gl.disable(glow::BLEND);
            gl.bind_vertex_array(Some(self.vao));
            let f1 = |n: &str, v: f32| {
                if let Some(l) = prog.loc(n) {
                    gl.uniform_1_f32(Some(l), v)
                }
            };
            f1("iSampleRate", sample_rate as f32);
            f1("iShotStart", shot.0);
            f1("iShotDuration", shot.1);
            if let Some(l) = prog.loc("iBlockWidth") {
                gl.uniform_1_i32(Some(l), W);
            }
            if let Some(l) = prog.loc("iBlockOffset") {
                gl.uniform_1_i32(Some(l), first as i32);
            }
            // Normally one draw. If the last block was slow (a heavy mainSound on a
            // slow GPU), draw in bands of a few rows, flushed one by one, so no single
            // job runs long enough to trip the driver's watchdog. Flushing costs
            // batching, so it's only done when needed. gl_FragCoord stays
            // window-relative, so the sample index is the same either way.
            let submitted = std::time::Instant::now();
            let band = if self.audio_banded { 4 } else { rows };
            let mut y0 = 0;
            while y0 < rows {
                let h = band.min(rows - y0);
                gl.viewport(0, y0, W, h);
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
                if self.audio_banded {
                    gl.flush();
                }
                y0 += h;
            }
            // Read back into a buffer on the GPU: queued like a draw, no wait.
            let pbo = take_buf(
                &gl,
                &mut self.audio_pbos,
                self.persistent,
                true,
                W * rows * 16,
            )?;
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(pbo.buf));
            gl.read_pixels(
                0,
                0,
                W,
                rows,
                glow::RGBA,
                glow::FLOAT,
                glow::PixelPackData::BufferOffset(0),
            );
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            self.audio_timing.draw_ms += ms_since(t_draw);
            let t_fence = std::time::Instant::now();
            let fence = AudioFence {
                gl: gl.clone(),
                sync: gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0)?,
            };
            // Make sure the fence reaches the GPU without anyone waiting on it.
            gl.flush();
            self.audio_timing.fence_ms += ms_since(t_fence);
            self.audio_ticket += 1;
            self.audio_jobs.insert(
                self.audio_ticket,
                AudioJob {
                    pbo,
                    staging,
                    fence,
                    count,
                    submitted,
                },
            );
            Ok(AudioTicket(self.audio_ticket))
        }
    }

    /// The samples of a submitted block if the GPU has finished it, without waiting.
    /// None = not yet; the ticket stays valid. Some = done; the ticket is used up.
    pub fn poll_audio(&mut self, t: AudioTicket) -> Option<std::result::Result<Vec<f32>, String>> {
        let Some(job) = self.audio_jobs.get(&t.0) else {
            return Some(Err("unknown sound block".into()));
        };
        let t_poll = std::time::Instant::now();
        let status = unsafe { self.gl.client_wait_sync(job.fence.sync, 0, 0) };
        self.audio_timing.poll_ms += ms_since(t_poll);
        match status {
            glow::ALREADY_SIGNALED | glow::CONDITION_SATISFIED => {}
            glow::WAIT_FAILED => {
                self.cancel_audio(t);
                return Some(Err("waiting for a sound block failed".into()));
            }
            _ => return None,
        }
        let job = self.audio_jobs.remove(&t.0).unwrap();
        Some(self.finish_audio(job, false))
    }

    /// The samples of a submitted block, waiting for the GPU if needed. This is a
    /// GPU→CPU sync (counted in `blocking_audio_reads`): only for headless use, or
    /// for the chunk under a paused playhead.
    pub fn wait_audio(&mut self, t: AudioTicket) -> std::result::Result<Vec<f32>, String> {
        let job = self.audio_jobs.remove(&t.0).ok_or("unknown sound block")?;
        self.blocking_audio_reads += 1;
        loop {
            match unsafe {
                self.gl
                    .client_wait_sync(job.fence.sync, glow::SYNC_FLUSH_COMMANDS_BIT, 100_000_000)
            } {
                glow::ALREADY_SIGNALED | glow::CONDITION_SATISFIED => break,
                glow::WAIT_FAILED => {
                    return Err("waiting for a sound block failed".into());
                }
                _ => {}
            }
        }
        self.finish_audio(job, true)
    }

    /// Forget a submitted block (its result is no longer wanted).
    pub fn cancel_audio(&mut self, t: AudioTicket) {
        // The GPU may still be using these buffers, so only completed jobs
        // can return their storage to the pools.
        self.audio_jobs.remove(&t.0);
    }

    /// Read a finished block's samples and recycle its buffers.
    fn finish_audio(
        &mut self,
        job: AudioJob,
        waited: bool,
    ) -> std::result::Result<Vec<f32>, String> {
        let n = job.count;
        let mut out = Vec::with_capacity(n * 2);
        let t_read = std::time::Instant::now();
        unsafe {
            let mapping = if job.pbo.ptr.is_null() {
                // The fence has passed, so mapping doesn't wait for the GPU (it is still
                // a driver round trip). WebGL 2 would use getBufferSubData here.
                let gl = &job.pbo.gl;
                gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(job.pbo.buf));
                Some(AudioReadMapping {
                    buffer: &job.pbo,
                    ptr: gl.map_buffer_range(
                        glow::PIXEL_PACK_BUFFER,
                        0,
                        (n * 16) as i32,
                        glow::MAP_READ_BIT,
                    ),
                })
            } else {
                // Persistent and coherent: the fence makes the samples available.
                None
            };
            let p = mapping.as_ref().map_or(job.pbo.ptr, |m| m.ptr);
            if p.is_null() {
                return Err("mapping a finished sound block failed".into());
            }
            // RGBA32F texels. Mappings are aligned on recent GL versions.
            if (p as usize).is_multiple_of(std::mem::align_of::<f32>()) {
                let px = std::slice::from_raw_parts(p as *const [f32; 4], n);
                for t in px {
                    out.push(sanitize(t[0]));
                    out.push(sanitize(t[1]));
                }
            } else {
                let px = std::slice::from_raw_parts(p as *const [u8; 16], n);
                for t in px {
                    let f = |i: usize| f32::from_ne_bytes(t[i..i + 4].try_into().unwrap());
                    out.push(sanitize(f(0)));
                    out.push(sanitize(f(4)));
                }
            }
        }
        self.audio_timing.read_ms += ms_since(t_read);
        // Normally one draw per block; after a slow one (a heavy mainSound on a slow
        // GPU), bands (see submit_audio). Judged by the time from submit to result, with
        // no timer queries (each would be another driver round trip). A blocking wait
        // measures the block itself, as before. A polled block's time also includes the
        // frames until it was polled and the scene queued ahead of it, so the
        // thresholds are looser: only a clearly heavy mainSound switches banding on.
        let ms = job.submitted.elapsed().as_secs_f32() * 1000.0;
        let (on, off) = if waited { (50.0, 20.0) } else { (250.0, 100.0) };
        self.audio_banded = if self.audio_banded { ms > off } else { ms > on };
        self.recycle_audio_buffers(job);
        Ok(out)
    }

    fn recycle_audio_buffers(&mut self, job: AudioJob) {
        for (pool, b) in [(true, Some(job.pbo)), (false, job.staging)] {
            let Some(b) = b else { continue };
            let pool = if pool {
                &mut self.audio_pbos
            } else {
                &mut self.audio_staging
            };
            if pool.len() < 8 {
                pool.push(b);
            }
        }
    }

    /// Use persistently mapped buffers for sound streaming (the default where the
    /// driver has GL 4.4 / ARB_buffer_storage), or not (tests). Returns whether they're on.
    pub fn set_persistent_buffers(&mut self, on: bool) -> bool {
        self.persistent = on && self.can_persist;
        self.persistent
    }

    pub fn persistent_buffers(&self) -> bool {
        self.persistent
    }

    /// Sound blocks submitted and not yet collected.
    pub fn audio_in_flight(&self) -> usize {
        self.audio_jobs.len()
    }

    /// The language `m`'s shaders compile in on this driver: the portable baseline,
    /// or the film's `glsl` opt-in. Err (a message for the user) if the driver can't
    /// provide the opt-in.
    pub fn dialect_for(&self, m: &Movie) -> std::result::Result<Dialect, String> {
        self.driver.dialect(m.glsl)
    }
}

/// The fragment shader for a shot's scene.
pub fn scene_source(dialect: Dialect, shot: &Shot, cues: &str, movie: &Movie) -> Result<Assembled> {
    let mut header = SCENE_HEADER.to_string();
    for (name, a) in &shot.uniforms {
        header += &format!("uniform {} {name};\n", a.glsl_type());
    }
    header += cues;
    Ok(Builder::new(dialect, &header, &movie.root)
        .libraries(&movie.libraries)
        .file(&shot.scene)?
        .footer(SCENE_FOOTER)
        .build())
}

/// The fragment shader for a layer (#69): the scene header plus the layer's
/// opacity, its uniforms (with `SG_HAS_<name>` defines, so a library pass can default
/// the ones a film leaves out), its params as `#define`s, the cues, then the scene
/// (a film file or a library pass) and the layer footer.
pub fn layer_source(
    dialect: Dialect,
    layer: &Layer,
    cues: &str,
    movie: &Movie,
) -> Result<Assembled> {
    let root = &movie.root;
    let mut header = SCENE_HEADER.to_string();
    for (name, a) in &layer.uniforms {
        header += &format!("uniform {} {name};\n#define SG_HAS_{name}\n", a.glsl_type());
    }
    for (name, v) in &layer.params {
        header += &format!("#define {name} {}\n", crate::shader::define_value(v));
    }
    header += cues;
    let b = Builder::new(dialect, &header, root).libraries(&movie.libraries);
    let b = match &layer.scene {
        LayerScene::File(p) => b.file(p)?,
        LayerScene::Pass(kind) => b.pass(root, "layer pass", kind, |_| {
            anyhow::anyhow!(
                "layer \"{}\": type \"{kind}\" needs a library or a .glsl file",
                layer.id
            )
        })?,
    };
    Ok(b.footer(LAYER_FOOTER).build())
}

/// The fragment shader for a layer's blend pass (#72): the transition header, with
/// `getBase`/`getLayer`/`opacity` naming the picture below, the layer and its opacity,
/// the layer's params as `#define`s, the cues, the pass (a library's or a film file),
/// then the footer that writes `blend(uv)`.
pub fn blend_source(
    dialect: Dialect,
    layer: &Layer,
    kind: &str,
    cues: &str,
    movie: &Movie,
) -> Result<Assembled> {
    let root = &movie.root;
    let mut header = TRANSITION_HEADER.to_string();
    header += BLEND_HEADER;
    for (name, v) in &layer.params {
        header += &format!("#define {name} {}\n", crate::shader::define_value(v));
    }
    header += cues;
    let b = Builder::new(dialect, &header, root).libraries(&movie.libraries);
    let b = b.pass(root, "blend", kind, |_| {
        anyhow::anyhow!(
            "layer \"{}\": blend \"{kind}\" needs a library or a .glsl file",
            layer.id
        )
    })?;
    Ok(b.footer(&format!("{BLEND_FOOTER}{TRANSITION_FOOTER}"))
        .build())
}

/// Which of `iChannel0..7` an assembled scene mentions, outside the header that
/// declares them (`check` warns when a shot reads a channel it binds nothing to).
pub fn channels_read(src: &Assembled) -> [bool; N_CHANNELS] {
    let text = src.text.replacen(SCENE_HEADER, "", 1);
    let mut out = [false; N_CHANNELS];
    for (n, name) in CHANNEL_UNIFORMS.iter().enumerate() {
        out[n] = crate::shader::mentions_ident(&text, name);
    }
    out
}

/// The fragment shader for a transition (a built-in name or a path ending in .glsl).
pub fn transition_source(
    dialect: Dialect,
    kind: &str,
    params: &[(String, Vec<f32>)],
    cues: &str,
    movie: &Movie,
) -> Result<Assembled> {
    let root = &movie.root;
    let mut header = TRANSITION_HEADER.to_string();
    for (name, v) in params {
        header += &format!("#define {name} {}\n", crate::shader::define_value(v));
    }
    header += cues;
    let b = Builder::new(dialect, &header, root).libraries(&movie.libraries);
    let b = match kind {
        "copy" => b.builtin("copy", COPY)?,
        "over" => b.builtin("over", OVER)?,
        _ => b.pass(root, "transition", kind, |_| {
            anyhow::anyhow!(
                "unknown transition \"{kind}\"; use a library's (\"transitions/iris\") or \
             a file in the film (\"fx/iris.glsl\")"
            )
        })?,
    };
    Ok(b.footer(TRANSITION_FOOTER).build())
}

/// The vertex shader every program uses: one full-screen triangle.
pub fn vertex_source(dialect: Dialect) -> String {
    format!("{}{VERTEX}", dialect.version_line())
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
        let buf = gl.create_buffer()?;
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
        let tex = gl.create_texture()?;
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
        let tex = gl.create_texture()?;
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
                self.gl.delete_program(p.prog);
            }
            self.audio_jobs.clear();
            self.audio_pbos.clear();
            self.audio_staging.clear();
            for (t, _) in self.audio_inputs.iter().flatten() {
                self.gl.delete_texture(*t);
            }
            if let Some((fbo, tex)) = self.audio_target.take() {
                self.gl.delete_framebuffer(fbo);
                self.gl.delete_texture(tex);
            }
            for t in [
                self.a.take(),
                self.b.take(),
                self.freeze.take(),
                self.out.take(),
            ]
            .into_iter()
            .flatten()
            {
                self.gl.delete_framebuffer(t.fbo);
                self.gl.delete_texture(t.tex);
            }
            for (_, e) in std::mem::take(&mut self.textures) {
                self.delete_texture_state(e.state);
            }
            for (_, t) in std::mem::take(&mut self.spare) {
                self.gl.delete_texture(t);
            }
            for (_, s) in std::mem::take(&mut self.samplers) {
                self.gl.delete_sampler(s);
            }
            self.gl.delete_vertex_array(self.vao);
            self.gl.delete_texture(self.dummy);
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
            gl.delete_framebuffer(old.fbo);
            gl.delete_texture(old.tex);
        }
        let tex = gl.create_texture().expect("create texture");
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
        let fbo = gl.create_framebuffer().expect("create framebuffer");
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
    let once = |t: f32| {
        if t < layer.start {
            layer.start - t
        } else if t >= layer.end {
            (t - layer.end) * 2.0
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
            queries.push(unsafe { gl.create_query().ok()? });
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
