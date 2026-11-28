//! `movie.toml` parsing and the timeline: which shot (or transition) is on screen at time t.

use crate::anim::{Anim, Ease, vector};
use crate::library::{self, Library, LibrarySpec};
use crate::project::{self, Project};
use crate::texture::{Filter, Wrap};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod buffers;
mod channels;
mod comps;
mod sequence;

use buffers::*;
pub use buffers::{stack_reach, warm_buffers};
use channels::*;
pub use comps::MAX_COMP_DEPTH;
use comps::*;
pub use sequence::wrap_time;
use sequence::*;

// ---------------------------------------------------------------- file format

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// `fps` → (numerator, denominator), reduced. NTSC-style decimals (29.97, 23.976,
/// 59.94) mean n×1000/1001, as video tools assume.
/// A rate from the command line: `24`, `29.97`, `30000/1001`.
#[cfg_attr(not(feature = "webm"), allow(dead_code))]
pub fn parse_fps_arg(s: &str) -> Result<(u32, u32)> {
    let f = if s.contains('/') {
        FpsFile::Ratio(s.to_string())
    } else if let Ok(i) = s.parse() {
        FpsFile::Int(i)
    } else {
        FpsFile::Float(
            s.parse()
                .map_err(|_| anyhow!("fps \"{s}\" must look like 24 or 30000/1001"))?,
        )
    };
    parse_fps(f)
}

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
    /// Step right after this layer is composited (#138), not before every layer: the
    /// buffer may then read `@input` (the composite so far) and `@<layer>` up to it.
    after: Option<String>,
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
    /// A video (#73): `pattern` is the WebM file (no #), frames come from it.
    pub video: Option<VideoOptions>,
}

/// A video channel's colour overrides (#73); None = from the file's tags.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct VideoOptions {
    pub matrix: Option<crate::video::Matrix>,
    pub full_range: Option<bool>,
}

impl Sequence {
    /// Source frames per second: `fps`, else a video's own rate, else the film's.
    pub fn rate(&self, film: (u32, u32)) -> (u32, u32) {
        self.fps
            .or_else(|| {
                self.scan
                    .as_ref()
                    .and_then(|s| s.video.as_ref())
                    .map(|v| v.clip.fps)
            })
            .unwrap_or(film)
    }
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
    /// Every single file the channel reads: the image, a video, or a text's file and
    /// fonts. A sequence's frames are in its scan (`Sequence::scan`), not here.
    pub fn files(&self) -> Vec<&FileRef> {
        match &self.source {
            Source::Frames(s) if s.video.is_some() => vec![&s.pattern],
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
        if seq.video.is_some() {
            return vec![]; // one file, in `files()`
        }
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
    /// `after = "<layer>"` (#138): the index of the layer it steps right after (None:
    /// before every layer). It reads the composite up to that layer (`@input`) and
    /// layers up to it (`@<layer>`), drawn at its size, scale 1.
    pub after: Option<usize>,
    /// The layers its composite reads (up to `after`) read buffer state, so its state
    /// at n depends on other buffers' states at n: never a pure history buffer.
    pub stack_reads_state: bool,
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
        !self.stack_reads_state
            && !self
                .channels
                .iter()
                .flatten()
                .any(|c| matches!(c.source, Source::Buffer { .. }))
    }

    /// Does it read the layer stack (`@input` or `@<layer>`, with `after`, #138)? Its
    /// steps then draw the composite up to its layer.
    pub fn reads_stack(&self) -> bool {
        self.channels
            .iter()
            .flatten()
            .any(|c| matches!(c.source, Source::Input | Source::Layer { .. }))
    }

    /// Where it steps in a frame (#138): 0 before every layer, j + 1 right after layer
    /// j. Buffers step in (rank, file order).
    pub fn rank(&self) -> usize {
        self.after.map_or(0, |j| j + 1)
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
    /// `after` (#138), the layer id as written: checked in `build_buffers`.
    after: Option<String>,
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
        let after = |m: &Movie| m.buffers.iter().any(|b| b.after.is_some());
        if after(self) || self.comps.iter().any(|c| after(&c.movie)) {
            features.push(
                "buffers in the layer stack (after = \"<layer>\"), which need shotglass 0.1.11",
            );
            need = semver::Version::new(0, 1, 11);
        }
        let video = |m: &Movie| {
            m.channels()
                .any(|c| c.sequence().is_some_and(|q| q.video.is_some()))
        };
        if video(self) || self.comps.iter().any(|c| video(&c.movie)) {
            features.push("video channels (video = \"….webm\"), which need shotglass 0.1.12");
            need = semver::Version::new(0, 1, 12);
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
            semver::Version::new(0, 1, 10),
            semver::Version::new(0, 1, 11),
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
        type Found = (
            (PathBuf, Option<u64>, Option<VideoOptions>),
            std::sync::Arc<crate::footage::Scan>,
        );
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
                // A missing video is reported by `check_textures`, with a "did you mean".
                if seq.video.is_some() && !seq.pattern.file.is_file() {
                    continue;
                }
                let key = (seq.pattern.file.clone(), seq.first, seq.video.clone());
                let scan = match found.iter().find(|(k, _)| *k == key) {
                    Some((_, s)) => s.clone(),
                    None if seq.video.is_some() => {
                        let opts = seq.video.as_ref().unwrap();
                        let s =
                            crate::footage::scan_video(&seq.pattern.file, &seq.pattern.label, opts)
                                .map_err(|e| anyhow!("{who}: channels[{n}]: {e}"))?;
                        let s = std::sync::Arc::new(s);
                        found.push((key, s.clone()));
                        s
                    }
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
                seq.scan = Some(scan.clone());
                let rate = seq.rate(film);
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
            src: seq.rate((self.fps_num, self.fps_den)),
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

/// A `[comps]` entry before its file is read (#74).
struct CompDecl {
    name: String,
    rel: String,
    size: Option<(u32, u32)>,
    looping: bool,
    /// `loop = "warm"` (#138).
    warm: bool,
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
mod tests;
