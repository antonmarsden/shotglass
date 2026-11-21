//! `movie.toml` parsing and the timeline: which shot (or transition) is on screen at time t.

use crate::anim::{Anim, Ease, vector};
use crate::library::{self, Library, LibraryDecl, LibrarySpec};
use crate::texture::{Filter, Wrap};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- file format

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// `fps` → (numerator, denominator), reduced. NTSC-style decimals (29.97, 23.976,
/// 59.94) mean n×1000/1001, as video tools assume.
fn parse_fps(f: FpsFile) -> Result<(u32, u32)> {
    let (num, den): (f64, f64) = match f {
        FpsFile::Int(n) => (n as f64, 1.0),
        FpsFile::Ratio(s) => {
            let (a, b) = s.split_once('/').unwrap_or((&s, "1"));
            let a: u32 = a
                .trim()
                .parse()
                .ok()
                .context("fps must look like 30 or \"30000/1001\"")?;
            let b: u32 = b
                .trim()
                .parse()
                .ok()
                .context("fps must look like 30 or \"30000/1001\"")?;
            (a as f64, b as f64)
        }
        FpsFile::Float(x) => {
            let milli = (x * 1000.0).round();
            if (milli / 1000.0 - x).abs() > 1e-9 {
                bail!("fps {x} has too many decimals; write it as a ratio, e.g. \"30000/1001\"");
            }
            let ntsc = x * 1.001;
            if x.fract() == 0.0 {
                (x, 1.0)
            } else if (ntsc - ntsc.round()).abs() < 0.005 && ntsc.round() >= 1.0 {
                (ntsc.round() * 1000.0, 1001.0)
            } else {
                (milli, 1000.0)
            }
        }
    };
    if !(num >= 1.0 && den >= 1.0 && num <= u32::MAX as f64 && num / den <= 1000.0) {
        bail!("fps must be between 1 and 1000 (and a ratio of positive whole numbers)");
    }
    let g = gcd(num as u64, den as u64);
    let (num, den) = (num as u64 / g, den as u64 / g);
    if num as f64 / (den as f64) < 1.0 {
        bail!("fps must be at least 1");
    }
    Ok((num as u32, den as u32))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MovieFile {
    /// Engine contract this film needs, e.g. "0.4".
    shotglass: Option<String>,
    title: Option<String>,
    /// Width:height, e.g. [16, 9]. Default 16:9.
    aspect: Option<[f32; 2]>,
    /// Frame rate: a number (30, 29.97) or a ratio string ("30000/1001"). Used for
    /// frame stepping, iFrame and export; playback itself runs at the display rate.
    fps: Option<FpsFile>,
    /// Opt in to a later desktop GLSL version: "330" (the default), "410" or "430".
    glsl: Option<String>,
    sound: Option<SoundFile>,
    #[serde(rename = "shot", default)]
    shots: Vec<ShotFile>,
    #[serde(rename = "marker", default)]
    markers: Vec<MarkerFile>,
    /// Shared GLSL by local name: `kit = { version = "0.4", source = "https://…", subdir = "core" }`.
    #[serde(default)]
    libraries: BTreeMap<String, LibraryDecl>,
    /// Film-wide iChannel0..3 for every shot that doesn't set its own `channels`.
    channels: Option<Vec<toml::Value>>,
    /// Named textures: `brick = "textures/brick.png"` or `{ file = "…", filter = … }`.
    #[serde(default)]
    textures: BTreeMap<String, toml::Value>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FpsFile {
    Int(i64),
    Float(f64),
    Ratio(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SoundFile {
    /// GLSL file with `vec2 mainSound(int samp, float time)`; time is movie time.
    #[serde(alias = "shader")]
    music: Option<String>,
    /// GLSL file with `vec2 mainMix(int samp, float time)`, reading music() and sfx().
    mix: Option<String>,
    /// How far back (seconds) the mix may read music()/sfx() (default 1).
    mix_lookback: Option<f32>,
    volume: Option<f32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShotFile {
    id: String,
    scene: String,
    duration: f32,
    /// iTime at the start of the shot (default 0). Lets two shots use different moments of one scene.
    time_offset: Option<f32>,
    /// iTime speed (default 1).
    time_scale: Option<f32>,
    #[serde(default)]
    uniforms: BTreeMap<String, toml::Value>,
    /// Transition *into* this shot from the previous one. No transition = hard cut.
    transition: Option<TransitionFile>,
    /// GLSL `mainSound` for this shot; `time` is shot-local, like iTime.
    sound: Option<String>,
    /// Start the sound this many seconds before the shot (a J-cut).
    sound_lead: Option<f32>,
    /// Let the sound ring on this many seconds after the shot (an L-cut).
    sound_tail: Option<f32>,
    sound_gain: Option<f32>,
    /// Images on iChannel0..3: names from [textures], paths, `<lib/path>`, tables, "".
    channels: Option<Vec<toml::Value>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitionFile {
    #[serde(rename = "type")]
    kind: String,
    duration: f32,
    #[serde(default)]
    freeze: bool,
    ease: Option<String>,
    #[serde(default)]
    params: BTreeMap<String, toml::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkerFile {
    t: f32,
    label: String,
}

// ---------------------------------------------------------------- resolved movie

#[derive(Clone, Debug)]
pub struct Transition {
    /// A library transition (`transitions/iris`: `iris.glsl` in the library the film calls
    /// `transitions`) or a path to a .glsl file in the film.
    pub kind: String,
    pub duration: f32,
    /// Render the outgoing shot once (its frame at the transition start) instead of live.
    pub freeze: bool,
    pub ease: Ease,
    /// Become `#define name value` in the transition shader.
    pub params: Vec<(String, Vec<f32>)>,
}

#[derive(Clone, Debug)]
pub struct ShotSound {
    pub path: PathBuf,
    pub lead: f32,
    pub tail: f32,
    pub gain: f32,
}

/// An image bound to an iChannel.
#[derive(Clone, Debug, PartialEq)]
pub struct Channel {
    /// The file: film root (or, once `resolve_libraries` ran, library root) joined with `rel`.
    pub file: PathBuf,
    /// As written, for messages: "textures/brick.png", "kit/textures/noise.png".
    pub label: String,
    /// The library's local name for `<lib/path>` textures.
    pub library: Option<String>,
    /// The path inside the film folder or the library.
    pub rel: String,
    pub filter: Filter,
    pub wrap: Wrap,
    /// Rows reversed on decode, so the image is upright with `fragCoord / iResolution.xy`.
    pub vflip: bool,
    /// Uploaded as SRGB8_ALPHA8: `texture()` returns linear values.
    pub srgb: bool,
}

#[derive(Clone, Debug)]
pub struct Shot {
    pub id: String,
    pub index: usize,
    pub start: f32,
    pub duration: f32,
    pub scene: PathBuf,
    pub time_offset: f32,
    pub time_scale: f32,
    pub uniforms: Vec<(String, Anim)>,
    pub transition_in: Option<Transition>,
    pub sound: Option<ShotSound>,
    /// What iChannel0..3 carry (None: black, `iChannelResolution` 0).
    pub channels: [Option<Channel>; 4],
}

impl Shot {
    pub fn end(&self) -> f32 {
        self.start + self.duration
    }
    /// Time at which the incoming transition has finished.
    pub fn settled(&self) -> f32 {
        self.start + self.transition_in.as_ref().map_or(0.0, |t| t.duration)
    }
    /// iTime for the scene shader at shot-local time `local`.
    pub fn scene_time(&self, local: f32) -> f32 {
        self.time_offset + local * self.time_scale
    }
}

#[derive(Clone, Debug)]
pub struct Marker {
    pub t: f32,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct Movie {
    pub root: PathBuf,
    pub title: String,
    pub aspect: f32,
    /// `fps_num / fps_den` as a float, for playback and uniforms.
    pub fps: f32,
    /// The exact frame rate. Frame n is at `frame_time(n)`.
    pub fps_num: u32,
    pub fps_den: u32,
    /// Desktop GLSL version the film opted in to (`glsl` in movie.toml); None = the
    /// portable baseline (330 core, or 300 es on OpenGL ES).
    pub glsl: Option<u32>,
    /// Movie-level music/ambience (`mainSound`, movie time).
    pub music: Option<PathBuf>,
    /// Final mix pass (`mainMix`).
    pub mix: Option<PathBuf>,
    pub mix_lookback: f32,
    pub volume: f32,
    pub shots: Vec<Shot>,
    pub markers: Vec<Marker>,
    pub duration: f32,
    /// `shotglass = "…"`: the engine contract range the film was made for.
    pub shotglass: Option<String>,
    /// `[libraries]` as declared (validated, not yet looked up on disk).
    pub library_specs: Vec<LibrarySpec>,
    /// Libraries found on disk; filled by `load` (empty after a bare `parse`).
    pub libraries: Vec<Library>,
    /// Portability warnings (e.g. a file name that only matches when ignoring case).
    pub warnings: Vec<String>,
}

/// What is on screen at a moment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum At {
    Shot {
        shot: usize,
        local: f32,
    },
    Transition {
        from: usize,
        to: usize,
        from_local: f32,
        to_local: f32,
        /// 0..1, linear.
        progress: f32,
    },
}

impl Movie {
    /// Load `movie.toml` from a movie folder (or the path of the toml itself).
    pub fn load(path: &Path) -> Result<Movie> {
        let mut m = Movie::read(path)?;
        m.resolve_libraries(&|k| std::env::var(k).ok(), &library::cache_dir())
            .with_context(|| format!("in {}", m.root.join("movie.toml").display()))?;
        m.check_case();
        m.check_textures()
            .with_context(|| format!("in {}", m.root.join("movie.toml").display()))?;
        Ok(m)
    }

    /// Load with libraries exactly as `shotglass.lock` pins them (cache or bundled
    /// copies), ignoring per-machine overrides (`SHOTGLASS_LIB_*`, `movie.local.toml`).
    /// `bundle` uses it: a bundle carries the locked files, never a working copy.
    pub fn load_locked(path: &Path, cache: &Path) -> Result<Movie> {
        let mut m = Movie::read(path)?;
        m.resolve_libraries_with(&BTreeMap::new(), cache)
            .with_context(|| format!("in {}", m.root.join("movie.toml").display()))?;
        m.check_case();
        m.check_textures()
            .with_context(|| format!("in {}", m.root.join("movie.toml").display()))?;
        Ok(m)
    }

    /// Does any shot bind a texture?
    pub fn uses_channels(&self) -> bool {
        self.shots
            .iter()
            .any(|s| s.channels.iter().any(Option::is_some))
    }

    /// Every distinct texture (by file and how its texels are made), in shot order.
    pub fn textures(&self) -> Vec<&Channel> {
        let mut v: Vec<&Channel> = vec![];
        for c in self.shots.iter().flat_map(|s| s.channels.iter().flatten()) {
            if !v
                .iter()
                .any(|o| o.file == c.file && o.vflip == c.vflip && o.srgb == c.srgb)
            {
                v.push(c);
            }
        }
        v
    }

    /// `check`'s warning for a film that binds textures but doesn't say it needs the
    /// engine contract that has them (0.1.1): older engines would then fail on an
    /// "unknown field" instead of asking for a newer shotglass.
    pub fn contract_warning(&self) -> Option<String> {
        if !self.uses_channels() {
            return None;
        }
        let old = semver::Version::new(0, 1, 0);
        let allows_old = match &self.shotglass {
            None => true,
            Some(r) => library::parse_req(r).is_ok_and(|r| r.matches(&old)),
        };
        allows_old.then(|| {
            format!(
                "this film uses channels (iChannel textures), which need shotglass 0.1.1; write                  shotglass = \"0.1.1\" in movie.toml so older shotglass versions ask for an \
                 upgrade instead of failing on an unknown field{}",
                self.shotglass
                    .as_ref()
                    .map_or(String::new(), |v| format!(" (it says \"{v}\")"))
            )
        })
    }

    /// Every texture file exists (a missing one refuses the version, like a missing
    /// `#include`), and no shot needs more texture memory than the budget.
    fn check_textures(&self) -> Result<()> {
        for s in &self.shots {
            for (n, c) in s.channels.iter().enumerate() {
                let Some(c) = c else { continue };
                if c.file.is_file() {
                    continue;
                }
                let base = match &c.library {
                    Some(lib) => self
                        .libraries
                        .iter()
                        .find(|l| &l.name == lib)
                        .map(|l| l.root.clone()),
                    None => Some(self.root.clone()),
                };
                let hint = base
                    .and_then(|b| crate::texture::did_you_mean(&b, &c.rel))
                    .map_or(String::new(), |m| match &c.library {
                        Some(lib) => format!(" (did you mean \"{lib}/{m}\"?)"),
                        None => format!(" (did you mean \"{m}\"?)"),
                    });
                bail!(
                    "shot \"{}\": channels[{n}]: \"{}\" not found{hint}",
                    s.id,
                    c.label
                );
            }
            let (total, parts) = crate::texture::shot_memory(&s.channels);
            if total > crate::texture::BUDGET {
                bail!(
                    "shot \"{}\": its textures need {}, more than the {} budget: {}",
                    s.id,
                    crate::texture::describe_bytes(total),
                    crate::texture::describe_bytes(crate::texture::BUDGET),
                    parts.join(", ")
                );
            }
        }
        Ok(())
    }

    /// Read and validate `movie.toml` without looking libraries up on disk.
    pub fn read(path: &Path) -> Result<Movie> {
        let file = if path.is_dir() {
            path.join("movie.toml")
        } else {
            path.to_path_buf()
        };
        let root = file.parent().map(Path::to_path_buf).unwrap_or_default();
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        Movie::parse(&text, &root).with_context(|| format!("in {}", file.display()))
    }

    /// Find the declared libraries on disk: local overrides (`env`, `movie.local.toml`),
    /// in-film paths, or the cache as pinned by `shotglass.lock`. No network.
    pub fn resolve_libraries(
        &mut self,
        env: &dyn Fn(&str) -> Option<String>,
        cache: &Path,
    ) -> Result<()> {
        if self.library_specs.is_empty() {
            return Ok(());
        }
        let o = library::overrides(&self.root, &self.library_specs, env)?;
        self.resolve_libraries_with(&o, cache)
    }

    fn resolve_libraries_with(
        &mut self,
        o: &BTreeMap<String, (PathBuf, library::Origin)>,
        cache: &Path,
    ) -> Result<()> {
        if self.library_specs.is_empty() {
            return Ok(());
        }
        let lock = library::Lock::load(&self.root)?;
        self.libraries =
            library::resolve(&self.root, &self.library_specs, o, lock.as_ref(), cache)?;
        let libs = self.libraries.clone();
        for c in self
            .shots
            .iter_mut()
            .flat_map(|s| s.channels.iter_mut().flatten())
        {
            if let Some(lib) = &c.library
                && let Some(l) = libs.iter().find(|l| &l.name == lib)
            {
                c.file = l.root.join(&c.rel);
            }
        }
        let film = self
            .glsl
            .unwrap_or(crate::shader::Dialect::BASELINE.version);
        for l in &self.libraries {
            if let Some(need) = l.glsl
                && need > film
            {
                bail!(
                    "library {} needs GLSL {need}, but this film compiles as GLSL {film}; add \
                     glsl = \"{need}\" to movie.toml if that's fine (the film then needs a \
                     driver with GLSL {need} and won't play on the web), or use another library",
                    l.describe()
                );
            }
        }
        Ok(())
    }

    /// Files the edit refers to directly (scenes, sounds, transition files, textures in
    /// the film folder).
    pub fn referenced_files(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.music.iter().chain(self.mix.iter()).cloned().collect();
        for s in &self.shots {
            v.push(s.scene.clone());
            v.extend(s.sound.as_ref().map(|x| x.path.clone()));
            if let Some(t) = &s.transition_in
                && let TransitionKind::File(p) = transition_kind(&t.kind)
            {
                v.push(self.root.join(p));
            }
            for c in s.channels.iter().flatten() {
                if c.library.is_none() {
                    v.push(c.file.clone());
                }
            }
        }
        v.sort();
        v.dedup();
        v
    }

    /// Warn about references that only match a file when ignoring case: they work on
    /// Windows and macOS but not on Linux or the web.
    fn check_case(&mut self) {
        for p in self.referenced_files() {
            let rel = p
                .strip_prefix(&self.root)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            if let Some(actual) = library::case_mismatch(&self.root, &rel) {
                self.warnings.push(format!(
                    "movie.toml refers to \"{rel}\", but the file is \"{actual}\" (names are \
                     case-sensitive on Linux and the web)"
                ));
            }
        }
    }

    pub fn parse(text: &str, root: &Path) -> Result<Movie> {
        // Check the engine contract first: a newer film may use fields this engine
        // doesn't know, and "upgrade shotglass" is the useful message then.
        match toml::from_str::<toml::Table>(text)
            .ok()
            .and_then(|t| t.get("shotglass").cloned())
        {
            Some(toml::Value::String(v)) => library::check_engine(&v)?,
            Some(_) => bail!("shotglass must be a version string, e.g. shotglass = \"0.1\""),
            None => {}
        }
        let f: MovieFile = toml::from_str(text)?;
        let path_rule = |what: String, p: &str| -> Result<()> {
            library::check_rel_path(p).map_err(|e| anyhow!("{what}: {e}"))
        };
        let mut library_specs = vec![];
        for (name, d) in &f.libraries {
            library_specs.push(LibrarySpec::parse(name, d)?);
        }
        if let Some(snd) = &f.sound {
            if let Some(p) = &snd.music {
                path_rule("[sound] music".into(), p)?;
            }
            if let Some(p) = &snd.mix {
                path_rule("[sound] mix".into(), p)?;
            }
        }
        if f.shots.is_empty() {
            bail!("a movie needs at least one [[shot]]");
        }
        let mut textures: BTreeMap<String, Channel> = BTreeMap::new();
        for (name, v) in &f.textures {
            let what = format!("[textures] {name}");
            if !is_ident(name) {
                bail!(
                    "{what}: a texture name may only use letters, digits and _ (and not start with a digit)"
                );
            }
            match texture_spec(&what, v, None, &library_specs, root)? {
                Some(c) => textures.insert(name.clone(), c),
                None => bail!("{what}: \"\" names no file"),
            };
        }
        let mut used: std::collections::HashSet<String> = Default::default();
        let mut channel_list = |what: &str, list: &[toml::Value]| -> Result<[Option<Channel>; 4]> {
            if list.len() > 4 {
                bail!(
                    "{what}channels has {} entries; there are 4 (iChannel0..3)",
                    list.len()
                );
            }
            let mut out: [Option<Channel>; 4] = Default::default();
            for (n, v) in list.iter().enumerate() {
                let ctx = format!("{what}channels[{n}]");
                if let toml::Value::String(s) = v
                    && is_texture_name(s)
                {
                    used.insert(s.clone());
                }
                out[n] = texture_spec(&ctx, v, Some(&textures), &library_specs, root)?;
            }
            Ok(out)
        };
        let default_channels = match &f.channels {
            Some(list) => channel_list("", list)?,
            None => Default::default(),
        };
        let mut shots: Vec<Shot> = Vec::new();
        let mut ids = std::collections::HashSet::new();
        for (index, s) in f.shots.into_iter().enumerate() {
            let ctx = || format!("shot \"{}\"", s.id);
            let channels = match &s.channels {
                Some(list) => channel_list(&format!("{}: ", ctx()), list)?,
                None => default_channels.clone(),
            };
            if !ids.insert(s.id.clone()) {
                bail!("duplicate shot id \"{}\"", s.id);
            }
            if s.id.is_empty()
                || !s
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                bail!("shot id \"{}\" may only use letters, digits, _ and -", s.id);
            }
            path_rule(format!("{}: scene", ctx()), &s.scene)?;
            if let Some(p) = &s.sound {
                path_rule(format!("{}: sound", ctx()), p)?;
            }
            if let Some(t) = &s.transition {
                match transition_kind(&t.kind) {
                    TransitionKind::File(p) => path_rule(format!("{}: transition", ctx()), p)?,
                    TransitionKind::Library(lib, file) => {
                        if !library_specs.iter().any(|l| l.name == lib) {
                            bail!(
                                "{}: transition \"{}\" names library \"{lib}\", but [libraries] \
                                 has {}",
                                ctx(),
                                t.kind,
                                if library_specs.is_empty() {
                                    "none".to_string()
                                } else {
                                    library_specs
                                        .iter()
                                        .map(|l| format!("\"{}\"", l.name))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                }
                            );
                        }
                        path_rule(format!("{}: transition", ctx()), &file)?;
                    }
                    TransitionKind::Name(name) => {
                        bail!("{}: {}", ctx(), bare_transition_error(name, &library_specs))
                    }
                }
            }
            if s.duration.is_nan() || s.duration <= 0.0 {
                bail!("{}: duration must be > 0", ctx());
            }
            let mut uniforms = Vec::new();
            for (name, v) in &s.uniforms {
                if !is_ident(name) {
                    bail!("{}: \"{name}\" is not a valid GLSL name", ctx());
                }
                let a = Anim::from_toml(v).with_context(|| format!("{}: uniform {name}", ctx()))?;
                uniforms.push((name.clone(), a));
            }
            let transition_in = match s.transition {
                None => None,
                Some(t) => {
                    if index == 0 {
                        bail!("{}: the first shot can't have a transition in", ctx());
                    }
                    if t.duration.is_nan() || t.duration <= 0.0 {
                        bail!("{}: transition duration must be > 0", ctx());
                    }
                    let mut params = Vec::new();
                    for (name, v) in &t.params {
                        if !is_ident(name) {
                            bail!(
                                "{}: transition param \"{name}\" is not a valid GLSL name",
                                ctx()
                            );
                        }
                        params.push((
                            name.clone(),
                            vector(v).with_context(|| format!("{}: param {name}", ctx()))?,
                        ));
                    }
                    Some(Transition {
                        kind: t.kind,
                        duration: t.duration,
                        freeze: t.freeze,
                        ease: match t.ease {
                            Some(e) => Ease::parse(&e)?,
                            None => Ease::Smooth,
                        },
                        params,
                    })
                }
            };
            // Shots overlap by the incoming transition's length.
            let start = match shots.last() {
                None => 0.0,
                Some(p) => {
                    let d = transition_in.as_ref().map_or(0.0, |t| t.duration);
                    let prev_in = p.transition_in.as_ref().map_or(0.0, |t| t.duration);
                    if d > s.duration {
                        bail!(
                            "{}: transition ({d}s) is longer than the shot ({}s)",
                            ctx(),
                            s.duration
                        );
                    }
                    if prev_in + d > p.duration + 1e-4 {
                        bail!(
                            "{}: transition ({d}s) overlaps the previous transition; shot \"{}\" is {}s \
                             but its two transitions need {}s",
                            ctx(),
                            p.id,
                            p.duration,
                            prev_in + d
                        );
                    }
                    p.end() - d
                }
            };
            let lead = s.sound_lead.unwrap_or(0.0);
            let tail = s.sound_tail.unwrap_or(0.0);
            if lead < 0.0 || tail < 0.0 || lead.is_nan() || tail.is_nan() {
                bail!("{}: sound_lead and sound_tail must be >= 0", ctx());
            }
            if s.sound.is_none()
                && (s.sound_lead.is_some() || s.sound_tail.is_some() || s.sound_gain.is_some())
            {
                bail!(
                    "{}: sound_lead/sound_tail/sound_gain need sound = \"file.glsl\"",
                    ctx()
                );
            }
            let sound = s.sound.map(|p| ShotSound {
                path: root.join(p),
                lead,
                tail,
                gain: s.sound_gain.unwrap_or(1.0),
            });
            shots.push(Shot {
                channels,
                sound,
                id: s.id,
                index,
                start,
                duration: s.duration,
                scene: root.join(&s.scene),
                time_offset: s.time_offset.unwrap_or(0.0),
                time_scale: s.time_scale.unwrap_or(1.0),
                uniforms,
                transition_in,
            });
        }
        let duration = shots.last().unwrap().end();
        let glsl = match f.glsl.as_deref().map(str::trim) {
            None => None,
            Some(v) => match v.parse::<u32>() {
                Ok(n) if crate::shader::DESKTOP_VERSIONS.contains(&n) => Some(n),
                _ => bail!(
                    "glsl = \"{v}\" isn't supported; use \"330\" (the portable default), \"410\" or \"430\""
                ),
            },
        };
        let aspect = match f.aspect {
            Some([w, h]) if w > 0.0 && h > 0.0 => w / h,
            Some(_) => bail!("aspect must be two positive numbers, e.g. [16, 9]"),
            None => 16.0 / 9.0,
        };
        let (fps_num, fps_den) = match f.fps {
            Some(f) => parse_fps(f)?,
            None => (30, 1),
        };
        Ok(Movie {
            root: root.to_path_buf(),
            title: f.title.unwrap_or_else(|| "Untitled".into()),
            aspect,
            fps: (fps_num as f64 / fps_den as f64) as f32,
            fps_num,
            fps_den,
            glsl,
            volume: f.sound.as_ref().and_then(|s| s.volume).unwrap_or(1.0),
            music: f
                .sound
                .as_ref()
                .and_then(|s| s.music.as_ref())
                .map(|p| root.join(p)),
            mix: f
                .sound
                .as_ref()
                .and_then(|s| s.mix.as_ref())
                .map(|p| root.join(p)),
            mix_lookback: f.sound.as_ref().and_then(|s| s.mix_lookback).unwrap_or(1.0),
            shots,
            markers: f
                .markers
                .into_iter()
                .map(|m| Marker {
                    t: m.t,
                    label: m.label,
                })
                .collect(),
            duration,
            shotglass: f.shotglass,
            warnings: textures
                .keys()
                .filter(|n| !used.contains(*n))
                .map(|n| format!("[textures] {n} isn't used by any shot's channels"))
                .collect(),
            library_specs,
            libraries: vec![],
        })
    }

    /// Time of frame `n`: exactly `n * den / num`, rounded once to f32.
    pub fn frame_time(&self, n: u64) -> f32 {
        (n as f64 * self.fps_den as f64 / self.fps_num as f64) as f32
    }

    /// The frame on screen at `t` (the inverse of `frame_time`: `frame_at(frame_time(n)) == n`).
    pub fn frame_at(&self, t: f32) -> i64 {
        let f = t.max(0.0) as f64 * self.fps_num as f64 / self.fps_den as f64;
        // frame_time rounds to f32, which can land just below n/fps; don't drop a frame for that.
        let n = f.round();
        if (f - n).abs() < 1e-4 * n.max(1.0) {
            n as i64
        } else {
            f.floor() as i64
        }
    }

    /// Frames in the film: every n with `frame_time(n) < duration`.
    pub fn frame_count(&self) -> u64 {
        let mut n =
            (self.duration as f64 * self.fps_num as f64 / self.fps_den as f64).ceil() as u64;
        while n > 0 && self.frame_time(n - 1) >= self.duration {
            n -= 1;
        }
        while self.frame_time(n) < self.duration {
            n += 1;
        }
        n
    }

    pub fn at(&self, t: f32) -> At {
        let t = t.clamp(0.0, self.duration);
        let i = self.shots.iter().rposition(|s| t >= s.start).unwrap_or(0);
        let s = &self.shots[i];
        if let Some(tr) = &s.transition_in
            && t < s.start + tr.duration
            && i > 0
        {
            let p = &self.shots[i - 1];
            return At::Transition {
                from: i - 1,
                to: i,
                from_local: t - p.start,
                to_local: t - s.start,
                progress: ((t - s.start) / tr.duration).clamp(0.0, 1.0),
            };
        }
        At::Shot {
            shot: i,
            local: t - s.start,
        }
    }

    pub fn has_sound(&self) -> bool {
        self.music.is_some() || self.shots.iter().any(|s| s.sound.is_some())
    }

    /// Where a shot's sound plays: (from, to) in movie seconds.
    pub fn sound_window(&self, i: usize) -> Option<(f32, f32)> {
        let s = &self.shots[i];
        let snd = s.sound.as_ref()?;
        Some((
            (s.start - snd.lead).max(0.0),
            (s.end() + snd.tail).min(self.duration),
        ))
    }

    /// Gain of shot `i`'s sound at movie time `t`: equal-power fades that follow the
    /// picture transitions, except where a lead or tail lets the sound carry over.
    pub fn sound_gain(&self, i: usize, t: f32) -> f32 {
        let Some((from, to)) = self.sound_window(i) else {
            return 0.0;
        };
        if t < from || t > to {
            return 0.0;
        }
        let s = &self.shots[i];
        let snd = s.sound.as_ref().unwrap();
        let mut g = snd.gain;
        if let (Some(tr), true) = (&s.transition_in, snd.lead == 0.0)
            && t < s.start + tr.duration
        {
            let p = tr.ease.apply((t - s.start) / tr.duration);
            g *= (p * std::f32::consts::FRAC_PI_2).sin();
        }
        if let (Some(next), true) = (self.shots.get(i + 1), snd.tail == 0.0)
            && let Some(tr) = &next.transition_in
            && t > next.start
        {
            let p = tr.ease.apply((t - next.start) / tr.duration);
            g *= (p * std::f32::consts::FRAC_PI_2).cos();
        }
        // 5 ms de-click ramps at the window edges
        let ramp = 0.005;
        g * ((t - from) / ramp).clamp(0.0, 1.0) * ((to - t) / ramp).clamp(0.0, 1.0)
    }

    pub fn shot_by_id(&self, id: &str) -> Option<&Shot> {
        self.shots.iter().find(|s| s.id == id)
    }

    /// `#define`s shared by every shader so pictures and sound agree on timing.
    pub fn cue_defines(&self) -> String {
        let mut s = format!("#define MOVIE_DURATION {}\n", glsl_float(self.duration));
        for shot in &self.shots {
            let id = shot.id.to_ascii_uppercase().replace('-', "_");
            s += &format!("#define CUE_{id}_START {}\n", glsl_float(shot.start));
            s += &format!("#define CUE_{id}_SETTLED {}\n", glsl_float(shot.settled()));
            s += &format!("#define CUE_{id}_END {}\n", glsl_float(shot.end()));
        }
        s
    }

    /// Regions worth looping: each shot, and each transition with a little padding.
    pub fn region_at(&self, t: f32) -> (f32, f32) {
        match self.at(t) {
            At::Transition { to, .. } => {
                let s = &self.shots[to];
                (
                    (s.start - 0.75).max(0.0),
                    (s.settled() + 0.75).min(self.duration),
                )
            }
            At::Shot { shot, .. } => {
                let s = &self.shots[shot];
                (s.start, s.end())
            }
        }
    }
}

/// A `channels` entry or `[textures]` value names a `[textures]` entry (not a path)?
fn is_texture_name(s: &str) -> bool {
    !s.is_empty() && !s.contains('.') && !s.contains('/') && !s.starts_with('<')
}

/// Resolve one `channels` entry (with `names`) or `[textures]` value (without):
/// "" (nothing), a `[textures]` name, a film path, `<lib/path>`, or a table with `file`
/// and options. Library paths stay relative until `resolve_libraries`.
fn texture_spec(
    what: &str,
    v: &toml::Value,
    names: Option<&BTreeMap<String, Channel>>,
    specs: &[LibrarySpec],
    root: &Path,
) -> Result<Option<Channel>> {
    let file_channel = |s: &str| -> Result<Channel> {
        let (library, rel, label) = match s.strip_prefix('<') {
            Some(inner) => {
                let Some(inner) = inner.strip_suffix('>') else {
                    bail!("{what}: \"{s}\" opens with < but doesn't end with >");
                };
                let Some((lib, rel)) = inner.split_once('/') else {
                    bail!(
                        "{what}: \"{s}\" needs a library name and a path, like \"<kit/textures/noise.png>\""
                    );
                };
                if !specs.iter().any(|l| l.name == lib) {
                    bail!(
                        "{what}: \"{s}\" names library \"{lib}\", but [libraries] has {}",
                        if specs.is_empty() {
                            "none".to_string()
                        } else {
                            specs
                                .iter()
                                .map(|l| format!("\"{}\"", l.name))
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    );
                }
                (Some(lib.to_string()), rel.to_string(), inner.to_string())
            }
            None => (None, s.to_string(), s.to_string()),
        };
        library::check_rel_path(&rel).map_err(|e| anyhow!("{what}: {e}"))?;
        if !crate::texture::has_image_extension(&rel) {
            bail!("{what}: {label}: shotglass reads PNG and JPEG images (.png, .jpg, .jpeg)");
        }
        Ok(Channel {
            file: if library.is_some() {
                PathBuf::from(&rel)
            } else {
                root.join(&rel)
            },
            label,
            library,
            rel,
            filter: Filter::Mipmap,
            wrap: Wrap::Repeat,
            vflip: true,
            srgb: false,
        })
    };
    match v {
        toml::Value::String(s) if s.is_empty() => {
            if names.is_none() {
                bail!("{what}: \"\" names no file");
            }
            Ok(None)
        }
        toml::Value::String(s) if is_texture_name(s) => {
            let Some(names) = names else {
                bail!(
                    "{what}: \"{s}\" isn't a file; give a path like \"textures/{s}.png\" or \
                     \"<kit/textures/{s}.png>\""
                );
            };
            match names.get(s) {
                Some(c) => Ok(Some(c.clone())),
                None => bail!(
                    "{what}: no texture called \"{s}\" in [textures] ({})",
                    if names.is_empty() {
                        "there are none".to_string()
                    } else {
                        format!(
                            "there are {}",
                            names
                                .keys()
                                .map(|k| format!("\"{k}\""))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    }
                ),
            }
        }
        toml::Value::String(s) => file_channel(s).map(Some),
        toml::Value::Table(t) => {
            let mut c = match t.get("file") {
                Some(toml::Value::String(s)) if !s.is_empty() && !is_texture_name(s) => {
                    file_channel(s)?
                }
                Some(toml::Value::String(s)) => bail!(
                    "{what}: file = \"{s}\" must be a path to a .png or .jpg (names from \
                     [textures] can't have options)"
                ),
                Some(_) => bail!("{what}: file must be a string"),
                None => bail!("{what}: a texture table needs file = \"…\""),
            };
            for (k, val) in t {
                let word = |names: &[&str]| -> Result<String> {
                    match val {
                        toml::Value::String(s) => Ok(s.clone()),
                        _ => bail!(
                            "{what}: {k} must be one of {}",
                            names
                                .iter()
                                .map(|n| format!("\"{n}\""))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    }
                };
                let flag = || -> Result<bool> {
                    val.as_bool()
                        .ok_or_else(|| anyhow!("{what}: {k} must be true or false"))
                };
                let not_one_of = |s: &str, names: &[&str]| {
                    anyhow!(
                        "{what}: {k} \"{s}\" isn't one of {}",
                        names
                            .iter()
                            .map(|n| format!("\"{n}\""))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                match k.as_str() {
                    "file" => {}
                    "filter" => {
                        let s = word(Filter::NAMES)?;
                        c.filter =
                            Filter::parse(&s).ok_or_else(|| not_one_of(&s, Filter::NAMES))?;
                    }
                    "wrap" => {
                        let s = word(Wrap::NAMES)?;
                        c.wrap = Wrap::parse(&s).ok_or_else(|| not_one_of(&s, Wrap::NAMES))?;
                    }
                    "vflip" => c.vflip = flag()?,
                    "srgb" => c.srgb = flag()?,
                    other => bail!(
                        "{what}: unknown option `{other}` (a texture has file, filter, wrap, vflip and srgb)"
                    ),
                }
            }
            Ok(Some(c))
        }
        _ => bail!(
            "{what}: expected a texture name, a path, \"<library/path>\", a table {{ file = \"…\" }} or \"\""
        ),
    }
}

/// What a transition's `type` names.
#[derive(Debug, PartialEq)]
pub enum TransitionKind<'a> {
    /// A .glsl file in the film (`fx/iris.glsl`).
    File(&'a str),
    /// A file in a library: (`transitions`, `iris.glsl`) for `transitions/iris`.
    Library(&'a str, String),
    /// A bare name (`iris`): not a film transition. The engine uses `copy` internally;
    /// the built-in looks were removed in favour of the kit's.
    Name(&'a str),
}

pub fn transition_kind(kind: &str) -> TransitionKind<'_> {
    if kind.ends_with(".glsl") {
        TransitionKind::File(kind)
    } else if let Some((lib, rest)) = kind.split_once('/') {
        TransitionKind::Library(lib, format!("{rest}.glsl"))
    } else {
        TransitionKind::Name(kind)
    }
}

/// The error for a bare transition name (`iris`), naming its kit replacement.
fn bare_transition_error(name: &str, specs: &[LibrarySpec]) -> String {
    let kit = specs.iter().find(|s| {
        s.subdir.as_deref() == Some("transitions")
            && s.source
                .as_deref()
                .is_some_and(|u| u.trim_end_matches('/').ends_with("/shotglass-kit"))
    });
    match kit {
        Some(s) => format!(
            "transition \"{name}\" isn't a library transition or a .glsl file; shotglass has \
             no built-in transitions: use type = \"{}/{name}\" from shotglass-kit",
            s.name
        ),
        None => format!(
            "transition \"{name}\" isn't a library transition or a .glsl file; shotglass has \
             no built-in transitions: add transitions = {{ source = \
             \"https://github.com/antonmarsden/shotglass-kit\", subdir = \"transitions\" }} \
             to [libraries], then use type = \"transitions/{name}\""
        ),
    }
}

pub fn glsl_float(x: f32) -> String {
    let s = format!("{x}");
    if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("NaN") {
        s
    } else {
        format!("{s}.0")
    }
}

fn is_ident(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        && !s.starts_with("gl_")
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: &str = r#"
        title = "t"
        [[shot]]
        id = "a"
        scene = "a.glsl"
        duration = 5
        [[shot]]
        id = "b"
        scene = "b.glsl"
        duration = 4
        transition = { type = "fx/crossfade.glsl", duration = 1 }
        [[shot]]
        id = "c"
        scene = "c.glsl"
        duration = 3
    "#;

    fn with_fps(fps: &str) -> Result<Movie> {
        Movie::parse(&format!("fps = {fps}\n{M}"), Path::new("."))
    }

    #[test]
    fn fps_is_rational() {
        let rate = |s: &str| with_fps(s).map(|m| (m.fps_num, m.fps_den)).unwrap();
        assert_eq!(rate("30"), (30, 1));
        assert_eq!(rate("24.0"), (24, 1));
        assert_eq!(rate("\"30000/1001\""), (30000, 1001));
        assert_eq!(rate("29.97"), (30000, 1001));
        assert_eq!(rate("23.976"), (24000, 1001));
        assert_eq!(rate("59.94"), (60000, 1001));
        assert_eq!(rate("12.5"), (25, 2));
        assert_eq!(rate("\"50/2\""), (25, 1));
        assert_eq!(rate("\" 60 \""), (60, 1));
        assert_eq!(Movie::parse(M, Path::new(".")).unwrap().fps_den, 1);
        for bad in [
            "0", "-30", "0.5", "\"30/0\"", "\"x\"", "\"1/2\"", "29.9701", "5000",
        ] {
            assert!(with_fps(bad).is_err(), "fps = {bad} should be rejected");
        }
    }

    #[test]
    fn frame_time_and_frame_at_agree() {
        for fps in [
            "30",
            "\"30000/1001\"",
            "\"24000/1001\"",
            "60",
            "12.5",
            "\"7/3\"",
        ] {
            let m = with_fps(fps).unwrap();
            for n in (0..200_000u64).step_by(7).chain([1, 2, 3, 299, 300, 301]) {
                assert_eq!(
                    m.frame_at(m.frame_time(n)) as u64,
                    n,
                    "fps {fps}, frame {n}"
                );
            }
            // Halfway between frames is still the earlier frame.
            let mid = (m.frame_time(10) + m.frame_time(11)) / 2.0;
            assert_eq!(m.frame_at(mid), 10);
            let n = m.frame_count();
            assert!(m.frame_time(n - 1) < m.duration && m.frame_time(n) >= m.duration);
        }
        assert_eq!(with_fps("30").unwrap().frame_count(), 330); // 11 s
    }

    #[test]
    fn layout_overlaps_transitions() {
        let m = Movie::parse(M, Path::new(".")).unwrap();
        let starts: Vec<f32> = m.shots.iter().map(|s| s.start).collect();
        assert_eq!(starts, vec![0.0, 4.0, 8.0]);
        assert_eq!(m.duration, 11.0);
    }

    #[test]
    fn at_resolves_shots_and_transitions() {
        let m = Movie::parse(M, Path::new(".")).unwrap();
        assert_eq!(
            m.at(1.0),
            At::Shot {
                shot: 0,
                local: 1.0
            }
        );
        match m.at(4.5) {
            At::Transition {
                from,
                to,
                from_local,
                to_local,
                progress,
            } => {
                assert_eq!((from, to), (0, 1));
                assert_eq!(from_local, 4.5);
                assert_eq!(to_local, 0.5);
                assert_eq!(progress, 0.5);
            }
            other => panic!("expected transition, got {other:?}"),
        }
        assert_eq!(
            m.at(5.0),
            At::Shot {
                shot: 1,
                local: 1.0
            }
        );
        // hard cut
        assert_eq!(
            m.at(8.0),
            At::Shot {
                shot: 2,
                local: 0.0
            }
        );
        // clamped
        assert_eq!(
            m.at(99.0),
            At::Shot {
                shot: 2,
                local: 3.0
            }
        );
        assert_eq!(
            m.at(-1.0),
            At::Shot {
                shot: 0,
                local: 0.0
            }
        );
    }

    #[test]
    fn cues_are_defines() {
        let m = Movie::parse(M, Path::new(".")).unwrap();
        let d = m.cue_defines();
        assert!(d.contains("#define CUE_B_START 4.0"));
        assert!(d.contains("#define CUE_B_SETTLED 5.0"));
        assert!(d.contains("#define MOVIE_DURATION 11.0"));
    }

    #[test]
    fn rejects_overlapping_transitions() {
        let bad = r#"
            [[shot]]
            id = "a"
            scene = "a.glsl"
            duration = 2
            [[shot]]
            id = "b"
            scene = "b.glsl"
            duration = 2
            transition = { type = "fx/fade.glsl", duration = 1.5 }
            [[shot]]
            id = "c"
            scene = "c.glsl"
            duration = 2
            transition = { type = "fx/fade.glsl", duration = 1.0 }
        "#;
        let e = Movie::parse(bad, Path::new(".")).unwrap_err().to_string();
        assert!(e.contains("overlaps"), "{e}");
    }

    #[test]
    fn shot_sound_follows_transitions() {
        let m = Movie::parse(
            r#"
            [[shot]]
            id = "a"
            scene = "a.glsl"
            duration = 5
            sound = "a.glsl"
            [[shot]]
            id = "b"
            scene = "b.glsl"
            duration = 4
            sound = "b.glsl"
            sound_lead = 0.5
            transition = { type = "fx/crossfade.glsl", duration = 1, ease = "linear" }
            "#,
            Path::new("."),
        )
        .unwrap();
        // a fades out across the transition (4..5) with equal power
        assert!((m.sound_gain(0, 2.0) - 1.0).abs() < 1e-6);
        assert!((m.sound_gain(0, 4.5) - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-5);
        assert!(m.sound_gain(0, 4.999) < 0.01);
        // b has a lead: it starts 0.5 s early at full level and skips the fade-in
        assert_eq!(m.sound_window(1), Some((3.5, 8.0)));
        assert_eq!(m.sound_gain(1, 3.4), 0.0);
        assert!((m.sound_gain(1, 4.2) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn transition_fades_keep_constant_power() {
        for ease in ["linear", "smooth", "inout"] {
            let m = Movie::parse(
                &format!(
                    "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=3\nsound=\"a.glsl\"\n\
                     [[shot]]\nid=\"b\"\nscene=\"b.glsl\"\nduration=3\nsound=\"b.glsl\"\n\
                     transition = {{ type = \"fx/crossfade.glsl\", duration = 1.5, ease = \"{ease}\" }}\n"
                ),
                Path::new("."),
            )
            .unwrap();
            // b starts at 1.5; the crossfade runs 1.5..3.0 (away from the 5 ms edge ramps)
            for i in 1..100 {
                let t = 1.5 + 1.5 * i as f32 / 100.0;
                let (ga, gb) = (m.sound_gain(0, t), m.sound_gain(1, t));
                let power = ga * ga + gb * gb;
                assert!((power - 1.0).abs() < 1e-4, "{ease} at {t}: {power}");
            }
        }
    }

    mod props {
        use super::super::*;
        use proptest::prelude::*;

        /// A random but valid timeline: durations, optional transitions that fit.
        fn timeline() -> impl Strategy<Value = String> {
            prop::collection::vec((0.2f32..6.0, prop::option::of(0.05f32..1.0), any::<bool>()), 1..12).prop_map(|shots| {
                let mut s = String::from("channels = [\"noise\"]\n[textures]\nnoise = { file = \"t/noise.png\", srgb = true }\n");
                let mut prev_in = 0.0f32;
                let mut prev_dur = 0.0f32;
                for (i, (dur, tr, freeze)) in shots.into_iter().enumerate() {
                    s += &format!("[[shot]]\nid=\"s{i}\"\nscene=\"x.glsl\"\nduration={dur}\n");
                    match i % 3 {
                        1 => s += "channels = [\"noise\", \"\", \"t/a.png\"]\n",
                        2 => s += "channels = [{ file = \"t/b.jpg\", filter = \"nearest\", wrap = \"clamp\" }]\n",
                        _ => {}
                    }
                    let mut this_in = 0.0;
                    if let (Some(frac), true) = (tr, i > 0) {
                        // keep it legal: fits both shots and doesn't overlap the previous one
                        let room = (prev_dur - prev_in).min(dur);
                        let d = frac * room * 0.95;
                        if d > 0.01 {
                            s += &format!("transition = {{ type = \"fx/crossfade.glsl\", duration = {d}, freeze = {freeze} }}\n");
                            this_in = d;
                        }
                    }
                    prev_in = this_in;
                    prev_dur = dur;
                }
                s
            })
        }

        proptest! {
            #[test]
            fn at_is_consistent_everywhere(toml in timeline(), samples in prop::collection::vec(0.0f32..1.0, 1..60)) {
                let m = Movie::parse(&toml, Path::new(".")).unwrap();
                prop_assert!(m.duration > 0.0);
                for u in samples {
                    let t = u * m.duration;
                    match m.at(t) {
                        At::Shot { shot, local } => {
                            prop_assert!(shot < m.shots.len());
                            prop_assert!(local >= -1e-4 && local <= m.shots[shot].duration + 1e-4);
                            prop_assert!((m.shots[shot].start + local - t).abs() < 1e-3);
                        }
                        At::Transition { from, to, from_local, to_local, progress } => {
                            prop_assert_eq!(from + 1, to);
                            prop_assert!(m.shots[to].transition_in.is_some());
                            prop_assert!((0.0..=1.0).contains(&progress));
                            prop_assert!(from_local <= m.shots[from].duration + 1e-3);
                            prop_assert!(to_local >= -1e-4);
                        }
                    }
                }
            }

            #[test]
            fn shots_tile_the_movie(toml in timeline()) {
                let m = Movie::parse(&toml, Path::new(".")).unwrap();
                prop_assert_eq!(m.shots[0].start, 0.0);
                for w in m.shots.windows(2) {
                    // the next shot starts when the previous ends, minus its transition
                    let d = w[1].transition_in.as_ref().map_or(0.0, |t| t.duration);
                    prop_assert!((w[1].start - (w[0].end() - d)).abs() < 1e-4);
                    prop_assert!(w[1].start >= w[0].start);
                }
                prop_assert!((m.duration - m.shots.last().unwrap().end()).abs() < 1e-4);
            }

            #[test]
            fn parser_never_panics(text in "\\PC{0,300}") {
                let _ = Movie::parse(&text, Path::new("."));
            }

            #[test]
            fn parser_never_panics_on_near_valid_input(toml in timeline(), cut in 0usize..400, junk in "[\\[\\]{}=\",.0-9a-z_ \n-]{0,12}") {
                let mut t = toml.clone();
                let at = cut.min(t.len());
                if t.is_char_boundary(at) {
                    t.insert_str(at, &junk);
                }
                let _ = Movie::parse(&t, Path::new("."));
            }
        }
    }

    const SHOT: &str = "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n";

    #[test]
    fn engine_contract_is_checked_first() {
        let v = crate::library::contract();
        let ok = format!("shotglass = \"{}.{}\"\n{SHOT}", v.major, v.minor);
        assert_eq!(
            Movie::parse(&ok, Path::new(".")).unwrap().shotglass,
            Some(format!("{}.{}", v.major, v.minor))
        );
        // A film from the future may use fields we don't know; the engine message wins.
        let future = format!("shotglass = \"99.1\"\nnew_thing = 3\n{SHOT}");
        let e = format!("{:#}", Movie::parse(&future, Path::new(".")).unwrap_err());
        assert!(
            e.contains("needs shotglass 99.1") && e.contains("newer"),
            "{e}"
        );
        assert!(Movie::parse(&format!("shotglass = 0.4\n{SHOT}"), Path::new(".")).is_err());
    }

    #[test]
    fn transition_types() {
        assert_eq!(
            transition_kind("fx/iris.glsl"),
            TransitionKind::File("fx/iris.glsl")
        );
        assert_eq!(
            transition_kind("transitions/iris"),
            TransitionKind::Library("transitions", "iris.glsl".into())
        );
        assert_eq!(
            transition_kind("kit/fx/wipe"),
            TransitionKind::Library("kit", "fx/wipe.glsl".into())
        );
        assert_eq!(transition_kind("iris"), TransitionKind::Name("iris"));

        let kit = "transitions = { source = \"https://github.com/antonmarsden/shotglass-kit\", \
                   subdir = \"transitions\" }\n";
        let film = |libs: &str, kinds: &[&str]| {
            let mut t = format!("[libraries]\n{libs}{SHOT}");
            for (i, k) in kinds.iter().enumerate() {
                t += &format!(
                    "[[shot]]\nid = \"s{i}\"\nscene = \"a.glsl\"\nduration = 1\n\
                     transition = {{ type = \"{k}\", duration = 0.5 }}\n"
                );
            }
            Movie::parse(&t, Path::new("."))
        };
        // Library transitions: no warning, and nothing in the film to watch for them.
        let m = film(kit, &["transitions/iris", "transitions/fade"]).unwrap();
        assert!(m.warnings.is_empty(), "{:?}", m.warnings);
        assert_eq!(m.referenced_files(), vec![PathBuf::from("./a.glsl")]);
        // The library must be declared, and the file stays inside it.
        let e = format!("{:#}", film(kit, &["kit/iris"]).unwrap_err());
        assert!(
            e.contains("names library \"kit\", but [libraries] has \"transitions\""),
            "{e}"
        );
        let e = format!("{:#}", film("", &["kit/iris"]).unwrap_err());
        assert!(e.contains("[libraries] has none"), "{e}");
        let e = format!("{:#}", film(kit, &["transitions/../x"]).unwrap_err());
        assert!(e.contains("climbs"), "{e}");
        // Bare names (the removed built-ins, or the engine's internal "copy") are
        // refused, and the error names the kit replacement.
        let e = format!("{:#}", film("", &["iris"]).unwrap_err());
        assert!(
            e.contains("no built-in transitions")
                && e.contains("subdir = \"transitions\"")
                && e.contains("type = \"transitions/iris\""),
            "{e}"
        );
        assert!(film(kit, &["copy"]).is_err());
        // With the kit's transitions already declared, it names the film's own local name.
        let fx = kit.replacen("transitions =", "fx =", 1);
        let e = format!("{:#}", film(&fx, &["fade"]).unwrap_err());
        assert!(e.contains("type = \"fx/fade\""), "{e}");
    }

    #[test]
    fn libraries_are_declared_by_local_name() {
        let m = Movie::parse(
            &format!(
                "[libraries]\n\
                 kit = {{ version = \"0.4\", source = \"https://github.com/antonmarsden/shotglass-kit\", subdir = \"core\" }}\n\
                 mine = {{ path = \"lib/mykit\" }}\n{SHOT}"
            ),
            Path::new("."),
        )
        .unwrap();
        assert_eq!(m.library_specs.len(), 2);
        assert_eq!(m.library_specs[0].name, "kit");
        assert_eq!(m.library_specs[0].subdir.as_deref(), Some("core"));
        assert_eq!(m.library_specs[1].path.as_deref(), Some("lib/mykit"));
        assert!(m.libraries.is_empty(), "parse doesn't touch the disk");
        let bad = format!("[libraries]\nkit = {{ path = \"../kit\" }}\n{SHOT}");
        assert!(
            format!("{:#}", Movie::parse(&bad, Path::new(".")).unwrap_err()).contains("climbs")
        );
        let bad = format!("[libraries]\nkit = {{ path = \"kit\", url = \"x\" }}\n{SHOT}");
        assert!(Movie::parse(&bad, Path::new(".")).is_err());
    }

    #[test]
    fn paths_must_be_web_friendly() {
        // Each line goes into shot "b".
        for (line, why) in [
            ("scene = \"/home/me/a.glsl\"", "absolute"),
            ("scene = \"C:/films/a.glsl\"", "drive letter"),
            ("scene = \"scenes\\\\a.glsl\"", "backslash"),
            ("scene = \"../other/a.glsl\"", "climbs"),
            ("scene = \"x.glsl\"\nsound = \"sfx/../../a.glsl\"", "climbs"),
            (
                "scene = \"x.glsl\"\ntransition = { type = \"/t/wipe.glsl\", duration = 0.5 }",
                "absolute",
            ),
        ] {
            let text = format!("{SHOT}[[shot]]\nid = \"b\"\nduration = 1\n{line}\n");
            let e = format!("{:#}", Movie::parse(&text, Path::new(".")).unwrap_err());
            assert!(e.contains(why) && e.contains("shot \"b\""), "{line}: {e}");
        }
        let text = format!("[sound]\nmusic = \"D:\\\\m.glsl\"\n{SHOT}");
        let e = format!("{:#}", Movie::parse(&text, Path::new(".")).unwrap_err());
        assert!(
            e.contains("[sound] music") && e.contains("backslash"),
            "{e}"
        );
        // Paths that climb and come back down inside the film are fine.
        let text =
            format!("{SHOT}[[shot]]\nid = \"b\"\nduration = 1\nscene = \"scenes/../b.glsl\"\n");
        assert!(Movie::parse(&text, Path::new(".")).is_ok());
    }

    #[test]
    fn load_resolves_libraries_and_warns_about_case() {
        let dir = std::env::temp_dir().join(format!("shotglass-movie-load-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("lib/mykit")).unwrap();
        std::fs::create_dir_all(dir.join("Scenes")).unwrap();
        std::fs::write(dir.join("Scenes/a.glsl"), "").unwrap();
        std::fs::write(
            dir.join("lib/mykit/library.toml"),
            "name = \"me/mykit\"\nversion = \"0.2.0\"\nlicences = [\"MIT\"]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("movie.toml"),
            "[libraries]\nmine = { path = \"lib/mykit\", version = \"0.2\" }\n\
             [[shot]]\nid = \"a\"\nscene = \"scenes/a.glsl\"\nduration = 1\n",
        )
        .unwrap();
        let m = Movie::load(&dir).unwrap();
        assert_eq!(m.libraries[0].canonical, "me/mykit");
        assert_eq!(m.libraries[0].root, dir.join("lib/mykit"));
        assert!(
            m.warnings.iter().any(|w| w.contains("\"Scenes/a.glsl\"")),
            "{:?}",
            m.warnings
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_library_needing_newer_glsl_needs_the_film_to_opt_in() {
        let dir =
            std::env::temp_dir().join(format!("shotglass-movie-libglsl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("lib/compute")).unwrap();
        std::fs::write(
            dir.join("lib/compute/library.toml"),
            "name = \"me/compute\"
version = \"1.0.0\"
glsl = 430
",
        )
        .unwrap();
        let toml = |glsl: &str| {
            std::fs::write(
                dir.join("movie.toml"),
                format!(
                    "{glsl}[libraries]
fx = {{ path = \"lib/compute\" }}
                     [[shot]]
id = \"a\"
scene = \"a.glsl\"
duration = 1
"
                ),
            )
            .unwrap();
        };
        for film in [
            "",
            "glsl = \"410\"
",
        ] {
            toml(film);
            let e = format!("{:#}", Movie::load(&dir).unwrap_err());
            assert!(
                e.contains("fx (me/compute 1.0.0) needs GLSL 430") && e.contains("glsl = \"430\""),
                "{e}"
            );
        }
        toml(
            "glsl = \"430\"
",
        );
        assert_eq!(Movie::load(&dir).unwrap().libraries[0].glsl, Some(430));
        // Only versions shotglass offers.
        std::fs::write(
            dir.join("lib/compute/library.toml"),
            "name = \"me/compute\"
version = \"1.0.0\"
glsl = 450
",
        )
        .unwrap();
        let e = format!("{:#}", Movie::load(&dir).unwrap_err());
        assert!(e.contains("glsl = 450"), "{e}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn glsl_opt_in() {
        let shot = "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=1\n";
        assert_eq!(Movie::parse(shot, Path::new(".")).unwrap().glsl, None);
        for v in ["330", "410", "430"] {
            let m = Movie::parse(&format!("glsl = \"{v}\"\n{shot}"), Path::new(".")).unwrap();
            assert_eq!(m.glsl, Some(v.parse().unwrap()));
        }
        for bad in ["\"450\"", "\"300 es\"", "\"\"", "430"] {
            let e = Movie::parse(&format!("glsl = {bad}\n{shot}"), Path::new("."));
            assert!(e.is_err(), "glsl = {bad} should be refused");
        }
        let e = Movie::parse(&format!("glsl = \"460\"\n{shot}"), Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(e.contains("\"410\" or \"430\""), "{e}");
    }

    fn channels_film(top: &str, textures: &str, shots: &[&str]) -> Result<Movie> {
        let mut t =
            format!("{top}[libraries]\nkit = {{ path = \"lib/kit\" }}\n[textures]\n{textures}");
        let shots = if shots.is_empty() { &[""][..] } else { shots };
        for (i, extra) in shots.iter().enumerate() {
            t += &format!("[[shot]]\nid = \"s{i}\"\nscene = \"a.glsl\"\nduration = 1\n{extra}\n");
        }
        Movie::parse(&t, Path::new("/film"))
    }

    fn err(r: Result<Movie>) -> String {
        format!("{:#}", r.unwrap_err())
    }

    #[test]
    fn channels_short_and_long_forms() {
        let m = channels_film(
            "",
            "noise = \"<kit/textures/noise.png>\"\n\
             grid = { file = \"textures/grid.png\", filter = \"nearest\", wrap = \"mirror\" }\n\
             moon = { file = \"textures/moon.JPG\", wrap = \"clamp\", srgb = true, vflip = false }\n",
            &["channels = [\"grid\", \"noise\", \"\", \"moon\"]",
              "channels = [\"\", \"textures/stars.png\", { file = \"<kit/logo.jpeg>\", wrap = \"clamp\" }]"],
        )
        .unwrap();
        let c = &m.shots[0].channels;
        let grid = c[0].as_ref().unwrap();
        assert_eq!(grid.file, PathBuf::from("/film/textures/grid.png"));
        assert_eq!(
            (grid.filter, grid.wrap, grid.vflip, grid.srgb),
            (Filter::Nearest, Wrap::Mirror, true, false)
        );
        let noise = c[1].as_ref().unwrap();
        assert_eq!(noise.library.as_deref(), Some("kit"));
        assert_eq!(noise.label, "kit/textures/noise.png");
        assert_eq!(noise.rel, "textures/noise.png");
        assert_eq!(
            (noise.filter, noise.wrap),
            (Filter::Mipmap, Wrap::Repeat),
            "Shadertoy's defaults"
        );
        assert!(c[2].is_none());
        let moon = c[3].as_ref().unwrap();
        assert_eq!(
            (moon.wrap, moon.srgb, moon.vflip),
            (Wrap::Clamp, true, false)
        );
        let c = &m.shots[1].channels;
        assert!(c[0].is_none() && c[3].is_none());
        assert_eq!(c[1].as_ref().unwrap().label, "textures/stars.png");
        assert_eq!(c[2].as_ref().unwrap().wrap, Wrap::Clamp);
        assert!(m.uses_channels());
        assert!(m.warnings.is_empty(), "{:?}", m.warnings);
        // Film textures are referenced files (case check, web preload); library ones aren't.
        let files = m.referenced_files();
        assert!(files.contains(&PathBuf::from("/film/textures/grid.png")));
        assert!(files.contains(&PathBuf::from("/film/textures/stars.png")));
        assert!(!files.iter().any(|f| f.to_string_lossy().contains("noise")));
        // A plain kit/… path (no brackets) is in the film folder, as for #include "…".
        let m = channels_film("", "", &["channels = [\"kit/textures/noise.png\"]"]).unwrap();
        let c = m.shots[0].channels[0].as_ref().unwrap();
        assert_eq!(
            (c.library.as_ref(), c.file.clone()),
            (None, PathBuf::from("/film/kit/textures/noise.png"))
        );
    }

    #[test]
    fn film_default_channels_and_replacement() {
        let m = channels_film(
            "channels = [\"noise\"]\n",
            "noise = \"t/noise.png\"\nbrick = \"t/brick.png\"\n",
            &["", "channels = [\"brick\"]", "channels = []"],
        )
        .unwrap();
        let label = |i: usize, n: usize| m.shots[i].channels[n].as_ref().map(|c| c.label.clone());
        assert_eq!(
            label(0, 0).as_deref(),
            Some("t/noise.png"),
            "default applies"
        );
        assert_eq!(
            label(1, 0).as_deref(),
            Some("t/brick.png"),
            "replaced, not merged"
        );
        assert_eq!(label(1, 1), None);
        assert!(
            m.shots[2].channels.iter().all(Option::is_none),
            "channels = [] binds nothing"
        );
        // Without any channels, nothing is bound and the film is unchanged.
        let m = Movie::parse(M, Path::new(".")).unwrap();
        assert!(!m.uses_channels() && m.contract_warning().is_none());
    }

    #[test]
    fn channels_are_validated() {
        let e = err(channels_film(
            "",
            "",
            &["channels = [\"\", \"\", \"\", \"\", \"\"]"],
        ));
        assert!(
            e.contains("shot \"s0\": channels has 5 entries; there are 4 (iChannel0..3)"),
            "{e}"
        );
        let e = err(channels_film(
            "",
            "brick = \"b.png\"\nnoise = \"n.png\"\ngrid = \"g.png\"\n",
            &["channels = [\"brick\", \"noize\"]"],
        ));
        assert!(
            e.contains("shot \"s0\": channels[1]: no texture called \"noize\" in [textures] (there are \"brick\", \"grid\", \"noise\")"),
            "{e}"
        );
        let e = err(channels_film("", "", &["channels = [\"noise\"]"]));
        assert!(e.contains("there are none"), "{e}");
        let e = err(channels_film("", "noise = \"<kat/t/noise.png>\"\n", &[]));
        assert!(
            e.contains("[textures] noise: \"<kat/t/noise.png>\" names library \"kat\", but [libraries] has \"kit\""),
            "{e}"
        );
        let e = err(channels_film(
            "",
            "",
            &["channels = [\"textures/sky.webp\"]"],
        ));
        assert!(
            e.contains("channels[0]: textures/sky.webp: shotglass reads PNG and JPEG"),
            "{e}"
        );
        let e = err(channels_film(
            "",
            "grid = { file = \"g.png\", filter = \"point\" }\n",
            &[],
        ));
        assert!(
            e.contains(
                "[textures] grid: filter \"point\" isn't one of \"mipmap\", \"linear\", \"nearest\""
            ),
            "{e}"
        );
        let e = err(channels_film(
            "",
            "grid = { file = \"g.png\", wrap = \"tile\" }\n",
            &[],
        ));
        assert!(
            e.contains("wrap \"tile\" isn't one of \"repeat\", \"clamp\", \"mirror\""),
            "{e}"
        );
        let e = err(channels_film(
            "",
            "grid = { file = \"g.png\", vflip = \"yes\" }\n",
            &[],
        ));
        assert!(e.contains("vflip must be true or false"), "{e}");
        // Unknown fields in a texture table (deny_unknown_fields, by hand).
        let e = err(channels_film(
            "",
            "grid = { file = \"g.png\", filtr = \"nearest\" }\n",
            &[],
        ));
        assert!(e.contains("unknown option `filtr`"), "{e}");
        let e = err(channels_film(
            "",
            "",
            &["channels = [{ filter = \"nearest\" }]"],
        ));
        assert!(e.contains("needs file"), "{e}");
        // Path rules, as for every other path.
        for (bad, why) in [
            ("../x.png", "climbs"),
            ("/x.png", "absolute"),
            ("t\\\\x.png", "backslash"),
            ("<kit/../../x.png>", "climbs"),
        ] {
            let e = err(channels_film("", "", &[&format!("channels = [\"{bad}\"]")]));
            assert!(e.contains(why) && e.contains("channels[0]"), "{bad}: {e}");
        }
        // Names are identifiers; [textures] values are files, not names.
        let e = err(channels_film("", "\"my-tex\" = \"a.png\"\n", &[]));
        assert!(e.contains("[textures] my-tex"), "{e}");
        let e = err(channels_film("", "a = \"noise\"\n", &[]));
        assert!(e.contains("isn't a file"), "{e}");
        let e = err(channels_film("", "", &["channels = [3]"]));
        assert!(e.contains("expected a texture name"), "{e}");
        // Shot-level unknown fields are still refused.
        assert!(channels_film("", "", &["channel = [\"a.png\"]"]).is_err());
    }

    #[test]
    fn unused_textures_warn_and_contract_is_checked() {
        let m = channels_film(
            "",
            "noise = \"n.png\"\nspare = \"s.png\"\n",
            &["channels = [\"noise\"]"],
        )
        .unwrap();
        assert_eq!(
            m.warnings,
            vec!["[textures] spare isn't used by any shot's channels".to_string()]
        );
        let w = m.contract_warning().unwrap();
        assert!(w.contains("shotglass = \"0.1.1\""), "{w}");
        let m = channels_film("shotglass = \"0.1\"\n", "", &["channels = [\"n.png\"]"]).unwrap();
        assert!(m.contract_warning().unwrap().contains("it says \"0.1\""));
        let m = channels_film("shotglass = \"0.1.1\"\n", "", &["channels = [\"n.png\"]"]).unwrap();
        assert_eq!(m.contract_warning(), None);
    }

    #[test]
    fn load_checks_texture_files_and_resolves_library_ones() {
        let dir = std::env::temp_dir().join(format!("shotglass-movie-tex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("lib/kit/textures")).unwrap();
        std::fs::create_dir_all(dir.join("textures")).unwrap();
        std::fs::write(dir.join("a.glsl"), "").unwrap();
        std::fs::write(
            dir.join("lib/kit/library.toml"),
            "name = \"me/kit\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let img = |p: &str, w: u32, h: u32| {
            image::RgbaImage::new(w, h).save(dir.join(p)).unwrap();
        };
        img("lib/kit/textures/noise.png", 4, 4);
        img("textures/stars.png", 2, 2);
        let film = |channels: &str| {
            std::fs::write(
                dir.join("movie.toml"),
                format!("[libraries]\nkit = {{ path = \"lib/kit\" }}\n[[shot]]\nid = \"sky\"\nscene = \"a.glsl\"\nduration = 1\nchannels = {channels}\n"),
            )
            .unwrap();
            Movie::load(&dir)
        };
        let m = film("[\"<kit/textures/noise.png>\", \"textures/stars.png\"]").unwrap();
        assert_eq!(
            m.shots[0].channels[0].as_ref().unwrap().file,
            dir.join("lib/kit/textures/noise.png")
        );
        assert_eq!(m.textures().len(), 2);
        let e = format!(
            "{:#}",
            film("[\"\", \"\", \"textures/strs.png\"]").unwrap_err()
        );
        assert!(
            e.contains("shot \"sky\": channels[2]: \"textures/strs.png\" not found (did you mean \"textures/stars.png\"?)"),
            "{e}"
        );
        let e = format!("{:#}", film("[\"<kit/textures/nois.png>\"]").unwrap_err());
        assert!(e.contains("did you mean \"kit/textures/noise.png\""), "{e}");
        // A case-only match is the usual case warning.
        let m = film("[\"textures/Stars.png\"]");
        if let Ok(m) = m {
            assert!(
                m.warnings.iter().any(|w| w.contains("Stars.png")),
                "{:?}",
                m.warnings
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_unknown_fields() {
        let bad = "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=1\nlenght=2\n";
        assert!(Movie::parse(bad, Path::new(".")).is_err());
    }
}
