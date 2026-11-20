//! OpenGL rendering: scene programs, transitions, off-screen targets, sound synthesis.
//!
//! Every frame: the shot (or both shots of a transition) renders into an off-screen
//! texture at `scale` × output size, then a compositor pass (a transition shader, or a
//! plain copy) draws the result into the output texture at full size.

use crate::anim::Anim;
use crate::movie::{At, Movie, Shot, glsl_float};
use crate::shader::{Assembled, Builder, Dialect, remap_log};
use anyhow::{Result, anyhow, bail};
use eframe::glow::{self, HasContext};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

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
out vec4 shotglass_out;
vec4 getFromColor(vec2 uv) { return texture(uFrom, min(clamp(uv, 0.0, 1.0) * uFromScale, uFromMax)); }
vec4 getToColor(vec2 uv) { return texture(uTo, min(clamp(uv, 0.0, 1.0) * uToScale, uToMax)); }
";

const TRANSITION_FOOTER: &str =
    "void main(){ shotglass_out = vec4(transition(gl_FragCoord.xy / uOutRes).rgb, 1.0); }";

const SOUND_HEADER: &str = "\
uniform float iSampleRate;
uniform int iBlockOffset;
uniform int iBlockWidth;
out vec4 shotglass_out;
";

const SOUND_FOOTER: &str = "void main(){\n  int s = iBlockOffset + int(gl_FragCoord.y) * iBlockWidth + int(gl_FragCoord.x);\n  vec2 v = mainSound(s, float(s) / iSampleRate);\n  shotglass_out = vec4(v, 0.0, 1.0);\n}";

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

struct ShotPrograms {
    scene: usize,
    transition_in: Option<usize>,
}

/// Compiled programs for one version of a movie.
struct Set {
    programs: Vec<usize>, // indices into the cache
    shots: Vec<ShotPrograms>,
    copy: usize,
}

pub struct Renderer {
    gl: Arc<glow::Context>,
    pub dialect: Dialect,
    pub gl_info: String,
    vao: glow::VertexArray,
    dummy: glow::Texture,
    /// Compiled programs keyed by their full source text; reused across reloads.
    cache: Vec<(u64, Program)>,
    set: Option<Set>,
    a: Option<Target>,
    b: Option<Target>,
    freeze: Option<Target>,
    out: Option<Target>,
    freeze_key: Option<(usize, u64)>,
    generation: u64,
    timer: Option<GpuTimer>,
}

/// Inputs for one frame.
pub struct FrameInfo {
    pub t: f32,
    pub dt: f32,
    pub frame: i32,
    pub scale: f32,
}

impl Renderer {
    pub fn new(gl: Arc<glow::Context>) -> Result<Renderer> {
        unsafe {
            let glsl = gl.get_parameter_string(glow::SHADING_LANGUAGE_VERSION);
            let version = gl.get_parameter_string(glow::VERSION);
            let renderer = gl.get_parameter_string(glow::RENDERER);
            let dialect = Dialect::detect(&glsl, &version);
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
            let timer = if dialect.es { None } else { GpuTimer::new(&gl) };
            Ok(Renderer {
                gl_info: format!("{renderer} · OpenGL {version} · GLSL {}", dialect.version),
                gl,
                dialect,
                vao,
                dummy,
                cache: vec![],
                set: None,
                a: None,
                b: None,
                freeze: None,
                out: None,
                freeze_key: None,
                generation: 0,
                timer,
            })
        }
    }

    /// Compile everything for `movie`. On any error the previous programs stay active.
    pub fn load(&mut self, movie: &Movie) -> std::result::Result<usize, Vec<ShaderError>> {
        let mut errors = vec![];
        let mut used = vec![];
        let mut shots = vec![];
        let cues = movie.cue_defines();
        let mut compiled = 0;
        let mut get = |r: &mut Renderer,
                       what: String,
                       src: Result<Assembled>,
                       errors: &mut Vec<ShaderError>|
         -> Option<usize> {
            let src = match src {
                Ok(s) => s,
                Err(e) => {
                    errors.push(ShaderError {
                        what,
                        messages: vec![format!("{e:#}")],
                    });
                    return None;
                }
            };
            match r.program_for(&src) {
                Ok((i, fresh)) => {
                    compiled += fresh as usize;
                    used.push(i);
                    Some(i)
                }
                Err(messages) => {
                    errors.push(ShaderError { what, messages });
                    None
                }
            }
        };
        let src = self.transition_source("copy", &[], &cues, &movie.root);
        let copy = get(self, "copy pass".into(), src, &mut errors);
        for shot in &movie.shots {
            let src = self.scene_source(shot, &cues, &movie.root);
            let scene = get(
                self,
                format!("shot \"{}\" ({})", shot.id, rel(&shot.scene, &movie.root)),
                src,
                &mut errors,
            );
            let transition_in = match &shot.transition_in {
                Some(t) => {
                    let src = self.transition_source(&t.kind, &t.params, &cues, &movie.root);
                    get(
                        self,
                        format!("transition into \"{}\" ({})", shot.id, t.kind),
                        src,
                        &mut errors,
                    )
                }
                None => None,
            };
            shots.push((scene, transition_in, shot.transition_in.is_some()));
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        let shots = shots
            .into_iter()
            .map(|(s, t, _)| ShotPrograms {
                scene: s.unwrap(),
                transition_in: t,
            })
            .collect();
        self.set = Some(Set {
            programs: used,
            shots,
            copy: copy.unwrap(),
        });
        self.generation += 1;
        self.freeze_key = None;
        self.evict_unused();
        self.warm_up();
        Ok(compiled)
    }

    fn scene_source(&self, shot: &Shot, cues: &str, root: &Path) -> Result<Assembled> {
        let mut header = SCENE_HEADER.to_string();
        for (name, a) in &shot.uniforms {
            header += &format!("uniform {} {name};\n", a.glsl_type());
        }
        header += cues;
        Ok(Builder::new(self.dialect, &header, root)
            .file(&shot.scene)?
            .footer(SCENE_FOOTER)
            .build())
    }

    fn transition_source(
        &self,
        kind: &str,
        params: &[(String, Vec<f32>)],
        cues: &str,
        root: &Path,
    ) -> Result<Assembled> {
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
        let b = Builder::new(self.dialect, &header, root);
        let b = if kind.ends_with(".glsl") {
            b.file(&root.join(kind))?
        } else {
            let src = BUILTIN_TRANSITIONS
                .iter()
                .find(|(n, _)| *n == kind)
                .map(|(_, s)| *s)
                .ok_or_else(|| {
                    anyhow!(
                        "unknown transition \"{kind}\"; built-ins are {}, or give a path ending in .glsl",
                        BUILTIN_TRANSITIONS.iter().map(|(n, _)| *n).filter(|n| *n != "copy").collect::<Vec<_>>().join(", ")
                    )
                })?;
            b.builtin(kind, src)?
        };
        Ok(b.footer(TRANSITION_FOOTER).build())
    }

    /// Returns (cache index, newly compiled?).
    fn program_for(&mut self, src: &Assembled) -> std::result::Result<(usize, bool), Vec<String>> {
        let h = hash(&src.text);
        if let Some(i) = self.cache.iter().position(|(k, _)| *k == h) {
            return Ok((i, false));
        }
        let p = self.compile(src)?;
        self.cache.push((h, p));
        Ok((self.cache.len() - 1, true))
    }

    fn compile(&self, src: &Assembled) -> std::result::Result<Program, Vec<String>> {
        let gl = &self.gl;
        unsafe {
            let vs_src = format!("{}{VERTEX}", self.dialect.version_line());
            let vs = gl.create_shader(glow::VERTEX_SHADER).map_err(|e| vec![e])?;
            gl.shader_source(vs, &vs_src);
            gl.compile_shader(vs);
            if !gl.get_shader_compile_status(vs) {
                let log = gl.get_shader_info_log(vs);
                gl.delete_shader(vs);
                return Err(vec![format!("vertex shader: {log}")]);
            }
            let fs = gl
                .create_shader(glow::FRAGMENT_SHADER)
                .map_err(|e| vec![e])?;
            gl.shader_source(fs, &src.text);
            gl.compile_shader(fs);
            if !gl.get_shader_compile_status(fs) {
                let log = gl.get_shader_info_log(fs);
                gl.delete_shader(vs);
                gl.delete_shader(fs);
                let mut m = remap_log(&log, &src.files);
                if m.is_empty() {
                    m.push("compile failed (driver gave no log)".into());
                }
                return Err(m);
            }
            let prog = gl.create_program().map_err(|e| vec![e])?;
            gl.attach_shader(prog, vs);
            gl.attach_shader(prog, fs);
            gl.link_program(prog);
            gl.detach_shader(prog, vs);
            gl.detach_shader(prog, fs);
            gl.delete_shader(vs);
            gl.delete_shader(fs);
            if !gl.get_program_link_status(prog) {
                let log = gl.get_program_info_log(prog);
                gl.delete_program(prog);
                return Err(remap_log(&format!("link: {log}"), &src.files));
            }
            let mut locs = HashMap::new();
            let n = gl.get_active_uniforms(prog);
            for i in 0..n {
                if let Some(u) = gl.get_active_uniform(prog, i) {
                    let name = u.name.trim_end_matches("[0]").to_string();
                    if let Some(l) = gl.get_uniform_location(prog, &name) {
                        locs.insert(name, l);
                    }
                }
            }
            Ok(Program { prog, locs })
        }
    }

    fn evict_unused(&mut self) {
        // Keep the cache small: drop programs not used by the current set, but keep
        // indices stable by only trimming when nothing references them.
        let Some(set) = &self.set else { return };
        if self.cache.len() <= set.programs.len() + 16 {
            return;
        }
        let keep: std::collections::HashSet<usize> = set.programs.iter().copied().collect();
        let mut remap = HashMap::new();
        let old = std::mem::take(&mut self.cache);
        for (i, (h, p)) in old.into_iter().enumerate() {
            if keep.contains(&i) {
                remap.insert(i, self.cache.len());
                self.cache.push((h, p));
            } else {
                unsafe { self.gl.delete_program(p.prog) };
            }
        }
        let set = self.set.as_mut().unwrap();
        set.programs = set.programs.iter().map(|i| remap[i]).collect();
        set.copy = remap[&set.copy];
        for s in &mut set.shots {
            s.scene = remap[&s.scene];
            s.transition_in = s.transition_in.map(|i| remap[&i]);
        }
    }

    /// Draw every program once into a tiny viewport so drivers finish compiling now,
    /// not on the first frame a shot appears.
    fn warm_up(&mut self) {
        let Some(set) = &self.set else { return };
        let progs: Vec<usize> = set.programs.clone();
        if self.a.is_none() {
            ensure_target(&self.gl, &mut self.a, 8, 8);
        }
        let t = self.a.as_ref().unwrap();
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(t.fbo));
            self.gl.viewport(0, 0, 4, 4);
            self.gl.bind_vertex_array(Some(self.vao));
            for i in progs {
                let p = &self.cache[i].1;
                self.gl.use_program(Some(p.prog));
                if let Some(l) = p.loc("iResolution") {
                    self.gl.uniform_3_f32(Some(l), 4.0, 4.0, 1.0);
                }
                if let Some(l) = p.loc("uOutRes") {
                    self.gl.uniform_2_f32(Some(l), 4.0, 4.0);
                }
                self.gl.draw_arrays(glow::TRIANGLES, 0, 3);
            }
            self.gl.finish();
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
    }

    /// Render the movie at `info.t` into the output texture (w × h). Returns the texture.
    pub fn render(
        &mut self,
        movie: &Movie,
        w: i32,
        h: i32,
        info: &FrameInfo,
    ) -> Option<glow::Texture> {
        let set = self.set.as_ref()?;
        let (w, h) = (w.max(1), h.max(1));
        let sw = ((w as f32 * info.scale).round() as i32).clamp(1, w);
        let sh = ((h as f32 * info.scale).round() as i32).clamp(1, h);
        let at = movie.at(info.t);
        if let Some(timer) = &mut self.timer {
            timer.begin(&self.gl);
        }
        let (copy, scenes): (usize, Vec<usize>) =
            (set.copy, set.shots.iter().map(|s| s.scene).collect());
        let trans: Vec<Option<usize>> = set.shots.iter().map(|s| s.transition_in).collect();
        ensure_target(&self.gl, &mut self.a, w, h);
        ensure_target(&self.gl, &mut self.b, w, h);
        ensure_target(&self.gl, &mut self.out, w, h);
        match at {
            At::Shot { shot, local } => {
                self.draw_scene(
                    scenes[shot],
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
                let tr = movie.shots[to].transition_in.as_ref().unwrap();
                let from_target = if tr.freeze {
                    let key = (to, self.generation);
                    if self.freeze_key != Some(key)
                        || self.freeze.as_ref().is_none_or(|f| f.alloc != (w, h))
                    {
                        ensure_target(&self.gl, &mut self.freeze, w, h);
                        let freeze_local = movie.shots[to].start - movie.shots[from].start;
                        self.draw_scene(
                            scenes[from],
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
                        scenes[from],
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
                    scenes[to],
                    &movie.shots[to],
                    to_local,
                    Which::B,
                    sw,
                    sh,
                    info,
                    movie,
                );
                let eased = tr.ease.apply(progress);
                self.composite(
                    trans[to].unwrap_or(copy),
                    from_target,
                    Which::B,
                    eased,
                    w,
                    h,
                );
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

    pub fn gpu_ms(&mut self) -> Option<f32> {
        let gl = self.gl.clone();
        self.timer.as_mut().and_then(|t| t.poll(&gl))
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
        prog: usize,
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
        let p = &self.cache[prog].1;
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
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
        }
    }

    fn composite(&mut self, prog: usize, from: Which, to: Which, progress: f32, w: i32, h: i32) {
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
        let p = &self.cache[prog].1;
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
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    /// Read the output texture as tightly packed RGBA8, top row first.
    pub fn read_output(&self) -> Option<(i32, i32, Vec<u8>)> {
        let t = self.out.as_ref()?;
        let (w, h) = t.alloc;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(t.fbo));
            self.gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            self.gl.read_pixels(
                0,
                0,
                w,
                h,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut buf)),
            );
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
        let row = (w * 4) as usize;
        let mut flipped = Vec::with_capacity(buf.len());
        for y in (0..h as usize).rev() {
            flipped.extend_from_slice(&buf[y * row..(y + 1) * row]);
        }
        Some((w, h, flipped))
    }

    pub fn finish(&self) {
        unsafe { self.gl.finish() };
    }

    /// Synthesize the movie's soundtrack on the GPU. Returns interleaved stereo f32.
    pub fn render_sound(&mut self, movie: &Movie, sample_rate: u32) -> Result<Vec<f32>> {
        let Some(path) = &movie.sound else {
            return Ok(vec![]);
        };
        let mut header = SOUND_HEADER.to_string();
        header += &movie.cue_defines();
        let src = Builder::new(self.dialect, &header, &movie.root)
            .file(path)?
            .footer(SOUND_FOOTER)
            .build();
        let prog = self.compile(&src).map_err(|m| {
            anyhow!(
                "{}",
                ShaderError {
                    what: format!("sound ({})", rel(path, &movie.root)),
                    messages: m
                }
            )
        })?;
        let gl = self.gl.clone();
        const W: i32 = 1024;
        const H: i32 = 128;
        let total = (movie.duration * sample_rate as f32).ceil() as usize;
        let mut out = Vec::with_capacity(total * 2);
        unsafe {
            let tex = gl.create_texture().map_err(|e| anyhow!(e))?;
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA32F as i32,
                W,
                H,
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
            let fbo = gl.create_framebuffer().map_err(|e| anyhow!(e))?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(tex),
                0,
            );
            if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
                gl.delete_framebuffer(fbo);
                gl.delete_texture(tex);
                gl.delete_program(prog.prog);
                bail!("this GPU can't render to float textures, so GLSL sound is unavailable");
            }
            gl.viewport(0, 0, W, H);
            gl.disable(glow::BLEND);
            gl.bind_vertex_array(Some(self.vao));
            gl.use_program(Some(prog.prog));
            if let Some(l) = prog.loc("iSampleRate") {
                gl.uniform_1_f32(Some(l), sample_rate as f32);
            }
            if let Some(l) = prog.loc("iBlockWidth") {
                gl.uniform_1_i32(Some(l), W);
            }
            let mut buf = vec![0f32; (W * H * 4) as usize];
            let mut offset = 0usize;
            while offset < total {
                if let Some(l) = prog.loc("iBlockOffset") {
                    gl.uniform_1_i32(Some(l), offset as i32);
                }
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
                let bytes =
                    std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, buf.len() * 4);
                gl.read_pixels(
                    0,
                    0,
                    W,
                    H,
                    glow::RGBA,
                    glow::FLOAT,
                    glow::PixelPackData::Slice(Some(bytes)),
                );
                let n = (total - offset).min((W * H) as usize);
                for i in 0..n {
                    out.push(sanitize(buf[i * 4]) * movie.volume);
                    out.push(sanitize(buf[i * 4 + 1]) * movie.volume);
                }
                offset += n;
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.delete_framebuffer(fbo);
            gl.delete_texture(tex);
            gl.delete_program(prog.prog);
            gl.use_program(None);
            gl.bind_vertex_array(None);
        }
        Ok(out)
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            for (_, p) in self.cache.drain(..) {
                self.gl.delete_program(p.prog);
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
    last_ms: Option<f32>,
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
            last_ms: None,
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
    fn poll(&mut self, gl: &glow::Context) -> Option<f32> {
        while let Some(&q) = self.pending.front() {
            let ready = unsafe { gl.get_query_parameter_u32(q, glow::QUERY_RESULT_AVAILABLE) } != 0;
            if !ready {
                break;
            }
            let ns = unsafe { gl.get_query_parameter_u32(q, glow::QUERY_RESULT) };
            self.last_ms = Some(ns as f32 / 1.0e6);
            self.pending.pop_front();
            self.free.push(q);
        }
        self.last_ms
    }
}
