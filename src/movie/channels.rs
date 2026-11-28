//! Channel specs: textures, footage, fonts and text blocks → `Channel`s.

use super::*;

pub(super) fn quoted_list<'a>(names: impl Iterator<Item = &'a String>) -> String {
    let v: Vec<String> = names.map(|n| format!("\"{n}\"")).collect();
    if v.is_empty() {
        "none".to_string()
    } else {
        v.join(", ")
    }
}

/// Extensions read as fonts (any case).
pub(super) const FONT_EXTENSIONS: &[&str] = &["ttf", "otf", "ttc"];

/// One `[fonts]` value: a path / `<lib/path>`, or `{ file, index, variations, fallback,
/// licence }`. Returns the font and its fallback names (checked by the caller).
pub(super) fn font_spec(
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
pub(super) fn font_chain(
    start: &str,
    fonts: &BTreeMap<String, (FontRef, Vec<String>)>,
) -> Vec<FontRef> {
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
pub(super) fn text_spec(
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
pub(super) fn file_ref(what: &str, s: &str, specs: &[LibrarySpec], root: &Path) -> Result<FileRef> {
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
pub(super) fn is_texture_name(s: &str) -> bool {
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
pub(super) fn texture_spec(
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
                video: None,
            })),
            filter: Filter::Linear,
            wrap: Wrap::Repeat,
            vflip: true,
            srgb: false,
            timing: Timing::default(),
        })
    };
    // A video (#73): AV1 in WebM. Other containers are converted once with
    // `shotglass conform`. Footage defaults: linear, upright.
    let video_channel = |s: &str| -> Result<Channel> {
        let path = s.trim_start_matches('<').trim_end_matches('>');
        if !path.to_ascii_lowercase().ends_with(".webm") {
            bail!(
                "{what}: \"{s}\": shotglass reads video as AV1 in WebM; convert it once: \
                 shotglass conform {path}"
            );
        }
        let f = file_ref(what, s, specs, root)?;
        Ok(Channel {
            source: Source::Frames(Box::new(Sequence {
                pattern: f,
                fps: None,
                first: None,
                scan: None,
                video: Some(VideoOptions::default()),
            })),
            filter: Filter::Linear,
            wrap: Wrap::Repeat,
            vflip: true,
            srgb: false,
            timing: Timing::default(),
        })
    };
    // A file entry by its kind.
    let file_source = |s: &str| -> Result<Channel> {
        match ChannelSource::of(s) {
            ChannelSource::Sequence(_) => sequence_channel(s),
            ChannelSource::Video(_) => video_channel(s),
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
                    let Some(refs) = on_layer else {
                        bail!(
                            "{what}: \"{s}\" is a buffer; shots and transitions draw before \
                             buffers step, so only layers and later buffers read one"
                        );
                    };
                    // Where buffer j steps (#138): before every layer, or right after
                    // its `after` layer (`rank` = that layer's index + 1).
                    let layer_at =
                        |id: &str| refs.earlier.iter().chain(refs.later).position(|x| x == id);
                    let rank =
                        |d: &BufDecl| d.after.as_deref().and_then(layer_at).map_or(0, |p| p + 1);
                    let rj = rank(&buffers[j]);
                    let anchor = buffers[j].after.as_deref().unwrap_or("");
                    match on_buffer {
                        Some(i) if j == i => bail!(
                            "{what}: \"{s}\" is this buffer; read its previous state with \"@prev\""
                        ),
                        // A buffer reads the current state of buffers that step before it.
                        Some(i) if (rj, j) > (rank(&buffers[i]), i) => {
                            if rj > rank(&buffers[i]) {
                                bail!(
                                    "{what}: \"{s}\" steps later in the frame (after layer \
                                     \"{anchor}\"); read its previous state with {{ source = \
                                     \"{s}\", frame = \"prev\" }}"
                                )
                            }
                            bail!(
                                "{what}: \"{s}\" is a later buffer (buffers step in file order); \
                                 read its previous state with {{ source = \"{s}\", frame = \"prev\" }}"
                            )
                        }
                        // A layer reads buffers that stepped below it.
                        None if rj > refs.earlier.len() => bail!(
                            "{what}: \"{s}\" steps after layer \"{anchor}\", which isn't below this \
                             layer; read its previous state with {{ source = \"{s}\", frame = \
                             \"prev\" }}"
                        ),
                        _ => return Ok(Some(buffer_channel(j, 0))),
                    }
                }
                if let (Some(_), Some(refs)) = (on_buffer, on_layer) {
                    // With `after` (#138), the layers up to it are `earlier`.
                    let anchor = refs.earlier.last();
                    if out == "input" && anchor.is_none() {
                        bail!(
                            "{what}: \"@input\" is the picture below a layer; buffers step \
                             before layers, so read \"@picture\" (the picture track), or give \
                             the buffer after = \"<layer>\" to read the composite up to that \
                             layer"
                        );
                    }
                    if refs.later.iter().any(|x| x == out) {
                        match anchor {
                            Some(a) => bail!(
                                "{what}: \"{s}\" is a layer above this buffer's after = \"{a}\"; \
                                 it draws after the buffer steps (a buffer reads layers up to \
                                 its after layer)"
                            ),
                            None => bail!(
                                "{what}: \"{s}\" is a layer; buffers step before layers, unless \
                                 they give after = \"<layer>\" (then they read layers up to it)"
                            ),
                        }
                    }
                    let layer = refs.earlier.iter().any(|x| x == out);
                    if out != "picture"
                        && out != "input"
                        && !layer
                        && !comps.iter().any(|c| c == out)
                    {
                        bail!(
                            "{what}: \"{s}\": a buffer reads @picture, @prev, earlier buffers \
                             and comps (with after = \"<layer>\", also @input and layers up to \
                             it)"
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
                } else if t.contains_key("video") {
                    ("video", false)
                } else {
                    ("file", true)
                };
                match t.get(key) {
                    Some(toml::Value::String(s)) if !s.is_empty() && !is_texture_name(s) => {
                        let c = file_source(s)?;
                        let is_video = c.sequence().is_some_and(|q| q.video.is_some());
                        if key == "video" && !is_video {
                            bail!("{what}: video = \"{s}\" must be a video file (.webm)");
                        }
                        if key != "video" && is_video {
                            bail!("{what}: \"{s}\" is a video; write video = \"{s}\"");
                        }
                        if key == "frames" && c.sequence().is_none() {
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
                let video = c.sequence().is_some_and(|q| q.video.is_some());
                if matches!(k.as_str(), "fps" | "first") && !footage {
                    bail!(
                        "{what}: {k} is for image sequences (frames = \"…\"); {} isn't one",
                        c.label()
                    );
                }
                if k == "first" && video {
                    bail!(
                        "{what}: first is for image sequences; a video starts at frame 0 (use in or in_frame)"
                    );
                }
                if matches!(k.as_str(), "matrix" | "range") && !video {
                    bail!(
                        "{what}: {k} is for videos (video = \"….webm\"); {} isn't one",
                        c.label()
                    );
                }
                match k.as_str() {
                    "file" | "text" | "frames" | "video" | "texture" | "part" => {}
                    "matrix" => {
                        let s = word(&["bt601", "bt709", "bt2020"])?;
                        let m = crate::video::Matrix::parse(&s)
                            .ok_or_else(|| not_one_of(&s, &["bt601", "bt709", "bt2020"]))?;
                        if let Source::Frames(q) = &mut c.source
                            && let Some(v) = &mut q.video
                        {
                            v.matrix = Some(m);
                        }
                    }
                    "range" => {
                        let s = word(&["tv", "pc"])?;
                        let full = match s.as_str() {
                            "tv" => false,
                            "pc" => true,
                            _ => return Err(not_one_of(&s, &["tv", "pc"])),
                        };
                        if let Source::Frames(q) = &mut c.source
                            && let Some(v) = &mut q.video
                        {
                            v.full_range = Some(full);
                        }
                    }
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
                    other if video => bail!(
                        "{what}: unknown option `{other}` (a video has video, fps, matrix, range, \
                         in, in_frame, speed, clock, end, filter, wrap, vflip and srgb)"
                    ),
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
