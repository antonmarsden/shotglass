//! `movie.toml` parsing and the timeline: which shot (or transition) is on screen at time t.

use crate::anim::{Anim, Ease, vector};
use crate::library::{self, Library, LibrarySpec};
use crate::project::{self, Project};
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
    title: Option<String>,
    /// Width:height, e.g. [16, 9]. Default 16:9.
    aspect: Option<[f32; 2]>,
    /// Frame rate: a number (30, 29.97) or a ratio string ("30000/1001"). Used for
    /// frame stepping, iFrame and export; playback itself runs at the display rate.
    fps: Option<FpsFile>,
    /// Opt in to a later desktop GLSL version: "330" (the default), "410" or "430".
    glsl: Option<String>,
    /// Play forever: the film repeats with its duration (a whole number of frames) as
    /// the period, and shaders only ever see time within one period.
    /// `"warm"` (#138): also a seamless loop for feedback buffers.
    #[serde(rename = "loop")]
    looping: Option<toml::Value>,
    sound: Option<SoundFile>,
    #[serde(rename = "shot", default)]
    shots: Vec<ShotFile>,
    #[serde(rename = "marker", default)]
    markers: Vec<MarkerFile>,
    /// Layers above the picture track, bottom to top (#69).
    #[serde(rename = "layer", default)]
    layers: Vec<LayerFile>,
    /// Feedback buffers (#134): stepped once per film frame, after the picture track
    /// and before any layer, in file order.
    #[serde(rename = "buffer", default)]
    buffers: Vec<BufferFile>,
    /// Film-wide channels for every shot that doesn't set its own `channels`.
    channels: Option<Vec<toml::Value>>,
    /// Named textures: `brick = "textures/brick.png"` or `{ file = "…", filter = … }`.
    #[serde(default)]
    textures: BTreeMap<String, toml::Value>,
    /// Named fonts for `[texts]`: `body = "fonts/Inter.ttf"` or `{ file, index, variations,
    /// fallback, licence }`.
    #[serde(default)]
    fonts: BTreeMap<String, toml::Value>,
    /// Named text blocks, bound to channels like textures: `{ text = "…", font = "body", … }`.
    #[serde(default)]
    texts: BTreeMap<String, toml::Value>,
    /// Named comps (#74): `intro = "comps/intro.toml"` or `{ file, size, loop }`.
    #[serde(default)]
    comps: BTreeMap<String, toml::Value>,
}

/// A comp file (#74): a sequence of its own, picture only. Everything else (fps,
/// aspect, sound, named textures and fonts, other comps) comes from the film.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompFile {
    #[serde(rename = "shot", default)]
    shots: Vec<ShotFile>,
    #[serde(rename = "layer", default)]
    layers: Vec<LayerFile>,
    channels: Option<Vec<toml::Value>>,
    /// Feedback buffers (#138): they fold over comp-local frames.
    #[serde(rename = "buffer", default)]
    buffers: Vec<BufferFile>,
}

/// `loop = true | false | "warm"`: (looping, warm). "warm" (#138) is a loop whose
/// buffers carry their state over the seam: frame 0 continues from a whole period of
/// pre-roll instead of a reset. Films and `[comps]` entries share it.
fn loop_mode(what: &str, v: Option<&toml::Value>) -> Result<(bool, bool)> {
    match v {
        None | Some(toml::Value::Boolean(false)) => Ok((false, false)),
        Some(toml::Value::Boolean(true)) => Ok((true, false)),
        Some(toml::Value::String(m)) if m == "warm" => Ok((true, true)),
        Some(other) => bail!(
            "{what}loop = {other}: loop must be true, false or \"warm\" (a loop whose feedback \
             buffers carry their state over the seam)"
        ),
    }
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
    /// A film file with `mainImage`; or, instead, `comp` (#74): a name from `[comps]`.
    scene: Option<String>,
    comp: Option<String>,
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
    /// Channel sources for iChannel0..: names from [textures], paths, `<lib/path>`, tables, "".
    channels: Option<Vec<toml::Value>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LayerFile {
    id: String,
    /// A film file whose `mainImage` writes straight alpha.
    scene: Option<String>,
    /// A library pass (`"fx/grain"`) or a film `.glsl` file, resolved like a transition.
    #[serde(rename = "type")]
    kind: Option<String>,
    /// From / to: movie seconds, or `"<shot or layer>[.start|.settled|.end] ± s"`.
    from: Option<toml::Value>,
    to: Option<toml::Value>,
    /// 0..1, or keyframes in layer-local seconds.
    opacity: Option<toml::Value>,
    /// Fade in / out, seconds (smooth), multiplied into `opacity`.
    fade: Option<[f32; 2]>,
    /// `"auto"` (the player's render scale) or a fixed fraction of the output size.
    scale: Option<toml::Value>,
    #[serde(default)]
    uniforms: BTreeMap<String, toml::Value>,
    /// `#define`s for library passes, as transition params.
    #[serde(default)]
    params: BTreeMap<String, toml::Value>,
    channels: Option<Vec<toml::Value>>,
    time_offset: Option<f32>,
    time_scale: Option<f32>,
    blend: Option<String>,
    /// A name from `[comps]` instead of `scene`/`type` (#74).
    comp: Option<String>,
    /// `{ source = "@id", channel = "alpha" | "luma", invert = false }` (#74).
    matte: Option<toml::Value>,
    // Later phases: recognised so the message names when they arrive.
    transition_in: Option<toml::Value>,
    transition_out: Option<toml::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BufferFile {
    id: String,
    /// A film file whose `mainImage` writes the new state (raw values, no premultiply).
    scene: String,
    /// Drawn once at each reset as the state before it (default: black).
    init: Option<String>,
    /// The span, as for layers (default: the whole film).
    from: Option<toml::Value>,
    to: Option<toml::Value>,
    /// Extra resets: shot or layer ids (their start), span-style points, seconds; or
    /// `"cuts"` (every shot start in the span).
    reset: Option<toml::Value>,
    /// `[w, h]` in pixels, or `scale` (a fraction of the output size); never "auto".
    size: Option<[u32; 2]>,
    scale: Option<toml::Value>,
    /// `"rgba16f"` (default), `"rgba32f"` or `"rgba8"`.
    format: Option<String>,
    /// Keep the last N states, read as `{ source = "@id", frame = -k }` (1 ≤ k ≤ N).
    history: Option<i64>,
    /// `"dense"`: checkpoints every frame near the playhead (player scrubbing, #137).
    cache: Option<String>,
    /// Player only: approximate a far seek by stepping only the last s seconds from
    /// black (#137). Never used by `still`, `export` or any headless command.
    preview_lookback: Option<f64>,
    channels: Option<Vec<toml::Value>>,
    #[serde(default)]
    uniforms: BTreeMap<String, toml::Value>,
    #[serde(default)]
    params: BTreeMap<String, toml::Value>,
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

/// How many iChannels a shot has: `iChannel0..N_CHANNELS-1`. The scene header in
/// `gfx.rs` declares exactly these (with `iChannelResolution`/`iChannelTime`/
/// `iChannelMeta` arrays of this size). Never bake the number into a file format.
pub const N_CHANNELS: usize = 8;

/// What a `channels` entry or `[textures]` value names, from its spelling alone. The
/// one place that tells the kinds apart (docs/channels.md, "Channel sources"):
///
/// - `""` binds nothing;
/// - `@word` is another pass's picture (`@input`, `@picture`, `@<layer>`, `@<comp>`);
/// - a word without `.`, `/` or `<` is a name from `[textures]` (later also `[texts]`);
/// - anything else is a file, in the film or `<library/path>`, and its file name says
///   which kind: a run of `#` is an image sequence (`fog_####.png`), a video extension
///   is a video, and the rest are images.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ChannelSource<'a> {
    Empty,
    Output(&'a str),
    Name(&'a str),
    Image(&'a str),
    Sequence(&'a str),
    Video(&'a str),
}

/// Extensions read as video (any case).
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "m4v", "mov", "mkv", "webm", "avi"];

impl<'a> ChannelSource<'a> {
    pub fn of(s: &'a str) -> ChannelSource<'a> {
        if s.is_empty() {
            return ChannelSource::Empty;
        }
        if let Some(out) = s.strip_prefix('@') {
            return ChannelSource::Output(out);
        }
        if is_texture_name(s) {
            return ChannelSource::Name(s);
        }
        let path = s
            .strip_prefix('<')
            .map_or(s, |p| p.strip_suffix('>').unwrap_or(p));
        let file = path.rsplit('/').next().unwrap_or(path);
        let ext = file.rsplit_once('.').map_or("", |(_, e)| e);
        if file.contains('#') {
            ChannelSource::Sequence(s)
        } else if VIDEO_EXTENSIONS.iter().any(|v| ext.eq_ignore_ascii_case(v)) {
            ChannelSource::Video(s)
        } else {
            ChannelSource::Image(s)
        }
    }
}

/// The message for a channel kind this shotglass knows the spelling of but can't bind
/// yet, naming the version that brings it.
fn not_yet(what: &str, s: &str, kind: &str) -> anyhow::Error {
    let (is, arrives) = match kind {
        "output" => (
            "not a layer below this one",
            "@ names input, picture, an earlier layer, or a comp from [comps]",
        ),
        "video" => (
            "a video",
            "video files arrive with footage phase 2, after shotglass 0.1.6; meanwhile \
             convert it to an image sequence (ffmpeg -i clip.mp4 frames/clip_%05d.png, then \
             \"frames/clip_#####.png\")",
        ),
        _ => (
            "a text block",
            "text arrives with [fonts] and [texts] in shotglass 0.1.3",
        ),
    };
    anyhow!("{what}: \"{s}\" is {is}; {arrives}")
}

/// A file a channel reads: an image, a font, a text file. In the film folder or a library.
#[derive(Clone, Debug, PartialEq)]
pub struct FileRef {
    /// Film root (or, once `resolve_libraries` ran, library root) joined with `rel`.
    pub file: PathBuf,
    /// As written, for messages: "textures/brick.png", "kit/textures/noise.png".
    pub label: String,
    /// The library's local name for `<lib/path>` files.
    pub library: Option<String>,
    /// The path inside the film folder or the library.
    pub rel: String,
}

/// A font as `[fonts]` names it.
#[derive(Clone, Debug, PartialEq)]
pub struct FontRef {
    pub name: String,
    pub file: FileRef,
    /// The face in a collection (`.ttc`).
    pub index: u32,
    /// Fixed variation-axis values: `{ wght = 700 }`.
    pub variations: Vec<(String, f32)>,
    /// SPDX licence of a film-folder font, for `licenses` (library fonts use the
    /// library's `licences`).
    pub licence: Option<String>,
}

/// What a text block says.
#[derive(Clone, Debug, PartialEq)]
pub enum TextBody {
    /// The words (for a glyph set: its characters).
    Inline(String),
    /// A UTF-8 file, read when the block is laid out (live-reloaded like an image).
    File(FileRef),
    /// An SRT or WebVTT file (a glyph run with an entry per cue).
    Subtitles(FileRef),
}

/// How a text is drawn (#71).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GlyphMode {
    /// One texture: coverage + SDF of the whole block (phase 1).
    Block,
    /// `glyphs = true`: an MTSDF atlas plus a glyph table.
    Run,
    /// `glyphs = "0123…"` with `set = true`: those characters, by code point.
    Set,
}

/// Which texture of a text a channel binds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TextPart {
    /// The block texture (`GlyphMode::Block`), or a glyph run's atlas.
    Main,
    /// A glyph run's table: `{ text = "credits", part = "glyphs" }`.
    Table,
}

/// Default `atlas_size` (px per em in a glyph run's atlas) and its range.
pub const ATLAS_SIZE: f32 = 48.0;
const ATLAS_SIZES: std::ops::RangeInclusive<f32> = 8.0..=256.0;
/// A glyph run atlas's distance-field spread (px each side), fixed: the kit's
/// effects are written for it, and `iChannelMeta` carries it.
pub const ATLAS_SPREAD: f32 = 4.0;

/// A `[texts]` entry: words, fonts (primary first, then the fallback chain) and layout.
#[derive(Clone, Debug, PartialEq)]
pub struct TextSource {
    pub name: String,
    pub body: TextBody,
    pub fonts: Vec<FontRef>,
    pub style: crate::text::TextStyle,
    pub glyphs: GlyphMode,
    /// Glyph runs and sets: the atlas's px per em.
    pub atlas_size: f32,
    /// What the channel binding this source reads.
    pub part: TextPart,
}

impl TextSource {
    /// Subtitle cues, for a `subtitles` text.
    pub fn subtitles(&self) -> Result<Option<Vec<crate::subtitles::Entry>>> {
        Ok(self.subtitles_reporting()?.map(|(e, _)| e))
    }

    /// Subtitle cues and the styling parsing dropped (`check` reports it), for a
    /// `subtitles` text.
    pub fn subtitles_reporting(
        &self,
    ) -> Result<Option<(Vec<crate::subtitles::Entry>, crate::subtitles::Dropped)>> {
        let TextBody::Subtitles(f) = &self.body else {
            return Ok(None);
        };
        let bytes = std::fs::read(&f.file)
            .with_context(|| format!("text \"{}\": can't read {}", self.name, f.label))?;
        let s = String::from_utf8(bytes)
            .map_err(|_| anyhow!("text \"{}\": {} isn't UTF-8", self.name, f.label))?;
        let format = crate::subtitles::Format::of(&f.file)
            .ok_or_else(|| anyhow!("text \"{}\": {} isn't .srt or .vtt", self.name, f.label))?;
        crate::subtitles::parse_reporting(&s, format, &f.label).map(Some)
    }

    /// The words, read from the file if the text has one (subtitles: the cues'
    /// texts, one after another).
    pub fn read_text(&self) -> Result<String> {
        match &self.body {
            TextBody::Inline(s) => Ok(s.clone()),
            TextBody::Subtitles(_) => Ok(self
                .subtitles()?
                .unwrap_or_default()
                .iter()
                .map(|e| e.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")),
            TextBody::File(f) => {
                let bytes = std::fs::read(&f.file)
                    .with_context(|| format!("text \"{}\": can't read {}", self.name, f.label))?;
                let s = String::from_utf8(bytes)
                    .map_err(|_| anyhow!("text \"{}\": {} isn't UTF-8", self.name, f.label))?;
                // One trailing newline is the editor's, not a blank last line.
                Ok(s.strip_suffix('\n').unwrap_or(&s).replace("\r\n", "\n"))
            }
        }
    }

    /// Load the fonts and lay the text out (what `check` and the renderer both do).
    pub fn layout(&self) -> Result<(crate::text::Layout, Vec<crate::text::Font>)> {
        let fonts = self
            .fonts
            .iter()
            .map(|f| {
                crate::text::Font::load(&crate::text::FontSpec {
                    file: f.file.file.clone(),
                    label: f.file.label.clone(),
                    index: f.index,
                    variations: f.variations.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let text = self.read_text()?;
        let refs: Vec<&crate::text::Font> = fonts.iter().collect();
        let layout = crate::text::layout(&text, &refs, &self.style)
            .with_context(|| format!("text \"{}\"", self.name))?;
        if !layout.missing.is_empty() {
            let chars: Vec<String> = layout
                .missing
                .iter()
                .map(|c| format!("'{c}' (U+{:04X})", *c as u32))
                .collect();
            bail!(
                "text \"{}\": no font in its chain ({}) has {}; add a font that does to \
                 fallback = [...]",
                self.name,
                self.fonts
                    .iter()
                    .map(|f| f.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                chars.join(", ")
            );
        }
        Ok((layout, fonts))
    }

    /// A glyph run's or set's atlas and table, built once for both parts: the
    /// atlas and table channels of one text version share it (the decode worker
    /// asks for each). Keyed by the source (part aside) and the size and mtime of
    /// every file it reads, so an edit rebuilds it.
    pub fn glyph_run_shared(&self) -> Result<std::sync::Arc<crate::glyphrun::GlyphRun>> {
        use std::sync::{Arc, Mutex};
        type Cached = (String, Arc<crate::glyphrun::GlyphRun>);
        static CACHE: Mutex<Vec<Cached>> = Mutex::new(vec![]);
        const KEEP: usize = 8;
        let mut key_src = self.clone();
        key_src.part = TextPart::Main;
        let mut key = format!("{key_src:?}");
        let mut files: Vec<&FileRef> = self.fonts.iter().map(|f| &f.file).collect();
        if let TextBody::File(f) | TextBody::Subtitles(f) = &self.body {
            files.push(f);
        }
        for f in files {
            let m = std::fs::metadata(&f.file).ok();
            key += &format!(
                "|{:?}{:?}",
                m.as_ref().map(|m| m.len()),
                m.and_then(|m| m.modified().ok())
            );
        }
        if let Some((_, run)) = CACHE.lock().unwrap().iter().find(|(k, _)| *k == key) {
            return Ok(run.clone());
        }
        let run = Arc::new(self.glyph_run()?);
        let mut cache = CACHE.lock().unwrap();
        cache.retain(|(k, _)| *k != key);
        cache.push((key, run.clone()));
        if cache.len() > KEEP {
            cache.remove(0);
        }
        Ok(run)
    }

    /// A glyph run's or set's atlas and table (on the decode worker).
    pub fn glyph_run(&self) -> Result<crate::glyphrun::GlyphRun> {
        let (layout, fonts) = self.layout()?;
        let refs: Vec<&crate::text::Font> = fonts.iter().collect();
        let what = || format!("text \"{}\"", self.name);
        match self.glyphs {
            GlyphMode::Block => bail!("{}: not a glyph run (engine bug)", what()),
            GlyphMode::Set => crate::glyphrun::build_set(
                &self.read_text()?,
                &refs,
                &self.style,
                self.atlas_size,
                ATLAS_SPREAD,
            ),
            GlyphMode::Run => match self.subtitles()? {
                Some(cues) => crate::glyphrun::build_subtitles(
                    &cues,
                    &refs,
                    &self.style,
                    self.atlas_size,
                    ATLAS_SPREAD,
                )
                .map(|(run, _)| run),
                None => crate::glyphrun::build(
                    &self.read_text()?,
                    &layout,
                    &refs,
                    self.atlas_size,
                    ATLAS_SPREAD,
                ),
            },
        }
        .with_context(what)
    }

    /// `iChannelMeta` for a glyph run's atlas: (atlas em, atlas spread, layout em,
    /// 0); its table: (table width, header texels, layout em, atlas spread). A
    /// block's is its layout's `meta`.
    pub fn glyph_meta(&self) -> [f32; 4] {
        match self.part {
            TextPart::Main => [self.atlas_size, ATLAS_SPREAD, self.style.size, 0.0],
            TextPart::Table => [
                crate::glyphrun::TABLE_WIDTH as f32,
                crate::glyphrun::HEADER_TEXELS as f32,
                self.style.size,
                ATLAS_SPREAD,
            ],
        }
    }
}

/// An image sequence (#70): `renders/fog_####.png`, one image per frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Sequence {
    /// The pattern as a path (label as written: "renders/fog_####.png"); its file name
    /// has one run of `#`. Library patterns resolve like any library file.
    pub pattern: FileRef,
    /// Source frames per second (`fps`); None = the film's.
    pub fps: Option<(u32, u32)>,
    /// The file number that is source frame 0 (`first`); None = the lowest found.
    pub first: Option<u64>,
    /// What `Movie::load` found on disk (None after a bare `parse`).
    pub scan: Option<std::sync::Arc<crate::footage::Scan>>,
}

/// Where footage starts: `in` (source seconds) or `in_frame` (a source frame).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum In {
    Seconds(f64),
    Frame(u64),
}

/// Footage's clock (`clock`): 0 at the item's start (a shot's or a layer's), or at
/// the film's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FootageClock {
    Shot,
    Movie,
}

/// Past the last source frame (`end`): keep it, wrap to frame 0, or read black.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    Hold,
    Loop,
    Black,
}

impl FootageClock {
    pub const NAMES: &[&str] = &["shot", "movie"];
}
impl End {
    pub const NAMES: &[&str] = &["hold", "loop", "black"];
}

/// Which source frame a footage channel shows when (`in`/`in_frame`, `speed`,
/// `clock`, `end`). Independent of the item's `time_offset`/`time_scale`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timing {
    pub start: In,
    /// Source seconds per clock second; 0 is a freeze frame.
    pub speed: f64,
    pub clock: FootageClock,
    pub end: End,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            start: In::Seconds(0.0),
            speed: 1.0,
            clock: FootageClock::Shot,
            end: End::Hold,
        }
    }
}

/// Everything `source_frame` needs for one footage channel of one shot or layer:
/// a pure function of the film frame number, shared by the renderer, `check` and
/// the tests.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameMap {
    /// The film's exact frame rate.
    pub film: (u32, u32),
    pub loop_frames: Option<u64>,
    /// Movie seconds where the item starts (its clock's 0 with `clock = "shot"`).
    pub origin: f32,
    pub timing: Timing,
    /// The source's exact frame rate.
    pub src: (u32, u32),
    /// Source frames (≥ 1).
    pub count: u64,
}

/// The source frame containing source position `f` (in frames): floor, except that a
/// value within floating-point error of a whole number is that number. The tolerance
/// (1e-9 absolute, 1e-11 relative) is far above f64 error (~1e-16 relative) and far
/// below any real fraction (24-in-30 steps by 0.2, 25 in 30000/1001 by 1/1200), so
/// exact ratios never lose a frame and long clips never gain one.
pub fn source_snap(f: f64) -> i64 {
    let n = f.round();
    if (f - n).abs() <= 1e-9 + 1e-11 * n.abs() {
        n as i64
    } else {
        f.floor() as i64
    }
}

impl FrameMap {
    /// The source frame shown on film frame `n`, or None for black (`end = "black"`
    /// past the last frame). Exact rationals in f64, never an f32 `t * fps`: film
    /// time `n·den/num`, the item's clock, `in + speed·clock` source seconds, then the
    /// source frame whose interval contains that instant (`source_snap`). A loop
    /// film's `n` is taken within one period first.
    pub fn source_frame(&self, n: u64) -> Option<u64> {
        let n = match self.loop_frames {
            Some(p) => n % p,
            None => n,
        };
        let (num, den) = (self.film.0 as f64, self.film.1 as f64);
        let t = n as f64 * den / num;
        let clock = match self.timing.clock {
            FootageClock::Shot => {
                // An item starts at an f32 time; on (or within 0.01 frame of) a frame
                // boundary it is that boundary exactly, so its clock is exact too.
                let o = self.origin as f64 * num / den;
                let r = o.round();
                let origin = if (o - r).abs() < 0.01 {
                    r * den / num
                } else {
                    self.origin as f64
                };
                t - origin
            }
            FootageClock::Movie => t,
        };
        let rate = self.src.0 as f64 / self.src.1 as f64;
        let start = match self.timing.start {
            In::Seconds(s) => s * rate,
            In::Frame(k) => k as f64,
        };
        let k = source_snap(start + self.timing.speed * clock * rate).max(0) as u64;
        if k < self.count {
            return Some(k);
        }
        match self.timing.end {
            End::Hold => Some(self.count - 1),
            End::Loop => Some(k % self.count),
            End::Black => None,
        }
    }

    /// Source time of frame `k` (`iChannelTime`): `k·den/num` seconds.
    pub fn source_time(&self, k: u64) -> f32 {
        (k as f64 * self.src.1 as f64 / self.src.0 as f64) as f32
    }
}

/// A shot or a layer, for `Movie::source_frame`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ItemRef {
    Shot(usize),
    Layer(usize),
    Buffer(usize),
}

/// What a channel shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Image(FileRef),
    Text(Box<TextSource>),
    /// An image sequence (#70): which frame depends on the film frame (`FrameMap`).
    Frames(Box<Sequence>),
    /// `@input` on a layer: the picture below it (everything under it, composited).
    Input,
    /// `@picture` on a layer: the picture track before any layer.
    Picture,
    /// `@<id>` on a layer: an earlier layer's own output (premultiplied, before it is
    /// composited); black while that layer isn't active.
    Layer {
        index: usize,
        id: String,
    },
    /// `@<comp>` (#74): a comp from `[comps]` (`Movie::comps[index]`), rendered at the
    /// consumer's iTime plus `time_offset`, at the comp's `size` (or the output size).
    Comp {
        index: usize,
        name: String,
        time_offset: f32,
    },
    /// `@<buffer>`, `@prev` or `{ source = "@<buffer>", frame = -k }` (#134): buffer
    /// `Movie::buffers[index]`'s state `back` frames ago (0: this frame's, after its
    /// step). Black while that buffer isn't active.
    Buffer {
        index: usize,
        id: String,
        back: u32,
    },
}

impl Source {
    /// Another pass's picture (`@…`), not a texture.
    pub fn is_pass(&self) -> bool {
        matches!(
            self,
            Source::Input
                | Source::Picture
                | Source::Layer { .. }
                | Source::Comp { .. }
                | Source::Buffer { .. }
        )
    }
}

/// What an iChannel is bound to, and how it's sampled.
#[derive(Clone, Debug, PartialEq)]
pub struct Channel {
    pub source: Source,
    pub filter: Filter,
    pub wrap: Wrap,
    /// Rows reversed on decode, so the image is upright with `fragCoord / iResolution.xy`.
    pub vflip: bool,
    /// Uploaded as SRGB8_ALPHA8: `texture()` returns linear values.
    pub srgb: bool,
    /// Footage only: which source frame shows when.
    pub timing: Timing,
}

impl Channel {
    /// Every single file the channel reads: the image, or a text's file and fonts.
    /// A sequence's frames are in its scan (`Sequence::scan`), not here.
    pub fn files(&self) -> Vec<&FileRef> {
        match &self.source {
            Source::Input
            | Source::Picture
            | Source::Layer { .. }
            | Source::Comp { .. }
            | Source::Buffer { .. }
            | Source::Frames(_) => vec![],
            Source::Image(f) => vec![f],
            Source::Text(t) => {
                let mut v: Vec<&FileRef> = vec![];
                if let TextBody::File(f) | TextBody::Subtitles(f) = &t.body {
                    v.push(f);
                }
                v.extend(t.fonts.iter().map(|f| &f.file));
                v
            }
        }
    }

    /// Every path as `movie.toml` writes it: `files()`, and a sequence's pattern.
    pub fn written(&self) -> Vec<&FileRef> {
        match &self.source {
            Source::Frames(s) => vec![&s.pattern],
            _ => self.files(),
        }
    }

    /// The written paths, to resolve against libraries (`written`).
    pub fn files_mut(&mut self) -> Vec<&mut FileRef> {
        match &mut self.source {
            Source::Input
            | Source::Picture
            | Source::Layer { .. }
            | Source::Comp { .. }
            | Source::Buffer { .. } => vec![],
            Source::Frames(s) => vec![&mut s.pattern],
            Source::Image(f) => vec![f],
            Source::Text(t) => {
                let t = &mut **t;
                let mut v: Vec<&mut FileRef> = vec![];
                if let TextBody::File(f) | TextBody::Subtitles(f) = &mut t.body {
                    v.push(f);
                }
                v.extend(t.fonts.iter_mut().map(|f| &mut f.file));
                v
            }
        }
    }

    /// For messages: the image's path, or `text "title"`.
    pub fn label(&self) -> String {
        match &self.source {
            Source::Image(f) => f.label.clone(),
            Source::Frames(s) => s.pattern.label.clone(),
            Source::Text(t) => format!("text \"{}\"", t.name),
            Source::Input => "@input".to_string(),
            Source::Picture => "@picture".to_string(),
            Source::Layer { id, .. } => format!("@{id}"),
            Source::Comp { name, .. } => format!("@{name}"),
            Source::Buffer { id, back: 0, .. } => format!("@{id}"),
            Source::Buffer { id, back, .. } => format!("@{id} (frame -{back})"),
        }
    }

    #[cfg(test)]
    pub fn image(&self) -> Option<&FileRef> {
        match &self.source {
            Source::Image(f) => Some(f),
            _ => None,
        }
    }

    pub fn text(&self) -> Option<&TextSource> {
        match &self.source {
            Source::Text(t) => Some(t),
            _ => None,
        }
    }

    pub fn sequence(&self) -> Option<&Sequence> {
        match &self.source {
            Source::Frames(s) => Some(s),
            _ => None,
        }
    }

    /// A library sequence's frames as library files (for `bundle`/`freeze`), once scanned.
    pub fn sequence_files(&self) -> Vec<FileRef> {
        let Some(seq) = self.sequence() else {
            return vec![];
        };
        let Some(scan) = &seq.scan else {
            return vec![];
        };
        let dir = seq.pattern.rel.rsplit_once('/').map(|(d, _)| d);
        (0..scan.count)
            .map(|k| {
                let name = scan.pattern.name(scan.first + k);
                let rel = dir.map_or(name.clone(), |d| format!("{d}/{name}"));
                FileRef {
                    file: scan.path(k),
                    label: match &seq.pattern.library {
                        Some(l) => format!("{l}/{rel}"),
                        None => rel.clone(),
                    },
                    library: seq.pattern.library.clone(),
                    rel,
                }
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct Shot {
    pub id: String,
    pub index: usize,
    pub start: f32,
    pub duration: f32,
    /// The scene file; for a comp shot, the comp's file (for messages and the watcher).
    pub scene: PathBuf,
    /// `comp = "…"` (#74): the shot shows `Movie::comps[i]` at `scene_time(local)`.
    pub comp: Option<usize>,
    pub time_offset: f32,
    pub time_scale: f32,
    pub uniforms: Vec<(String, Anim)>,
    pub transition_in: Option<Transition>,
    pub sound: Option<ShotSound>,
    /// What iChannel0..N_CHANNELS-1 carry (None: black, `iChannelResolution` 0).
    pub channels: [Option<Channel>; N_CHANNELS],
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

/// What a layer draws.
#[derive(Clone, Debug, PartialEq)]
pub enum LayerScene {
    /// `scene = "layers/title.glsl"`.
    File(PathBuf),
    /// `type = "fx/grain"` or `type = "fx/my.glsl"`: resolved like a transition type.
    Pass(String),
    /// `comp = "intro"` (#74): `Movie::comps[i]` at the layer's iTime.
    Comp(usize),
}

/// What a layer's `matte` reads (#74).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MatteSource {
    /// An earlier layer's own output (premultiplied; black while it isn't active).
    Layer(usize),
    /// A comp at the layer's iTime, at the comp's `size` (or the output size).
    Comp(usize),
}

/// `matte = { source, channel, invert }` (#74): the layer is multiplied by the
/// source's alpha or Rec. 709 luma (or one minus it) when it is composited.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Matte {
    pub source: MatteSource,
    pub luma: bool,
    pub invert: bool,
}

/// A `[comps]` entry (#74): a sequence of its own (shots, transitions, layers; picture
/// only) that shots, layers, channels and mattes can show.
#[derive(Clone, Debug)]
pub struct Comp {
    pub name: String,
    /// The comp file, and as written (film-relative).
    pub file: PathBuf,
    pub rel: String,
    /// Its own resolution (`size`); None = the size of whatever shows it.
    pub size: Option<(u32, u32)>,
    /// `loop = true`: comp time wraps at its duration (a whole number of frames);
    /// otherwise it holds its first / last frame outside `[0, duration)`.
    pub looping: bool,
    /// Its shots and layers as a movie: the film's fps, aspect, libraries and root; no
    /// sound, no comps of its own (references are indices into the film's `comps`).
    pub movie: Box<Movie>,
}

impl Comp {
    /// Comp-local time for comp time `t`: wrapped for a loop comp; held within
    /// `[0, last frame]` otherwise (as `Movie::at` clamps, but on a frame, so footage
    /// and iFrame hold too).
    pub fn local(&self, t: f32) -> f32 {
        let m = &self.movie;
        if self.looping {
            return m.wrap(t);
        }
        let last = m.frame_time(m.frame_count().saturating_sub(1));
        if t.is_nan() { 0.0 } else { t.clamp(0.0, last) }
    }
}

/// A `[[layer]]`: a scene drawn over everything below it within its span (#69).
#[derive(Clone, Debug)]
pub struct Layer {
    pub id: String,
    /// The span, movie seconds: active in [start, end).
    pub start: f32,
    pub end: f32,
    pub scene: LayerScene,
    pub uniforms: Vec<(String, Anim)>,
    pub params: Vec<(String, Vec<f32>)>,
    pub channels: [Option<Channel>; N_CHANNELS],
    pub opacity: Anim,
    /// Fade in / out in seconds.
    pub fade: (f32, f32),
    /// None: the frame's (auto) render scale; Some(s): a fixed fraction of the output.
    pub scale: Option<f32>,
    pub time_offset: f32,
    pub time_scale: f32,
    /// How the layer is composited: None = "over" (the engine's source-over), or a
    /// blend pass (`"blend/screen"`, a film `.glsl`) resolved like a transition type.
    pub blend: Option<String>,
    /// Multiplied by a matte when composited (#74).
    pub matte: Option<Matte>,
}

impl Layer {
    pub fn duration(&self) -> f32 {
        self.end - self.start
    }
    /// iTime for the layer's shader at layer-local time `local`.
    pub fn scene_time(&self, local: f32) -> f32 {
        self.time_offset + local * self.time_scale
    }
    /// Opacity at layer-local time `local`: `opacity` (keyframes or constant) times the
    /// fades (smooth), in 0..1.
    pub fn opacity_at(&self, local: f32) -> f32 {
        let fade = |x: f32, len: f32| {
            if len <= 0.0 {
                1.0
            } else {
                Ease::Smooth.apply((x / len).clamp(0.0, 1.0))
            }
        };
        let o = self.opacity.eval(local)[0].clamp(0.0, 1.0);
        o * fade(local, self.fade.0) * fade(self.duration() - local, self.fade.1)
    }
}

/// A buffer's texel format (#134).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BufferFormat {
    Rgba16f,
    Rgba32f,
    Rgba8,
}

impl BufferFormat {
    pub const NAMES: [&'static str; 3] = ["rgba16f", "rgba32f", "rgba8"];

    pub fn parse(s: &str) -> Option<BufferFormat> {
        match s {
            "rgba16f" => Some(BufferFormat::Rgba16f),
            "rgba32f" => Some(BufferFormat::Rgba32f),
            "rgba8" => Some(BufferFormat::Rgba8),
            _ => None,
        }
    }

    /// Bytes per texel.
    pub fn bytes(self) -> usize {
        match self {
            BufferFormat::Rgba16f => 8,
            BufferFormat::Rgba32f => 16,
            BufferFormat::Rgba8 => 4,
        }
    }
}

/// A buffer's size: fixed pixels, or a fraction of the output size (never the render
/// scale).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BufferSize {
    Fixed(u32, u32),
    Scale(f32),
}

impl BufferSize {
    /// The size for an output of `w` × `h`.
    pub fn at(self, (w, h): (i32, i32)) -> (i32, i32) {
        match self {
            BufferSize::Fixed(bw, bh) => (bw as i32, bh as i32),
            BufferSize::Scale(s) => (
                ((w as f32 * s).round() as i32).clamp(1, w.max(1)),
                ((h as f32 * s).round() as i32).clamp(1, h.max(1)),
            ),
        }
    }
}

/// The most states a buffer's `history` may keep.
pub const MAX_HISTORY: u32 = 64;
/// The most buffers a film may have.
pub const MAX_BUFFERS: usize = 64;
/// Required buffer memory (rings of every buffer active at once) a film may need.
pub const BUFFER_MEMORY_LIMIT: usize = 1 << 30;
/// The output height `scale = …` buffers are sized for in the memory check.
const MEMORY_CHECK_HEIGHT: f32 = 1080.0;

/// A `[[buffer]]` (#134): state kept from one film frame to the next. Its state at
/// film frame n is its state at the last reset r ≤ n, stepped once per film frame from
/// r to n (`state(r) = step(r, init)`): a pure function of the movie and n.
#[derive(Clone, Debug)]
pub struct Buffer {
    pub id: String,
    pub scene: PathBuf,
    pub init: Option<PathBuf>,
    /// The span in movie seconds, and the film frames it covers: active on frames
    /// `start_frame..end_frame` (those whose time is in `[start, end)`).
    pub start: f32,
    pub end: f32,
    pub start_frame: u64,
    pub end_frame: u64,
    /// Reset frames, sorted, the span start first.
    pub resets: Vec<u64>,
    pub size: BufferSize,
    pub format: BufferFormat,
    /// States kept (`history`, default 1: `@prev`).
    pub history: u32,
    /// `cache = "dense"`: checkpoint every frame instead of every second (#137).
    pub dense: bool,
    /// `preview_lookback`, in film frames (player only, #137).
    pub preview_lookback: Option<u64>,
    pub channels: [Option<Channel>; N_CHANNELS],
    pub uniforms: Vec<(String, Anim)>,
    pub params: Vec<(String, Vec<f32>)>,
}

impl Buffer {
    /// Is the buffer active on film frame `m`?
    pub fn active(&self, m: u64) -> bool {
        (self.start_frame..self.end_frame).contains(&m)
    }

    /// The last reset at or before frame `m` (None before the span).
    pub fn reset_at(&self, m: u64) -> Option<u64> {
        let i = self.resets.partition_point(|&r| r <= m);
        (i > 0).then(|| self.resets[i - 1])
    }

    /// A pure history buffer reads no buffer state (not even its own `@prev`): its
    /// state at n depends only on frame n's inputs, so it needs no checkpoints.
    pub fn is_pure(&self) -> bool {
        !self
            .channels
            .iter()
            .flatten()
            .any(|c| matches!(c.source, Source::Buffer { .. }))
    }

    /// GPU memory its ring needs at an output of `out` (history + 1 states).
    pub fn ring_bytes(&self, out: (i32, i32)) -> usize {
        let (w, h) = self.size.at(out);
        (self.history as usize + 1) * w as usize * h as usize * self.format.bytes()
    }

    /// iTime etc. are span-local, like a layer's.
    pub fn duration(&self) -> f32 {
        self.end - self.start
    }
}

/// What a channel needs to know about the film's buffers while they are parsed.
#[derive(Clone, Debug)]
struct BufDecl {
    id: String,
    history: u32,
    format: BufferFormat,
}

#[derive(Clone, Debug)]
pub struct Marker {
    pub t: f32,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct Movie {
    /// The film folder.
    pub root: PathBuf,
    /// The project root (`shotglass.toml`): libraries, `shotglass.lock` and overrides
    /// live there. The film folder itself for a one-film, frozen or bare project.
    pub project: PathBuf,
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
    /// `loop = true`: the film repeats every `n` frames (its whole duration). See
    /// `loop_period`, `wrap`, `sound_samples`.
    pub loop_frames: Option<u64>,
    /// `loop = "warm"` (#138): a loop film's (or comp's) buffers carry their state over
    /// the seam. Their fold runs over two periods (the first is pre-roll): frame n's
    /// state is the fold from the start of the period before, so frame 0 follows a
    /// whole period instead of a reset (`gfx::fold`). Always false without a loop.
    pub warm: bool,
    /// Movie-level music/ambience (`mainSound`, movie time).
    pub music: Option<PathBuf>,
    /// Final mix pass (`mainMix`).
    pub mix: Option<PathBuf>,
    pub mix_lookback: f32,
    pub volume: f32,
    pub shots: Vec<Shot>,
    /// Layers above the picture track, bottom to top.
    pub layers: Vec<Layer>,
    /// Feedback buffers (#134), in step order. A comp's fold over comp-local frames (#138).
    pub buffers: Vec<Buffer>,
    /// `[comps]` (#74), in name order. Always empty in a comp's own movie: comps refer
    /// to the film's by index.
    pub comps: Vec<Comp>,
    pub markers: Vec<Marker>,
    pub duration: f32,
    /// `shotglass = "…"` in the project: the engine contract range the film was made for.
    pub shotglass: Option<String>,
    /// The project's `[libraries]` as declared (validated, not yet looked up on disk).
    pub library_specs: Vec<LibrarySpec>,
    /// Libraries found on disk; filled by `load` (empty after a bare `parse`).
    pub libraries: Vec<Library>,
    /// Portability warnings (e.g. a file name that only matches when ignoring case).
    pub warnings: Vec<String>,
    /// Libraries `movie.toml` names a file of (`<lib/…>` in `[textures]`, `[fonts]`,
    /// `[texts]`), used or not: a frozen copy must still declare them.
    pub library_refs: std::collections::BTreeSet<String>,
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
        m.scan_footage()
            .with_context(|| format!("in {}", m.root.join("movie.toml").display()))?;
        m.check_case();
        m.check_textures()
            .with_context(|| format!("in {}", m.root.join("movie.toml").display()))?;
        Ok(m)
    }

    /// Load with libraries exactly as `shotglass.lock` pins them (cache or frozen
    /// copies), ignoring per-machine overrides (`SHOTGLASS_LIB_*`, `shotglass.local.toml`).
    /// `freeze` and `bundle` use it: they carry the locked files, never a working copy.
    pub fn load_locked(path: &Path, cache: &Path) -> Result<Movie> {
        Movie::read(path)?.finish_locked_load(cache)
    }

    /// Validate a film against an explicit project, independently of project discovery.
    pub(crate) fn load_locked_in(root: &Path, project: &Project, cache: &Path) -> Result<Movie> {
        let file = root.join(crate::project::MOVIE_FILE);
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        Movie::parse_in(&text, root, project)
            .with_context(|| format!("in {}", file.display()))?
            .finish_locked_load(cache)
    }

    fn finish_locked_load(mut self, cache: &Path) -> Result<Movie> {
        self.resolve_libraries_with(&BTreeMap::new(), cache)
            .with_context(|| format!("in {}", self.root.join("movie.toml").display()))?;
        self.scan_footage()
            .with_context(|| format!("in {}", self.root.join("movie.toml").display()))?;
        self.check_case();
        self.check_textures()
            .with_context(|| format!("in {}", self.root.join("movie.toml").display()))?;
        Ok(self)
    }

    /// Does any shot bind a texture?
    pub fn uses_channels(&self) -> bool {
        self.shots
            .iter()
            .any(|s| s.channels.iter().any(Option::is_some))
    }

    /// Every bound channel of every shot, then of every layer, then of every buffer,
    /// then of every comp.
    pub fn channels(&self) -> impl Iterator<Item = &Channel> + '_ {
        let comps: Box<dyn Iterator<Item = &Channel>> =
            Box::new(self.comps.iter().flat_map(|c| c.movie.channels()));
        self.shots
            .iter()
            .flat_map(|s| s.channels.iter().flatten())
            .chain(self.layers.iter().flat_map(|l| l.channels.iter().flatten()))
            .chain(
                self.buffers
                    .iter()
                    .flat_map(|b| b.channels.iter().flatten()),
            )
            .chain(comps)
    }

    /// Every distinct texture (by file and how its texels are made), in shot order,
    /// then layer order. `@input` isn't a texture.
    pub fn textures(&self) -> Vec<&Channel> {
        let mut v: Vec<&Channel> = vec![];
        for c in self.channels().filter(|c| !c.source.is_pass()) {
            if !v
                .iter()
                .any(|o| o.source == c.source && o.vflip == c.vflip && o.srgb == c.srgb)
            {
                v.push(c);
            }
        }
        v
    }

    /// `check`'s warning for a film that uses a feature newer than the engine contract
    /// it says it needs (textures: 0.1.1, `loop`: 0.1.2, text and iChannel4..7: 0.1.3,
    /// layers: 0.1.4, blends: 0.1.5, image sequences: 0.1.6, comps: 0.1.7, glyph runs:
    /// 0.1.8, buffers: 0.1.9):
    /// older engines would then fail
    /// on an "unknown field" instead of asking for a newer shotglass.
    pub fn contract_warning(&self) -> Option<String> {
        let mut features = vec![];
        let mut need = semver::Version::new(0, 1, 0);
        if self.uses_channels() {
            features.push("channels (iChannel textures), which need shotglass 0.1.1");
            need = semver::Version::new(0, 1, 1);
        }
        if self.loop_frames.is_some() {
            features.push("loop, which needs shotglass 0.1.2");
            need = semver::Version::new(0, 1, 2);
        }
        let channels = self.shots.iter().flat_map(|s| s.channels.iter().flatten());
        if channels.clone().any(|c| c.text().is_some()) {
            features.push("text ([fonts] and [texts]), which needs shotglass 0.1.3");
            need = semver::Version::new(0, 1, 3);
        }
        if self
            .shots
            .iter()
            .any(|s| s.channels[4..].iter().any(Option::is_some))
        {
            features.push("iChannel4..7, which need shotglass 0.1.3");
            need = semver::Version::new(0, 1, 3);
        }
        if !self.layers.is_empty() {
            features.push("layers, which need shotglass 0.1.4");
            need = semver::Version::new(0, 1, 4);
        }
        let phase2 = self.layers.iter().any(|l| {
            l.blend.is_some()
                || l.channels
                    .iter()
                    .flatten()
                    .any(|c| matches!(c.source, Source::Picture | Source::Layer { .. }))
        });
        if phase2 {
            features.push("blend modes, @picture or @<layer>, which need shotglass 0.1.5");
            need = semver::Version::new(0, 1, 5);
        }
        if self.channels().any(|c| c.sequence().is_some()) {
            features.push("image sequences, which need shotglass 0.1.6");
            need = semver::Version::new(0, 1, 6);
        }
        if !self.comps.is_empty() || self.layers.iter().any(|l| l.matte.is_some()) {
            features.push("comps or mattes, which need shotglass 0.1.7");
            need = semver::Version::new(0, 1, 7);
        }
        if self
            .channels()
            .any(|c| c.text().is_some_and(|t| t.glyphs != GlyphMode::Block))
        {
            features.push("glyph runs, glyph sets or subtitles, which need shotglass 0.1.8");
            need = semver::Version::new(0, 1, 8);
        }
        if !self.buffers.is_empty() {
            features.push("feedback buffers ([[buffer]]), which need shotglass 0.1.9");
            need = semver::Version::new(0, 1, 9);
        }
        if self.comps.iter().any(|c| !c.movie.buffers.is_empty()) {
            features.push("buffers in comps, which need shotglass 0.1.10");
            need = semver::Version::new(0, 1, 10);
        }
        if self.warm || self.comps.iter().any(|c| c.movie.warm) {
            features.push("loop = \"warm\", which needs shotglass 0.1.10");
            need = semver::Version::new(0, 1, 10);
        }
        if features.is_empty() {
            return None;
        }
        // Any version below `need` that the film's range allows is one that would fail.
        let older = [
            semver::Version::new(0, 1, 0),
            semver::Version::new(0, 1, 1),
            semver::Version::new(0, 1, 2),
            semver::Version::new(0, 1, 3),
            semver::Version::new(0, 1, 4),
            semver::Version::new(0, 1, 5),
            semver::Version::new(0, 1, 6),
            semver::Version::new(0, 1, 7),
            semver::Version::new(0, 1, 8),
            semver::Version::new(0, 1, 9),
        ];
        let allows_old = match &self.shotglass {
            None => true,
            Some(r) => {
                library::parse_req(r).is_ok_and(|r| older.iter().any(|v| *v < need && r.matches(v)))
            }
        };
        allows_old.then(|| {
            format!(
                "this film uses {}; write shotglass = \"{need}\" in shotglass.toml so older \
                 shotglass versions ask for an upgrade instead of failing on an unknown field{}",
                features.join(" and "),
                self.shotglass
                    .as_ref()
                    .map_or(String::new(), |v| format!(" (it says \"{v}\")"))
            )
        })
    }

    /// Find every sequence's frames (once per pattern): a missing sequence or a gap
    /// refuses the version, like a missing `#include`; so does an `in` past the end.
    pub fn scan_footage(&mut self) -> Result<()> {
        for c in &mut self.comps {
            c.movie
                .scan_footage()
                .with_context(|| format!("comp \"{}\" ({})", c.name, c.rel))?;
        }
        let film = (self.fps_num, self.fps_den);
        // Each pattern (and `first`) is scanned once, whichever channels use it.
        type Found = ((PathBuf, Option<u64>), std::sync::Arc<crate::footage::Scan>);
        let mut found: Vec<Found> = vec![];
        let items = self
            .shots
            .iter_mut()
            .map(|s| (format!("shot \"{}\"", s.id), &mut s.channels))
            .chain(
                self.layers
                    .iter_mut()
                    .map(|l| (format!("layer \"{}\"", l.id), &mut l.channels)),
            )
            .chain(
                self.buffers
                    .iter_mut()
                    .map(|b| (format!("buffer \"{}\"", b.id), &mut b.channels)),
            );
        for (who, channels) in items {
            for (n, c) in channels.iter_mut().enumerate() {
                let Some(c) = c else { continue };
                let timing = c.timing;
                let Source::Frames(seq) = &mut c.source else {
                    continue;
                };
                let key = (seq.pattern.file.clone(), seq.first);
                let scan = match found.iter().find(|(k, _)| *k == key) {
                    Some((_, s)) => s.clone(),
                    None => {
                        let file = &seq.pattern.file;
                        let name = file.file_name().map(|n| n.to_string_lossy().into_owned());
                        let dir = file.parent().unwrap_or(Path::new("."));
                        let s = crate::footage::Pattern::parse(name.as_deref().unwrap_or(""))
                            .and_then(|p| {
                                crate::footage::scan(dir, &seq.pattern.label, &p, seq.first)
                            })
                            .map_err(|e| anyhow!("{who}: channels[{n}]: {e}"))?;
                        let s = std::sync::Arc::new(s);
                        found.push((key, s.clone()));
                        s
                    }
                };
                let rate = seq.fps.unwrap_or(film);
                let start = match timing.start {
                    In::Seconds(s) => source_snap(s * rate.0 as f64 / rate.1 as f64).max(0) as u64,
                    In::Frame(k) => k,
                };
                if start >= scan.count && timing.end != End::Loop {
                    let what = match timing.start {
                        In::Seconds(s) => format!("in = {s}"),
                        In::Frame(k) => format!("in_frame = {k}"),
                    };
                    bail!(
                        "{who}: channels[{n}]: {what} is past the end of {} ({:.2} s, {} frames)",
                        seq.pattern.label,
                        scan.count as f64 * rate.1 as f64 / rate.0 as f64,
                        scan.count
                    );
                }
                seq.scan = Some(scan);
            }
        }
        Ok(())
    }

    /// Every texture file exists (a missing one refuses the version, like a missing
    /// `#include`), and no shot needs more texture memory than the budget.
    fn check_textures(&self) -> Result<()> {
        for c in &self.comps {
            c.movie
                .check_textures()
                .with_context(|| format!("comp \"{}\" ({})", c.name, c.rel))?;
        }
        let items = self
            .shots
            .iter()
            .map(|s| (format!("shot \"{}\"", s.id), &s.channels))
            .chain(
                self.layers
                    .iter()
                    .map(|l| (format!("layer \"{}\"", l.id), &l.channels)),
            )
            .chain(
                self.buffers
                    .iter()
                    .map(|b| (format!("buffer \"{}\"", b.id), &b.channels)),
            );
        for (who, channels) in items {
            for (n, c) in channels.iter().enumerate() {
                let Some(c) = c else { continue };
                for f in c.files() {
                    if f.file.is_file() {
                        continue;
                    }
                    let base = match &f.library {
                        Some(lib) => self
                            .libraries
                            .iter()
                            .find(|l| &l.name == lib)
                            .map(|l| l.root.clone()),
                        None => Some(self.root.clone()),
                    };
                    let hint = base
                        .and_then(|b| crate::texture::did_you_mean(&b, &f.rel))
                        .map_or(String::new(), |m| match &f.library {
                            Some(lib) => format!(" (did you mean \"{lib}/{m}\"?)"),
                            None => format!(" (did you mean \"{m}\"?)"),
                        });
                    match c.text() {
                        None => bail!("{who}: channels[{n}]: \"{}\" not found{hint}", f.label),
                        Some(t) => bail!(
                            "{who}: channels[{n}]: text \"{}\": \"{}\" not found{hint}",
                            t.name,
                            f.label
                        ),
                    }
                }
            }
            // Texts lay out now (the fonts exist): missing glyphs, max_lines and block
            // size are edit mistakes that refuse the version, like a missing file.
            for c in channels.iter().flatten() {
                if let Some(t) = c.text() {
                    t.layout().with_context(|| who.clone())?;
                    if let Some(cues) = t.subtitles().with_context(|| who.clone())? {
                        let limit = (self.duration * 1000.0).round() as u64;
                        if let Some(e) = cues.iter().find(|e| e.end as u64 > limit) {
                            bail!(
                                "{who}: text \"{}\": the cue at {} ends at {}, after the film's \
                                 {}{}",
                                t.name,
                                crate::subtitles::stamp(e.start),
                                crate::subtitles::stamp(e.end),
                                crate::subtitles::stamp(limit as u32),
                                if self.loop_frames.is_some() {
                                    " (one loop period)"
                                } else {
                                    ""
                                }
                            );
                        }
                    }
                }
            }
            let (total, parts) = crate::texture::shot_memory(channels);
            if total > crate::texture::BUDGET {
                bail!(
                    "{who}: its textures need {}, more than the {} budget: {}",
                    crate::texture::describe_bytes(total),
                    crate::texture::describe_bytes(crate::texture::BUDGET),
                    parts.join(", ")
                );
            }
        }
        Ok(())
    }

    /// Read and validate `movie.toml` (and its project's `shotglass.toml`) without
    /// looking libraries up on disk.
    pub fn read(path: &Path) -> Result<Movie> {
        let file = if path.is_dir() {
            path.join(project::MOVIE_FILE)
        } else {
            path.to_path_buf()
        };
        let root = file.parent().map(Path::to_path_buf).unwrap_or_default();
        // The project first: its engine check speaks before movie.toml's unknown fields.
        let project = Project::for_film(&root)?;
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        Movie::parse_in(&text, &root, &project).with_context(|| format!("in {}", file.display()))
    }

    /// Find the project's libraries on disk: frozen copies, local overrides (`env`,
    /// `shotglass.local.toml`), project paths, or the cache as pinned by `shotglass.lock`.
    /// No network.
    pub fn resolve_libraries(
        &mut self,
        env: &dyn Fn(&str) -> Option<String>,
        cache: &Path,
    ) -> Result<()> {
        if self.library_specs.is_empty() {
            return Ok(());
        }
        let o = library::overrides(&self.project, &self.library_specs, env)?;
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
        let lock = library::Lock::load(&self.project)?;
        self.libraries =
            library::resolve(&self.project, &self.library_specs, o, lock.as_ref(), cache)?;
        let libs = self.libraries.clone();
        for c in &mut self.comps {
            c.movie.set_libraries(&libs);
        }
        self.set_libraries(&libs);
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

    /// Use these resolved libraries: library file references point into them.
    fn set_libraries(&mut self, libs: &[Library]) {
        self.libraries = libs.to_vec();
        let layer_channels = self
            .layers
            .iter_mut()
            .flat_map(|l| l.channels.iter_mut().flatten());
        for f in self
            .shots
            .iter_mut()
            .flat_map(|s| s.channels.iter_mut().flatten())
            .chain(layer_channels)
            .flat_map(Channel::files_mut)
        {
            if let Some(lib) = &f.library
                && let Some(l) = libs.iter().find(|l| &l.name == lib)
            {
                f.file = l.root.join(&f.rel);
            }
        }
    }

    /// `#define`s a comp's shaders get after the film's cues (#74): its duration and its
    /// shots' and layers' cues, in comp-local seconds.
    pub fn comp_cue_defines(&self) -> String {
        let mut s = format!("#define COMP_DURATION {}\n", glsl_float(self.duration));
        for shot in &self.shots {
            let id = shot.id.to_ascii_uppercase().replace('-', "_");
            s += &format!("#define COMP_CUE_{id}_START {}\n", glsl_float(shot.start));
            s += &format!(
                "#define COMP_CUE_{id}_SETTLED {}\n",
                glsl_float(shot.settled())
            );
            s += &format!("#define COMP_CUE_{id}_END {}\n", glsl_float(shot.end()));
        }
        for layer in &self.layers {
            let id = layer.id.to_ascii_uppercase().replace('-', "_");
            s += &format!("#define COMP_CUE_{id}_START {}\n", glsl_float(layer.start));
            s += &format!("#define COMP_CUE_{id}_END {}\n", glsl_float(layer.end));
        }
        s
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
            for f in s.channels.iter().flatten().flat_map(Channel::files) {
                if f.library.is_none() {
                    v.push(f.file.clone());
                }
            }
        }
        for l in &self.layers {
            match &l.scene {
                LayerScene::File(p) => v.push(p.clone()),
                LayerScene::Pass(k) => {
                    if let TransitionKind::File(p) = transition_kind(k) {
                        v.push(self.root.join(p));
                    }
                }
                LayerScene::Comp(_) => {}
            }
            for f in l.channels.iter().flatten().flat_map(Channel::files) {
                if f.library.is_none() {
                    v.push(f.file.clone());
                }
            }
        }
        for b in &self.buffers {
            v.push(b.scene.clone());
            v.extend(b.init.clone());
            for f in b.channels.iter().flatten().flat_map(Channel::files) {
                if f.library.is_none() {
                    v.push(f.file.clone());
                }
            }
        }
        // Comps (#74): their files (the watcher reloads on an edit) and what they read.
        for c in &self.comps {
            v.push(c.file.clone());
            v.extend(c.movie.referenced_files());
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

    /// Parse a film that is a project of its own with no `shotglass.toml` (no libraries).
    #[cfg(test)]
    pub fn parse(text: &str, root: &Path) -> Result<Movie> {
        Movie::parse_in(text, root, &Project::bare(root))
    }

    /// Parse `movie.toml`'s text for the film at `root` in `project`.
    pub fn parse_in(text: &str, root: &Path, project: &Project) -> Result<Movie> {
        // What moved to the project file gets a pointer rather than "unknown field".
        if let Ok(t) = toml::from_str::<toml::Table>(text) {
            for (key, what) in [
                ("shotglass", "the engine contract (shotglass = \"…\")"),
                ("libraries", "[libraries]"),
            ] {
                if t.contains_key(key) {
                    bail!(
                        "{what} goes in {}, the project file (next to movie.toml, or in a \
                         folder above it for libraries shared by several films), not in \
                         movie.toml",
                        project::PROJECT_FILE
                    );
                }
            }
        }
        let f: MovieFile = toml::from_str(text)?;
        let path_rule = |what: String, p: &str| -> Result<()> {
            library::check_rel_path(p).map_err(|e| anyhow!("{what}: {e}"))
        };
        let library_specs = project.specs.clone();
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
            match texture_spec(&what, v, None, &library_specs, root, None, &[], &[])? {
                Some(c) => textures.insert(name.clone(), c),
                None => bail!("{what}: \"\" names no file"),
            };
        }
        // [fonts], then [texts] (which name fonts); texts share the channel namespace.
        let mut fonts: BTreeMap<String, (FontRef, Vec<String>)> = BTreeMap::new();
        for (name, v) in &f.fonts {
            let what = format!("[fonts] {name}");
            if !is_ident(name) {
                bail!(
                    "{what}: a font name may only use letters, digits and _ (and not start with a digit)"
                );
            }
            fonts.insert(
                name.clone(),
                font_spec(&what, name, v, &library_specs, root)?,
            );
        }
        for (name, (_, fallback)) in &fonts {
            for fb in fallback {
                if !fonts.contains_key(fb) {
                    bail!(
                        "[fonts] {name}: fallback \"{fb}\" isn't in [fonts] (there are {})",
                        quoted_list(fonts.keys())
                    );
                }
            }
        }
        let mut names = textures.clone();
        for (name, v) in &f.texts {
            let what = format!("[texts] {name}");
            if !is_ident(name) {
                bail!(
                    "{what}: a text name may only use letters, digits and _ (and not start with a digit)"
                );
            }
            if textures.contains_key(name) {
                bail!(
                    "{what}: [textures] has a \"{name}\" too; textures and texts share names \
                     (channels name either)"
                );
            }
            names.insert(
                name.clone(),
                text_spec(&what, name, v, &fonts, &library_specs, root)?,
            );
        }
        let library_refs: std::collections::BTreeSet<String> = names
            .values()
            .flat_map(Channel::written)
            .chain(fonts.values().map(|(f, _)| &f.file))
            .filter_map(|f| f.library.clone())
            .collect();
        let textures = names;
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
        let buffer_decls = buffer_decls(&f.buffers)?;
        // [comps] (#74): names first; shots, layers and comps refer to them by index.
        let film_loop = loop_mode("", f.looping.as_ref())?;
        let decls = comp_decls(&f.comps)?;
        let comp_names: Vec<String> = decls.iter().map(|d| d.name.clone()).collect();
        let comp_rels: Vec<String> = decls.iter().map(|d| d.rel.clone()).collect();
        let mut used: std::collections::HashSet<String> = Default::default();
        let ctx = SeqCtx {
            root,
            specs: &library_specs,
            names: &textures,
            comps: &comp_names,
            comp_rels: &comp_rels,
            comp: None,
            fps: (fps_num, fps_den),
            buffers: &buffer_decls,
            aspect,
            size: None,
        };
        let (shots, layers, buffers, duration, loop_frames) = build_sequence(
            &ctx,
            &mut used,
            f.shots,
            f.layers,
            f.buffers,
            f.channels.as_deref(),
            film_loop.0,
        )?;
        if loop_frames.is_some() {
            let first = &shots[0];
            if first.sound.as_ref().is_some_and(|s| s.lead > 0.0) {
                bail!(
                    "shot \"{}\": sound_lead on the first shot of a loop film would cross the \
                     loop seam, which isn't supported (yet); start the sound inside the shot",
                    first.id
                );
            }
            let last = shots.last().unwrap();
            if last.sound.as_ref().is_some_and(|s| s.tail > 0.0) {
                bail!(
                    "shot \"{}\": sound_tail on the last shot of a loop film would cross the \
                     loop seam, which isn't supported (yet); let the sound end inside the shot \
                     (the mix's echo does ring over the seam)",
                    last.id
                );
            }
        }
        let mut comps = vec![];
        for d in decls {
            let what = format!("[comps] {}", d.name);
            let file = root.join(&d.rel);
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("{what}: reading {}", d.rel))?;
            let cf: CompFile = parse_comp_file(&text).with_context(|| format!("in {}", d.rel))?;
            let comp_decls = self::buffer_decls(&cf.buffers)
                .with_context(|| format!("comp \"{}\" ({})", d.name, d.rel))?;
            let ctx = SeqCtx {
                comp: Some(&d.name),
                buffers: &comp_decls,
                aspect: match d.size {
                    Some((w, h)) => w as f32 / h as f32,
                    None => aspect,
                },
                size: d.size,
                ..ctx
            };
            let (shots, layers, buffers, duration, loop_frames) = build_sequence(
                &ctx,
                &mut used,
                cf.shots,
                cf.layers,
                cf.buffers,
                cf.channels.as_deref(),
                d.looping,
            )
            .with_context(|| format!("comp \"{}\" ({})", d.name, d.rel))?;
            comps.push(Comp {
                movie: Box::new(Movie {
                    root: root.to_path_buf(),
                    project: project.root.clone(),
                    title: d.name.clone(),
                    aspect: match d.size {
                        Some((w, h)) => w as f32 / h as f32,
                        None => aspect,
                    },
                    fps: (fps_num as f64 / fps_den as f64) as f32,
                    fps_num,
                    fps_den,
                    glsl,
                    loop_frames,
                    warm: d.warm,
                    music: None,
                    mix: None,
                    mix_lookback: 1.0,
                    volume: 1.0,
                    shots,
                    layers,
                    buffers,
                    comps: vec![],
                    markers: vec![],
                    duration,
                    shotglass: project.shotglass.clone(),
                    library_specs: library_specs.clone(),
                    libraries: vec![],
                    warnings: vec![],
                    library_refs: Default::default(),
                }),
                name: d.name,
                file,
                rel: d.rel,
                size: d.size,
                looping: d.looping,
            });
        }
        let movie = Movie {
            root: root.to_path_buf(),
            title: f.title.unwrap_or_else(|| "Untitled".into()),
            aspect,
            fps: (fps_num as f64 / fps_den as f64) as f32,
            fps_num,
            fps_den,
            glsl,
            loop_frames,
            warm: film_loop.1,
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
            layers,
            buffers,
            comps,
            markers: f
                .markers
                .into_iter()
                .map(|m| Marker {
                    t: m.t,
                    label: m.label,
                })
                .collect(),
            duration,
            shotglass: project.shotglass.clone(),
            warnings: textures
                .keys()
                .filter(|n| !used.contains(*n))
                .map(|n| match textures[n].source {
                    Source::Text(_) => format!("[texts] {n} isn't used by any shot's channels"),
                    _ => {
                        format!("[textures] {n} isn't used by any shot's channels")
                    }
                })
                .collect(),
            library_specs,
            libraries: vec![],
            project: project.root.clone(),
            library_refs,
        };
        check_comp_graph(&movie)?;
        check_comp_buffers(&movie, &movie.comps)?;
        check_buffer_memory(&movie)?;
        let mut movie = movie;
        let cold: Vec<String> = std::iter::once(("the film".to_string(), &movie))
            .chain(
                movie
                    .comps
                    .iter()
                    .map(|c| (format!("comp \"{}\"", c.name), &*c.movie)),
            )
            .filter(|(_, m)| m.warm && warm_buffers(m).next().is_none())
            .map(|(w, _)| w)
            .collect();
        for w in cold {
            movie.warnings.push(format!(
                "{w}: loop = \"warm\" has no effect: no feedback buffer runs from frame 0 to the \
                 end of the period, so none carries state over the seam (use loop = true)"
            ));
        }
        Ok(movie)
    }

    /// Time of frame `n`: exactly `n * den / num`, rounded once to f32.
    pub fn frame_time(&self, n: u64) -> f32 {
        self.frame_time_precise(n) as f32
    }

    pub fn frame_time_precise(&self, n: u64) -> f64 {
        n as f64 * self.fps_den as f64 / self.fps_num as f64
    }

    /// Select a frame before rounding the live clock to a shader timestamp.
    pub fn frame_at_precise(&self, t: f64) -> u64 {
        let n = (t.max(0.0) * self.fps_num as f64 / self.fps_den as f64).round() as u64;
        if t < self.frame_time_precise(n) {
            n.saturating_sub(1)
        } else {
            n
        }
    }

    /// The frame on screen at `t` (the inverse of `frame_time`: `frame_at(frame_time(n)) == n`).
    pub fn frame_at(&self, t: f32) -> i64 {
        let f = t.max(0.0) as f64 * self.fps_num as f64 / self.fps_den as f64;
        // Compare against the same rounded boundary used by frame_time.
        let n = f.round() as u64;
        // Wrapping an already rounded timestamp can land just below a boundary.
        let rounded_loop_boundary = self.loop_frames.is_some() && (f - n as f64).abs() < 0.001;
        let index = if t < self.frame_time(n) && !rounded_loop_boundary {
            n.saturating_sub(1)
        } else {
            n
        };
        index.min(i64::MAX as u64) as i64
    }

    /// The footage mapping of channel `c` of a shot or layer (None if it isn't a
    /// scanned sequence).
    pub fn frame_map(&self, item: ItemRef, c: usize) -> Option<FrameMap> {
        let origin = match item {
            ItemRef::Shot(i) => self.shots[i].start,
            ItemRef::Layer(j) => self.layers[j].start,
            ItemRef::Buffer(b) => self.buffers[b].start,
        };
        let ch = self.item_channels(item)[c].as_ref()?;
        let seq = ch.sequence()?;
        Some(FrameMap {
            film: (self.fps_num, self.fps_den),
            loop_frames: self.loop_frames,
            origin,
            timing: ch.timing,
            src: seq.fps.unwrap_or((self.fps_num, self.fps_den)),
            count: seq.scan.as_ref()?.count,
        })
    }

    /// The channels of a shot, layer or buffer.
    pub fn item_channels(&self, item: ItemRef) -> &[Option<Channel>; N_CHANNELS] {
        match item {
            ItemRef::Shot(i) => &self.shots[i].channels,
            ItemRef::Layer(j) => &self.layers[j].channels,
            ItemRef::Buffer(b) => &self.buffers[b].channels,
        }
    }

    /// The source frame channel `c` of a shot or layer shows on film frame `n` (None:
    /// black, or not footage). See `FrameMap::source_frame`.
    pub fn source_frame(&self, item: ItemRef, c: usize, n: u64) -> Option<u64> {
        self.frame_map(item, c)?.source_frame(n)
    }

    /// Frames in the film: every n with `frame_time(n) < duration` (one period of a
    /// loop film).
    pub fn frame_count(&self) -> u64 {
        if let Some(n) = self.loop_frames {
            return n;
        }
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

    /// The exact loop period in seconds (`loop_frames` frames), for loop films.
    pub fn loop_period(&self) -> Option<f64> {
        self.loop_frames
            .map(|n| n as f64 * self.fps_den as f64 / self.fps_num as f64)
    }

    /// `t` within one period for a loop film (so t + k·period is t); unchanged otherwise.
    pub fn wrap(&self, t: f32) -> f32 {
        match self.loop_period() {
            Some(p) => wrap_time(t as f64, p) as f32,
            None => t,
        }
    }

    /// Frame `n` within one period for a loop film; unchanged otherwise.
    pub fn wrap_frame(&self, n: u64) -> u64 {
        match self.loop_frames {
            Some(f) => n % f,
            None => n,
        }
    }

    /// Samples in the soundtrack at `rate`: `ceil(duration × rate)`, or for a loop film
    /// exactly one period, rounded to whole samples (the player wraps its clock at this
    /// many samples, so picture and sound can't drift apart over many loops).
    pub fn sound_samples(&self, rate: u32) -> usize {
        match self.loop_period() {
            Some(p) => ((p * rate as f64).round() as usize).max(1),
            None => (self.duration * rate as f32).ceil() as usize,
        }
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
        for layer in &self.layers {
            let id = layer.id.to_ascii_uppercase().replace('-', "_");
            s += &format!("#define CUE_{id}_START {}\n", glsl_float(layer.start));
            s += &format!("#define CUE_{id}_END {}\n", glsl_float(layer.end));
        }
        s
    }

    /// The layers active at `t` (half-open spans `[start, end)`), bottom to top, with
    /// their layer-local times.
    pub fn layers_at(&self, t: f32) -> Vec<(usize, f32)> {
        self.layers
            .iter()
            .enumerate()
            .filter(|(_, l)| t >= l.start && t < l.end)
            .map(|(j, l)| (j, t - l.start))
            .collect()
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

/// What a sequence's shots and layers are parsed against (#74): the film's named
/// channels, libraries and comps; `comp` names the comp being parsed (None: the film).
#[derive(Clone, Copy)]
struct SeqCtx<'a> {
    root: &'a Path,
    specs: &'a [LibrarySpec],
    names: &'a BTreeMap<String, Channel>,
    comps: &'a [String],
    /// Each comp's file as written, by index (for comp shots' `scene`).
    comp_rels: &'a [String],
    comp: Option<&'a str>,
    fps: (u32, u32),
    /// The sequence's buffers (#134; a comp's, #138), by index.
    buffers: &'a [BufDecl],
    /// The output aspect (the memory check sizes `scale` buffers with it).
    aspect: f32,
    /// A comp's own `size` (#138): its `scale` buffers are that fraction of it.
    size: Option<(u32, u32)>,
}

impl SeqCtx<'_> {
    /// The index of comp `name`, or an error listing them.
    fn comp_ref(&self, what: &str, name: &str) -> Result<usize> {
        if self.comp == Some(name) {
            bail!("{what}: comp \"{name}\" can't show itself");
        }
        self.comps.iter().position(|c| c == name).ok_or_else(|| {
            anyhow!(
                "{what}: comp \"{name}\": no [comps] entry called that ({})",
                if self.comps.is_empty() {
                    "there are none".to_string()
                } else {
                    format!("there are {}", quoted_list(self.comps.iter()))
                }
            )
        })
    }

    /// A comp shot's `scene`: the comp's file as written.
    fn comp_rel(&self, comp: Option<usize>) -> &str {
        comp.and_then(|c| self.comp_rels.get(c))
            .map_or("", String::as_str)
    }
}

/// Shots, then layers (bottom to top), of the film or a comp: (shots, layers,
/// duration, loop frames).
type SeqParts = (Vec<Shot>, Vec<Layer>, Vec<Buffer>, f32, Option<u64>);

/// Builds a [`SeqParts`].
fn build_sequence(
    sc: &SeqCtx,
    used: &mut std::collections::HashSet<String>,
    shot_files: Vec<ShotFile>,
    layer_files: Vec<LayerFile>,
    buffer_files: Vec<BufferFile>,
    channels: Option<&[toml::Value]>,
    looping: bool,
) -> Result<SeqParts> {
    let path_rule = |what: String, p: &str| -> Result<()> {
        library::check_rel_path(p).map_err(|e| anyhow!("{what}: {e}"))
    };
    if shot_files.is_empty() {
        match sc.comp {
            None => bail!("a movie needs at least one [[shot]]"),
            Some(_) => bail!("a comp needs at least one [[shot]]"),
        }
    }
    let mut channel_list = |what: &str,
                            list: &[toml::Value],
                            on_layer: Option<LayerRefs>|
     -> Result<[Option<Channel>; N_CHANNELS]> {
        if list.len() > N_CHANNELS {
            bail!(
                "{what}channels has {} entries; there are {N_CHANNELS} (iChannel0..{})",
                list.len(),
                N_CHANNELS - 1
            );
        }
        let mut out: [Option<Channel>; N_CHANNELS] = Default::default();
        for (n, v) in list.iter().enumerate() {
            let ctx = format!("{what}channels[{n}]");
            if let toml::Value::String(s) = v
                && let ChannelSource::Name(name) = ChannelSource::of(s)
            {
                used.insert(name.to_string());
            }
            for key in ["texture", "text"] {
                if let Some(name) = v.get(key).and_then(toml::Value::as_str) {
                    used.insert(name.to_string());
                }
            }
            out[n] = texture_spec(
                &ctx,
                v,
                Some(sc.names),
                sc.specs,
                sc.root,
                on_layer,
                sc.comps,
                sc.buffers,
            )?;
        }
        Ok(out)
    };
    let default_channels = match channels {
        Some(list) => channel_list("", list, None)?,
        None => Default::default(),
    };
    let mut shots: Vec<Shot> = Vec::new();
    let mut ids = std::collections::HashSet::new();
    for (index, s) in shot_files.into_iter().enumerate() {
        let ctx = || format!("shot \"{}\"", s.id);
        let comp = match (&s.scene, &s.comp) {
            (Some(_), Some(_)) => bail!("{}: give scene or comp, not both", ctx()),
            (None, None) => bail!(
                "{}: a shot needs scene = \"…\" (a film file) or comp = \"…\" (a name \
                     from [comps])",
                ctx()
            ),
            (Some(p), None) => {
                path_rule(format!("{}: scene", ctx()), p)?;
                None
            }
            (None, Some(name)) => Some(sc.comp_ref(&ctx(), name)?),
        };
        if comp.is_some() && (s.channels.is_some() || !s.uniforms.is_empty()) {
            bail!(
                "{}: a comp shot has no shader of its own, so no channels or uniforms (the \
                     comp's shots and layers have theirs)",
                ctx()
            );
        }
        if sc.comp.is_some()
            && (s.sound.is_some()
                || s.sound_lead.is_some()
                || s.sound_tail.is_some()
                || s.sound_gain.is_some())
        {
            bail!(
                "{}: comps are picture only; put the sound on the film's shot that shows \
                     the comp",
                ctx()
            );
        }
        let channels = match &s.channels {
            Some(list) => channel_list(&format!("{}: ", ctx()), list, None)?,
            None if comp.is_some() => Default::default(),
            None => default_channels.clone(),
        };
        if !ids.insert(s.id.clone()) {
            bail!("duplicate shot id \"{}\"", s.id);
        }
        if !is_pass_id(&s.id) {
            bail!("shot id \"{}\" may only use letters, digits, _ and -", s.id);
        }
        if let Some(p) = &s.sound {
            path_rule(format!("{}: sound", ctx()), p)?;
        }
        if let Some(t) = &s.transition {
            match transition_kind(&t.kind) {
                TransitionKind::File(p) => path_rule(format!("{}: transition", ctx()), p)?,
                TransitionKind::Library(lib, file) => {
                    if !sc.specs.iter().any(|l| l.name == lib) {
                        bail!(
                            "{}: transition \"{}\" names library \"{lib}\", but [libraries] \
                                 has {}",
                            ctx(),
                            t.kind,
                            if sc.specs.is_empty() {
                                "none".to_string()
                            } else {
                                sc.specs
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
                    bail!("{}: {}", ctx(), bare_transition_error(name, sc.specs))
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
            path: sc.root.join(p),
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
            scene: match &s.scene {
                Some(p) => sc.root.join(p),
                None => sc.root.join(sc.comp_rel(comp)),
            },
            comp,
            time_offset: s.time_offset.unwrap_or(0.0),
            time_scale: s.time_scale.unwrap_or(1.0),
            uniforms,
            transition_in,
        });
    }
    let duration = shots.last().unwrap().end();
    let loop_frames = match looping {
        true => Some(loop_frames(duration, sc.fps.0, sc.fps.1)?),
        false => None,
    };
    // Layers, bottom to top. Ids share the shot namespace (fallback keys, cue names).
    let mut layers: Vec<Layer> = vec![];
    let layer_ids: Vec<String> = layer_files.iter().map(|l| l.id.clone()).collect();
    // `type` and `blend` name a pass the way a transition's `type` does.
    let pass_kind = |what: &str, key: &str, k: &str, example: &str| -> Result<()> {
        match transition_kind(k) {
            TransitionKind::File(p) => path_rule(format!("{what}: {key}"), p),
            TransitionKind::Library(lib, file) => {
                if !sc.specs.iter().any(|s| s.name == lib) {
                    bail!(
                        "{what}: {key} \"{k}\" names library \"{lib}\", but [libraries] has {}",
                        quoted_list(sc.specs.iter().map(|s| &s.name))
                    );
                }
                path_rule(format!("{what}: {key}"), &file)
            }
            TransitionKind::Name(n) => bail!(
                "{what}: {key} \"{n}\" needs a library (\"{example}/{n}\") or a film file \
                     (\"fx/{n}.glsl\")"
            ),
        }
    };
    for (index, l) in layer_files.into_iter().enumerate() {
        let what = format!("layer \"{}\"", l.id);
        if !is_pass_id(&l.id) {
            bail!(
                "layer id \"{}\" may only use letters, digits, _ and -",
                l.id
            );
        }
        if shots.iter().any(|s| s.id == l.id) || layers.iter().any(|o| o.id == l.id) {
            bail!(
                "{what}: the id is taken; shots and layers share ids (they name cues and \
                     fallbacks)"
            );
        }
        if sc.comps.contains(&l.id) {
            bail!(
                "{what}: [comps] has a \"{}\" too; layers and comps share @ names",
                l.id
            );
        }
        if l.transition_in.is_some() || l.transition_out.is_some() {
            bail!("{what}: layer transitions arrive in a later phase; use fade = [in, out]");
        }
        let blend = match l.blend.as_deref() {
            None | Some("over") => None,
            Some(b) => {
                pass_kind(&what, "blend", b, "blend")?;
                Some(b.to_string())
            }
        };
        let scene = match (&l.scene, &l.kind, &l.comp) {
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
                bail!("{what}: give one of scene, type and comp")
            }
            (None, None, None) => bail!(
                "{what}: a layer needs scene = \"…\" (a film file), type = \"…\" (a \
                     library pass like \"fx/grain\") or comp = \"…\" (a name from [comps])"
            ),
            (None, None, Some(name)) => {
                if l.channels.is_some() || !l.uniforms.is_empty() || !l.params.is_empty() {
                    bail!(
                        "{what}: a comp layer has no shader of its own, so no channels, \
                             uniforms or params"
                    );
                }
                LayerScene::Comp(sc.comp_ref(&what, name)?)
            }
            (Some(p), None, None) => {
                path_rule(format!("{what}: scene"), p)?;
                LayerScene::File(sc.root.join(p))
            }
            (None, Some(k), None) => {
                pass_kind(&what, "type", k, "fx")?;
                LayerScene::Pass(k.clone())
            }
        };
        let start = match &l.from {
            None => 0.0,
            Some(v) => span_point(&format!("{what}: from"), v, true, &shots, &layers)?,
        };
        let end = match &l.to {
            None => duration,
            Some(v) => span_point(&format!("{what}: to"), v, false, &shots, &layers)?,
        };
        if !(0.0..duration).contains(&start) || !(start < end && end <= duration + 1e-4) {
            bail!(
                "{what}: the span {}–{} s must lie within the film (0–{} s), from before to{}",
                glsl_float(start),
                glsl_float(end),
                glsl_float(duration),
                if loop_frames.is_some() {
                    "; a loop film's layers stay inside one period"
                } else {
                    ""
                }
            );
        }
        let end = end.min(duration);
        let (fade_in, fade_out) = match l.fade {
            None => (0.0, 0.0),
            Some([a, b]) => {
                if !(a >= 0.0 && b >= 0.0) || a + b > end - start + 1e-4 {
                    bail!(
                        "{what}: fade = [{a}, {b}] must be ≥ 0 and fit the span ({} s)",
                        glsl_float(end - start)
                    );
                }
                (a, b)
            }
        };
        let opacity = match &l.opacity {
            None => Anim::from_toml(&toml::Value::Float(1.0))?,
            Some(v) => {
                let a = Anim::from_toml(v).with_context(|| format!("{what}: opacity"))?;
                if a.dim() != 1 {
                    bail!("{what}: opacity is one number (or keyframes of one number)");
                }
                a
            }
        };
        let scale = match &l.scale {
            None => None,
            Some(toml::Value::String(s)) if s == "auto" => None,
            Some(v) => {
                let x = v
                    .as_float()
                    .or_else(|| v.as_integer().map(|i| i as f64))
                    .unwrap_or(-1.0) as f32;
                if !(x > 0.0 && x <= 1.0) {
                    bail!(
                        "{what}: scale must be \"auto\" or a fraction of the output size in (0, 1]"
                    );
                }
                Some(x)
            }
        };
        let mut uniforms = Vec::new();
        for (name, v) in &l.uniforms {
            if !is_ident(name) {
                bail!("{what}: \"{name}\" is not a valid GLSL name");
            }
            let a = Anim::from_toml(v).with_context(|| format!("{what}: uniform {name}"))?;
            uniforms.push((name.clone(), a));
        }
        let mut params = Vec::new();
        for (name, v) in &l.params {
            if !is_ident(name) {
                bail!("{what}: param \"{name}\" is not a valid GLSL name");
            }
            params.push((
                name.clone(),
                vector(v).with_context(|| format!("{what}: param {name}"))?,
            ));
        }
        let channels = match &l.channels {
            Some(list) => channel_list(
                &format!("{what}: "),
                list,
                Some(LayerRefs {
                    earlier: &layer_ids[..index],
                    later: &layer_ids[index..],
                    on_buffer: None,
                }),
            )?,
            None => Default::default(),
        };
        let matte = match &l.matte {
            None => None,
            Some(v) => Some(matte_spec(&what, v, &layer_ids[..index], sc)?),
        };
        layers.push(Layer {
            id: l.id,
            start,
            end,
            scene,
            uniforms,
            params,
            channels,
            opacity,
            fade: (fade_in, fade_out),
            scale,
            time_offset: l.time_offset.unwrap_or(0.0),
            time_scale: l.time_scale.unwrap_or(1.0),
            blend,
            matte,
        });
    }
    let buffers = build_buffers(
        sc,
        &mut channel_list,
        buffer_files,
        &shots,
        &layers,
        duration,
        loop_frames,
    )?;
    Ok((shots, layers, buffers, duration, loop_frames))
}

/// The film's `[[buffer]]`s (#134), after its shots and layers (spans and resets may
/// name either).
#[allow(clippy::type_complexity)]
fn build_buffers(
    sc: &SeqCtx,
    channel_list: &mut dyn FnMut(
        &str,
        &[toml::Value],
        Option<LayerRefs>,
    ) -> Result<[Option<Channel>; N_CHANNELS]>,
    files: Vec<BufferFile>,
    shots: &[Shot],
    layers: &[Layer],
    duration: f32,
    loop_frames: Option<u64>,
) -> Result<Vec<Buffer>> {
    let (num, den) = (sc.fps.0 as f64, sc.fps.1 as f64);
    // The first film frame at or after `t` (frame m shows at m·den/num).
    let frame_ceil = |t: f32| -> u64 { ((t as f64 * num / den) - 1e-6).ceil().max(0.0) as u64 };
    let n_frames = loop_frames.unwrap_or_else(|| frame_ceil(duration));
    let mut out: Vec<Buffer> = vec![];
    for (i, b) in files.into_iter().enumerate() {
        let what = format!("buffer \"{}\"", b.id);
        if !is_pass_id(&b.id) || b.id == "prev" || b.id == "input" || b.id == "picture" {
            bail!(
                "buffer id \"{}\" may only use letters, digits, _ and - (and isn't prev, \
                 input or picture)",
                b.id
            );
        }
        if shots.iter().any(|s| s.id == b.id)
            || layers.iter().any(|l| l.id == b.id)
            || out.iter().any(|o| o.id == b.id)
            || sc.comps.contains(&b.id)
        {
            bail!(
                "{what}: the id is taken; shots, layers, buffers and comps share ids (they \
                 name cues, fallbacks and @ sources)"
            );
        }
        library::check_rel_path(&b.scene).map_err(|e| anyhow!("{what}: scene: {e}"))?;
        if let Some(p) = &b.init {
            library::check_rel_path(p).map_err(|e| anyhow!("{what}: init: {e}"))?;
        }
        let start = match &b.from {
            None => 0.0,
            Some(v) => span_point(&format!("{what}: from"), v, true, shots, layers)?,
        };
        let end = match &b.to {
            None => duration,
            Some(v) => span_point(&format!("{what}: to"), v, false, shots, layers)?,
        };
        if !(0.0..duration).contains(&start) || !(start < end && end <= duration + 1e-4) {
            bail!(
                "{what}: the span {}–{} s must lie within the film (0–{} s), from before to",
                glsl_float(start),
                glsl_float(end),
                glsl_float(duration)
            );
        }
        let end = end.min(duration);
        let start_frame = frame_ceil(start);
        let end_frame = frame_ceil(end).min(n_frames);
        if start_frame >= end_frame {
            bail!("{what}: the span covers no film frame");
        }
        let size = match (&b.size, &b.scale) {
            (Some(_), Some(_)) => bail!("{what}: give size or scale, not both"),
            (Some([w, h]), None) => {
                if !(1..=8192).contains(w) || !(1..=8192).contains(h) {
                    bail!("{what}: size = [{w}, {h}] must be 1–8192 pixels each way");
                }
                BufferSize::Fixed(*w, *h)
            }
            (None, Some(v)) => {
                if v.as_str() == Some("auto") {
                    bail!(
                        "{what}: scale can't be \"auto\": a buffer's state would then change \
                         with the player's render scale; give a fraction of the output size \
                         or size = [w, h]"
                    );
                }
                let x = v
                    .as_float()
                    .or_else(|| v.as_integer().map(|i| i as f64))
                    .unwrap_or(-1.0) as f32;
                if !(x > 0.0 && x <= 1.0) {
                    bail!("{what}: scale must be a fraction of the output size in (0, 1]");
                }
                BufferSize::Scale(x)
            }
            (None, None) => BufferSize::Scale(1.0),
        };
        let decl = &sc.buffers[i];
        let point = |v: &toml::Value| -> Result<u64> {
            let t = span_point(&format!("{what}: reset"), v, true, shots, layers)?;
            let f = frame_ceil(t);
            if !(start_frame..end_frame).contains(&f) {
                bail!(
                    "{what}: reset at {} s is outside the buffer's span ({}–{} s)",
                    glsl_float(t),
                    glsl_float(start),
                    glsl_float(end)
                );
            }
            Ok(f)
        };
        let mut resets = vec![start_frame];
        match &b.reset {
            None => {}
            Some(toml::Value::String(s)) if s == "cuts" => {
                resets.extend(
                    shots
                        .iter()
                        .map(|s| frame_ceil(s.start))
                        .filter(|f| (start_frame..end_frame).contains(f)),
                );
            }
            Some(toml::Value::Array(list)) => {
                for v in list {
                    resets.push(point(v)?);
                }
            }
            Some(_) => bail!(
                "{what}: reset is \"cuts\" or a list of shot or layer ids, \"<id> ± s\" points \
                 and seconds"
            ),
        }
        resets.sort_unstable();
        resets.dedup();
        let mut uniforms = Vec::new();
        for (name, v) in &b.uniforms {
            if !is_ident(name) {
                bail!("{what}: \"{name}\" is not a valid GLSL name");
            }
            let a = Anim::from_toml(v).with_context(|| format!("{what}: uniform {name}"))?;
            uniforms.push((name.clone(), a));
        }
        let mut params = Vec::new();
        for (name, v) in &b.params {
            if !is_ident(name) {
                bail!("{what}: param \"{name}\" is not a valid GLSL name");
            }
            params.push((
                name.clone(),
                vector(v).with_context(|| format!("{what}: param {name}"))?,
            ));
        }
        let channels = match &b.channels {
            Some(list) => channel_list(
                &format!("{what}: "),
                list,
                Some(LayerRefs {
                    earlier: &[],
                    later: &[],
                    on_buffer: Some(i),
                }),
            )?,
            None => Default::default(),
        };
        let dense = match b.cache.as_deref() {
            None | Some("second") => false,
            Some("dense") => true,
            Some(c) => bail!("{what}: cache = \"{c}\": use \"dense\" (or leave it out)"),
        };
        let preview_lookback = match b.preview_lookback {
            None => None,
            Some(s) if s > 0.0 && s.is_finite() => {
                Some(((s * sc.fps.0 as f64 / sc.fps.1 as f64).ceil() as u64).max(1))
            }
            Some(s) => bail!("{what}: preview_lookback = {s}: seconds, more than 0"),
        };
        out.push(Buffer {
            id: b.id,
            scene: sc.root.join(&b.scene),
            init: b.init.as_ref().map(|p| sc.root.join(p)),
            start,
            end,
            start_frame,
            end_frame,
            resets,
            size,
            format: decl.format,
            history: decl.history,
            dense,
            preview_lookback,
            channels,
            uniforms,
            params,
        });
    }
    // Required memory: every ring active on the worst frame (the most buffers at once).
    let out_size = memory_check_out(sc.aspect, sc.size);
    for b in &out {
        let at = b.start_frame;
        let live: Vec<&Buffer> = out.iter().filter(|o| o.active(at)).collect();
        let total: usize = live.iter().map(|o| o.ring_bytes(out_size)).sum();
        if total > BUFFER_MEMORY_LIMIT {
            let list: Vec<String> = live
                .iter()
                .map(|o| {
                    let (w, h) = o.size.at(out_size);
                    format!(
                        "\"{}\" {w}×{h} {} × {} states = {} MB",
                        o.id,
                        BufferFormat::NAMES[o.format as usize],
                        o.history + 1,
                        o.ring_bytes(out_size) >> 20
                    )
                })
                .collect();
            bail!(
                "buffers need {} MB of GPU memory at once (limit {} MB; `scale` buffers counted \
                 at a {}-pixel-high output): {}; use a smaller size or scale, rgba8, or a \
                 shorter history",
                total >> 20,
                BUFFER_MEMORY_LIMIT >> 20,
                MEMORY_CHECK_HEIGHT,
                list.join(", ")
            );
        }
    }
    Ok(out)
}

/// The buffers of a warm loop sequence (#138) that carry state over its seam: active
/// from frame 0 to the end of the period.
pub fn warm_buffers(m: &Movie) -> impl Iterator<Item = &Buffer> {
    let p = m.loop_frames.filter(|_| m.warm);
    m.buffers
        .iter()
        .filter(move |b| p.is_some_and(|p| b.start_frame == 0 && b.end_frame >= p))
}

/// The output `scale` buffers are sized against in the memory checks: a comp's own
/// `size` (#138), else a 1080-pixel-high output of the aspect.
fn memory_check_out(aspect: f32, size: Option<(u32, u32)>) -> (i32, i32) {
    match size {
        Some((w, h)) => (w as i32, h as i32),
        None => (
            (MEMORY_CHECK_HEIGHT * aspect).round() as i32,
            MEMORY_CHECK_HEIGHT as i32,
        ),
    }
}

/// Required buffer memory across the film and its comps (#138): each sequence's worst
/// frame, summed (comps can be active at any film frame). Only comps add anything here:
/// `build_buffers` already checked each sequence on its own.
fn check_buffer_memory(film: &Movie) -> Result<()> {
    let worst = |m: &Movie, size: Option<(u32, u32)>| -> usize {
        let out = memory_check_out(m.aspect, size);
        m.buffers
            .iter()
            .map(|b| {
                m.buffers
                    .iter()
                    .filter(|o| o.active(b.start_frame))
                    .map(|o| o.ring_bytes(out))
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0)
    };
    if film.comps.iter().all(|c| c.movie.buffers.is_empty()) {
        return Ok(());
    }
    let parts: Vec<(String, usize)> = std::iter::once(("the film".to_string(), worst(film, None)))
        .chain(
            film.comps
                .iter()
                .map(|c| (format!("comp \"{}\"", c.name), worst(&c.movie, c.size))),
        )
        .filter(|(_, b)| *b > 0)
        .collect();
    let total: usize = parts.iter().map(|(_, b)| b).sum();
    if total > BUFFER_MEMORY_LIMIT {
        bail!(
            "buffers in the film and its comps need up to {} MB of GPU memory at once (limit \
             {} MB; `scale` buffers counted at a {}-pixel-high output): {}; use smaller sizes, \
             rgba8, or shorter histories",
            total >> 20,
            BUFFER_MEMORY_LIMIT >> 20,
            MEMORY_CHECK_HEIGHT,
            parts
                .iter()
                .map(|(w, b)| format!("{w} {} MB", b >> 20))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

/// `[[buffer]]` ids, formats and histories (#134), known before any channel is parsed.
fn buffer_decls(files: &[BufferFile]) -> Result<Vec<BufDecl>> {
    if files.len() > MAX_BUFFERS {
        bail!(
            "a film has at most {MAX_BUFFERS} buffers (this one has {})",
            files.len()
        );
    }
    files
        .iter()
        .map(|b| {
            let what = format!("buffer \"{}\"", b.id);
            let format = match b.format.as_deref() {
                None => BufferFormat::Rgba16f,
                Some(f) => BufferFormat::parse(f).ok_or_else(|| {
                    anyhow!(
                        "{what}: format \"{f}\" isn't one of {:?}",
                        BufferFormat::NAMES
                    )
                })?,
            };
            let history = match b.history {
                None => 1,
                Some(n) if (1..=MAX_HISTORY as i64).contains(&n) => n as u32,
                Some(n) => bail!(
                    "{what}: history = {n}: keep 1 to {MAX_HISTORY} states (memory limits it \
                     further)"
                ),
            };
            Ok(BufDecl {
                id: b.id.clone(),
                history,
                format,
            })
        })
        .collect()
}

/// `t` in [0, period): the f64 remainder, kept below the period after rounding.
pub fn wrap_time(t: f64, period: f64) -> f64 {
    let w = t.rem_euclid(period);
    if w >= period { 0.0 } else { w }
}

/// The frame count of a `loop = true` film: its duration must be a whole number of
/// frames, so the frame after the last is frame 0.
fn loop_frames(duration: f32, num: u32, den: u32) -> Result<u64> {
    let exact = duration as f64 * num as f64 / den as f64;
    let n = exact.round();
    if n >= 1.0 && (exact - n).abs() <= 1e-3 {
        return Ok(n as u64);
    }
    let secs = |f: f64| f * den as f64 / num as f64;
    let (lo, hi) = (exact.floor().max(1.0), exact.ceil().max(1.0));
    let fps = if den == 1 {
        num.to_string()
    } else {
        format!("{num}/{den}")
    };
    bail!(
        "loop = true needs a film that lasts a whole number of frames (so the frame after the \
         last is frame 0), but it lasts {duration}s, {exact:.3} frames at {fps} fps; make it \
         {:.4}s ({lo} frames) or {:.4}s ({hi} frames)",
        secs(lo),
        secs(hi)
    )
}

/// One end of a layer's span: movie seconds, or `"<id>[.start|.settled|.end]"` with an
/// optional `+ s` / `- s`. A bare shot id means its start for `from` and its end for
/// `to`; layers (earlier ones only) have `.start` and `.end`.
fn span_point(
    what: &str,
    v: &toml::Value,
    is_from: bool,
    shots: &[Shot],
    layers: &[Layer],
) -> Result<f32> {
    if let Some(x) = v.as_float().or_else(|| v.as_integer().map(|i| i as f64)) {
        return Ok(x as f32);
    }
    let Some(s) = v.as_str() else {
        bail!("{what}: a number of seconds or \"<shot>[.start|.settled|.end] ± s\"");
    };
    let (base, offset) = match s
        .find(['+', '-'])
        .filter(|&i| i > 0 && s[..i].ends_with(' '))
    {
        Some(i) => {
            let n: f32 = s[i + 1..].trim().parse().map_err(|_| {
                anyhow!(
                    "{what}: \"{s}\": the offset after {} must be seconds",
                    &s[i..=i]
                )
            })?;
            (s[..i].trim(), if &s[i..=i] == "-" { -n } else { n })
        }
        None => (s.trim(), 0.0),
    };
    let (id, part) = base.split_once('.').unwrap_or((base, ""));
    let t = if let Some(shot) = shots.iter().find(|x| x.id == id) {
        match part {
            "" if is_from => shot.start,
            "" => shot.end(),
            "start" => shot.start,
            "settled" => shot.settled(),
            "end" => shot.end(),
            p => bail!("{what}: \"{s}\": a shot has .start, .settled and .end, not .{p}"),
        }
    } else if let Some(layer) = layers.iter().find(|x| x.id == id) {
        match part {
            "" if is_from => layer.start,
            "" => layer.end,
            "start" => layer.start,
            "end" => layer.end,
            p => bail!("{what}: \"{s}\": a layer has .start and .end, not .{p}"),
        }
    } else {
        bail!(
            "{what}: \"{s}\": no shot or earlier layer called \"{id}\" (shots: {}{})",
            shots
                .iter()
                .map(|x| x.id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            if layers.is_empty() {
                String::new()
            } else {
                format!(
                    "; layers: {}",
                    layers
                        .iter()
                        .map(|x| x.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        );
    };
    Ok(t + offset)
}

fn quoted_list<'a>(names: impl Iterator<Item = &'a String>) -> String {
    let v: Vec<String> = names.map(|n| format!("\"{n}\"")).collect();
    if v.is_empty() {
        "none".to_string()
    } else {
        v.join(", ")
    }
}

/// Extensions read as fonts (any case).
const FONT_EXTENSIONS: &[&str] = &["ttf", "otf", "ttc"];

/// One `[fonts]` value: a path / `<lib/path>`, or `{ file, index, variations, fallback,
/// licence }`. Returns the font and its fallback names (checked by the caller).
fn font_spec(
    what: &str,
    name: &str,
    v: &toml::Value,
    specs: &[LibrarySpec],
    root: &Path,
) -> Result<(FontRef, Vec<String>)> {
    let path = |s: &str| -> Result<FileRef> {
        let f = file_ref(what, s, specs, root)?;
        let ext = f.rel.rsplit_once('.').map_or("", |(_, e)| e);
        if ext.eq_ignore_ascii_case("woff") || ext.eq_ignore_ascii_case("woff2") {
            bail!(
                "{what}: {}: WOFF/WOFF2 fonts aren't read; convert it to TTF or OTF",
                f.label
            );
        }
        if !FONT_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)) {
            bail!(
                "{what}: {}: shotglass reads TrueType and OpenType fonts (.ttf, .otf, .ttc)",
                f.label
            );
        }
        Ok(f)
    };
    let mut font = FontRef {
        name: name.to_string(),
        file: FileRef {
            file: PathBuf::new(),
            label: String::new(),
            library: None,
            rel: String::new(),
        },
        index: 0,
        variations: vec![],
        licence: None,
    };
    let mut fallback = vec![];
    match v {
        toml::Value::String(s) => font.file = path(s)?,
        toml::Value::Table(t) => {
            match t.get("file") {
                Some(toml::Value::String(s)) => font.file = path(s)?,
                Some(_) => bail!("{what}: file must be a string"),
                None => bail!("{what}: a font table needs file = \"…\""),
            }
            for (k, val) in t {
                match k.as_str() {
                    "file" => {}
                    "index" => {
                        font.index = val
                            .as_integer()
                            .and_then(|i| u32::try_from(i).ok())
                            .ok_or_else(|| anyhow!("{what}: index must be a whole number ≥ 0"))?
                    }
                    "variations" => {
                        let Some(axes) = val.as_table() else {
                            bail!("{what}: variations must be a table like {{ wght = 700 }}");
                        };
                        for (tag, value) in axes {
                            if tag.len() != 4 || !tag.is_ascii() {
                                bail!(
                                    "{what}: variations: \"{tag}\" isn't an axis tag (four letters, like wght)"
                                );
                            }
                            let value = value
                                .as_float()
                                .or_else(|| value.as_integer().map(|i| i as f64))
                                .ok_or_else(|| {
                                    anyhow!("{what}: variations: {tag} must be a number")
                                })?;
                            font.variations.push((tag.clone(), value as f32));
                        }
                    }
                    "fallback" => {
                        let list = val.as_array().ok_or_else(|| {
                            anyhow!("{what}: fallback must be a list of [fonts] names")
                        })?;
                        for f in list {
                            let Some(f) = f.as_str() else {
                                bail!("{what}: fallback must be a list of [fonts] names");
                            };
                            fallback.push(f.to_string());
                        }
                    }
                    "licence" | "license" => {
                        font.licence = Some(
                            val.as_str()
                                .ok_or_else(|| {
                                    anyhow!("{what}: licence must be an SPDX id like \"OFL-1.1\"")
                                })?
                                .to_string(),
                        )
                    }
                    other => bail!(
                        "{what}: unknown option `{other}` (a font has file, index, variations, fallback and licence)"
                    ),
                }
            }
        }
        _ => bail!("{what}: expected a path or a table {{ file = \"…\" }}"),
    }
    Ok((font, fallback))
}

/// The fonts a text uses: its font, then the fallback chain depth first, each once.
fn font_chain(start: &str, fonts: &BTreeMap<String, (FontRef, Vec<String>)>) -> Vec<FontRef> {
    let mut out: Vec<FontRef> = vec![];
    let mut stack = vec![start.to_string()];
    while let Some(n) = stack.pop() {
        if out.iter().any(|f| f.name == n) {
            continue;
        }
        let (font, fallback) = &fonts[&n];
        out.push(font.clone());
        stack.extend(fallback.iter().rev().cloned());
    }
    out
}

/// One `[texts]` value: `{ text | file | subtitles, font, size, width, align,
/// line_height, letter_spacing, max_lines, direction, features, sdf_spread, glyphs,
/// set, atlas_size, filter, wrap, vflip }`.
fn text_spec(
    what: &str,
    name: &str,
    v: &toml::Value,
    fonts: &BTreeMap<String, (FontRef, Vec<String>)>,
    specs: &[LibrarySpec],
    root: &Path,
) -> Result<Channel> {
    use crate::text::{Align, Direction, TextStyle};
    let Some(t) = v.as_table() else {
        bail!("{what}: a text is a table like {{ text = \"Hello\", font = \"body\", size = 96 }}");
    };
    let set = match t.get("set") {
        None => false,
        Some(toml::Value::Boolean(b)) => *b,
        Some(_) => bail!("{what}: set must be true or false"),
    };
    let glyphs = match (t.get("glyphs"), set) {
        (None | Some(toml::Value::Boolean(false)), false) => GlyphMode::Block,
        (Some(toml::Value::Boolean(true)), false) => GlyphMode::Run,
        (Some(toml::Value::String(_)), true) => GlyphMode::Set,
        (Some(toml::Value::String(_)), false) => bail!(
            "{what}: glyphs = \"…\" lists a glyph set's characters; add set = true (or use \
             glyphs = true for a glyph run of the text)"
        ),
        (_, true) => bail!("{what}: set = true needs the characters as glyphs = \"0123456789:.\""),
        (Some(_), false) => {
            bail!("{what}: glyphs must be true, false or (with set = true) a string")
        }
    };
    let sources: Vec<&str> = ["text", "file", "subtitles"]
        .into_iter()
        .filter(|k| t.contains_key(*k))
        .collect();
    let body = if glyphs == GlyphMode::Set {
        if let Some(k) = sources.first() {
            bail!(
                "{what}: a glyph set's characters are glyphs = \"…\"; {k} doesn't go with set = true"
            );
        }
        let chars = t["glyphs"].as_str().unwrap_or_default();
        if chars.is_empty() {
            bail!("{what}: glyphs = \"\" has no characters");
        }
        TextBody::Inline(chars.to_string())
    } else {
        if sources.len() > 1 {
            bail!(
                "{what}: give one of text, file or subtitles (this one has {})",
                sources.join(" and ")
            );
        }
        let path = |s: &str| file_ref(what, s, specs, root);
        match sources.first().map(|k| (*k, &t[*k])) {
            Some(("text", toml::Value::String(s))) => TextBody::Inline(s.clone()),
            Some(("file", toml::Value::String(s))) => TextBody::File(path(s)?),
            Some(("subtitles", toml::Value::String(s))) => {
                if crate::subtitles::Format::of(Path::new(s)).is_none() {
                    bail!("{what}: subtitles = \"{s}\" must be a .srt or .vtt file");
                }
                if glyphs != GlyphMode::Run {
                    bail!("{what}: subtitles are drawn as a glyph run; add glyphs = true");
                }
                TextBody::Subtitles(path(s)?)
            }
            Some((k, _)) => bail!("{what}: {k} must be a string"),
            None => bail!(
                "{what}: a text needs text = \"…\", file = \"…\" (UTF-8) or subtitles = \"…\" (.srt/.vtt)"
            ),
        }
    };
    let font = match t.get("font") {
        Some(toml::Value::String(f)) => f.clone(),
        Some(_) => bail!("{what}: font must be the name of a [fonts] entry"),
        None => bail!("{what}: a text needs font = \"…\" (a name from [fonts])"),
    };
    if !fonts.contains_key(&font) {
        bail!(
            "{what}: font \"{font}\" isn't in [fonts] (there are {})",
            quoted_list(fonts.keys())
        );
    }
    let mut style = TextStyle::default();
    let mut channel = Channel {
        source: Source::Image(FileRef {
            file: PathBuf::new(),
            label: String::new(),
            library: None,
            rel: String::new(),
        }),
        filter: Filter::Linear,
        wrap: Wrap::Clamp,
        vflip: true,
        srgb: false,
        timing: Timing::default(),
    };
    let number = |k: &str, val: &toml::Value| -> Result<f32> {
        val.as_float()
            .or_else(|| val.as_integer().map(|i| i as f64))
            .map(|x| x as f32)
            .filter(|x| x.is_finite())
            .ok_or_else(|| anyhow!("{what}: {k} must be a number"))
    };
    let word = |k: &str, val: &toml::Value, names: &[&str]| -> Result<String> {
        val.as_str().map(str::to_string).ok_or_else(|| {
            anyhow!(
                "{what}: {k} must be one of {}",
                names
                    .iter()
                    .map(|n| format!("\"{n}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
    };
    let one_of = |k: &str, s: &str, names: &[&str]| {
        anyhow!(
            "{what}: {k} \"{s}\" isn't one of {}",
            names
                .iter()
                .map(|n| format!("\"{n}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let mut atlas_size = ATLAS_SIZE;
    for (k, val) in t {
        match k.as_str() {
            "text" | "file" | "subtitles" | "font" | "glyphs" | "set" => {}
            "atlas_size" => {
                if glyphs == GlyphMode::Block {
                    bail!("{what}: atlas_size is for glyph runs (glyphs = true) and glyph sets");
                }
                atlas_size = number(k, val)?;
                if !ATLAS_SIZES.contains(&atlas_size) {
                    bail!("{what}: atlas_size must be 8–256 (atlas pixels per em)");
                }
            }
            "size" => {
                style.size = number(k, val)?;
                if !(4.0..=1024.0).contains(&style.size) {
                    bail!("{what}: size must be 4–1024 (texture pixels per em)");
                }
            }
            "width" => {
                let w = number(k, val)?;
                if !(1.0..=4096.0).contains(&w) {
                    bail!("{what}: width must be 1–4096 (pixels)");
                }
                style.width = Some(w);
            }
            "align" => {
                let s = word(k, val, Align::NAMES)?;
                style.align = Align::parse(&s).ok_or_else(|| one_of(k, &s, Align::NAMES))?;
            }
            "line_height" => {
                style.line_height = number(k, val)?;
                if !(0.1..=10.0).contains(&style.line_height) {
                    bail!("{what}: line_height must be 0.1–10 (ems)");
                }
            }
            "letter_spacing" => {
                style.letter_spacing = number(k, val)?;
                if !(-1.0..=10.0).contains(&style.letter_spacing) {
                    bail!("{what}: letter_spacing must be -1–10 (ems)");
                }
            }
            "max_lines" => {
                style.max_lines = Some(
                    val.as_integer()
                        .filter(|&n| n >= 1)
                        .ok_or_else(|| anyhow!("{what}: max_lines must be a whole number ≥ 1"))?
                        as usize,
                )
            }
            "direction" => {
                let s = word(k, val, Direction::NAMES)?;
                style.direction =
                    Direction::parse(&s).ok_or_else(|| one_of(k, &s, Direction::NAMES))?;
            }
            "features" => {
                let list = val.as_array().ok_or_else(|| {
                    anyhow!("{what}: features must be a list like [\"ss01\", \"-liga\"]")
                })?;
                for f in list {
                    let f = f
                        .as_str()
                        .ok_or_else(|| anyhow!("{what}: features must be a list of strings"))?;
                    if f.parse::<harfrust::Feature>().is_err() {
                        bail!(
                            "{what}: \"{f}\" isn't an OpenType feature (like \"ss01\", \"-liga\" or \"tnum\")"
                        );
                    }
                    style.features.push(f.to_string());
                }
            }
            "sdf_spread" => {
                style.spread = number(k, val)?;
                if !(1.0..=64.0).contains(&style.spread) {
                    bail!("{what}: sdf_spread must be 1–64 (pixels)");
                }
            }
            "filter" => {
                let s = word(k, val, Filter::NAMES)?;
                channel.filter = Filter::parse(&s).ok_or_else(|| one_of(k, &s, Filter::NAMES))?;
            }
            "wrap" => {
                let s = word(k, val, Wrap::NAMES)?;
                channel.wrap = Wrap::parse(&s).ok_or_else(|| one_of(k, &s, Wrap::NAMES))?;
            }
            "vflip" => {
                if glyphs != GlyphMode::Block {
                    bail!(
                        "{what}: vflip isn't for glyph runs: their atlas and table are read \
                         with texelFetch, top row first (the kit maps it)"
                    );
                }
                channel.vflip = val
                    .as_bool()
                    .ok_or_else(|| anyhow!("{what}: vflip must be true or false"))?
            }
            "srgb" => bail!("{what}: srgb is for images; a text's channels are data"),
            other => bail!(
                "{what}: unknown option `{other}` (a text has text, file or subtitles, font, \
                 size, width, align, line_height, letter_spacing, max_lines, direction, \
                 features, sdf_spread, glyphs, set, atlas_size, filter, wrap and vflip)"
            ),
        }
    }
    if glyphs != GlyphMode::Block {
        if channel.filter == Filter::Mipmap {
            bail!(
                "{what}: a glyph atlas isn't mipmapped (MSDF doesn't mip); use filter = \"linear\""
            );
        }
        channel.vflip = false;
        style.tall = true;
        if glyphs == GlyphMode::Set && (style.width.is_some() || style.max_lines.is_some()) {
            bail!("{what}: width and max_lines are for laid-out text, not a glyph set");
        }
    }
    channel.source = Source::Text(Box::new(TextSource {
        name: name.to_string(),
        body,
        fonts: font_chain(&font, fonts),
        style,
        glyphs,
        atlas_size,
        part: TextPart::Main,
    }));
    Ok(channel)
}

/// A film path or `<lib/path>`, checked against the path rules. Library paths stay
/// relative until `resolve_libraries`.
fn file_ref(what: &str, s: &str, specs: &[LibrarySpec], root: &Path) -> Result<FileRef> {
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
    Ok(FileRef {
        file: if library.is_some() {
            PathBuf::from(&rel)
        } else {
            root.join(&rel)
        },
        label,
        library,
        rel,
    })
}

/// A `channels` entry or `[textures]` value names a `[textures]` entry (not a path)?
fn is_texture_name(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('.')
        && !s.contains('/')
        && !s.starts_with('<')
        && !s.starts_with('@')
}

/// Resolve one `channels` entry (with `names`) or `[textures]` value (without):
/// "" (nothing), a `[textures]` name, a film path, `<lib/path>`, or a table with `file`
/// and options. Library paths stay relative until `resolve_libraries`.
#[allow(clippy::too_many_arguments)]
fn texture_spec(
    what: &str,
    v: &toml::Value,
    names: Option<&BTreeMap<String, Channel>>,
    specs: &[LibrarySpec],
    root: &Path,
    on_layer: Option<LayerRefs>,
    comps: &[String],
    buffers: &[BufDecl],
) -> Result<Option<Channel>> {
    // A buffer's state (#134): linear, clamped; rgba32f nearest (float filtering isn't
    // portable: WebGL2 needs OES_texture_float_linear).
    let buffer_channel = |j: usize, back: u32| Channel {
        source: Source::Buffer {
            index: j,
            id: buffers[j].id.clone(),
            back,
        },
        filter: if buffers[j].format == BufferFormat::Rgba32f {
            Filter::Nearest
        } else {
            Filter::Linear
        },
        wrap: Wrap::Clamp,
        vflip: false,
        srgb: false,
        timing: Timing::default(),
    };
    let on_buffer = on_layer.and_then(|r| r.on_buffer);
    let file_channel = |s: &str| -> Result<Channel> {
        let f = file_ref(what, s, specs, root)?;
        if !crate::texture::has_image_extension(&f.rel) {
            bail!(
                "{what}: {}: shotglass reads PNG and JPEG images (.png, .jpg, .jpeg)",
                f.label
            );
        }
        Ok(Channel {
            source: Source::Image(f),
            filter: Filter::Mipmap,
            wrap: Wrap::Repeat,
            vflip: true,
            srgb: false,
            timing: Timing::default(),
        })
    };
    // An image sequence: one run of # in the file name. Footage samples linearly by
    // default (no mipmaps to make for every frame).
    let sequence_channel = |s: &str| -> Result<Channel> {
        let path = s.trim_start_matches('<').trim_end_matches('>');
        let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
        if dir.contains('#') {
            bail!("{what}: \"{s}\": # marks frame numbers and may only appear in the file name");
        }
        if file.split(|c| c != '#').filter(|r| !r.is_empty()).count() > 1 {
            bail!("{what}: \"{s}\": an image sequence has one run of # (the frame number)");
        }
        let f = file_ref(what, s, specs, root)?;
        if !crate::texture::has_image_extension(&f.rel) {
            bail!(
                "{what}: {}: an image sequence's frames are PNG or JPEG images (.png, .jpg, .jpeg)",
                f.label
            );
        }
        Ok(Channel {
            source: Source::Frames(Box::new(Sequence {
                pattern: f,
                fps: None,
                first: None,
                scan: None,
            })),
            filter: Filter::Linear,
            wrap: Wrap::Repeat,
            vflip: true,
            srgb: false,
            timing: Timing::default(),
        })
    };
    // A file entry by its kind: images bind now, the rest name the version that brings them.
    let file_source = |s: &str| -> Result<Channel> {
        match ChannelSource::of(s) {
            ChannelSource::Sequence(_) => sequence_channel(s),
            ChannelSource::Video(_) => Err(not_yet(what, s, "video")),
            _ => {
                if let Some(i) = s.find('%')
                    && s[i + 1..]
                        .trim_start_matches(|c: char| c.is_ascii_digit())
                        .starts_with('d')
                {
                    let digits: usize = s[i + 1..]
                        .chars()
                        .take_while(char::is_ascii_digit)
                        .collect::<String>()
                        .parse()
                        .unwrap_or(1);
                    let end = i + 1 + s[i + 1..].find('d').unwrap_or(0) + 1;
                    bail!(
                        "{what}: \"{s}\": write the frame number as # (\"{}{}{}\")",
                        &s[..i],
                        "#".repeat(digits.max(1)),
                        &s[end..]
                    );
                }
                file_channel(s)
            }
        }
    };
    match v {
        toml::Value::String(s) => match ChannelSource::of(s) {
            ChannelSource::Empty => {
                if names.is_none() {
                    bail!("{what}: \"\" names no file");
                }
                Ok(None)
            }
            ChannelSource::Output(out) => {
                if names.is_none() {
                    bail!("{what}: \"{s}\" isn't a file; @ sources go in a shot's channels");
                }
                if !is_pass_id(out) {
                    bail!(
                        "{what}: \"{s}\": after @ comes input, picture, or the id of a layer or comp"
                    );
                }
                if out == "prev" {
                    let Some(i) = on_buffer else {
                        bail!(
                            "{what}: \"@prev\" is a buffer's own previous state; only a \
                             [[buffer]]'s channels can read it (layers read {{ source = \
                             \"@<buffer>\", frame = \"prev\" }})"
                        );
                    };
                    return Ok(Some(buffer_channel(i, 1)));
                }
                if let Some(j) = buffers.iter().position(|b| b.id == out) {
                    match on_buffer {
                        _ if on_layer.is_none() => bail!(
                            "{what}: \"{s}\" is a buffer; shots and transitions draw before \
                             buffers step, so only layers and later buffers read one"
                        ),
                        Some(i) if j == i => bail!(
                            "{what}: \"{s}\" is this buffer; read its previous state with \"@prev\""
                        ),
                        Some(i) if j > i => bail!(
                            "{what}: \"{s}\" is a later buffer (buffers step in file order); \
                             read its previous state with {{ source = \"{s}\", frame = \"prev\" }}"
                        ),
                        _ => return Ok(Some(buffer_channel(j, 0))),
                    }
                }
                if on_buffer.is_some() {
                    if out == "input" {
                        bail!(
                            "{what}: \"@input\" is the picture below a layer; buffers step \
                             before layers, so read \"@picture\" (the picture track)"
                        );
                    }
                    if out != "picture" && !comps.iter().any(|c| c == out) {
                        bail!(
                            "{what}: \"{s}\": a buffer reads @picture, @prev, earlier buffers \
                             and comps; layers draw after buffers step (render it in a comp \
                             and read @<comp>)"
                        );
                    }
                }
                let comp = comps.iter().position(|c| c == out);
                let earlier = on_layer.is_some_and(|r| r.earlier.iter().any(|e| e == out));
                if let (Some(index), false) = (comp, earlier) {
                    return Ok(Some(Channel {
                        source: Source::Comp {
                            index,
                            name: out.to_string(),
                            time_offset: 0.0,
                        },
                        filter: Filter::Linear,
                        wrap: Wrap::Clamp,
                        vflip: false,
                        srgb: false,
                        timing: Timing::default(),
                    }));
                }
                let Some(refs) = on_layer else {
                    bail!(
                        "{what}: \"{s}\" is another pass's picture; only a [[layer]]'s channels \
                         can read one (a shot has nothing below it)"
                    );
                };
                let source = match out {
                    "input" => Source::Input,
                    "picture" => Source::Picture,
                    id => match refs.earlier.iter().position(|e| e == id) {
                        Some(index) => Source::Layer {
                            index,
                            id: id.to_string(),
                        },
                        None if refs.later.first().is_some_and(|x| x == id) => {
                            bail!("{what}: \"{s}\" is this layer; a layer can't read itself")
                        }
                        None if refs.later.iter().any(|x| x == id) => bail!(
                            "{what}: \"{s}\" is a later layer; a layer reads only layers below \
                             it (earlier in movie.toml)"
                        ),
                        None => return Err(not_yet(what, s, "output")),
                    },
                };
                Ok(Some(Channel {
                    source,
                    filter: Filter::Linear,
                    wrap: Wrap::Clamp,
                    vflip: false,
                    srgb: false,
                    timing: Timing::default(),
                }))
            }
            ChannelSource::Name(s) => {
                let Some(names) = names else {
                    bail!(
                        "{what}: \"{s}\" isn't a file; give a path like \"textures/{s}.png\" or \
                         \"<kit/textures/{s}.png>\""
                    );
                };
                match names.get(s) {
                    Some(c) => Ok(Some(c.clone())),
                    None => bail!(
                        "{what}: no [textures] or [texts] entry called \"{s}\" ({})",
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
            _ => file_source(s).map(Some),
        },
        toml::Value::Table(t) if t.contains_key("source") => {
            // `{ source = "@tv", time_offset = 2, filter, wrap }` (#74).
            let Some(src) = t["source"].as_str().filter(|s| s.starts_with('@')) else {
                bail!("{what}: source must be an @ source (\"@<comp>\", \"@input\", …)");
            };
            let mut c = if let Some(fv) = t.get("frame") {
                // `frame = "prev" | -k` (#134): a buffer's state k frames ago.
                let back = match fv {
                    toml::Value::String(p) if p == "prev" => 1,
                    toml::Value::Integer(k) if *k < 0 => {
                        k.unsigned_abs().min(u32::MAX as u64) as u32
                    }
                    _ => bail!(
                        "{what}: frame must be \"prev\" or a negative whole number of frames \
                         (-1 is the previous state)"
                    ),
                };
                let id = &src[1..];
                let j = match buffers.iter().position(|b| b.id == id) {
                    Some(j) => j,
                    None if id == "prev" => match on_buffer {
                        Some(i) => i,
                        None => bail!("{what}: \"@prev\" is for buffers' channels"),
                    },
                    None => bail!("{what}: frame is for buffers; \"{src}\" isn't a [[buffer]]"),
                };
                if on_layer.is_none() {
                    bail!(
                        "{what}: \"{src}\" is a buffer; shots and transitions draw before \
                         buffers step, so only layers and buffers read one"
                    );
                }
                if back > buffers[j].history {
                    bail!(
                        "{what}: frame = -{back}, but buffer \"{}\" keeps {} state{}; set \
                         history = {back} on it",
                        buffers[j].id,
                        buffers[j].history,
                        if buffers[j].history == 1 { "" } else { "s" }
                    );
                }
                buffer_channel(j, back)
            } else {
                let Some(c) = texture_spec(
                    what,
                    &toml::Value::String(src.to_string()),
                    names,
                    specs,
                    root,
                    on_layer,
                    comps,
                    buffers,
                )?
                else {
                    unreachable!("an @ source binds something")
                };
                c
            };
            for (k, val) in t {
                match k.as_str() {
                    "source" | "frame" => {}
                    "filter" => {
                        let s = val.as_str().unwrap_or("");
                        c.filter = match Filter::parse(s) {
                            Some(Filter::Mipmap) => bail!(
                                "{what}: filter \"mipmap\" isn't available on @ sources (use \
                                 \"linear\" or \"nearest\")"
                            ),
                            Some(f) => f,
                            None => bail!("{what}: filter must be \"linear\" or \"nearest\""),
                        };
                    }
                    "wrap" => {
                        let s = val.as_str().unwrap_or("");
                        c.wrap = Wrap::parse(s).ok_or_else(|| {
                            anyhow!("{what}: wrap \"{s}\" isn't one of {:?}", Wrap::NAMES)
                        })?;
                    }
                    "time_offset" => {
                        let x = val
                            .as_float()
                            .or_else(|| val.as_integer().map(|i| i as f64))
                            .filter(|x| x.is_finite())
                            .ok_or_else(|| anyhow!("{what}: time_offset must be seconds"))?;
                        match &mut c.source {
                            Source::Comp { time_offset, .. } => *time_offset = x as f32,
                            _ => bail!("{what}: time_offset is for comps (@<comp>)"),
                        }
                    }
                    other => bail!(
                        "{what}: unknown option `{other}` (an @ source has source, filter, wrap \
                         and, for a comp, time_offset, for a buffer, frame)"
                    ),
                }
            }
            Ok(Some(c))
        }
        toml::Value::Table(t) => {
            // Exactly one key says what the table binds.
            let sources: Vec<&str> = ["file", "frames", "video", "texture", "text"]
                .into_iter()
                .filter(|k| t.contains_key(*k))
                .collect();
            if sources.len() > 1 {
                bail!(
                    "{what}: a channel table takes one of file, frames, video, texture or text \
                     (this one has {})",
                    sources.join(" and ")
                );
            }
            let shown = |k: &str| match t.get(k) {
                Some(toml::Value::String(s)) => s.clone(),
                _ => "…".to_string(),
            };
            if sources.first() == Some(&"video") {
                return Err(not_yet(what, &shown("video"), "video"));
            }
            if t.contains_key("part") && !t.contains_key("text") {
                bail!("{what}: part picks a glyph run's texture; it goes with text = \"…\"");
            }
            // `texture = "name"`: a [textures] footage entry, re-timed.
            let retimed = sources.first() == Some(&"texture");
            let mut c = if let Some(v) = t.get("text") {
                let Some(name) = v.as_str() else {
                    bail!("{what}: text must be the name of a [texts] entry");
                };
                let mut c = match names.and_then(|n| n.get(name)) {
                    Some(c) if c.text().is_some() => c.clone(),
                    _ if names.is_none() => {
                        bail!("{what}: text = \"{name}\": [textures] entries are images")
                    }
                    _ => bail!("{what}: text = \"{name}\": no [texts] entry called \"{name}\""),
                };
                if let Some(p) = t.get("part") {
                    let run = c.text().is_some_and(|x| x.glyphs != GlyphMode::Block);
                    let part = match p.as_str() {
                        Some("atlas") => TextPart::Main,
                        Some("glyphs") => TextPart::Table,
                        _ => bail!("{what}: part must be \"atlas\" or \"glyphs\""),
                    };
                    if !run {
                        bail!(
                            "{what}: part = {p} is for glyph runs; text \"{name}\" is a block \
                             (add glyphs = true to its [texts] entry)"
                        );
                    }
                    if part == TextPart::Table {
                        for k in ["filter", "wrap", "vflip"] {
                            if t.contains_key(k) {
                                bail!(
                                    "{what}: a glyph table is read with texelFetch; {k} doesn't apply"
                                );
                            }
                        }
                        c.filter = Filter::Nearest;
                        c.wrap = Wrap::Clamp;
                    }
                    if let Source::Text(x) = &mut c.source {
                        x.part = part;
                    }
                }
                c
            } else if retimed {
                let Some(name) = t["texture"].as_str() else {
                    bail!("{what}: texture must be the name of a [textures] entry");
                };
                let Some(names) = names else {
                    bail!(
                        "{what}: texture = \"{name}\" re-times a [textures] entry; it goes in a \
                         shot's or layer's channels"
                    );
                };
                match names.get(name) {
                    Some(c) if c.sequence().is_some() => c.clone(),
                    Some(c) => bail!(
                        "{what}: texture = \"{name}\" re-times footage, and {} isn't footage \
                         (bind it as \"{name}\")",
                        c.label()
                    ),
                    None => bail!(
                        "{what}: texture = \"{name}\": no [textures] entry called \"{name}\" ({})",
                        if names.is_empty() {
                            "there are none".to_string()
                        } else {
                            format!("there are {}", quoted_list(names.keys()))
                        }
                    ),
                }
            } else {
                let (key, image) = if t.contains_key("frames") {
                    ("frames", false)
                } else {
                    ("file", true)
                };
                match t.get(key) {
                    Some(toml::Value::String(s)) if !s.is_empty() && !is_texture_name(s) => {
                        let c = file_source(s)?;
                        if !image && c.sequence().is_none() {
                            bail!(
                                "{what}: frames = \"{s}\" needs one run of # in its file name, \
                                 the frame number (\"renders/fog_####.png\")"
                            );
                        }
                        c
                    }
                    Some(toml::Value::String(s)) => bail!(
                        "{what}: {key} = \"{s}\" must be a path to a .png or .jpg{} (names from \
                         [textures] can't have options)",
                        if image { "" } else { " sequence" }
                    ),
                    Some(_) => bail!("{what}: {key} must be a string"),
                    None => bail!(
                        "{what}: a channel table needs file = \"…\" (an image), frames = \"…\" \
                         (an image sequence), texture = \"…\" (re-timed footage) or text = \"…\" \
                         (a [texts] entry)"
                    ),
                }
            };
            if t.contains_key("in") && t.contains_key("in_frame") {
                bail!("{what}: give in (seconds) or in_frame (a source frame), not both");
            }
            let footage = c.sequence().is_some();
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
                let number = || -> Result<f64> {
                    val.as_float()
                        .or_else(|| val.as_integer().map(|i| i as f64))
                        .filter(|x| x.is_finite() && *x >= 0.0)
                        .ok_or_else(|| anyhow!("{what}: {k} must be a number ≥ 0"))
                };
                let whole = || -> Result<u64> {
                    val.as_integer()
                        .and_then(|i| u64::try_from(i).ok())
                        .ok_or_else(|| anyhow!("{what}: {k} must be a whole number ≥ 0"))
                };
                const TIMING: &[&str] = &["in", "in_frame", "speed", "clock", "end"];
                if TIMING.contains(&k.as_str()) && !footage {
                    bail!(
                        "{what}: \"{k}\" is for footage (image sequences); {} isn't footage",
                        c.label()
                    );
                }
                if retimed && !TIMING.contains(&k.as_str()) && k != "texture" {
                    bail!(
                        "{what}: only timing options (in, in_frame, speed, clock, end) may follow \
                         texture = \"…\"; `{k}` goes on the [textures] entry"
                    );
                }
                if matches!(k.as_str(), "fps" | "first") && !footage {
                    bail!(
                        "{what}: {k} is for image sequences (frames = \"…\"); {} isn't one",
                        c.label()
                    );
                }
                match k.as_str() {
                    "file" | "text" | "frames" | "texture" | "part" => {}
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
                    "srgb" if c.text().is_some() => {
                        bail!("{what}: srgb is for images; a text's channels are data")
                    }
                    "srgb" => c.srgb = flag()?,
                    "fps" => {
                        let f = match val {
                            toml::Value::Integer(i) => FpsFile::Int(*i),
                            toml::Value::Float(x) => FpsFile::Float(*x),
                            toml::Value::String(s) => FpsFile::Ratio(s.clone()),
                            _ => bail!("{what}: fps must look like 24 or \"30000/1001\""),
                        };
                        let fps = parse_fps(f).map_err(|e| anyhow!("{what}: {e}"))?;
                        if let Source::Frames(s) = &mut c.source {
                            s.fps = Some(fps);
                        }
                    }
                    "first" => {
                        let n = whole()?;
                        if let Source::Frames(s) = &mut c.source {
                            s.first = Some(n);
                        }
                    }
                    "in" => c.timing.start = In::Seconds(number()?),
                    "in_frame" => c.timing.start = In::Frame(whole()?),
                    "speed" => c.timing.speed = number()?,
                    "clock" => {
                        let s = word(FootageClock::NAMES)?;
                        c.timing.clock = match s.as_str() {
                            "shot" => FootageClock::Shot,
                            "movie" => FootageClock::Movie,
                            _ => return Err(not_one_of(&s, FootageClock::NAMES)),
                        };
                    }
                    "end" => {
                        let s = word(End::NAMES)?;
                        c.timing.end = match s.as_str() {
                            "hold" => End::Hold,
                            "loop" => End::Loop,
                            "black" => End::Black,
                            _ => return Err(not_one_of(&s, End::NAMES)),
                        };
                    }
                    other if footage => bail!(
                        "{what}: unknown option `{other}` (an image sequence has frames, fps, \
                         first, in, in_frame, speed, clock, end, filter, wrap, vflip and srgb)"
                    ),
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

/// A `[comps]` entry before its file is read (#74).
struct CompDecl {
    name: String,
    rel: String,
    size: Option<(u32, u32)>,
    looping: bool,
    /// `loop = "warm"` (#138).
    warm: bool,
}

/// Deepest comp nesting (each level holds its own targets).
pub const MAX_COMP_DEPTH: usize = 4;

/// `[comps]`: `name = "file.toml"` or `{ file, size = [w, h], loop }`, in name order.
fn comp_decls(t: &BTreeMap<String, toml::Value>) -> Result<Vec<CompDecl>> {
    let mut out = vec![];
    for (name, v) in t {
        let what = format!("[comps] {name}");
        if !is_pass_id(name) || name == "input" || name == "picture" {
            bail!(
                "{what}: a comp name may only use letters, digits, _ and - (and isn't input or \
                 picture)"
            );
        }
        let (rel, size, looping, warm) = match v {
            toml::Value::String(s) => (s.clone(), None, false, false),
            toml::Value::Table(t) => {
                let mut file = None;
                let mut size = None;
                let mut looping = false;
                let mut warm = false;
                for (k, val) in t {
                    match k.as_str() {
                        "file" => {
                            file = Some(
                                val.as_str()
                                    .ok_or_else(|| anyhow!("{what}: file must be a path"))?
                                    .to_string(),
                            )
                        }
                        "size" => {
                            let wh: Vec<i64> = val
                                .as_array()
                                .map(|a| a.iter().filter_map(toml::Value::as_integer).collect())
                                .unwrap_or_default();
                            match wh[..] {
                                [w, h] if (1..=16384).contains(&w) && (1..=16384).contains(&h) => {
                                    size = Some((w as u32, h as u32))
                                }
                                _ => bail!("{what}: size must be [width, height] in pixels"),
                            }
                        }
                        "loop" => (looping, warm) = loop_mode(&format!("{what}: "), Some(val))?,
                        other => bail!(
                            "{what}: unknown option `{other}` (a comp has file, size and loop)"
                        ),
                    }
                }
                let Some(file) = file else {
                    bail!("{what}: a comp table needs file = \"comps/….toml\"");
                };
                (file, size, looping, warm)
            }
            _ => bail!("{what}: a comp is \"comps/….toml\" or {{ file = \"…\", size, loop }}"),
        };
        library::check_rel_path(&rel).map_err(|e| anyhow!("{what}: {e}"))?;
        if !rel.ends_with(".toml") {
            bail!("{what}: \"{rel}\": a comp file is a .toml with [[shot]]s and [[layer]]s");
        }
        out.push(CompDecl {
            name: name.clone(),
            rel,
            size,
            looping,
            warm,
        });
    }
    Ok(out)
}

/// A comp file's text: what belongs to the film gets a pointer, not "unknown field".
fn parse_comp_file(text: &str) -> Result<CompFile> {
    if let Ok(t) = toml::from_str::<toml::Table>(text) {
        for key in [
            "title", "aspect", "fps", "glsl", "loop", "sound", "marker", "textures", "fonts",
            "texts", "comps",
        ] {
            if t.contains_key(key) {
                bail!(
                    "`{key}` belongs to the film (movie.toml); a comp file has [[shot]], \
                     [[layer]] and channels (size and loop go on its [comps] entry)"
                );
            }
        }
    }
    Ok(toml::from_str(text)?)
}

/// Every place a sequence shows a comp (#74): shots, layers, mattes and the channels of
/// shots, layers and buffers, one entry per use, with what uses it.
fn comp_uses(m: &Movie) -> Vec<(usize, String)> {
    let mut v = vec![];
    let mut chans = |what: String, list: &[Option<Channel>]| {
        for c in list.iter().flatten() {
            if let Source::Comp { index, .. } = c.source {
                v.push((index, what.clone()));
            }
        }
    };
    for s in &m.shots {
        chans(format!("shot \"{}\"'s channels", s.id), &s.channels);
    }
    for l in &m.layers {
        chans(format!("layer \"{}\"'s channels", l.id), &l.channels);
    }
    for b in &m.buffers {
        chans(format!("buffer \"{}\"'s channels", b.id), &b.channels);
    }
    for s in &m.shots {
        if let Some(c) = s.comp {
            v.push((c, format!("shot \"{}\"", s.id)));
        }
    }
    for l in &m.layers {
        if let LayerScene::Comp(c) = l.scene {
            v.push((c, format!("layer \"{}\"", l.id)));
        }
        if let Some(Matte {
            source: MatteSource::Comp(c),
            ..
        }) = l.matte
        {
            v.push((c, format!("layer \"{}\"'s matte", l.id)));
        }
    }
    v
}

/// The comps a sequence's shots, layers, buffers, channels and mattes show.
fn comps_used(m: &Movie) -> Vec<usize> {
    let mut v: Vec<usize> = comp_uses(m).into_iter().map(|(c, _)| c).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Comps with buffers (#138), directly or through a comp they show, are shown in at
/// most one place in the film and its comps: one fold per comp, over comp-local frames,
/// serves one comp time per film frame. (Two uses could ask for two comp frames at once.)
fn check_comp_buffers(film: &Movie, comps: &[Comp]) -> Result<()> {
    fn has_buffers(c: usize, comps: &[Comp]) -> bool {
        let m = &comps[c].movie;
        !m.buffers.is_empty() || comps_used(m).into_iter().any(|d| has_buffers(d, comps))
    }
    let mut uses: Vec<Vec<String>> = vec![vec![]; comps.len()];
    for (c, what) in comp_uses(film) {
        uses[c].push(what);
    }
    for comp in comps {
        for (c, what) in comp_uses(&comp.movie) {
            uses[c].push(format!("comp \"{}\": {what}", comp.name));
        }
    }
    for (c, u) in uses.iter().enumerate() {
        if u.len() > 1 && has_buffers(c, comps) {
            bail!(
                "comp \"{}\" has feedback buffers{}, so it can be shown in one place only \
                 (its state follows one comp time per film frame); it is shown by {}. Make a \
                 second comp file for another use",
                comps[c].name,
                if comps[c].movie.buffers.is_empty() {
                    " (in a comp it shows)"
                } else {
                    ""
                },
                u.join(", ")
            );
        }
    }
    Ok(())
}

/// Comps don't show themselves through others, and nest at most `MAX_COMP_DEPTH` deep.
fn check_comp_graph(film: &Movie) -> Result<()> {
    let comps = &film.comps[..];
    fn walk(c: usize, comps: &[Comp], path: &mut Vec<usize>) -> Result<()> {
        if let Some(i) = path.iter().position(|&p| p == c) {
            let mut names: Vec<&str> = path[i..].iter().map(|&p| comps[p].name.as_str()).collect();
            names.push(&comps[c].name);
            bail!("comps show each other in a cycle: {}", names.join(" → "));
        }
        path.push(c);
        if path.len() > MAX_COMP_DEPTH {
            let names: Vec<&str> = path.iter().map(|&p| comps[p].name.as_str()).collect();
            bail!(
                "comps nest {} deep ({}); at most {MAX_COMP_DEPTH} levels",
                path.len(),
                names.join(" → ")
            );
        }
        let m = &comps[c].movie;
        for d in comps_used(m) {
            walk(d, comps, path)?;
        }
        path.pop();
        Ok(())
    }
    for c in 0..comps.len() {
        walk(c, comps, &mut vec![])?;
    }
    // Depth from the film: what it shows is level 1.
    fn depth(c: usize, comps: &[Comp]) -> usize {
        let m = &comps[c].movie;
        1 + comps_used(m)
            .into_iter()
            .map(|d| depth(d, comps))
            .max()
            .unwrap_or(0)
    }
    for c in comps_used(film) {
        if depth(c, comps) > MAX_COMP_DEPTH {
            bail!(
                "comp \"{}\" nests more than {MAX_COMP_DEPTH} levels deep",
                comps[c].name
            );
        }
    }
    Ok(())
}

/// A layer's `matte = { source = "@id", channel = "alpha" | "luma", invert = false }`.
fn matte_spec(what: &str, v: &toml::Value, earlier: &[String], sc: &SeqCtx) -> Result<Matte> {
    let what = format!("{what}: matte");
    let Some(t) = v.as_table() else {
        bail!("{what}: a table {{ source = \"@<layer or comp>\", channel = \"alpha\" }}");
    };
    let mut source = None;
    let mut luma = false;
    let mut invert = false;
    for (k, val) in t {
        match k.as_str() {
            "source" => {
                let s = val.as_str().unwrap_or("");
                let Some(id) = s.strip_prefix('@') else {
                    bail!("{what}: source \"{s}\" must be \"@<earlier layer>\" or \"@<comp>\"");
                };
                source = Some(if let Some(j) = earlier.iter().position(|e| e == id) {
                    MatteSource::Layer(j)
                } else if sc.comps.iter().any(|c| c == id) {
                    MatteSource::Comp(sc.comp_ref(&what, id)?)
                } else {
                    bail!(
                        "{what}: source \"{s}\" is neither an earlier layer nor a comp (layers \
                         below: {}; comps: {})",
                        quoted_list(earlier.iter()),
                        quoted_list(sc.comps.iter())
                    );
                });
            }
            "channel" => {
                luma = match val.as_str() {
                    Some("alpha") => false,
                    Some("luma") => true,
                    _ => bail!("{what}: channel must be \"alpha\" or \"luma\""),
                }
            }
            "invert" => {
                invert = val
                    .as_bool()
                    .ok_or_else(|| anyhow!("{what}: invert must be true or false"))?
            }
            other => bail!("{what}: unknown option `{other}` (source, channel, invert)"),
        }
    }
    let Some(source) = source else {
        bail!("{what}: needs source = \"@<earlier layer or comp>\"");
    };
    Ok(Matte {
        source,
        luma,
        invert,
    })
}

/// The layer ids a layer's `@<id>` channels may name (`earlier`), and the rest
/// (`later`, this layer first) for messages.
#[derive(Clone, Copy)]
struct LayerRefs<'a> {
    earlier: &'a [String],
    later: &'a [String],
    /// The channels are buffer `i`'s (#134): `@prev`, and earlier buffers only.
    on_buffer: Option<usize>,
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
             to [libraries] in shotglass.toml, then use type = \"transitions/{name}\""
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

fn is_pass_id(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
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

    /// Parse a test film written as one text: `shotglass = …` and `[libraries]` go to
    /// the project file (see `project::split_fixture`), the rest to movie.toml.
    fn fixture(text: &str, root: &Path) -> Result<Movie> {
        let (p, m) = crate::project::split_fixture(text);
        let project = crate::project::Project::parse(&p, root)?;
        Movie::parse_in(&m, root, &project)
    }

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
        fixture(&format!("fps = {fps}\n{M}"), Path::new("."))
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
        assert_eq!(fixture(M, Path::new(".")).unwrap().fps_den, 1);
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
            let mid = (m.frame_time(10_000) + m.frame_time(10_001)) / 2.0;
            assert_eq!(m.frame_at(mid), 10_000);
            let boundary = m.frame_time_precise(10_001);
            assert_eq!(m.frame_at_precise(boundary - 1e-9), 10_000);
            assert_eq!(m.frame_at_precise(boundary), 10_001);
            let n = m.frame_count();
            assert!(m.frame_time(n - 1) < m.duration && m.frame_time(n) >= m.duration);
        }
        assert_eq!(with_fps("30").unwrap().frame_count(), 330); // 11 s
    }

    #[test]
    fn layout_overlaps_transitions() {
        let m = fixture(M, Path::new(".")).unwrap();
        let starts: Vec<f32> = m.shots.iter().map(|s| s.start).collect();
        assert_eq!(starts, vec![0.0, 4.0, 8.0]);
        assert_eq!(m.duration, 11.0);
    }

    #[test]
    fn at_resolves_shots_and_transitions() {
        let m = fixture(M, Path::new(".")).unwrap();
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
        let m = fixture(M, Path::new(".")).unwrap();
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
        let e = fixture(bad, Path::new(".")).unwrap_err().to_string();
        assert!(e.contains("overlaps"), "{e}");
    }

    #[test]
    fn shot_sound_follows_transitions() {
        let m = fixture(
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
            let m = fixture(
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
            /// Every spelling of a channel entry either binds or fails with a message that
            /// names the entry; the classifier and its error paths never panic.
            #[test]
            fn channel_spellings_never_panic(entry in "[a-zA-Z0-9_#@<>/.%-]{0,24}") {
                let toml = format!(
                    "[[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 1\nchannels = [\"{entry}\"]\n"
                );
                let _ = ChannelSource::of(&entry);
                if let Err(e) = Movie::parse(&toml, Path::new("/film")) {
                    let e = format!("{e:#}");
                    prop_assert!(e.contains("channels[0]"), "{}: {}", entry, e);
                }
            }
        }

        proptest! {
            /// For speed ≥ 0, a footage channel's source frame never goes back within a
            /// shot (before `loop` wraps), whatever the rates, in and origin, and it never
            /// panics; `black` only ever turns black, and stays so.
            #[test]
            fn source_frame_never_goes_back(
                film in prop::sample::select(vec![(24u32, 1u32), (25, 1), (30, 1), (30000, 1001), (60, 1), (24000, 1001)]),
                src in prop::sample::select(vec![(12u32, 1u32), (24, 1), (25, 1), (30000, 1001), (60, 1), (7, 3)]),
                start in prop::option::of(0.0f64..20.0),
                start_frame in 0u64..500,
                speed in prop::sample::select(vec![0.0f64, 0.25, 0.5, 1.0, 1.5, 2.0, 3.7]),
                origin in 0.0f32..100.0,
                count in 1u64..400,
                black in any::<bool>(),
                movie_clock in any::<bool>(),
                from in 0u64..5000,
            ) {
                let m = FrameMap {
                    film,
                    loop_frames: None,
                    origin,
                    timing: Timing {
                        start: start.map_or(In::Frame(start_frame), In::Seconds),
                        speed,
                        clock: if movie_clock { FootageClock::Movie } else { FootageClock::Shot },
                        end: if black { End::Black } else { End::Hold },
                    },
                    src,
                    count,
                };
                let mut prev: Option<u64> = Some(0);
                for n in from..from + 300 {
                    let k = m.source_frame(n);
                    match (prev, k) {
                        (Some(p), Some(k)) => prop_assert!(k >= p && k < count, "{} then {} at {}", p, k, n),
                        (None, Some(k)) => prop_assert!(false, "black, then frame {} at {}", k, n),
                        (_, None) => prop_assert!(black),
                    }
                    prev = k;
                }
            }
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
            fixture(&ok, Path::new(".")).unwrap().shotglass,
            Some(format!("{}.{}", v.major, v.minor))
        );
        // A film from the future may use fields we don't know; the engine message wins.
        let future = format!("shotglass = \"99.1\"\nnew_thing = 3\n{SHOT}");
        let e = format!("{:#}", fixture(&future, Path::new(".")).unwrap_err());
        assert!(
            e.contains("needs shotglass 99.1") && e.contains("newer"),
            "{e}"
        );
        assert!(fixture(&format!("shotglass = 0.4\n{SHOT}"), Path::new(".")).is_err());
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
            fixture(&t, Path::new("."))
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
        let m = fixture(
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
        assert!(format!("{:#}", fixture(&bad, Path::new(".")).unwrap_err()).contains("climbs"));
        let bad = format!("[libraries]\nkit = {{ path = \"kit\", url = \"x\" }}\n{SHOT}");
        assert!(fixture(&bad, Path::new(".")).is_err());
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
            let e = format!("{:#}", fixture(&text, Path::new(".")).unwrap_err());
            assert!(e.contains(why) && e.contains("shot \"b\""), "{line}: {e}");
        }
        let text = format!("[sound]\nmusic = \"D:\\\\m.glsl\"\n{SHOT}");
        let e = format!("{:#}", fixture(&text, Path::new(".")).unwrap_err());
        assert!(
            e.contains("[sound] music") && e.contains("backslash"),
            "{e}"
        );
        // Paths that climb and come back down inside the film are fine.
        let text =
            format!("{SHOT}[[shot]]\nid = \"b\"\nduration = 1\nscene = \"scenes/../b.glsl\"\n");
        assert!(fixture(&text, Path::new(".")).is_ok());
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
        crate::project::write_fixture(
            &dir,
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
            crate::project::write_fixture(
                &dir,
                &format!(
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
        assert_eq!(fixture(shot, Path::new(".")).unwrap().glsl, None);
        for v in ["330", "410", "430"] {
            let m = fixture(&format!("glsl = \"{v}\"\n{shot}"), Path::new(".")).unwrap();
            assert_eq!(m.glsl, Some(v.parse().unwrap()));
        }
        for bad in ["\"450\"", "\"300 es\"", "\"\"", "430"] {
            let e = fixture(&format!("glsl = {bad}\n{shot}"), Path::new("."));
            assert!(e.is_err(), "glsl = {bad} should be refused");
        }
        let e = fixture(&format!("glsl = \"460\"\n{shot}"), Path::new("."))
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
        fixture(&t, Path::new("/film"))
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
        assert_eq!(
            grid.image().unwrap().file,
            PathBuf::from("/film/textures/grid.png")
        );
        assert_eq!(
            (grid.filter, grid.wrap, grid.vflip, grid.srgb),
            (Filter::Nearest, Wrap::Mirror, true, false)
        );
        let noise = c[1].as_ref().unwrap();
        assert_eq!(noise.image().unwrap().library.as_deref(), Some("kit"));
        assert_eq!(noise.label(), "kit/textures/noise.png");
        assert_eq!(noise.image().unwrap().rel, "textures/noise.png");
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
        assert_eq!(c[1].as_ref().unwrap().label(), "textures/stars.png");
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
            (
                c.image().unwrap().library.as_ref(),
                c.image().unwrap().file.clone()
            ),
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
        let label = |i: usize, n: usize| m.shots[i].channels[n].as_ref().map(|c| c.label());
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
        let m = fixture(M, Path::new(".")).unwrap();
        assert!(!m.uses_channels() && m.contract_warning().is_none());
    }

    #[test]
    fn channels_are_validated() {
        let e = err(channels_film(
            "",
            "",
            &["channels = [\"\", \"\", \"\", \"\", \"\", \"\", \"\", \"\", \"\"]"],
        ));
        assert!(
            e.contains("shot \"s0\": channels has 9 entries; there are 8 (iChannel0..7)"),
            "{e}"
        );
        let e = err(channels_film(
            "",
            "brick = \"b.png\"\nnoise = \"n.png\"\ngrid = \"g.png\"\n",
            &["channels = [\"brick\", \"noize\"]"],
        ));
        assert!(
            e.contains("shot \"s0\": channels[1]: no [textures] or [texts] entry called \"noize\" (there are \"brick\", \"grid\", \"noise\")"),
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
    fn channel_sources_by_spelling() {
        use ChannelSource::*;
        for (s, kind) in [
            ("", Empty),
            ("@input", Output("input")),
            ("@title", Output("title")),
            ("brick", Name("brick")),
            ("my_tex2", Name("my_tex2")),
            ("textures/brick.png", Image("textures/brick.png")),
            ("brick.JPG", Image("brick.JPG")),
            (
                "<kit/textures/noise.png>",
                Image("<kit/textures/noise.png>"),
            ),
            ("textures/sky.webp", Image("textures/sky.webp")), // refused later, as an image
            ("renders/fog_####.png", Sequence("renders/fog_####.png")),
            ("<kit/grain_##.png>", Sequence("<kit/grain_##.png>")),
            ("footage/surf.mp4", Video("footage/surf.mp4")),
            ("plate.MOV", Video("plate.MOV")),
            (
                "<kit/loops/caustics.webm>",
                Video("<kit/loops/caustics.webm>"),
            ),
            ("clips.mp4/still.png", Image("clips.mp4/still.png")), // the file name decides
        ] {
            assert_eq!(ChannelSource::of(s), kind, "{s}");
        }
    }

    #[test]
    fn channel_kinds_that_arrive_later() {
        let shot = |entry: &str| err(channels_film("", "", &[&format!("channels = [{entry}]")]));
        for (entry, says) in [
            (
                r#""@picture""#,
                r#"channels[0]: "@picture" is another pass's picture; only a [[layer]]'s channels can read one"#,
            ),
            (
                r#""footage/surf.mp4""#,
                r#"channels[0]: "footage/surf.mp4" is a video; video files arrive with footage phase 2"#,
            ),
            (
                r#"{ video = "a.mov", in = 2.0 }"#,
                "is a video; video files arrive",
            ),
            (r#"{ file = "a.mp4" }"#, "is a video; video files arrive"),
        ] {
            let e = shot(entry);
            assert!(e.contains(says), "{entry}: {e}");
        }
        // Malformed spellings get their own messages, not "arrives later".
        for (entry, says) in [
            (
                r#""@""#,
                "after @ comes input, picture, or the id of a layer or comp",
            ),
            (r#""@my layer""#, "after @ comes"),
            (
                r#""r_##/f_##.png""#,
                "# marks frame numbers and may only appear in the file name",
            ),
            (r#""r/f_##_##.png""#, "an image sequence has one run of #"),
            (
                r#""renders/fog_%04d.png""#,
                r#"write the frame number as # ("renders/fog_####.png")"#,
            ),
            (
                r#"{ file = "a.png", video = "b.mp4" }"#,
                "a channel table takes one of file, frames, video, texture or text (this one has file and video)",
            ),
        ] {
            let e = shot(entry);
            assert!(e.contains(says), "{entry}: {e}");
        }
        // [textures] values are files: @ sources only go in a shot's channels.
        let e = err(channels_film("", "below = \"@input\"\n", &[]));
        assert!(
            e.contains("[textures] below: \"@input\" isn't a file"),
            "{e}"
        );
        // A video names its replacement for now: an image sequence.
        let e = shot(r#""a.mp4""#);
        assert!(e.contains("\"frames/clip_#####.png\""), "{e}");
    }

    #[test]
    fn sequence_channels_parse() {
        let m = channels_film(
            "",
            "fog = \"renders/fog_####.png\"\n\
             matte = { frames = \"<kit/m/matte_####.png>\", fps = \"24000/1001\", first = 1001, \
             clock = \"movie\", end = \"loop\", filter = \"nearest\", srgb = true }\n",
            &["channels = [\"fog\", \"matte\", { texture = \"fog\", in = 2.5, speed = 0.5 }, \
               { texture = \"matte\", in_frame = 7, end = \"black\" }, { frames = \"a/b_##.jpg\", in = 1 }, \
               { file = \"x/y_#.png\", fps = 12 }]"],
        )
        .unwrap();
        let c = |n: usize| m.shots[0].channels[n].as_ref().unwrap();
        let fog = c(0).sequence().unwrap();
        assert_eq!(
            fog.pattern.file,
            PathBuf::from("/film/renders/fog_####.png")
        );
        assert_eq!(
            (fog.fps, fog.first, fog.scan.is_some()),
            (None, None, false)
        );
        assert_eq!(
            (c(0).filter, c(0).wrap, c(0).vflip, c(0).srgb, c(0).timing),
            (Filter::Linear, Wrap::Repeat, true, false, Timing::default()),
            "footage samples linearly by default (no mipmaps per frame)"
        );
        let matte = c(1).sequence().unwrap();
        assert_eq!(matte.pattern.library.as_deref(), Some("kit"));
        assert_eq!(c(1).label(), "kit/m/matte_####.png");
        assert_eq!((matte.fps, matte.first), (Some((24000, 1001)), Some(1001)));
        assert_eq!((c(1).filter, c(1).srgb), (Filter::Nearest, true));
        assert_eq!(
            (c(1).timing.clock, c(1).timing.end),
            (FootageClock::Movie, End::Loop)
        );
        // Re-timed: the entry's source and options, the channel's own timing on top.
        assert_eq!(c(2).source, c(0).source);
        assert_eq!(
            c(2).timing,
            Timing {
                start: In::Seconds(2.5),
                speed: 0.5,
                ..Timing::default()
            }
        );
        assert_eq!(c(3).source, c(1).source);
        assert_eq!(
            c(3).timing,
            Timing {
                start: In::Frame(7),
                end: End::Black,
                ..c(1).timing
            }
        );
        assert_eq!(c(4).timing.start, In::Seconds(1.0));
        assert_eq!(c(4).label(), "a/b_##.jpg");
        // The spelling decides, in a table too.
        assert_eq!(c(5).sequence().unwrap().fps, Some((12, 1)));
        // Used through texture = "…" counts as used; the library is referenced.
        assert!(m.warnings.is_empty(), "{:?}", m.warnings);
        assert!(m.library_refs.contains("kit"));
        // A sequence's frames aren't files to check by name (the scan finds them).
        assert!(c(0).files().is_empty() && c(0).written().len() == 1);
        assert!(
            !m.referenced_files()
                .iter()
                .any(|f| f.to_string_lossy().contains('#'))
        );
        let w = m.contract_warning().unwrap();
        assert!(
            w.contains("image sequences, which need shotglass 0.1.6"),
            "{w}"
        );
        let m = channels_film("", "fog = \"r/f_##.png\"\n", &["channels = [\"fog\"]"]).unwrap();
        assert!(m.textures().len() == 1 && m.uses_channels());
    }

    #[test]
    fn sequence_channels_are_validated() {
        let shot = |entry: &str| {
            err(channels_film(
                "",
                "fog = \"r/fog_####.png\"\nimg = \"t/i.png\"\n",
                &[&format!("channels = [\"fog\", \"img\", {entry}]")],
            ))
        };
        for (entry, says) in [
            (
                r#"{ file = "a.png", in = 1 }"#,
                r#"channels[2]: "in" is for footage (image sequences); a.png isn't footage"#,
            ),
            (
                r#"{ file = "a.png", fps = 24 }"#,
                "fps is for image sequences",
            ),
            (
                r#"{ frames = "a/b_##.png", in = 1, in_frame = 2 }"#,
                "give in (seconds) or in_frame (a source frame), not both",
            ),
            (
                r#"{ frames = "a/b_##.png", speed = -1 }"#,
                "speed must be a number ≥ 0",
            ),
            (
                r#"{ frames = "a/b_##.png", in = "x" }"#,
                "in must be a number ≥ 0",
            ),
            (
                r#"{ frames = "a/b_##.png", in_frame = 1.5 }"#,
                "in_frame must be a whole number ≥ 0",
            ),
            (
                r#"{ frames = "a/b_##.png", clock = "wall" }"#,
                r#"clock "wall" isn't one of "shot", "movie""#,
            ),
            (
                r#"{ frames = "a/b_##.png", end = "bounce" }"#,
                r#"end "bounce" isn't one of "hold", "loop", "black""#,
            ),
            (r#"{ frames = "a/b_##.png", fps = 0 }"#, "fps must be"),
            (
                r#"{ frames = "a/b_##.png", fps = "24/x" }"#,
                "fps must look like",
            ),
            (
                r#"{ frames = "a/b_##.png", first = -1 }"#,
                "first must be a whole number ≥ 0",
            ),
            (
                r#"{ frames = "a/b.png" }"#,
                "needs one run of # in its file name",
            ),
            (
                r#"{ frames = "a/b_%03d.png" }"#,
                r#"write the frame number as # ("a/b_###.png")"#,
            ),
            (
                r#"{ frames = "a/b_##.gif" }"#,
                "an image sequence's frames are PNG or JPEG images",
            ),
            (
                r#"{ frames = "a/b_##.png", bogus = 1 }"#,
                "unknown option `bogus` (an image sequence has frames, fps, first, in",
            ),
            (
                r#"{ frames = "fog" }"#,
                "frames = \"fog\" must be a path to a .png or .jpg sequence",
            ),
            (
                r#"{ texture = "img" }"#,
                "texture = \"img\" re-times footage, and t/i.png isn't footage (bind it as \"img\")",
            ),
            (
                r#"{ texture = "nope" }"#,
                r#"texture = "nope": no [textures] entry called "nope" (there are "fog", "img")"#,
            ),
            (
                r#"{ texture = "fog", filter = "nearest" }"#,
                "only timing options (in, in_frame, speed, clock, end) may follow texture",
            ),
            (
                r#"{ texture = "fog", fps = 24 }"#,
                "`fps` goes on the [textures] entry",
            ),
            (
                r#"{ texture = 3 }"#,
                "texture must be the name of a [textures] entry",
            ),
        ] {
            let e = shot(entry);
            assert!(e.contains(says), "{entry}: {e}");
        }
        let e = err(channels_film(
            "",
            "fog = \"r/f_##.png\"\nx = { texture = \"fog\" }\n",
            &[],
        ));
        assert!(
            e.contains("[textures] x: texture = \"fog\" re-times a [textures] entry"),
            "{e}"
        );
    }

    fn rate_map(film: (u32, u32), src: (u32, u32)) -> FrameMap {
        FrameMap {
            film,
            loop_frames: None,
            origin: 0.0,
            timing: Timing {
                clock: FootageClock::Movie,
                ..Timing::default()
            },
            src,
            count: u64::MAX,
        }
    }

    #[test]
    fn source_frame_is_exact_for_rate_pairs() {
        // Equal rates are the identity for a million frames, NTSC included.
        for r in [(30, 1), (30000, 1001), (24000, 1001), (25, 1)] {
            let m = rate_map(r, r);
            for n in 0..1_000_000u64 {
                assert_eq!(m.source_frame(n), Some(n), "{r:?} frame {n}");
            }
        }
        // Different rates: "the source frame whose interval contains this instant",
        // which is exact integer arithmetic: floor(n · src · film_den / (film · src_den)).
        for (film, src) in [
            ((30, 1), (24, 1)),
            ((24, 1), (25, 1)),
            ((24, 1), (60, 1)),
            ((30000, 1001), (24000, 1001)),
            ((60, 1), (24000, 1001)),
            ((25, 1), (30000, 1001)),
        ] {
            let m = rate_map(film, src);
            for n in 0..200_000u64 {
                let want =
                    (n as u128 * src.0 as u128 * film.1 as u128) / (film.0 as u128 * src.1 as u128);
                assert_eq!(
                    m.source_frame(n),
                    Some(want as u64),
                    "{src:?} in {film:?}, frame {n}"
                );
            }
        }
        // clock = "shot" with an item starting late in a film (an f32 start, rounded):
        // still the identity from its first frame.
        for r in [(30, 1), (30000, 1001)] {
            for n0 in [1u64, 33, 1799, 107_892, 1_000_003] {
                let start = (n0 as f64 * r.1 as f64 / r.0 as f64) as f32;
                let m = FrameMap {
                    origin: start,
                    timing: Timing::default(),
                    ..rate_map(r, r)
                };
                for j in 0..2000 {
                    assert_eq!(m.source_frame(n0 + j), Some(j), "{r:?} from {n0}, +{j}");
                }
            }
        }
        let m = rate_map((30, 1), (24, 1));
        let v: Vec<u64> = (0..12).map(|n| m.source_frame(n).unwrap()).collect();
        assert_eq!(
            v,
            [0, 0, 1, 2, 3, 4, 4, 5, 6, 7, 8, 8],
            "one repeat every five"
        );
    }

    #[test]
    fn source_frame_timing_options() {
        let base = FrameMap {
            film: (30, 1),
            loop_frames: None,
            origin: 0.0,
            timing: Timing::default(),
            src: (24, 1),
            count: 48,
        };
        let with = |t: Timing| FrameMap { timing: t, ..base };
        let at = |m: FrameMap, n: u64| m.source_frame(n);
        // in (seconds) and in_frame agree: 0.5 s at 24 fps is frame 12.
        let a = with(Timing {
            start: In::Seconds(0.5),
            ..Timing::default()
        });
        let b = with(Timing {
            start: In::Frame(12),
            ..Timing::default()
        });
        for n in 0..40 {
            assert_eq!(at(a, n), at(b, n));
        }
        assert_eq!(at(a, 0), Some(12));
        // speed: half (each source frame on ~2.5 film frames), double, 0 = freeze at in.
        let half = with(Timing {
            speed: 0.5,
            ..Timing::default()
        });
        assert_eq!(
            (0..10).map(|n| at(half, n).unwrap()).collect::<Vec<_>>(),
            [0, 0, 0, 1, 1, 2, 2, 2, 3, 3]
        );
        let double = with(Timing {
            speed: 2.0,
            ..Timing::default()
        });
        assert_eq!(at(double, 5), Some(8));
        let freeze = with(Timing {
            start: In::Frame(7),
            speed: 0.0,
            ..Timing::default()
        });
        assert!((0..100).all(|n| at(freeze, n) == Some(7)));
        // end: frame 47 is the last (film frame 59 is source 47.2); past it…
        assert_eq!(at(base, 59), Some(47));
        assert_eq!(at(base, 60), Some(47), "hold");
        assert_eq!(at(base, 1000), Some(47), "hold");
        let looped = with(Timing {
            end: End::Loop,
            ..Timing::default()
        });
        assert_eq!(at(looped, 60), Some(0));
        assert_eq!(at(looped, 61), Some(0));
        assert_eq!(at(looped, 62), Some(1));
        let black = with(Timing {
            end: End::Black,
            ..Timing::default()
        });
        assert_eq!((at(black, 59), at(black, 60)), (Some(47), None));
        // clock = "shot": 0 at the item's start, even an f32 start that isn't exact
        // (1.1 s is film frame 33); clock = "movie" ignores it. Before the start (and
        // rounding below 0) clamps to frame 0.
        let shot = FrameMap {
            origin: 1.1,
            ..base
        };
        assert_eq!(
            (at(shot, 33), at(shot, 34), at(shot, 35)),
            (Some(0), Some(0), Some(1))
        );
        assert_eq!(at(shot, 0), Some(0));
        let movie = FrameMap {
            origin: 1.1,
            timing: Timing {
                clock: FootageClock::Movie,
                ..Timing::default()
            },
            ..base
        };
        assert_eq!(at(movie, 33), Some(26));
        // The outgoing shot of a transition keeps moving past its own end.
        let long = FrameMap {
            count: 1000,
            ..base
        };
        assert_eq!(at(long, 100), Some(80));
        // Loop films: frame n of loop k is frame n of loop 0.
        let lp = FrameMap {
            loop_frames: Some(45),
            timing: Timing {
                end: End::Loop,
                ..Timing::default()
            },
            ..base
        };
        for n in 0..45 {
            for k in 1..4 {
                assert_eq!(at(lp, n + 45 * k), at(lp, n));
            }
        }
        assert_eq!(
            (
                base.source_time(12),
                FrameMap {
                    src: (30000, 1001),
                    ..base
                }
                .source_time(30)
            ),
            (0.5, 1.001)
        );
    }

    #[test]
    fn load_scans_sequences_and_maps_frames() {
        let dir = std::env::temp_dir().join(format!("shotglass-movie-seq-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("r")).unwrap();
        std::fs::write(dir.join("a.glsl"), "").unwrap();
        for k in 1..=6 {
            image::RgbaImage::new(4, 2)
                .save(dir.join(format!("r/f_{k:02}.png")))
                .unwrap();
        }
        let film = |channels: &str| {
            crate::project::write_fixture(
                &dir,
                &format!(
                    "fps = 30\n[[shot]]\nid = \"lead\"\nscene = \"a.glsl\"\nduration = 0.5\n\
                     [[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 1\nchannels = {channels}\n"
                ),
            )
            .unwrap();
            Movie::load(&dir)
        };
        let m = film("[\"r/f_##.png\", { frames = \"r/f_##.png\", first = 3, fps = 15 }]").unwrap();
        let seq = m.shots[1].channels[0].as_ref().unwrap().sequence().unwrap();
        let scan = seq.scan.as_ref().unwrap();
        assert_eq!((scan.first, scan.count, scan.size), (1, 6, Some((4, 2))));
        assert_eq!(scan.path(0), dir.join("r/f_01.png"));
        let s2 = m.shots[1].channels[1].as_ref().unwrap().sequence().unwrap();
        assert_eq!(s2.scan.as_ref().unwrap().count, 4);
        // The second shot starts at film frame 15: its clock ("shot") starts there.
        let item = ItemRef::Shot(1);
        assert_eq!(m.source_frame(item, 0, 15), Some(0));
        assert_eq!(m.source_frame(item, 0, 20), Some(5));
        assert_eq!(m.source_frame(item, 0, 25), Some(5), "hold");
        assert_eq!(
            m.source_frame(item, 1, 17),
            Some(1),
            "15 fps: one frame per two"
        );
        assert_eq!(m.source_frame(item, 2, 17), None, "unbound");
        assert_eq!(m.source_frame(ItemRef::Shot(0), 0, 0), None, "no channels");
        // Edit mistakes refuse the version.
        let e = format!(
            "{:#}",
            film("[{ frames = \"r/f_##.png\", in = 5 }]").unwrap_err()
        );
        assert!(
            e.contains(
                "shot \"s\": channels[0]: in = 5 is past the end of r/f_##.png (0.20 s, 6 frames)"
            ),
            "{e}"
        );
        assert!(film("[{ frames = \"r/f_##.png\", in = 5, end = \"loop\" }]").is_ok());
        let e = format!("{:#}", film("[\"r/g_##.png\"]").unwrap_err());
        assert!(
            e.contains("channels[0]: r/g_##.png: matches no files"),
            "{e}"
        );
        std::fs::remove_file(dir.join("r/f_03.png")).unwrap();
        let e = format!("{:#}", film("[\"r/f_##.png\"]").unwrap_err());
        assert!(
            e.contains("r/f_##.png: frame 3 is missing (between 1 and 6)"),
            "{e}"
        );
        // With first after the gap, the frames from there on are a sequence.
        assert!(film("[{ frames = \"r/f_##.png\", first = 4 }]").is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A PNG that is only a signature and a header: enough for `texture::dimensions`.
    fn png_header(w: u32, h: u32) -> Vec<u8> {
        fn crc(data: &[u8]) -> u32 {
            let mut c = 0xFFFF_FFFFu32;
            for &b in data {
                c ^= b as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 {
                        0xEDB8_8320 ^ (c >> 1)
                    } else {
                        c >> 1
                    };
                }
            }
            !c
        }
        let mut chunk = b"IHDR".to_vec();
        chunk.extend(w.to_be_bytes());
        chunk.extend(h.to_be_bytes());
        chunk.extend([8, 6, 0, 0, 0]);
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend(13u32.to_be_bytes());
        v.extend(&chunk);
        v.extend(crc(&chunk).to_be_bytes());
        // An empty IDAT: the decoder reads the header up to the first one.
        v.extend(0u32.to_be_bytes());
        v.extend(b"IDAT");
        v.extend(crc(b"IDAT").to_be_bytes());
        v
    }

    #[test]
    fn a_shot_whose_footage_cant_fit_is_refused() {
        let dir =
            std::env::temp_dir().join(format!("shotglass-movie-seqmem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("r")).unwrap();
        std::fs::write(dir.join("a.glsl"), "").unwrap();
        // Two 4096² sequences: three frames each (the minimum window) are 384 MB.
        for name in ["a_1.png", "b_1.png"] {
            std::fs::write(dir.join("r").join(name), png_header(4096, 4096)).unwrap();
        }
        let film = |channels: &str| {
            crate::project::write_fixture(
                &dir,
                &format!("[[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 1\nchannels = {channels}\n"),
            )
            .unwrap();
            Movie::load(&dir)
        };
        assert!(film("[\"r/a_#.png\"]").is_ok(), "one fits (192 MB)");
        let e = format!("{:#}", film("[\"r/a_#.png\", \"r/b_#.png\"]").unwrap_err());
        assert!(
            e.contains("shot \"s\": its textures need 384 MB, more than the 256 MB budget"),
            "{e}"
        );
        assert!(
            e.contains("r/a_#.png 192 MB (3 frames of 4096×4096)"),
            "{e}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    fn layers_film(layers: &str) -> Result<Movie> {
        fixture(
            &format!(
                "[libraries]\nfx = {{ path = \"lib/fx\" }}\n\
                 [[shot]]\nid = \"moon\"\nscene = \"a.glsl\"\nduration = 3\n\
                 [[shot]]\nid = \"closeup\"\nscene = \"b.glsl\"\nduration = 3\n\
                 transition = {{ type = \"fx/mix.glsl\", duration = 1 }}\n{layers}"
            ),
            Path::new("/film"),
        )
    }

    #[test]
    fn layers_parse_and_resolve_spans() {
        // moon: 0–3, closeup: 2–5 (settled at 3).
        let m = layers_film(
            "[[layer]]\nid = \"title\"\nscene = \"layers/title.glsl\"\nfrom = \"moon + 0.5\"\nto = \"closeup.settled\"\nfade = [0.4, 0.6]\nscale = 1\nuniforms.uSize = 0.08\nchannels = [\"\", \"@input\"]\n\
             [[layer]]\nid = \"grain\"\ntype = \"fx/grain\"\nscale = 0.5\nopacity = 0.6\nparams = { amount = 0.3 }\n\
             [[layer]]\nid = \"bars\"\ntype = \"fx/bars.glsl\"\nfrom = \"title.end - 0.5\"\nto = 4.5\nopacity = { keys = [[0, 0.0], [1, 1.0]] }\n",
        )
        .unwrap();
        let [title, grain, bars] = &m.layers[..] else {
            panic!("three layers")
        };
        assert_eq!((title.start, title.end), (0.5, 3.0));
        assert_eq!(title.fade, (0.4, 0.6));
        assert_eq!(title.scale, Some(1.0));
        assert_eq!(
            title.scene,
            LayerScene::File(PathBuf::from("/film/layers/title.glsl"))
        );
        assert_eq!(title.channels[1].as_ref().unwrap().source, Source::Input);
        // No span: the whole film.
        assert_eq!((grain.start, grain.end), (0.0, 5.0));
        assert_eq!(grain.scene, LayerScene::Pass("fx/grain".into()));
        assert_eq!(grain.params, vec![("amount".to_string(), vec![0.3])]);
        assert_eq!(bars.start, 2.5);
        // Opacity: fades (smooth) times the keyframes / constant.
        let near = |a: f32, b: f32| (a - b).abs() < 1e-5;
        assert!(near(title.opacity_at(0.0), 0.0));
        assert!(near(title.opacity_at(0.2), 0.5));
        assert!(near(title.opacity_at(1.0), 1.0));
        assert!(near(title.opacity_at(2.5 - 0.3), 0.5));
        assert!(near(grain.opacity_at(3.0), 0.6));
        assert!(near(bars.opacity_at(0.5), 0.5));
        // Active layers: half-open spans, bottom to top, with layer-local times.
        assert_eq!(m.layers_at(0.25), vec![(1, 0.25)]);
        assert_eq!(m.layers_at(2.75), vec![(0, 2.25), (1, 2.75), (2, 0.25)]);
        assert_eq!(m.layers_at(3.0), vec![(1, 3.0), (2, 0.5)]);
        // Cues for layers; the contract asks for 0.1.4.
        let cues = m.cue_defines();
        assert!(cues.contains("#define CUE_TITLE_START 0.5\n"), "{cues}");
        assert!(cues.contains("#define CUE_TITLE_END 3.0\n"), "{cues}");
        let w = m.contract_warning().unwrap();
        assert!(w.contains("layers, which need shotglass 0.1.4"), "{w}");
        // A film without layers has exactly the cues it had.
        let plain = layers_film("").unwrap();
        assert!(plain.layers.is_empty() && !plain.cue_defines().contains("TITLE"));
    }

    #[test]
    fn blends_and_layer_references() {
        let m = layers_film(
            "[[layer]]\nid = \"bright-pass\"\nscene = \"l.glsl\"\nopacity = 0\n\
             [[layer]]\nid = \"screen\"\nscene = \"l.glsl\"\nblend = \"fx/screen\"\nchannels = [\"@bright-pass\", \"@picture\", \"@input\"]\n\
             [[layer]]\nid = \"local\"\nscene = \"l.glsl\"\nblend = \"fx/my-blend.glsl\"\n\
             [[layer]]\nid = \"plain\"\nscene = \"l.glsl\"\nblend = \"over\"\n",
        )
        .unwrap();
        let screen = &m.layers[1];
        assert_eq!(screen.blend.as_deref(), Some("fx/screen"));
        let src: Vec<&Source> = screen
            .channels
            .iter()
            .flatten()
            .map(|c| &c.source)
            .collect();
        assert_eq!(
            src,
            [
                &Source::Layer {
                    index: 0,
                    id: "bright-pass".into()
                },
                &Source::Picture,
                &Source::Input
            ]
        );
        assert_eq!(screen.channels[0].as_ref().unwrap().label(), "@bright-pass");
        assert_eq!(m.layers[2].blend.as_deref(), Some("fx/my-blend.glsl"));
        // "over" is the engine's source-over: no blend pass.
        assert_eq!(m.layers[3].blend, None);
        // Pass outputs aren't textures.
        assert!(m.textures().is_empty());
        let w = m.contract_warning().unwrap();
        assert!(
            w.contains("blend modes, @picture or @<layer>, which need shotglass 0.1.5"),
            "{w}"
        );
        // A later layer can't be read (the stack stays a DAG).
        let e = err(layers_film(
            "[[layer]]\nid = \"a\"\nscene = \"l.glsl\"\nchannels = [\"@b\"]\n\
             [[layer]]\nid = \"b\"\nscene = \"l.glsl\"\n",
        ));
        assert!(e.contains("\"@b\" is a later layer"), "{e}");
    }

    /// A film in a temp folder with comp files: `files` are (path, text).
    fn comps_film(movie: &str, files: &[(&str, &str)]) -> (tempfile::TempDir, Result<Movie>) {
        let dir = tempfile::tempdir().unwrap();
        for (p, text) in files {
            let f = dir.path().join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, text).unwrap();
        }
        let m = fixture(movie, dir.path());
        (dir, m)
    }

    const COMP_AB: &str = "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n\
         [[shot]]\nid = \"b\"\nscene = \"b.glsl\"\nduration = 1\n\
         transition = { type = \"x.glsl\", duration = 0.5 }\n\
         [[layer]]\nid = \"t\"\nscene = \"t.glsl\"\nfrom = \"b\"\n";

    #[test]
    fn comps_parse() {
        let (_d, m) = comps_film(
            "[comps]\nintro = \"comps/intro.toml\"\ntv = { file = \"comps/tv.toml\", size = [64, 36], loop = true }\n\
             [[shot]]\nid = \"open\"\ncomp = \"intro\"\nduration = 2\ntime_offset = 0.5\n\
             [[shot]]\nid = \"room\"\nscene = \"r.glsl\"\nduration = 2\n\
             channels = [\"@tv\", { source = \"@tv\", time_offset = 2, filter = \"nearest\" }]\n\
             [[layer]]\nid = \"lt\"\ncomp = \"intro\"\nfrom = 1\nopacity = 0.5\n\
             [[layer]]\nid = \"title\"\nscene = \"t.glsl\"\nmatte = { source = \"@lt\", channel = \"luma\", invert = true }\n\
             [[layer]]\nid = \"m2\"\nscene = \"t.glsl\"\nmatte = { source = \"@tv\" }\n",
            &[
                ("comps/intro.toml", COMP_AB),
                (
                    "comps/tv.toml",
                    "[[shot]]\nid = \"s\"\nscene = \"s.glsl\"\nduration = 0.5\n",
                ),
            ],
        );
        let m = m.unwrap();
        assert_eq!(m.comps.len(), 2);
        let (intro, tv) = (&m.comps[0], &m.comps[1]);
        assert_eq!(
            (intro.name.as_str(), intro.size, intro.looping),
            ("intro", None, false)
        );
        assert_eq!((tv.size, tv.looping), (Some((64, 36)), true));
        assert_eq!(intro.movie.duration, 1.5);
        assert_eq!(intro.movie.layers[0].start, 0.5);
        assert_eq!(tv.movie.loop_frames, Some(15));
        assert_eq!(m.shots[0].comp, Some(0));
        assert_eq!(m.shots[0].scene, m.root.join("comps/intro.toml"));
        let ch = m.shots[1].channels[1].as_ref().unwrap();
        assert_eq!(
            ch.source,
            Source::Comp {
                index: 1,
                name: "tv".into(),
                time_offset: 2.0
            }
        );
        assert_eq!(ch.filter, Filter::Nearest);
        assert_eq!(m.layers[0].scene, LayerScene::Comp(0));
        assert_eq!(
            m.layers[1].matte,
            Some(Matte {
                source: MatteSource::Layer(0),
                luma: true,
                invert: true
            })
        );
        assert_eq!(m.layers[2].matte.unwrap().source, MatteSource::Comp(1));
        // Comp cues are comp-local; the film's cues don't change.
        let c = intro.movie.comp_cue_defines();
        assert!(c.contains("#define COMP_DURATION 1.5\n"), "{c}");
        assert!(c.contains("#define COMP_CUE_B_START 0.5\n"), "{c}");
        assert!(c.contains("#define COMP_CUE_T_END 1.5\n"), "{c}");
        assert!(!m.cue_defines().contains("COMP_"));
        // The comp files are watched.
        let files = m.referenced_files();
        assert!(files.contains(&m.root.join("comps/tv.toml")));
        assert!(files.contains(&m.root.join("s.glsl")));
        // Comps and mattes need 0.1.7.
        let w = m.contract_warning().unwrap();
        assert!(
            w.contains("comps or mattes, which need shotglass 0.1.7"),
            "{w}"
        );
        // Glyph runs need 0.1.8.
        let runs = texts_film(
            "body = \"fonts/Body.ttf\"\n",
            "run = { text = \"a\", font = \"body\", glyphs = true }\n",
            &["channels = [\"run\"]"],
        )
        .unwrap();
        let w = runs.contract_warning().unwrap();
        assert!(
            w.contains("glyph runs, glyph sets or subtitles, which need shotglass 0.1.8"),
            "{w}"
        );
    }

    #[test]
    fn comp_time_holds_or_wraps() {
        let (_d, m) = comps_film(
            "[comps]\nhold = \"h.toml\"\nwrap = { file = \"h.toml\", loop = true }\n\
             [[shot]]\nid = \"s\"\ncomp = \"hold\"\nduration = 1\n",
            &[(
                "h.toml",
                "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n",
            )],
        );
        let m = m.unwrap();
        let (hold, wrap) = (&m.comps[0], &m.comps[1]);
        let last = m.frame_time(29);
        assert_eq!(hold.local(-3.0), 0.0);
        assert_eq!(hold.local(0.25), 0.25);
        assert_eq!(hold.local(5.0), last);
        assert!((wrap.local(1.25) - 0.25).abs() < 1e-6);
        assert!((wrap.local(-0.25) - 0.75).abs() < 1e-6);
    }

    #[test]
    fn comps_are_validated() {
        let one = "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n";
        for (movie, files, says) in [
            (
                "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"y\"\nduration = 1\n",
                vec![("x.toml", one)],
                "comp \"y\": no [comps] entry called that (there are \"x\")",
            ),
            (
                "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nscene = \"a.glsl\"\nduration = 1\n",
                vec![("x.toml", one)],
                "give scene or comp, not both",
            ),
            (
                "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\nchannels = [\"\"]\n",
                vec![("x.toml", one)],
                "a comp shot has no shader of its own",
            ),
            (
                "[comps]\nx = \"missing.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\n",
                vec![],
                "[comps] x: reading missing.toml",
            ),
            (
                "[comps]\nx = \"x.png\"\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\n",
                vec![],
                "a comp file is a .toml",
            ),
            (
                "[comps]\nx = { file = \"x.toml\", size = [0, 4] }\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\n",
                vec![("x.toml", one)],
                "size must be [width, height]",
            ),
            (
                "[comps]\nx = { file = \"x.toml\", speed = 2 }\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\n",
                vec![("x.toml", one)],
                "unknown option `speed` (a comp has file, size and loop)",
            ),
            (
                "[comps]\ninput = \"x.toml\"\n[[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 1\n",
                vec![("x.toml", one)],
                "isn't input or picture",
            ),
            (
                "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\n",
                vec![(
                    "x.toml",
                    "fps = 24\n[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n",
                )],
                "`fps` belongs to the film",
            ),
            (
                "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\n",
                vec![(
                    "x.toml",
                    "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\nsound = \"s.glsl\"\n",
                )],
                "comps are picture only",
            ),
            (
                "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\n",
                vec![("x.toml", "")],
                "a comp needs at least one [[shot]]",
            ),
            (
                "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\n",
                vec![(
                    "x.toml",
                    "[[shot]]\nid = \"a\"\ncomp = \"x\"\nduration = 1\n",
                )],
                "comp \"x\" can't show itself",
            ),
            (
                "[comps]\nx = \"x.toml\"\ny = \"y.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\n",
                vec![
                    (
                        "x.toml",
                        "[[shot]]\nid = \"a\"\ncomp = \"y\"\nduration = 1\n",
                    ),
                    (
                        "y.toml",
                        "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\nchannels = [\"@x\"]\n",
                    ),
                ],
                "comps show each other in a cycle: x → y → x",
            ),
            (
                "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 1\n[[layer]]\nid = \"x\"\nscene = \"l.glsl\"\n",
                vec![("x.toml", one)],
                "layers and comps share @ names",
            ),
            (
                "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 1\nchannels = [{ source = \"@x\", filter = \"mipmap\" }]\n",
                vec![("x.toml", one)],
                "filter \"mipmap\" isn't available on @ sources",
            ),
            (
                "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 1\n[[layer]]\nid = \"l\"\nscene = \"l.glsl\"\nchannels = [{ source = \"@input\", time_offset = 1 }]\n",
                vec![("x.toml", one)],
                "time_offset is for comps",
            ),
            (
                "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 1\n[[layer]]\nid = \"l\"\ncomp = \"x\"\nuniforms.u = 1\n",
                vec![("x.toml", one)],
                "a comp layer has no shader of its own",
            ),
        ] {
            let (_d, m) = comps_film(movie, &files);
            let e = err(m);
            assert!(e.contains(says), "{movie}: {e}");
        }
        // Five levels deep is refused; four are fine.
        let chain = |n: usize| {
            let mut files = vec![];
            let mut comps = String::from("[comps]\n");
            for i in 0..n {
                comps += &format!("c{i} = \"c{i}.toml\"\n");
                let body = if i + 1 < n {
                    format!(
                        "[[shot]]\nid = \"a\"\ncomp = \"c{}\"\nduration = 1\n",
                        i + 1
                    )
                } else {
                    "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n".to_string()
                };
                files.push((format!("c{i}.toml"), body));
            }
            let movie = format!("{comps}[[shot]]\nid = \"s\"\ncomp = \"c0\"\nduration = 1\n");
            let files: Vec<(&str, &str)> = files
                .iter()
                .map(|(a, b)| (a.as_str(), b.as_str()))
                .collect();
            let (_d, m) = comps_film(&movie, &files);
            m
        };
        assert!(chain(4).is_ok());
        let e = err(chain(5));
        assert!(e.contains("at most 4 levels"), "{e}");
    }

    #[test]
    fn layers_are_validated() {
        for (layer, says) in [
            (
                "id = \"moon\"\nscene = \"l.glsl\"",
                "the id is taken; shots and layers share ids",
            ),
            ("id = \"a b\"\nscene = \"l.glsl\"", "may only use letters"),
            ("id = \"x\"", "a layer needs scene = \"…\""),
            (
                "id = \"x\"\nscene = \"l.glsl\"\ntype = \"fx/grain\"",
                "give one of scene, type and comp",
            ),
            (
                "id = \"x\"\ntype = \"grain\"",
                "needs a library (\"fx/grain\")",
            ),
            (
                "id = \"x\"\ntype = \"kit/grain\"",
                "names library \"kit\", but [libraries] has \"fx\"",
            ),
            (
                "id = \"x\"\nscene = \"l.glsl\"\nfrom = \"mooon\"",
                "no shot or earlier layer called \"mooon\" (shots: moon, closeup)",
            ),
            (
                "id = \"x\"\nscene = \"l.glsl\"\nfrom = \"moon.middle\"",
                "a shot has .start, .settled and .end",
            ),
            (
                "id = \"x\"\nscene = \"l.glsl\"\nfrom = \"moon + soon\"",
                "the offset after + must be seconds",
            ),
            (
                "id = \"x\"\nscene = \"l.glsl\"\nfrom = 3\nto = 2",
                "must lie within the film (0–5.0 s), from before to",
            ),
            (
                "id = \"x\"\nscene = \"l.glsl\"\nto = 9",
                "must lie within the film",
            ),
            (
                "id = \"x\"\nscene = \"l.glsl\"\nfrom = 1\nto = 2\nfade = [0.8, 0.8]",
                "must be ≥ 0 and fit the span",
            ),
            (
                "id = \"x\"\nscene = \"l.glsl\"\nscale = 2",
                "scale must be \"auto\" or a fraction",
            ),
            (
                "id = \"x\"\nscene = \"l.glsl\"\nscale = \"half\"",
                "scale must be \"auto\" or a fraction",
            ),
            (
                "id = \"x\"\nscene = \"l.glsl\"\nopacity = [1, 2]",
                "opacity is one number",
            ),
            (
                "id = \"x\"\nscene = \"l.glsl\"\nblend = \"screen\"",
                "blend \"screen\" needs a library (\"blend/screen\") or a film file",
            ),
            (
                "id = \"x\"\nscene = \"l.glsl\"\ncomp = \"intro\"",
                "give one of scene, type and comp",
            ),
            (
                "id = \"x\"\ncomp = \"intro\"",
                "comp \"intro\": no [comps] entry called that (there are none)",
            ),
            (
                "id = \"x\"\nscene = \"l.glsl\"\nmatte = { source = \"@t\" }",
                "source \"@t\" is neither an earlier layer nor a comp",
            ),
            (
                "id = \"x\"\nscene = \"l.glsl\"\nmatte = { source = \"t\" }",
                "must be \"@<earlier layer>\" or \"@<comp>\"",
            ),
            (
                "id = \"x\"\nscene = \"l.glsl\"\ntransition_in = { type = \"fx/iris\" }",
                "layer transitions arrive in a later phase",
            ),
            (
                "id = \"x\"\nscene = \"l.glsl\"\nchannels = [\"@nobody\"]",
                "\"@nobody\" is not a layer below this one",
            ),
            (
                "id = \"x\"\nscene = \"l.glsl\"\nchannels = [\"@x\"]",
                "\"@x\" is this layer; a layer can't read itself",
            ),
            (
                "id = \"x\"\nscene = \"l.glsl\"\nblend = \"kit/screen\"",
                "blend \"kit/screen\" names library \"kit\", but [libraries] has \"fx\"",
            ),
            (
                "id = \"x\"\nscene = \"l.glsl\"\nopactiy = 1",
                "unknown field `opactiy`",
            ),
        ] {
            let e = err(layers_film(&format!("[[layer]]\n{layer}\n")));
            assert!(e.contains(says), "{layer}: {e}");
        }
        // Layers can't be spanned by a later layer's id; a shot can't read @input.
        let e = err(layers_film(
            "[[layer]]\nid = \"a\"\nscene = \"l.glsl\"\nto = \"b.end\"\n[[layer]]\nid = \"b\"\nscene = \"l.glsl\"\n",
        ));
        assert!(e.contains("no shot or earlier layer called \"b\""), "{e}");
        let e = err(fixture(
            "[[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 1\nchannels = [\"@input\"]\n",
            Path::new("/film"),
        ));
        assert!(
            e.contains("\"@input\" is another pass's picture; only a [[layer]]'s channels can read one (a shot has nothing below it)"),
            "{e}"
        );
        // A loop film's layers stay inside one period.
        let e = err(fixture(
            "loop = true\n[[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 2\n[[layer]]\nid = \"x\"\nscene = \"l.glsl\"\nfrom = 1\nto = 3\n",
            Path::new("/film"),
        ));
        assert!(
            e.contains("a loop film's layers stay inside one period"),
            "{e}"
        );
    }

    fn texts_film(fonts: &str, texts: &str, shots: &[&str]) -> Result<Movie> {
        let mut t = format!(
            "[libraries]\nkit = {{ path = \"lib/kit\" }}\n[textures]\nbrick = \"t/brick.png\"\n[fonts]\n{fonts}[texts]\n{texts}"
        );
        let shots = if shots.is_empty() { &[""][..] } else { shots };
        for (i, extra) in shots.iter().enumerate() {
            t += &format!("[[shot]]\nid = \"s{i}\"\nscene = \"a.glsl\"\nduration = 1\n{extra}\n");
        }
        fixture(&t, Path::new("/film"))
    }

    #[test]
    fn fonts_and_texts_parse() {
        let m = texts_film(
            "body = \"fonts/Body.ttf\"\nbold = { file = \"<kit/fonts/Var.ttf>\", variations = { wght = 700 }, fallback = [\"body\", \"hebrew\"], licence = \"OFL-1.1\" }\nhebrew = { file = \"fonts/Hebrew.otf\", index = 1 }\n",
            "title = { text = \"Big Shadow\", font = \"bold\", size = 160, align = \"center\", width = 1400, line_height = 1.25, letter_spacing = 0.05, max_lines = 2, direction = \"auto\", features = [\"ss01\", \"-liga\"], sdf_spread = 12 }\ncredits = { file = \"credits.txt\", font = \"body\" }\n",
            &["channels = [\"title\", \"brick\", { text = \"credits\", wrap = \"repeat\" }, \"\", \"\", \"\", \"\", \"title\"]"],
        )
        .unwrap();
        let c = &m.shots[0].channels;
        let title = c[0].as_ref().unwrap();
        let t = title.text().unwrap();
        assert_eq!(t.name, "title");
        assert_eq!(t.body, TextBody::Inline("Big Shadow".into()));
        // The chain: the font, then its fallbacks depth first.
        let chain: Vec<&str> = t.fonts.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(chain, ["bold", "body", "hebrew"]);
        assert_eq!(t.fonts[0].variations, vec![("wght".to_string(), 700.0)]);
        assert_eq!(t.fonts[0].file.library.as_deref(), Some("kit"));
        assert_eq!(t.fonts[0].licence.as_deref(), Some("OFL-1.1"));
        assert_eq!(t.fonts[2].index, 1);
        assert_eq!(t.fonts[1].file.file, PathBuf::from("/film/fonts/Body.ttf"));
        let s = &t.style;
        assert_eq!(
            (s.size, s.width, s.line_height, s.letter_spacing, s.spread),
            (160.0, Some(1400.0), 1.25, 0.05, 12.0)
        );
        assert_eq!(
            (s.align, s.max_lines, s.direction),
            (
                crate::text::Align::Center,
                Some(2),
                crate::text::Direction::Auto
            )
        );
        assert_eq!(s.features, ["ss01", "-liga"]);
        // Text defaults suit text: linear, clamp, upright.
        assert_eq!(
            (title.filter, title.wrap, title.vflip, title.srgb),
            (Filter::Linear, Wrap::Clamp, true, false)
        );
        assert_eq!(title.label(), "text \"title\"");
        // Images and texts mix; a table can re-sample a text; channel 7 is usable.
        assert!(c[1].as_ref().unwrap().image().is_some());
        let credits = c[2].as_ref().unwrap();
        assert_eq!(credits.wrap, Wrap::Repeat);
        assert!(
            matches!(&credits.text().unwrap().body, TextBody::File(f) if f.label == "credits.txt")
        );
        assert_eq!(c[7].as_ref(), Some(title));
        // Files a text reads: its file and every font.
        let files: Vec<&str> = credits.files().iter().map(|f| f.label.as_str()).collect();
        assert_eq!(files, ["credits.txt", "fonts/Body.ttf"]);
        // Text and channels 4..7 need 0.1.3.
        let w = m.contract_warning().unwrap();
        assert!(
            w.contains("text ([fonts] and [texts]), which needs shotglass 0.1.3"),
            "{w}"
        );
        assert!(
            w.contains("iChannel4..7, which need shotglass 0.1.3"),
            "{w}"
        );
        assert!(w.contains("shotglass = \"0.1.3\""), "{w}");
    }

    #[test]
    fn fonts_and_texts_are_validated() {
        let body = "body = \"fonts/Body.ttf\"\n";
        let text_err = |texts: &str| err(texts_film(body, texts, &[]));
        for (texts, says) in [
            (
                "t = { font = \"body\" }\n",
                "a text needs text = \"…\", file = \"…\" (UTF-8) or subtitles",
            ),
            (
                "t = { text = \"a\", file = \"a.txt\", font = \"body\" }\n",
                "give one of text, file or subtitles (this one has text and file)",
            ),
            ("t = { text = \"a\" }\n", "a text needs font = \"…\""),
            (
                "t = { text = \"a\", font = \"bodyy\" }\n",
                "font \"bodyy\" isn't in [fonts] (there are \"body\")",
            ),
            (
                "t = { text = \"a\", font = \"body\", size = 2 }\n",
                "size must be 4–1024",
            ),
            (
                "t = { text = \"a\", font = \"body\", width = 0 }\n",
                "width must be 1–4096",
            ),
            (
                "t = { text = \"a\", font = \"body\", align = \"justify\" }\n",
                "align \"justify\" isn't one of \"left\", \"center\", \"right\", \"start\", \"end\"",
            ),
            (
                "t = { text = \"a\", font = \"body\", direction = \"ttb\" }\n",
                "direction \"ttb\" isn't one of",
            ),
            (
                "t = { text = \"a\", font = \"body\", max_lines = 0 }\n",
                "max_lines must be a whole number ≥ 1",
            ),
            (
                "t = { text = \"a\", font = \"body\", features = [\"not a feature!\"] }\n",
                "isn't an OpenType feature",
            ),
            (
                "t = { text = \"a\", font = \"body\", srgb = true }\n",
                "srgb is for images",
            ),
            (
                "t = { text = \"a\", font = \"body\", colour = \"red\" }\n",
                "unknown option `colour`",
            ),
            (
                "t = { text = \"a\", font = \"body\", glyphs = \"0123\" }\n",
                "add set = true",
            ),
            (
                "t = { font = \"body\", set = true }\n",
                "set = true needs the characters as glyphs = \"0123456789:.\"",
            ),
            (
                "t = { text = \"a\", font = \"body\", glyphs = \"01\", set = true }\n",
                "text doesn't go with set = true",
            ),
            (
                "t = { font = \"body\", glyphs = \"\", set = true }\n",
                "has no characters",
            ),
            (
                "t = { font = \"body\", glyphs = 3 }\n",
                "glyphs must be true, false or",
            ),
            (
                "t = { subtitles = \"subs.srt\", font = \"body\" }\n",
                "subtitles are drawn as a glyph run; add glyphs = true",
            ),
            (
                "t = { subtitles = \"subs.txt\", font = \"body\", glyphs = true }\n",
                "must be a .srt or .vtt file",
            ),
            (
                "t = { text = \"a\", subtitles = \"s.srt\", font = \"body\", glyphs = true }\n",
                "give one of text, file or subtitles (this one has text and subtitles)",
            ),
            (
                "t = { text = \"a\", font = \"body\", atlas_size = 64 }\n",
                "atlas_size is for glyph runs",
            ),
            (
                "t = { text = \"a\", font = \"body\", glyphs = true, atlas_size = 4 }\n",
                "atlas_size must be 8–256",
            ),
            (
                "t = { text = \"a\", font = \"body\", glyphs = true, filter = \"mipmap\" }\n",
                "a glyph atlas isn't mipmapped",
            ),
            (
                "t = { text = \"a\", font = \"body\", glyphs = true, vflip = false }\n",
                "vflip isn't for glyph runs",
            ),
            (
                "t = { font = \"body\", glyphs = \"01\", set = true, width = 100 }\n",
                "width and max_lines are for laid-out text, not a glyph set",
            ),
            ("t = \"Hello\"\n", "a text is a table"),
            (
                "brick = { text = \"a\", font = \"body\" }\n",
                "[texts] brick: [textures] has a \"brick\" too",
            ),
            (
                "my-text = { text = \"a\", font = \"body\" }\n",
                "a text name may only use letters",
            ),
        ] {
            let e = text_err(texts);
            assert!(e.contains(says), "{texts}: {e}");
        }
        for (fonts, says) in [
            ("f = \"fonts/a.woff2\"\n", "WOFF/WOFF2 fonts aren't read"),
            (
                "f = \"fonts/a.png\"\n",
                "shotglass reads TrueType and OpenType fonts",
            ),
            (
                "f = { file = \"a.ttf\", fallback = [\"nope\"] }\n",
                "fallback \"nope\" isn't in [fonts]",
            ),
            (
                "f = { file = \"a.ttf\", variations = { weight = 7 } }\n",
                "\"weight\" isn't an axis tag",
            ),
            (
                "f = { file = \"a.ttf\", index = -1 }\n",
                "index must be a whole number ≥ 0",
            ),
            (
                "f = { file = \"a.ttf\", size = 3 }\n",
                "unknown option `size`",
            ),
            ("f = { index = 1 }\n", "a font table needs file"),
            ("f = \"../a.ttf\"\n", "climbs"),
        ] {
            let e = err(texts_film(fonts, "", &[]));
            assert!(e.contains(says), "{fonts}: {e}");
        }
        // A fallback cycle is fine: each font once.
        let m = texts_film(
            "a = { file = \"a.ttf\", fallback = [\"b\"] }\nb = { file = \"b.ttf\", fallback = [\"a\"] }\n",
            "t = { text = \"x\", font = \"a\" }\n",
            &["channels = [\"t\"]"],
        )
        .unwrap();
        let t = m.shots[0].channels[0].as_ref().unwrap().text().unwrap();
        let names: Vec<&str> = t.fonts.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        // Channel tables: text names a [texts] entry; srgb stays refused.
        let one = "t = { text = \"a\", font = \"body\" }\n";
        let e = err(texts_film(
            body,
            one,
            &["channels = [{ text = \"brick\" }]"],
        ));
        assert!(
            e.contains("text = \"brick\": no [texts] entry called \"brick\""),
            "{e}"
        );
        let e = err(texts_film(
            body,
            one,
            &["channels = [{ text = \"t\", srgb = true }]"],
        ));
        assert!(e.contains("srgb is for images"), "{e}");
        // Unused texts are pointed out like unused textures.
        let m = texts_film(body, one, &["channels = [\"brick\"]"]).unwrap();
        assert_eq!(
            m.warnings,
            vec!["[texts] t isn't used by any shot's channels".to_string()]
        );
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
            crate::project::write_fixture(
                &dir,
                &format!("[libraries]\nkit = {{ path = \"lib/kit\" }}\n[[shot]]\nid = \"sky\"\nscene = \"a.glsl\"\nduration = 1\nchannels = {channels}\n"),
            )
            .unwrap();
            Movie::load(&dir)
        };
        let m = film("[\"<kit/textures/noise.png>\", \"textures/stars.png\"]").unwrap();
        assert_eq!(
            m.shots[0].channels[0]
                .as_ref()
                .unwrap()
                .image()
                .unwrap()
                .file,
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
    fn glyph_runs_bind_their_atlas_and_table() {
        let m = texts_film(
            "body = \"fonts/Body.ttf\"\n",
            "credits = { file = \"credits.txt\", font = \"body\", glyphs = true, atlas_size = 64 }\n\
             digits = { glyphs = \"0123456789:\", set = true, font = \"body\" }\n\
             subs = { subtitles = \"subs.vtt\", font = \"body\", width = 900, glyphs = true }\n\
             title = { text = \"t\", font = \"body\" }\n",
            &["channels = [\"credits\", { text = \"credits\", part = \"glyphs\" }, \"digits\", \
               { text = \"digits\", part = \"glyphs\" }, { text = \"subs\", part = \"atlas\" }, \
               { text = \"subs\", part = \"glyphs\" }, \"title\"]"],
        )
        .unwrap();
        let c = &m.shots[0].channels;
        let t = |i: usize| c[i].as_ref().unwrap().text().unwrap();
        assert_eq!(
            (t(0).glyphs, t(0).part, t(0).atlas_size),
            (GlyphMode::Run, TextPart::Main, 64.0)
        );
        assert_eq!((t(1).glyphs, t(1).part), (GlyphMode::Run, TextPart::Table));
        assert_eq!((t(2).glyphs, t(3).part), (GlyphMode::Set, TextPart::Table));
        assert_eq!(t(2).body, TextBody::Inline("0123456789:".into()));
        assert_eq!(t(2).atlas_size, ATLAS_SIZE);
        assert!(matches!(&t(4).body, TextBody::Subtitles(f) if f.label == "subs.vtt"));
        assert_eq!((t(6).glyphs, t(6).part), (GlyphMode::Block, TextPart::Main));
        // Atlases filter linearly; tables are nearest + clamp; glyph runs never flip;
        // their layout isn't limited to one texture's height.
        let ch = |i: usize| c[i].as_ref().unwrap();
        assert_eq!((ch(0).filter, ch(0).vflip), (Filter::Linear, false));
        assert_eq!(
            (ch(1).filter, ch(1).wrap, ch(1).vflip),
            (Filter::Nearest, Wrap::Clamp, false)
        );
        assert!(t(0).style.tall && !t(6).style.tall);
        assert!(ch(6).vflip);
        // The subtitle file is one of the channel's files (watched, bundled).
        assert!(ch(4).files().iter().any(|f| f.label == "subs.vtt"));

        let bind_err = |entry: &str| {
            err(texts_film(
                "body = \"fonts/Body.ttf\"\n",
                "run = { text = \"a\", font = \"body\", glyphs = true }\nblock = { text = \"a\", font = \"body\" }\n",
                &[&format!("channels = [{entry}]")],
            ))
        };
        for (entry, says) in [
            (
                r#"{ text = "block", part = "glyphs" }"#,
                "part = \"glyphs\" is for glyph runs; text \"block\" is a block",
            ),
            (
                r#"{ text = "run", part = "table" }"#,
                "part must be \"atlas\" or \"glyphs\"",
            ),
            (
                r#"{ text = "run", part = "glyphs", filter = "linear" }"#,
                "filter doesn't apply",
            ),
            (
                r#"{ file = "a.png", part = "glyphs" }"#,
                "it goes with text = \"…\"",
            ),
        ] {
            let e = bind_err(entry);
            assert!(e.contains(says), "{entry}: {e}");
        }
    }

    #[test]
    fn glyph_runs_load_lay_out_and_check_subtitle_times() {
        use crate::glyphrun::{HEADER_TEXELS, KIND_RUN, KIND_SET, TABLE_WIDTH, decode};
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        std::fs::create_dir_all(dir.join("fonts")).unwrap();
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/films/text/fonts/NotoSans-subset.ttf"),
            dir.join("fonts/sans.ttf"),
        )
        .unwrap();
        std::fs::write(dir.join("a.glsl"), "").unwrap();
        std::fs::write(
            dir.join("subs.srt"),
            "1\n00:00:00,500 --> 00:00:01,000\nBig Shadow\n\n2\n00:00:01,000 --> 00:00:01,900\nthe wall\n",
        )
        .unwrap();
        let film = |duration: f32| {
            crate::project::write_fixture(
                dir,
                &format!(
                    "[fonts]\nsans = \"fonts/sans.ttf\"\n[texts]\n\
                     run = {{ text = \"Big Shadow\\nover the wall\", font = \"sans\", size = 40, glyphs = true }}\n\
                     digits = {{ glyphs = \"0123456789\", set = true, font = \"sans\" }}\n\
                     subs = {{ subtitles = \"subs.srt\", font = \"sans\", glyphs = true }}\n\
                     [[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = {duration}\n\
                     channels = [\"run\", {{ text = \"run\", part = \"glyphs\" }}, \"digits\", \
                     {{ text = \"digits\", part = \"glyphs\" }}, {{ text = \"subs\", part = \"glyphs\" }}]\n"
                ),
            )
            .unwrap();
            Movie::load(dir)
        };
        let m = film(2.0).unwrap();
        let text = |i: usize| {
            m.shots[0].channels[i]
                .as_ref()
                .unwrap()
                .text()
                .unwrap()
                .clone()
        };
        // The decode worker's output: the atlas, and a table a shader can read.
        let atlas = crate::texture::load_text(&text(0), true).unwrap();
        assert_eq!(atlas.meta, [ATLAS_SIZE, ATLAS_SPREAD, 40.0, 0.0]);
        assert!(atlas.width > 0 && atlas.rgba.len() == atlas.width * atlas.height * 4);
        let table = crate::texture::load_text(&text(1), true).unwrap();
        assert_eq!(table.width, TABLE_WIDTH);
        assert_eq!(
            table.meta,
            [TABLE_WIDTH as f32, HEADER_TEXELS as f32, 40.0, ATLAS_SPREAD]
        );
        let texels: Vec<[u8; 4]> = table
            .rgba
            .chunks(4)
            .map(|c| c.try_into().unwrap())
            .collect();
        let d = decode(&texels).unwrap();
        assert_eq!((d.kind, d.lines.len(), d.glyphs.len()), (KIND_RUN, 2, 23));
        let set: Vec<[u8; 4]> = crate::texture::load_text(&text(3), true)
            .unwrap()
            .rgba
            .chunks(4)
            .map(|c| c.try_into().unwrap())
            .collect();
        assert_eq!(decode(&set).unwrap().kind, KIND_SET);
        let subs: Vec<[u8; 4]> = crate::texture::load_text(&text(4), true)
            .unwrap()
            .rgba
            .chunks(4)
            .map(|c| c.try_into().unwrap())
            .collect();
        assert_eq!(
            decode(&subs).unwrap().entries,
            [(500, 1000, 0, 1), (1000, 1900, 1, 1)]
        );
        // The atlas and table of one text share one build; editing a file it
        // reads makes a new one.
        let a = text(0).glyph_run_shared().unwrap();
        let b = text(1).glyph_run_shared().unwrap();
        assert!(std::sync::Arc::ptr_eq(&a, &b));
        let before = text(4).glyph_run_shared().unwrap();
        std::fs::write(
            dir.join("subs.srt"),
            "1\n00:00:00,500 --> 00:00:01,000\nBig Shadows\n\n\
             2\n00:00:01,000 --> 00:00:01,900\nthe walls\n",
        )
        .unwrap();
        let after = text(4).glyph_run_shared().unwrap();
        assert!(!std::sync::Arc::ptr_eq(&before, &after));
        assert_eq!(decode(after.table.data()).unwrap().entries.len(), 2);
        // Bound only through a channel table, the texts still count as used.
        assert!(m.warnings.is_empty(), "{:?}", m.warnings);
        // Memory accounting sees every part.
        let (bytes, parts) = crate::texture::shot_memory(&m.shots[0].channels);
        assert!(bytes > 0 && parts.len() == 5, "{parts:?}");
        // A cue past the end of the film refuses the version.
        let e = format!("{:#}", film(1.5).unwrap_err());
        assert!(
            e.contains("the cue at 0:00:01.000 ends at 0:00:01.900, after the film's 0:00:01.500"),
            "{e}"
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        let bad = "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=1\nlenght=2\n";
        assert!(fixture(bad, Path::new(".")).is_err());
    }

    fn loop_film(extra: &str, shots: &str) -> Result<Movie> {
        fixture(&format!("loop = true\n{extra}\n{shots}"), Path::new("."))
    }

    #[test]
    fn loop_field_sets_a_whole_frame_period() {
        let m = loop_film("", M).unwrap();
        // 5 + 4 + 3 - 1 (transition) = 11 s = 330 frames at 30 fps
        assert_eq!(m.loop_frames, Some(330));
        assert_eq!(m.frame_count(), 330);
        assert_eq!(m.loop_period(), Some(11.0));
        assert_eq!(m.sound_samples(48000), 528000);
        assert_eq!(m.sound_samples(44100), 485100);
        let m = fixture(M, Path::new(".")).unwrap();
        assert_eq!(m.loop_frames, None);
        assert_eq!(m.loop_period(), None);
        let m = fixture(&format!("loop = false\n{M}"), Path::new(".")).unwrap();
        assert_eq!(m.loop_frames, None);
        // NTSC: 300 frames at 30000/1001 = 10.01 s; the period is exact, the samples round.
        let one = "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 10.01\n";
        let m = loop_film("fps = \"30000/1001\"", one).unwrap();
        assert_eq!(m.loop_frames, Some(300));
        assert!((m.loop_period().unwrap() - 10.01).abs() < 1e-12);
        assert_eq!(m.sound_samples(48000), 480480);
    }

    /// `loop = "warm"` (#138): a loop film (or comp) whose seam-spanning buffers carry
    /// their state over the seam.
    #[test]
    fn loop_warm() {
        let one = "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 2\n";
        let buf = "[[buffer]]\nid = \"t\"\nscene = \"b.glsl\"\nchannels = [\"@prev\"]\n";
        let m = fixture(
            &format!("shotglass = \"0.1.10\"\nloop = \"warm\"\n{one}{buf}"),
            Path::new("."),
        )
        .unwrap();
        assert_eq!((m.loop_frames, m.warm), (Some(60), true));
        assert_eq!(warm_buffers(&m).count(), 1);
        assert!(m.warnings.is_empty(), "{:?}", m.warnings);
        assert_eq!(m.contract_warning(), None);
        let m = fixture(
            &format!("shotglass = \"0.1.9\"\nloop = \"warm\"\n{one}{buf}"),
            Path::new("."),
        )
        .unwrap();
        assert!(m.contract_warning().unwrap().contains("loop = \"warm\""));
        // A plain loop isn't warm.
        let m = loop_film("", one).unwrap();
        assert!(!m.warm);
        // No buffer spans the seam: a warning, not an error.
        let late = "[[buffer]]\nid = \"t\"\nscene = \"b.glsl\"\nfrom = 1\n";
        let m = fixture(&format!("loop = \"warm\"\n{one}{late}"), Path::new(".")).unwrap();
        assert!(
            m.warnings.iter().any(|w| w.contains("has no effect")),
            "{:?}",
            m.warnings
        );
        assert_eq!(warm_buffers(&m).count(), 0);
        // Anything else is refused.
        for bad in ["\"hot\"", "1", "[true]"] {
            let e = err(fixture(&format!("loop = {bad}\n{one}"), Path::new(".")));
            assert!(
                e.contains("loop must be true, false or \"warm\""),
                "{bad}: {e}"
            );
        }
        // Comps too.
        let (_d, m) = comps_film(
            "[comps]\nc = { file = \"c.toml\", loop = \"warm\" }\n\
             [[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n",
            &[("c.toml", &format!("{one}{buf}"))],
        );
        let m = m.unwrap();
        assert!(m.comps[0].looping && m.comps[0].movie.warm && !m.warm);
        let (_d, m) = comps_film(
            "[comps]\nc = { file = \"c.toml\", loop = \"hot\" }\n\
             [[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n",
            &[("c.toml", one)],
        );
        assert!(err(m).contains("loop must be true, false or \"warm\""));
    }

    #[test]
    fn loop_period_must_be_whole_frames() {
        let one = |d: &str| format!("[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = {d}\n");
        let e = loop_film("", &one("2.01")).unwrap_err().to_string();
        assert!(e.contains("whole number of frames"), "{e}");
        assert!(e.contains("60.300 frames at 30 fps"), "{e}");
        assert!(
            e.contains("2.0000s (60 frames) or 2.0333s (61 frames)"),
            "{e}"
        );
        let e = loop_film("fps = 24", &one("0.01")).unwrap_err().to_string();
        assert!(e.contains("(1 frames)"), "{e}");
        // Without loop, any duration is fine.
        assert!(fixture(&one("2.01"), Path::new(".")).is_ok());
        // A typo'd value is a type error, not silently ignored.
        assert!(fixture(&format!("loop = 3\n{}", one("2")), Path::new(".")).is_err());
    }

    #[test]
    fn loop_refuses_sound_across_the_seam() {
        let shots = |a: &str, b: &str| {
            format!(
                "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 2\nsound = \"a.glsl\"\n{a}\n\
                 [[shot]]\nid = \"b\"\nscene = \"b.glsl\"\nduration = 2\nsound = \"b.glsl\"\n{b}\n"
            )
        };
        let e = loop_film("", &shots("sound_lead = 0.5", ""))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("shot \"a\": sound_lead") && e.contains("seam"),
            "{e}"
        );
        let e = loop_film("", &shots("", "sound_tail = 0.5"))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("shot \"b\": sound_tail") && e.contains("seam"),
            "{e}"
        );
        // Inside the film (between the shots) they're fine, and so is either without loop.
        assert!(loop_film("", &shots("sound_tail = 0.5", "sound_lead = 0.5")).is_ok());
        assert!(
            fixture(
                &shots("sound_lead = 0.5", "sound_tail = 0.5"),
                Path::new(".")
            )
            .is_ok()
        );
    }

    #[test]
    fn loop_time_wraps_to_the_period() {
        let m = loop_film("", M).unwrap();
        assert_eq!(m.wrap(0.0), 0.0);
        assert_eq!(m.wrap(3.25), 3.25);
        assert_eq!(m.wrap(11.0), 0.0);
        assert_eq!(m.wrap(14.25), 3.25);
        assert_eq!(m.wrap(-1.0), 10.0);
        // Far out, an f32 time is coarse, but it wraps into the period, never onto it.
        let w = m.wrap(1.0e7);
        assert!((0.0..11.0).contains(&w), "{w}");
        assert_eq!(m.wrap_frame(329), 329);
        assert_eq!(m.wrap_frame(330), 0);
        assert_eq!(m.wrap_frame(330 * 1000 + 7), 7);
        // f64: hours in, precise to well under a sample.
        let p = 11.0;
        let t = 1000.0 * p + 3.25;
        assert!((wrap_time(t, p) - 3.25).abs() < 1e-9);
        assert!((0.0..p).contains(&wrap_time(p * 3.0 - 1e-15, p)));
        // Frames of loop k land on the frames of loop 0 (a few loops in: an f32 time
        // 1000 loops in is too coarse for that, which is why the player and export
        // wrap in f64 or by frame number).
        for k in [1u64, 2, 3] {
            for n in [0u64, 1, 165, 329] {
                let t = m.frame_time(k * 330 + n);
                assert_eq!(m.frame_at(m.wrap(t)), n as i64, "loop {k} frame {n}");
            }
        }
        // Not a loop film: unchanged.
        let m = fixture(M, Path::new(".")).unwrap();
        assert_eq!(m.wrap(14.25), 14.25);
        assert_eq!(m.wrap_frame(400), 400);
    }

    #[test]
    fn loop_needs_contract_0_1_2() {
        let m = loop_film("", M).unwrap();
        let w = m.contract_warning().unwrap();
        assert!(
            w.contains("loop") && w.contains("shotglass = \"0.1.2\""),
            "{w}"
        );
        let m = loop_film("shotglass = \"0.1.1\"", M).unwrap();
        assert!(m.contract_warning().unwrap().contains("it says \"0.1.1\""));
        let m = loop_film("shotglass = \"0.1.2\"", M).unwrap();
        assert_eq!(m.contract_warning(), None);
    }

    #[test]
    fn buffers_parse_spans_resets_and_channels() {
        // moon: 0–3 s, closeup: 2–5 s (frames 60–150 at 30 fps).
        let m = layers_film(
            "[[buffer]]\nid = \"trail\"\nscene = \"buf/trail.glsl\"\ninit = \"buf/seed.glsl\"\n\
             from = \"moon\"\nto = \"closeup.end\"\nsize = [96, 54]\nreset = [\"closeup\", 4.5]\n\
             channels = [\"@prev\", \"@picture\"]\nuniforms = { decay = 0.9 }\n\
             [[buffer]]\nid = \"hist\"\nscene = \"buf/copy.glsl\"\nscale = 0.5\nformat = \"rgba8\"\n\
             history = 8\nreset = \"cuts\"\n\
             channels = [\"@trail\", { source = \"@trail\", frame = \"prev\" }, \"@picture\"]\n\
             [[buffer]]\nid = \"sim\"\nscene = \"buf/rd.glsl\"\nformat = \"rgba32f\"\nfrom = 1\n\
             channels = [\"@prev\", { source = \"@sim\", frame = -1 }]\n\
             [[layer]]\nid = \"show\"\nscene = \"l.glsl\"\n\
             channels = [\"@trail\", { source = \"@hist\", frame = -8 }, \"@input\"]\n",
        )
        .unwrap();
        assert_eq!(m.buffers.len(), 3);
        let t = &m.buffers[0];
        assert_eq!((t.start_frame, t.end_frame), (0, 150));
        assert_eq!(t.resets, vec![0, 60, 135]);
        assert_eq!(t.reset_at(59), Some(0));
        assert_eq!(t.reset_at(60), Some(60));
        assert_eq!(t.reset_at(149), Some(135));
        assert_eq!(t.size, BufferSize::Fixed(96, 54));
        assert_eq!(t.format, BufferFormat::Rgba16f);
        assert_eq!(t.history, 1);
        assert!(t.init.is_some() && !t.is_pure());
        assert_eq!(
            t.channels[0].as_ref().unwrap().source,
            Source::Buffer {
                index: 0,
                id: "trail".into(),
                back: 1
            }
        );
        assert_eq!(t.channels[1].as_ref().unwrap().source, Source::Picture);
        let h = &m.buffers[1];
        assert_eq!(h.resets, vec![0, 60], "cuts: every shot start in the span");
        assert_eq!((h.history, h.format), (8, BufferFormat::Rgba8));
        assert_eq!(h.size.at((1920, 1080)), (960, 540));
        assert!(!h.is_pure(), "it reads another buffer");
        let sim = &m.buffers[2];
        assert_eq!((sim.start_frame, sim.resets.clone()), (30, vec![30]));
        assert!(!sim.active(29) && sim.active(30) && sim.active(149) && !sim.active(150));
        // rgba32f is read nearest (float filtering isn't portable).
        assert_eq!(sim.channels[0].as_ref().unwrap().filter, Filter::Nearest);
        assert_eq!(t.channels[0].as_ref().unwrap().filter, Filter::Linear);
        let l = &m.layers[0];
        assert_eq!(
            l.channels[1].as_ref().unwrap().source,
            Source::Buffer {
                index: 1,
                id: "hist".into(),
                back: 8
            }
        );
        // A pure history buffer reads no buffer state.
        let m = layers_film(
            "[[buffer]]\nid = \"h\"\nscene = \"c.glsl\"\nhistory = 4\nchannels = [\"@picture\"]\n",
        )
        .unwrap();
        assert!(m.buffers[0].is_pure());
        // Loop films reset every buffer at frame 0 (its span start).
        let m = loop_film(
            "shotglass = \"0.1.9\"",
            "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 2\n\
             [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\nchannels = [\"@prev\"]\n",
        )
        .unwrap();
        assert_eq!(m.buffers[0].resets, vec![0]);
        assert_eq!(m.buffers[0].end_frame, 60);
    }

    #[test]
    fn buffers_are_validated() {
        let bad = |b: &str| err(layers_film(b));
        let one = |extra: &str| {
            bad(&format!(
                "[[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\n{extra}\n"
            ))
        };
        assert!(one("scale = \"auto\"").contains("can't be \"auto\""));
        assert!(one("scale = 1.5").contains("(0, 1]"));
        assert!(one("size = [10, 10]\nscale = 0.5").contains("not both"));
        assert!(one("size = [0, 10]").contains("1–8192"));
        assert!(one("format = \"rgb8\"").contains("isn't one of"));
        assert!(one("history = 0").contains("1 to 64"));
        assert!(one("history = 65").contains("1 to 64"));
        assert!(one("cache = \"sparse\"").contains("use \"dense\""));
        assert!(one("preview_lookback = 0").contains("more than 0"));
        assert!(one("preview_lookback = -1").contains("more than 0"));
        assert!(one("reset = [\"nope\"]").contains("no shot or earlier layer"));
        assert!(one("from = 3\nreset = [1]").contains("outside the buffer's span"));
        assert!(one("reset = 3").contains("\"cuts\" or a list"));
        assert!(one("blend = \"x\"").contains("unknown field"));
        assert!(one("channels = [\"@input\"]").contains("buffers step before layers"));
        assert!(one("channels = [\"@b\"]").contains("is this buffer; read its previous state"));
        assert!(one("channels = [{ source = \"@b\", frame = -2 }]").contains("history = 2"));
        assert!(one("channels = [{ source = \"@b\", frame = 1 }]").contains("negative"));
        assert!(
            one("channels = [{ source = \"@picture\", frame = -1 }]")
                .contains("isn't a [[buffer]]")
        );
        // Forward-only: a later buffer's current state can't be read, its previous can.
        let two = |chans: &str| {
            layers_film(&format!(
                "[[buffer]]\nid = \"a\"\nscene = \"a.glsl\"\nchannels = {chans}\n\
                 [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\n"
            ))
        };
        assert!(err(two("[\"@b\"]")).contains("later buffer"));
        assert!(two("[{ source = \"@b\", frame = \"prev\" }]").is_ok());
        // Layers aren't readable from buffers; buffers aren't readable from shots.
        let e = bad("[[layer]]\nid = \"t\"\nscene = \"t.glsl\"\n\
             [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\nchannels = [\"@t\"]\n");
        assert!(e.contains("layers draw after buffers step"), "{e}");
        let e = err(fixture(
            "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\nchannels = [\"@b\"]\n\
             [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\n",
            Path::new("/film"),
        ));
        assert!(e.contains("draw before buffers step"), "{e}");
        let e = err(fixture(
            "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\nchannels = [\"@prev\"]\n",
            Path::new("/film"),
        ));
        assert!(e.contains("only a [[buffer]]'s channels"), "{e}");
        let e = bad("[[layer]]\nid = \"t\"\nscene = \"t.glsl\"\nchannels = [\"@prev\"]\n");
        assert!(e.contains("only a [[buffer]]'s channels"), "{e}");
        // Ids are shared with shots, layers and comps.
        let e = bad("[[buffer]]\nid = \"moon\"\nscene = \"b.glsl\"\n");
        assert!(e.contains("the id is taken"), "{e}");
        let e = bad("[[buffer]]\nid = \"prev\"\nscene = \"b.glsl\"\n");
        assert!(e.contains("isn't prev"), "{e}");
        // Required memory is checked at load.
        let e = bad(
            "[[buffer]]\nid = \"big\"\nscene = \"b.glsl\"\nformat = \"rgba32f\"\nhistory = 64\n",
        );
        assert!(
            e.contains("MB of GPU memory at once") && e.contains("\"big\""),
            "{e}"
        );
    }

    /// Buffers in comps (#138): they parse against the comp's shots and layers, read
    /// comp-local sources, and a comp with buffers (directly or through a comp it shows)
    /// is shown in one place only.
    #[test]
    fn comp_buffers() {
        const C: &str = "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 2\n\
             [[layer]]\nid = \"show\"\nscene = \"t.glsl\"\nchannels = [\"@b\"]\n\
             [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\nchannels = [\"@prev\", \"@picture\"]\n\
             reset = [1]\nscale = 0.5\n";
        let (_d, m) = comps_film(
            "shotglass = \"0.1.10\"\n[comps]\nc = { file = \"c.toml\", size = [64, 36] }\n\
             [[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n",
            &[("c.toml", C)],
        );
        let m = m.unwrap();
        assert!(m.buffers.is_empty());
        let b = &m.comps[0].movie.buffers;
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].start_frame, b[0].end_frame), (0, 60));
        assert_eq!(b[0].resets, vec![0, 30]);
        let l = &m.comps[0].movie.layers[0];
        assert!(matches!(
            l.channels[0].as_ref().unwrap().source,
            Source::Buffer {
                index: 0,
                back: 0,
                ..
            }
        ));
        assert_eq!(m.contract_warning(), None);
        // An older contract is told about 0.1.10.
        let (_d, m) = comps_film(
            "shotglass = \"0.1.9\"\n[comps]\nc = \"c.toml\"\n\
             [[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n",
            &[("c.toml", C)],
        );
        assert!(m.unwrap().contract_warning().unwrap().contains("0.1.10"));
        // Shown twice: refused, naming both uses.
        let (_d, m) = comps_film(
            "[comps]\nc = \"c.toml\"\n\
             [[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n\
             [[layer]]\nid = \"l\"\nscene = \"t.glsl\"\nchannels = [\"@c\"]\n",
            &[("c.toml", C)],
        );
        let e = err(m);
        assert!(
            e.contains("shown in one place only")
                && e.contains("shot \"s\"")
                && e.contains("layer \"l\"'s channels"),
            "{e}"
        );
        // Through a comp that shows it: the outer comp is shown twice.
        let (_d, m) = comps_film(
            "[comps]\nc = \"c.toml\"\nouter = \"o.toml\"\n\
             [[shot]]\nid = \"s\"\ncomp = \"outer\"\nduration = 1\n\
             [[shot]]\nid = \"s2\"\ncomp = \"outer\"\nduration = 1\n",
            &[
                ("c.toml", C),
                (
                    "o.toml",
                    "[[shot]]\nid = \"x\"\ncomp = \"c\"\nduration = 1\n",
                ),
            ],
        );
        let e = err(m);
        assert!(
            e.contains("comp \"outer\"") && e.contains("in a comp it shows"),
            "{e}"
        );
        // Read once from a film buffer: fine (the comp's channel use counts).
        let (_d, m) = comps_film(
            "[comps]\nc = \"c.toml\"\n[[shot]]\nid = \"s\"\nscene = \"s.glsl\"\nduration = 1\n\
             [[buffer]]\nid = \"fb\"\nscene = \"b.glsl\"\nchannels = [\"@c\"]\n",
            &[("c.toml", C)],
        );
        assert!(m.is_ok(), "{:?}", m.err());
        // A comp without buffers may still be shown many times.
        let (_d, m) = comps_film(
            "[comps]\nc = \"c.toml\"\n\
             [[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n\
             [[shot]]\nid = \"s2\"\ncomp = \"c\"\nduration = 1\n",
            &[(
                "c.toml",
                "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n",
            )],
        );
        assert!(m.is_ok());
        // Comp buffers follow the buffer rules: @input is refused, ids are the comp's.
        let (_d, m) = comps_film(
            "[comps]\nc = \"c.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n",
            &[(
                "c.toml",
                "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n\
                 [[buffer]]\nid = \"a\"\nscene = \"b.glsl\"\n",
            )],
        );
        assert!(err(m).contains("the id is taken"));
        // Memory: the film's and every comp's worst frames add up.
        let big = "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n\
             [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\nformat = \"rgba32f\"\nhistory = 17\n";
        let (_d, m) = comps_film(
            "[comps]\nc = \"c.toml\"\nd = \"d.toml\"\n\
             [[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n\
             [[shot]]\nid = \"t\"\ncomp = \"d\"\nduration = 1\n",
            &[("c.toml", big), ("d.toml", big)],
        );
        let e = err(m);
        assert!(
            e.contains("the film and its comps") && e.contains("comp \"d\""),
            "{e}"
        );
        // A sized comp's `scale` buffers are counted at its size, not at 1080p: 64×36
        // rgba32f × 64 states is under 3 MB (at 1920×1080 it would be 2 GB).
        let (_d, m) = comps_film(
            "[comps]\nc = { file = \"c.toml\", size = [64, 36] }\n\
             [[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n",
            &[(
                "c.toml",
                "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n\
                 [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\nformat = \"rgba32f\"\nhistory = 63\n",
            )],
        );
        assert!(m.is_ok(), "{:?}", m.err());
    }

    #[test]
    fn buffers_need_contract_0_1_9() {
        let film = |v: &str| {
            fixture(
                &format!(
                    "shotglass = \"{v}\"\n[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n\
                     [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\n"
                ),
                Path::new("/film"),
            )
            .unwrap()
        };
        assert!(film("0.1.8").contract_warning().unwrap().contains("0.1.9"));
        assert_eq!(film("0.1.9").contract_warning(), None);
    }
}
