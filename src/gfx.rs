//! OpenGL rendering: scene programs, transitions, off-screen targets, sound synthesis.
//!
//! Every frame: the shot (or both shots of a transition) renders into an off-screen
//! texture at `scale` × output size, then a compositor pass (a transition shader, or a
//! plain copy) draws the result into the output texture at full size.

use crate::anim::Anim;
use crate::movie::TransitionKind;
use crate::movie::{At, Movie, Shot, glsl_float};
use crate::shader::{Assembled, Builder, Dialect, DriverGlsl, remap_log};
use anyhow::{Result, anyhow};
use eframe::glow::{self, HasContext};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use toodee::{TooDeeOps, TooDeeOpsMut, TranslateOps};

pub const BUILTIN_TRANSITIONS: &[(&str, &str)] = &[
    ("copy", include_str!("glsl/transitions/copy.glsl")),
    ("crossfade", include_str!("glsl/transitions/crossfade.glsl")),
    ("fade", include_str!("glsl/transitions/fade.glsl")),
    ("iris", include_str!("glsl/transitions/iris.glsl")),
    ("dissolve", include_str!("glsl/transitions/dissolve.glsl")),
    ("wipe", include_str!("glsl/transitions/wipe.glsl")),
];

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
uniform vec3 iChannelResolution[4];
uniform float iChannelTime[4];
uniform float iMovieTime;
uniform float iShotTime;
uniform float iShotDuration;
uniform float iShotProgress;
out vec4 shotglass_out;
";

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

/// Program keys for one shot: its scene and the transition into it.
#[derive(Clone, Copy, PartialEq)]
struct ShotKeys {
    scene: u64,
    trans: Option<u64>,
}

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
    /// Programs for each shot of the current movie version.
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
    /// Strips a scene is drawn in (more when frames are very slow; see draw_scene).
    tiles: u32,
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
    /// Draw the output upside down for readback (`set_readback`), and whether the
    /// current output was drawn that way.
    flip_output: bool,
    out_flipped: bool,
    freeze_key: Option<(usize, u64)>,
    generation: u64,
    timer: Option<GpuTimer>,
    /// Portability warnings from assembling the current movie's shaders.
    pub warnings: Vec<String>,
}

/// A sound block submitted with `Renderer::submit_audio`, not yet collected.
/// Tickets order like submissions, and fences pass in that order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AudioTicket(u64);

struct AudioJob {
    pbo: GpuBuf,
    /// Mix inputs written for the GPU to copy from (persistent buffers only).
    staging: Option<GpuBuf>,
    fence: glow::Fence,
    count: usize,
    submitted: std::time::Instant,
}

/// A pixel pack (readback) or unpack (staging) buffer for sound blocks. With
/// persistent mapping (`ptr` non-null) it stays mapped for its whole life, so reading
/// or writing it needs no GL call at all; it's only touched while no GPU command that
/// uses it is pending (its block's fence has passed, or it was just taken from the pool).
#[derive(Clone, Copy)]
struct GpuBuf {
    buf: glow::Buffer,
    bytes: i32,
    ptr: *mut u8,
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
    /// `still`, `strip` and `export` give the same picture for the same time.
    pub fn at(m: &Movie, t: f32) -> FrameInfo {
        FrameInfo {
            t,
            dt: m.fps_den as f32 / m.fps_num as f32,
            frame: m.frame_at(t).min(i32::MAX as i64) as i32,
            scale: 1.0,
        }
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
                wanted: vec![],
                wanted_ids: vec![],
                fallback: HashMap::new(),
                parallel,
                binary_dir,
                frame_no: 0,
                binary_hits: 0,
                tiles: 1,
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
                flip_output: false,
                out_flipped: false,
                freeze_key: None,
                generation: 0,
                timer,
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

    /// Force the number of strips scenes are drawn in (tests).
    pub fn set_tiles(&mut self, n: u32) {
        self.tiles = n.max(1);
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
        for shot in &movie.shots {
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
            if let (Some(scene), Some(trans)) = (scene, trans) {
                wanted.push(ShotKeys { scene, trans });
            }
        }
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
        self.wanted_ids = movie.shots.iter().map(|s| s.id.clone()).collect();
        self.fallback
            .retain(|id, _| movie.shots.iter().any(|s| &s.id == id));
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
                    return;
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
        // Drop fallbacks once their replacement works.
        let ready_ids: Vec<String> = self
            .wanted_ids
            .iter()
            .zip(&self.wanted)
            .filter(|(_, k)| self.keys_ready(**k))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ready_ids {
            self.fallback.remove(&id);
        }
        if self.frame_no.is_multiple_of(30) {
            self.collect_garbage();
            self.evict(&order);
        }
    }

    /// Program keys sorted by how soon the playhead needs them (seconds).
    fn priority(&self, movie: &Movie, t: f32) -> Vec<(u64, f32)> {
        let mut v: Vec<(u64, f32)> = vec![];
        for (i, shot) in movie.shots.iter().enumerate() {
            let Some(k) = self.wanted.get(i) else {
                continue;
            };
            let d = if t < shot.start {
                shot.start - t
            } else if t > shot.end() {
                (t - shot.end()) * 2.0 // behind the playhead: only needed when scrubbing back
            } else {
                0.0
            };
            v.push((k.scene, d));
            if let Some(tr) = k.trans {
                v.push((tr, d));
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
        ok(k.scene) && k.trans.is_none_or(ok)
    }

    /// The programs to draw shot `i` with: the current version if it compiled,
    /// otherwise the last working one. With `blocking`, the current version is
    /// compiled first, so the decision is made knowing whether it works.
    fn usable(&mut self, i: usize, blocking: bool) -> Option<ShotKeys> {
        let k = *self.wanted.get(i)?;
        if blocking {
            self.compile_now(k.scene);
            if let Some(t) = k.trans {
                self.compile_now(t);
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
        if let Some(f) = fb.filter(|f| self.keys_ready(*f)) {
            return Some(f);
        }
        // Nothing working to show: with `blocking`, draw what we have (a failed
        // scene shows black); otherwise wait for the compile.
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
        let mut keep: std::collections::HashSet<u64> = self.copy.into_iter().collect();
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
            self.gl.draw_arrays(glow::TRIANGLES, 0, 3);
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
        let needed: Vec<usize> = match at {
            At::Shot { shot, .. } => vec![shot],
            At::Transition { from, to, .. } => vec![from, to],
        };
        let mut keys = vec![];
        for &i in &needed {
            keys.push(self.usable(i, blocking)?);
        }
        let frame_no = self.frame_no;
        for k in &keys {
            for x in [Some(k.scene), k.trans].into_iter().flatten() {
                if let Some(e) = self.programs.get_mut(&x) {
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
        match at {
            At::Shot { shot, local } => {
                self.draw_scene(
                    keys[0].scene,
                    &movie.shots[shot],
                    local,
                    Which::A,
                    sw,
                    sh,
                    info,
                    movie,
                );
                self.composite(copy, Which::A, Which::A, 0.0, w, h);
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
                    let key = (to, self.generation);
                    if self.freeze_key != Some(key)
                        || self.freeze.as_ref().is_none_or(|f| f.alloc != (w, h))
                    {
                        ensure_target(&self.gl, &mut self.freeze, w, h);
                        let freeze_local = movie.shots[to].start - movie.shots[from].start;
                        self.draw_scene(
                            keys[0].scene,
                            &movie.shots[from],
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
                        keys[0].scene,
                        &movie.shots[from],
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
                    keys[1].scene,
                    &movie.shots[to],
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
                self.composite(tp, from_target, Which::B, eased, w, h);
            }
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
            if per_tile > 100.0 || (self.tiles > 1 && ms < 50.0 * self.tiles as f32) {
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
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_scene(
        &mut self,
        prog: u64,
        shot: &Shot,
        local: f32,
        into: Which,
        sw: i32,
        sh: i32,
        info: &FrameInfo,
        movie: &Movie,
    ) {
        let gl = self.gl.clone();
        let t = match into {
            Which::A => self.a.as_mut().unwrap(),
            Which::B => self.b.as_mut().unwrap(),
            Which::Freeze => self.freeze.as_mut().unwrap(),
        };
        t.used = (sw, sh);
        let fbo = t.fbo;
        let Some(p) = self.prog(prog) else {
            // failed with no working version: show black
            unsafe {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                gl.viewport(0, 0, sw, sh);
                gl.clear_color(0.0, 0.0, 0.0, 1.0);
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
            f1("iTime", shot.scene_time(local));
            f1("iTimeDelta", info.dt);
            f1("iFrameRate", movie.fps);
            f1("iSampleRate", 44100.0);
            f1("iMovieTime", shot.start + local);
            f1("iShotTime", local);
            f1("iShotDuration", shot.duration);
            f1("iShotProgress", (local / shot.duration).clamp(0.0, 1.0));
            if let Some(l) = p.loc("iFrame") {
                gl.uniform_1_i32(Some(l), info.frame);
            }
            if let Some(l) = p.loc("iMouse") {
                gl.uniform_4_f32(Some(l), 0.0, 0.0, 0.0, 0.0);
            }
            if let Some(l) = p.loc("iDate") {
                gl.uniform_4_f32(Some(l), 2026.0, 0.0, 1.0, shot.start + local);
            }
            for (i, ch) in ["iChannel0", "iChannel1", "iChannel2", "iChannel3"]
                .iter()
                .enumerate()
            {
                if let Some(l) = p.loc(ch) {
                    gl.active_texture(glow::TEXTURE0 + i as u32);
                    gl.bind_texture(glow::TEXTURE_2D, Some(self.dummy));
                    gl.uniform_1_i32(Some(l), i as i32);
                }
            }
            for (name, a) in &shot.uniforms {
                if let Some(l) = p.loc(name) {
                    set_anim(&gl, l, a, local);
                }
            }
            // A very slow frame is drawn in horizontal strips, flushed one by one, so
            // no single GPU job runs long enough to trip the driver's watchdog
            // (Windows resets the GPU after ~2 s).
            let tiles = self.tiles.clamp(1, sh.max(1) as u32) as i32;
            if tiles == 1 {
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            } else {
                gl.enable(glow::SCISSOR_TEST);
                for i in 0..tiles {
                    let y0 = sh * i / tiles;
                    let y1 = sh * (i + 1) / tiles;
                    gl.scissor(0, y0, sw, y1 - y0);
                    gl.draw_arrays(glow::TRIANGLES, 0, 3);
                    gl.flush();
                }
                gl.disable(glow::SCISSOR_TEST);
            }
        }
    }

    fn composite(&mut self, prog: u64, from: Which, to: Which, progress: f32, w: i32, h: i32) {
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
        let out = self.out.as_ref().unwrap().fbo;
        self.out_flipped = self.flip_output;
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
                gl.uniform_1_i32(Some(l), self.flip_output as i32);
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
        let (w, h) = (t.alloc.0 as usize, t.alloc.1 as usize);
        let (dw, dh) = (dst.num_cols(), dst.num_rows());
        // glReadPixels writes wherever the pack state points; it never sees the
        // slice length. Everything below relies on this check.
        assert!(
            x.checked_add(w).is_some_and(|r| r <= dw) && y.checked_add(h).is_some_and(|b| b <= dh),
            "read_output_into: {w}x{h} at ({x}, {y}) doesn't fit in {dw}x{dh}"
        );
        // GL fills rows bottom-up; with a flipped output, GL's first row is the
        // image's top row, so it goes at row y. Otherwise we flip afterwards.
        let buf = dst.data_mut().as_flattened_mut();
        unsafe {
            let gl = &self.gl;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(t.fbo));
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            gl.pixel_store_i32(glow::PACK_ROW_LENGTH, dw as i32);
            gl.pixel_store_i32(glow::PACK_SKIP_PIXELS, x as i32);
            gl.pixel_store_i32(glow::PACK_SKIP_ROWS, y as i32);
            gl.read_pixels(
                0,
                0,
                w as i32,
                h as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(buf)),
            );
            // Global state that egui and every other readback assume is default.
            gl.pixel_store_i32(glow::PACK_ROW_LENGTH, 0);
            gl.pixel_store_i32(glow::PACK_SKIP_PIXELS, 0);
            gl.pixel_store_i32(glow::PACK_SKIP_ROWS, 0);
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 4);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
        if !self.out_flipped {
            dst.view_mut((x, y), (x + w, y + h)).flip_rows();
        }
    }

    /// The pack state (alignment, row length, skip pixels, skip rows), for tests.
    pub fn pack_state(&self) -> [i32; 4] {
        unsafe {
            [
                glow::PACK_ALIGNMENT,
                glow::PACK_ROW_LENGTH,
                glow::PACK_SKIP_PIXELS,
                glow::PACK_SKIP_ROWS,
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
                let fbo = gl.create_framebuffer()?;
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
            let mut input_tex = vec![];
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
                        if st.ptr.is_null() {
                            let mut padded = vec![0u8; rows * row];
                            padded[..bytes.len()].copy_from_slice(bytes);
                            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(st.buf));
                            gl.buffer_sub_data_u8_slice(
                                glow::PIXEL_UNPACK_BUFFER,
                                off as i32,
                                &padded,
                            );
                        } else {
                            let dst = std::slice::from_raw_parts_mut(st.ptr.add(off), rows * row);
                            dst[..bytes.len()].copy_from_slice(bytes);
                            dst[bytes.len()..].fill(0);
                        }
                    }
                    gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(st.buf));
                    for (unit, name) in ["sg_music", "sg_sfx"].into_iter().enumerate() {
                        gl.active_texture(glow::TEXTURE0 + unit as u32);
                        let tex = match self.audio_inputs[unit] {
                            Some((t, r)) if r >= rows => t,
                            old => {
                                if let Some((t, _)) = old {
                                    gl.delete_texture(t);
                                }
                                let t = new_track_texture(&gl, rows)?;
                                self.audio_inputs[unit] = Some((t, rows));
                                t
                            }
                        };
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
                        input_tex.push(tex);
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
            let fence = gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0)?;
            // Make sure the fence reaches the GPU without anyone waiting on it.
            gl.flush();
            self.audio_timing.fence_ms += ms_since(t_fence);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            for (unit, t) in input_tex.iter().enumerate() {
                gl.active_texture(glow::TEXTURE0 + unit as u32);
                gl.bind_texture(glow::TEXTURE_2D, None);
                gl.delete_texture(*t); // deletion waits for the draw that uses it
            }
            gl.active_texture(glow::TEXTURE0);
            gl.use_program(None);
            gl.bind_vertex_array(None);
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
        let status = unsafe { self.gl.client_wait_sync(job.fence, 0, 0) };
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
        Some(Ok(self.finish_audio(job, false)))
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
                    .client_wait_sync(job.fence, glow::SYNC_FLUSH_COMMANDS_BIT, 100_000_000)
            } {
                glow::ALREADY_SIGNALED | glow::CONDITION_SATISFIED => break,
                glow::WAIT_FAILED => {
                    self.release_audio(job);
                    return Err("waiting for a sound block failed".into());
                }
                _ => {}
            }
        }
        Ok(self.finish_audio(job, true))
    }

    /// Forget a submitted block (its result is no longer wanted).
    pub fn cancel_audio(&mut self, t: AudioTicket) {
        if let Some(job) = self.audio_jobs.remove(&t.0) {
            self.release_audio(job);
        }
    }

    /// Read a finished block's samples and recycle its buffer, fence and query.
    fn finish_audio(&mut self, job: AudioJob, waited: bool) -> Vec<f32> {
        let gl = self.gl.clone();
        let n = job.count;
        let mut out = Vec::with_capacity(n * 2);
        let t_read = std::time::Instant::now();
        unsafe {
            let mapped = !job.pbo.ptr.is_null();
            let p = if mapped {
                // Persistent and coherent: once the fence has passed the samples are
                // simply there. No GL call, so nothing can wait for the driver.
                job.pbo.ptr
            } else {
                // The fence has passed, so mapping doesn't wait for the GPU (it is still
                // a driver round trip). WebGL 2 would use getBufferSubData here.
                gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(job.pbo.buf));
                gl.map_buffer_range(
                    glow::PIXEL_PACK_BUFFER,
                    0,
                    (n * 16) as i32,
                    glow::MAP_READ_BIT,
                )
            };
            if p.is_null() {
                out.resize(n * 2, 0.0);
            } else {
                // RGBA32F texels. Mappings are aligned (GL guarantees ≥ 64 bytes), but
                // don't bet on it.
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
                if !mapped {
                    gl.unmap_buffer(glow::PIXEL_PACK_BUFFER);
                }
            }
            if !mapped {
                gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
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
        self.release_audio(job);
        out
    }

    fn release_audio(&mut self, job: AudioJob) {
        unsafe { self.gl.delete_sync(job.fence) };
        for (pool, b) in [(true, Some(job.pbo)), (false, job.staging)] {
            let Some(b) = b else { continue };
            let pool = if pool {
                &mut self.audio_pbos
            } else {
                &mut self.audio_staging
            };
            if pool.len() < 8 {
                pool.push(b);
            } else {
                unsafe { self.gl.delete_buffer(b.buf) };
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
        let val = match v.len() {
            1 => glsl_float(v[0]),
            n => format!(
                "vec{n}({})",
                v.iter()
                    .map(|x| glsl_float(*x))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        header += &format!("#define {name} {val}\n");
    }
    header += cues;
    let b = Builder::new(dialect, &header, root).libraries(&movie.libraries);
    let b = match crate::movie::transition_kind(kind) {
        TransitionKind::File(p) => b.file(&root.join(p))?,
        TransitionKind::Library(lib, file) => b.generated(
            &format!("transition \"{kind}\""),
            &format!("#include <{lib}/{file}>\n"),
        )?,
        TransitionKind::Builtin(_) => {
            let src = BUILTIN_TRANSITIONS
                .iter()
                .find(|(n, _)| *n == kind)
                .map(|(_, s)| *s)
                .ok_or_else(|| {
                    anyhow!(
                        "unknown transition \"{kind}\"; use a library's (\"transitions/iris\") or \
                     a file in the film (\"fx/iris.glsl\")"
                    )
                })?;
            b.builtin(kind, src)?
        }
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
    gl: &glow::Context,
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
        Ok(GpuBuf { buf, bytes, ptr })
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
            for (_, j) in std::mem::take(&mut self.audio_jobs) {
                self.release_audio(j);
            }
            for b in std::mem::take(&mut self.audio_pbos)
                .into_iter()
                .chain(std::mem::take(&mut self.audio_staging))
            {
                self.gl.delete_buffer(b.buf);
            }
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
            let ready = unsafe { gl.get_query_parameter_u32(q, glow::QUERY_RESULT_AVAILABLE) } != 0;
            if !ready {
                break;
            }
            let ns = unsafe { gl.get_query_parameter_u32(q, glow::QUERY_RESULT) };
            fresh = Some(ns as f32 / 1.0e6);
            self.pending.pop_front();
            self.free.push(q);
        }
        fresh
    }
}
