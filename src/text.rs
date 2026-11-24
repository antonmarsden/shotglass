//! Text as an iChannel texture (#68): fonts → shaping → layout → an RGBA8 block whose
//! red and alpha hold antialiased coverage and whose green holds a signed distance.
//!
//! Everything here is CPU-side and deterministic: fonts come only from files the film
//! names (never the system), there is no hinting, and the raster uses plain IEEE f32/f64
//! maths and an exact distance transform, so a block's bytes depend only on the font
//! files, the text options and the engine version. The renderer rasterises blocks on its
//! decode worker; `Movie::parse` lays them out (cheap) to know their sizes early.
//!
//! Layout model (docs/channels.md, "Text"): sizes are texture pixels (`size` = the em), the block is
//! the text's content box plus `padding` (= the distance-field spread) on every side.
//! Lines break greedily at UAX #14 opportunities; paragraphs (`\n`) get their own bidi
//! base direction (UAX #9) and are reordered per line; each grapheme takes the first font
//! in the fallback chain that covers it.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use rustybuzz::ttf_parser;
use unicode_segmentation::UnicodeSegmentation;

/// One font file as a film names it: the file, the face in a collection, and fixed
/// variation-axis values.
#[derive(Clone, Debug, PartialEq)]
pub struct FontSpec {
    pub file: PathBuf,
    /// As written, for messages.
    pub label: String,
    pub index: u32,
    pub variations: Vec<(String, f32)>,
}

/// A font file read into memory and checked once.
pub struct Font {
    pub spec: FontSpec,
    data: Arc<Vec<u8>>,
}

impl Font {
    pub fn load(spec: &FontSpec) -> Result<Font> {
        let data = std::fs::read(&spec.file)
            .with_context(|| format!("{}: can't read the font", spec.label))?;
        Font::from_bytes(spec.clone(), data)
    }

    pub fn from_bytes(spec: FontSpec, data: Vec<u8>) -> Result<Font> {
        if data.starts_with(b"wOFF") || data.starts_with(b"wOF2") {
            bail!(
                "{}: WOFF/WOFF2 fonts aren't read; convert it to TTF or OTF",
                spec.label
            );
        }
        if data.starts_with(b"version https://git-lfs") {
            bail!(
                "{}: this is a Git LFS pointer, not the font (run `git lfs pull`)",
                spec.label
            );
        }
        let font = Font {
            spec,
            data: Arc::new(data),
        };
        let face = ttf_parser::Face::parse(&font.data, font.spec.index).map_err(|e| {
            anyhow!(
                "{}: not a font shotglass can read (TTF, OTF or TTC): {e}",
                font.spec.label
            )
        })?;
        for (tag, _) in &font.spec.variations {
            let t = tag_of(tag)?;
            if !face.variation_axes().into_iter().any(|a| a.tag == t) {
                bail!(
                    "{}: the font has no \"{tag}\" axis ({})",
                    font.spec.label,
                    axis_list(&face)
                );
            }
        }
        Ok(font)
    }

    /// The shaping face with this font's variations applied.
    fn face(&self) -> rustybuzz::Face<'_> {
        let mut face = rustybuzz::Face::from_slice(&self.data, self.spec.index)
            .expect("checked in Font::from_bytes");
        let vars: Vec<rustybuzz::Variation> = self
            .spec
            .variations
            .iter()
            .map(|(t, v)| rustybuzz::Variation {
                tag: tag_of(t).expect("checked in Font::from_bytes"),
                value: *v,
            })
            .collect();
        if !vars.is_empty() {
            face.set_variations(&vars);
        }
        face
    }
}

fn tag_of(s: &str) -> Result<ttf_parser::Tag> {
    if s.len() != 4 || !s.is_ascii() {
        bail!("\"{s}\" isn't a variation axis tag (four letters, like \"wght\")");
    }
    Ok(ttf_parser::Tag::from_bytes_lossy(s.as_bytes()))
}

fn axis_list(face: &ttf_parser::Face) -> String {
    let axes: Vec<String> = face
        .variation_axes()
        .into_iter()
        .map(|a| format!("\"{}\"", a.tag))
        .collect();
    if axes.is_empty() {
        "it isn't a variable font".to_string()
    } else {
        format!("it has {}", axes.join(", "))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
    /// Left in a left-to-right paragraph, right in a right-to-left one.
    Start,
    End,
}

impl Align {
    pub const NAMES: &'static [&'static str] = &["left", "center", "right", "start", "end"];
    pub fn parse(s: &str) -> Option<Align> {
        Some(match s {
            "left" => Align::Left,
            "center" => Align::Center,
            "right" => Align::Right,
            "start" => Align::Start,
            "end" => Align::End,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From the first strong character of each paragraph (UAX #9 P2–P3).
    Auto,
    Ltr,
    Rtl,
}

impl Direction {
    pub const NAMES: &'static [&'static str] = &["auto", "ltr", "rtl"];
    pub fn parse(s: &str) -> Option<Direction> {
        Some(match s {
            "auto" => Direction::Auto,
            "ltr" => Direction::Ltr,
            "rtl" => Direction::Rtl,
            _ => return None,
        })
    }
}

/// Everything that decides a text block's pixels (besides the font files).
#[derive(Clone, Debug, PartialEq)]
pub struct TextStyle {
    /// The em, in texture pixels.
    pub size: f32,
    /// Wrap width in pixels; None: lines break only at `\n`.
    pub width: Option<f32>,
    pub align: Align,
    /// Line pitch as a multiple of the em.
    pub line_height: f32,
    /// Extra space after each grapheme, in ems.
    pub letter_spacing: f32,
    /// More lines than this is an error (a title that wraps silently is a bug).
    pub max_lines: Option<usize>,
    pub direction: Direction,
    /// OpenType features: "ss01", "-liga", "tnum".
    pub features: Vec<String>,
    /// Distance-field spread in pixels, also the padding on every side.
    pub spread: f32,
}

impl Default for TextStyle {
    fn default() -> Self {
        TextStyle {
            size: 64.0,
            width: None,
            align: Align::Start,
            line_height: 1.2,
            letter_spacing: 0.0,
            max_lines: None,
            direction: Direction::Auto,
            features: vec![],
            spread: 8.0,
        }
    }
}

/// A glyph placed in the block: pen position on the baseline, in pixels from the
/// block's top-left corner (padding included), y down.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlacedGlyph {
    /// Index into the font chain.
    pub font: usize,
    pub id: u16,
    pub x: f32,
    pub y: f32,
}

/// The result of laying out a text: where every glyph goes and the block's size.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub glyphs: Vec<PlacedGlyph>,
    pub width: usize,
    pub height: usize,
    pub lines: usize,
    /// What reaches the shader as `iChannelMeta`: (em, first baseline from the top,
    /// line pitch, padding), in texels.
    pub meta: [f32; 4],
    /// Characters no font in the chain covers (drawn as the primary font's .notdef).
    pub missing: Vec<char>,
    /// Things worth saying that don't stop the text (a word broken mid-word).
    pub warnings: Vec<String>,
}

/// Largest block side, as for images (`texture::MAX_SIZE`).
pub const MAX_SIZE: usize = 4096;

/// Lay out `text` with `fonts` (primary first, then fallbacks) and `style`.
pub fn layout(text: &str, fonts: &[&Font], style: &TextStyle) -> Result<Layout> {
    if fonts.is_empty() {
        bail!("a text needs a font");
    }
    let faces: Vec<rustybuzz::Face> = fonts.iter().map(|f| f.face()).collect();
    let features = parse_features(&style.features)?;
    let primary = &faces[0];
    let scale = style.size / primary.units_per_em() as f32;
    let ascent = primary.ascender() as f32 * scale;
    let descent = -(primary.descender() as f32) * scale;
    let pitch = style.line_height * style.size;
    let half_leading = (pitch - (ascent + descent)) / 2.0;
    let pad = style.spread;
    let spacing = style.letter_spacing * style.size;

    let mut missing = vec![];
    let mut warnings = vec![];
    // Lines of each paragraph: (paragraph text, its bidi info, line byte ranges).
    struct Line {
        para: usize,
        range: Range<usize>,
    }
    let paras: Vec<&str> = text.split('\n').collect();
    let mut lines: Vec<Line> = vec![];
    let mut bidis = vec![];
    let mut font_of: Vec<Vec<usize>> = vec![];
    for (pi, para) in paras.iter().enumerate() {
        let level = match style.direction {
            Direction::Auto => None,
            Direction::Ltr => Some(unicode_bidi::Level::ltr()),
            Direction::Rtl => Some(unicode_bidi::Level::rtl()),
        };
        let bidi = unicode_bidi::ParagraphBidiInfo::new(para, level);
        // Font per byte: each grapheme takes the first font that covers all of it, else
        // the first that covers its first character, else the primary font (missing).
        let mut fonts_here = vec![0usize; para.len()];
        for (start, g) in para.grapheme_indices(true) {
            let covers =
                |f: &rustybuzz::Face, c: char| c.is_whitespace() || f.glyph_index(c).is_some();
            let pick = faces
                .iter()
                .position(|f| g.chars().all(|c| covers(f, c)))
                .or_else(|| {
                    let c = g.chars().next()?;
                    faces.iter().position(|f| covers(f, c))
                });
            let fi = match pick {
                Some(i) => i,
                None => {
                    missing.extend(g.chars().filter(|c| !c.is_control()));
                    0
                }
            };
            fonts_here[start..start + g.len()].fill(fi);
        }
        // Advance of each byte's cluster (logical order), for measuring line candidates.
        let mut adv = vec![0f32; para.len() + 1];
        for run in level_font_runs(&bidi, &fonts_here, 0..para.len()) {
            let shaped = shape(
                &faces[run.font],
                &features,
                &para[run.range.clone()],
                run.rtl,
            );
            for (info, pos) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
                adv[run.range.start + info.cluster as usize] += pos.x_advance as f32 * scale;
            }
        }
        let graphemes: Vec<usize> = para.grapheme_indices(true).map(|(i, _)| i).collect();
        for &g in &graphemes {
            adv[g] += spacing;
        }
        let mut prefix = vec![0f32; para.len() + 1];
        for i in 0..para.len() {
            prefix[i + 1] = prefix[i] + adv[i];
        }
        let trim = |a: usize, b: usize| a + para[a..b].trim_end().len();
        let measure = |a: usize, b: usize| {
            let b = trim(a, b);
            prefix[b] - prefix[a]
        };
        let fits = |a: usize, b: usize| style.width.is_none_or(|w| measure(a, b) <= w + 1e-3);
        let mut start = 0;
        let mut cand: Option<usize> = None;
        let opportunities = unicode_linebreak::linebreaks(para)
            .map(|(i, _)| i)
            .filter(|&i| i > 0);
        for p in opportunities {
            if fits(start, p) {
                cand = Some(p);
                continue;
            }
            if let Some(c) = cand.take() {
                lines.push(Line {
                    para: pi,
                    range: start..c,
                });
                start = c;
            }
            if fits(start, p) {
                cand = Some(p);
                continue;
            }
            // A word wider than the line: break it between graphemes.
            let word = para[start..p].trim_end();
            warnings.push(format!(
                "\"{word}\" is wider than width = {}; it's broken between letters",
                style.width.unwrap_or(0.0)
            ));
            let mut s = start;
            let mut prev = start;
            for &g in graphemes.iter().filter(|&&g| g > start && g < p) {
                if !fits(s, g) && prev > s {
                    lines.push(Line {
                        para: pi,
                        range: s..prev,
                    });
                    s = prev;
                }
                prev = g;
            }
            start = s;
            cand = Some(p);
        }
        lines.push(Line {
            para: pi,
            range: start..para.len(),
        });
        bidis.push(bidi);
        font_of.push(fonts_here);
    }
    if let Some(max) = style.max_lines
        && lines.len() > max
    {
        bail!(
            "the text takes {} lines, more than max_lines = {max}",
            lines.len()
        );
    }

    // Place each line's visual runs left to right.
    let mut placed: Vec<(Vec<PlacedGlyph>, f32, bool)> = vec![];
    for line in &lines {
        let para = paras[line.para];
        let bidi = &bidis[line.para];
        let end = line.range.start + para[line.range.clone()].trim_end().len();
        let range = line.range.start..end;
        let mut pen = 0f32;
        let mut glyphs = vec![];
        if !range.is_empty() {
            let (levels, runs) = bidi.visual_runs(range.clone());
            for run in runs {
                let rtl = levels[run.start - range.start].is_rtl();
                let mut subs = font_runs(&font_of[line.para], run.clone());
                if rtl {
                    subs.reverse();
                }
                for (font, sub) in subs {
                    let shaped = shape(&faces[font], &features, &para[sub.clone()], rtl);
                    let mut last_cluster = None;
                    for (info, pos) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
                        if spacing != 0.0 && last_cluster.is_some_and(|c| c != info.cluster) {
                            pen += spacing;
                        }
                        last_cluster = Some(info.cluster);
                        glyphs.push(PlacedGlyph {
                            font,
                            id: info.glyph_id as u16,
                            x: pen + pos.x_offset as f32 * scale,
                            y: -(pos.y_offset as f32) * scale,
                        });
                        pen += pos.x_advance as f32 * scale;
                    }
                    if spacing != 0.0 && last_cluster.is_some() {
                        pen += spacing;
                    }
                }
            }
            if spacing != 0.0 {
                pen -= spacing; // nothing follows the last grapheme
            }
        }
        placed.push((glyphs, pen, bidi.paragraph_level.is_rtl()));
    }
    let content_w = style
        .width
        .unwrap_or_else(|| placed.iter().map(|p| p.1).fold(0.0, f32::max))
        .max(1.0);
    let content_h = (pitch * lines.len() as f32).max(1.0);
    let width = (content_w + 2.0 * pad).ceil() as usize;
    let height = (content_h + 2.0 * pad).ceil() as usize;
    if width > MAX_SIZE || height > MAX_SIZE {
        bail!(
            "the text block is {width}×{height}, larger than {MAX_SIZE} on a side; use a \
             smaller size or a narrower width"
        );
    }
    let first_baseline = pad + half_leading + ascent;
    let mut glyphs = vec![];
    for (i, (line, line_w, rtl)) in placed.into_iter().enumerate() {
        let free = content_w - line_w;
        let dx = match (style.align, rtl) {
            (Align::Left, _) | (Align::Start, false) | (Align::End, true) => 0.0,
            (Align::Right, _) | (Align::End, false) | (Align::Start, true) => free,
            (Align::Center, _) => free / 2.0,
        };
        let baseline = first_baseline + i as f32 * pitch;
        glyphs.extend(line.into_iter().map(|g| PlacedGlyph {
            x: g.x + pad + dx,
            y: g.y + baseline,
            ..g
        }));
    }
    missing.dedup();
    Ok(Layout {
        glyphs,
        width,
        height,
        lines: lines.len(),
        meta: [style.size, first_baseline, pitch, pad],
        missing,
        warnings,
    })
}

fn parse_features(names: &[String]) -> Result<Vec<rustybuzz::Feature>> {
    names
        .iter()
        .map(|n| {
            n.parse::<rustybuzz::Feature>().map_err(|_| {
                anyhow!("\"{n}\" isn't an OpenType feature (like \"ss01\", \"-liga\" or \"tnum\")")
            })
        })
        .collect()
}

fn shape(
    face: &rustybuzz::Face,
    features: &[rustybuzz::Feature],
    text: &str,
    rtl: bool,
) -> rustybuzz::GlyphBuffer {
    let mut buf = rustybuzz::UnicodeBuffer::new();
    buf.push_str(text);
    buf.set_direction(if rtl {
        rustybuzz::Direction::RightToLeft
    } else {
        rustybuzz::Direction::LeftToRight
    });
    buf.guess_segment_properties();
    rustybuzz::shape(face, features, buf)
}

struct Run {
    range: Range<usize>,
    font: usize,
    rtl: bool,
}

/// Logical runs of one bidi level and one font in `range`.
fn level_font_runs(
    bidi: &unicode_bidi::ParagraphBidiInfo,
    fonts: &[usize],
    range: Range<usize>,
) -> Vec<Run> {
    let mut out: Vec<Run> = vec![];
    for i in range {
        let (font, rtl) = (fonts[i], bidi.levels[i].is_rtl());
        match out.last_mut() {
            Some(r) if r.font == font && r.rtl == rtl && r.range.end == i => r.range.end = i + 1,
            _ => out.push(Run {
                range: i..i + 1,
                font,
                rtl,
            }),
        }
    }
    out
}

/// Logical runs of one font in `range`.
fn font_runs(fonts: &[usize], range: Range<usize>) -> Vec<(usize, Range<usize>)> {
    let mut out: Vec<(usize, Range<usize>)> = vec![];
    for i in range {
        match out.last_mut() {
            Some((f, r)) if *f == fonts[i] => r.end = i + 1,
            _ => out.push((fonts[i], i..i + 1)),
        }
    }
    out
}

/// A rasterised text block: RGBA8, top row first. R and A: coverage; G: signed
/// distance (0.5 at the outline, +0.5 per `spread` pixels inside); B: 0.
pub struct TextImage {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// Coverage samples per pixel side (4×4 box filter).
const COVER: usize = 4;
/// Distance-field samples per pixel side.
const DIST: usize = 2;

/// Rasterise `layout` with the same `fonts` it was laid out with.
pub fn rasterize(layout: &Layout, fonts: &[&Font], spread: f32) -> TextImage {
    let faces: Vec<rustybuzz::Face> = fonts.iter().map(|f| f.face()).collect();
    let (w, h) = (layout.width, layout.height);
    // Outlines as line segments, in coverage-sample units (y down).
    let mut edges = vec![];
    for g in &layout.glyphs {
        let face = &faces[g.font];
        let scale = layout.meta[0] / face.units_per_em() as f32;
        let mut b = Flatten {
            edges: &mut edges,
            ox: g.x,
            oy: g.y,
            scale,
            k: COVER as f32,
            start: (0.0, 0.0),
            last: (0.0, 0.0),
        };
        face.outline_glyph(ttf_parser::GlyphId(g.id), &mut b);
        b.close();
    }
    let mask = fill(&edges, w * COVER, h * COVER);
    let mut rgba = vec![0u8; w * h * 4];
    // Coverage: the share of the 4×4 samples inside.
    for y in 0..h {
        for x in 0..w {
            let mut n = 0u32;
            for sy in 0..COVER {
                let row = (y * COVER + sy) * w * COVER + x * COVER;
                n += mask[row..row + COVER]
                    .iter()
                    .map(|&v| v as u32)
                    .sum::<u32>();
            }
            let c = ((n * 255 + 8) / (COVER * COVER) as u32) as u8;
            let i = (y * w + x) * 4;
            rgba[i] = c;
            rgba[i + 3] = c;
        }
    }
    // Distance: an exact Euclidean transform of a 2×2-per-pixel mask (a sample is in
    // when at least half its coverage samples are), signed, averaged at pixel centres.
    let (dw, dh) = (w * DIST, h * DIST);
    let f = COVER / DIST;
    let mut inside = vec![false; dw * dh];
    for y in 0..dh {
        for x in 0..dw {
            let mut n = 0;
            for sy in 0..f {
                let row = (y * f + sy) * w * COVER + x * f;
                n += mask[row..row + f].iter().filter(|&&v| v != 0).count();
            }
            inside[y * dw + x] = n * 2 >= f * f;
        }
    }
    let to_out = edt(&inside, dw, dh, true);
    let to_in = edt(&inside, dw, dh, false);
    let signed = |i: usize| -> f64 {
        if inside[i] {
            to_out[i].sqrt() - 0.5
        } else {
            -(to_in[i].sqrt() - 0.5)
        }
    };
    for y in 0..h {
        for x in 0..w {
            let mut d = 0.0;
            for sy in 0..DIST {
                for sx in 0..DIST {
                    d += signed((y * DIST + sy) * dw + x * DIST + sx);
                }
            }
            let d_px = d / (DIST * DIST) as f64 / DIST as f64;
            let v = (0.5 + 0.5 * d_px / spread.max(1e-3) as f64).clamp(0.0, 1.0);
            rgba[(y * w + x) * 4 + 1] = (v * 255.0).round() as u8;
        }
    }
    TextImage {
        width: w,
        height: h,
        rgba,
    }
}

/// Flattens glyph outlines into line segments (in sample units).
struct Flatten<'a> {
    edges: &'a mut Vec<[f32; 4]>,
    ox: f32,
    oy: f32,
    scale: f32,
    k: f32,
    start: (f32, f32),
    last: (f32, f32),
}

impl Flatten<'_> {
    fn pt(&self, x: f32, y: f32) -> (f32, f32) {
        (
            (self.ox + x * self.scale) * self.k,
            (self.oy - y * self.scale) * self.k,
        )
    }
    fn line(&mut self, p: (f32, f32)) {
        if p != self.last {
            self.edges.push([self.last.0, self.last.1, p.0, p.1]);
        }
        self.last = p;
    }
    fn close(&mut self) {
        let s = self.start;
        self.line(s);
    }
    /// Segments for a curve of the given control-polygon length (≈ 0.25 sample error).
    fn steps(len: f32) -> usize {
        ((len / 2.0).sqrt().ceil() as usize).clamp(1, 64)
    }
}

impl ttf_parser::OutlineBuilder for Flatten<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.close();
        let p = self.pt(x, y);
        self.start = p;
        self.last = p;
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.pt(x, y);
        self.line(p);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (p0, p1, p2) = (self.last, self.pt(x1, y1), self.pt(x, y));
        let len = dist(p0, p1) + dist(p1, p2);
        let n = Self::steps(len);
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let u = 1.0 - t;
            self.line((
                u * u * p0.0 + 2.0 * u * t * p1.0 + t * t * p2.0,
                u * u * p0.1 + 2.0 * u * t * p1.1 + t * t * p2.1,
            ));
        }
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (p0, p1, p2, p3) = (self.last, self.pt(x1, y1), self.pt(x2, y2), self.pt(x, y));
        let len = dist(p0, p1) + dist(p1, p2) + dist(p2, p3);
        let n = Self::steps(len);
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let u = 1.0 - t;
            let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
            self.line((
                a * p0.0 + b * p1.0 + c * p2.0 + d * p3.0,
                a * p0.1 + b * p1.1 + c * p2.1 + d * p3.1,
            ));
        }
    }
    fn close(&mut self) {
        Flatten::close(self);
    }
}

fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

/// Non-zero-winding fill: 1 where a sample's centre is inside the outlines.
fn fill(edges: &[[f32; 4]], w: usize, h: usize) -> Vec<u8> {
    let mut rows: Vec<Vec<(f32, i32)>> = vec![vec![]; h];
    for &[x0, y0, x1, y1] in edges {
        if y0 == y1 {
            continue;
        }
        let (dir, (xa, ya, xb, yb)) = if y0 < y1 {
            (1, (x0, y0, x1, y1))
        } else {
            (-1, (x1, y1, x0, y0))
        };
        // Rows whose centre y + 0.5 lies in [ya, yb).
        let first = (ya - 0.5).ceil().max(0.0) as usize;
        let last = ((yb - 0.5).ceil() as isize).min(h as isize);
        for y in first as isize..last {
            let yc = y as f32 + 0.5;
            let x = xa + (xb - xa) * (yc - ya) / (yb - ya);
            rows[y as usize].push((x, dir));
        }
    }
    let mut mask = vec![0u8; w * h];
    for (y, row) in rows.iter_mut().enumerate() {
        row.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let mut wind = 0;
        for pair in row.windows(2) {
            wind += pair[0].1;
            if wind != 0 {
                // Samples whose centre x + 0.5 lies in [pair[0].x, pair[1].x).
                let a = (pair[0].0 - 0.5).ceil().max(0.0) as usize;
                let b = ((pair[1].0 - 0.5).ceil().max(0.0) as usize).min(w);
                if a < b {
                    mask[y * w + a..y * w + b].fill(1);
                }
            }
        }
    }
    mask
}

/// Squared Euclidean distance from each sample to the nearest sample whose `inside`
/// differs from `target` (Felzenszwalb & Huttenlocher, separable and exact).
fn edt(inside: &[bool], w: usize, h: usize, target: bool) -> Vec<f64> {
    const INF: f64 = 1e20;
    let mut grid: Vec<f64> = inside
        .iter()
        .map(|&v| if v == target { INF } else { 0.0 })
        .collect();
    let mut f = vec![0f64; w.max(h)];
    let mut d = vec![0f64; w.max(h)];
    let mut v = vec![0usize; w.max(h)];
    let mut z = vec![0f64; w.max(h) + 1];
    // Columns in blocks of B, gathered row by row, so the reads stay contiguous
    // (a column at a time strides through memory and is several times slower).
    const B: usize = 16;
    let mut cols = vec![0f64; B * h];
    for x0 in (0..w).step_by(B) {
        let n = B.min(w - x0);
        for y in 0..h {
            for i in 0..n {
                cols[i * h + y] = grid[y * w + x0 + i];
            }
        }
        for i in 0..n {
            edt_1d(&cols[i * h..(i + 1) * h], &mut d[..h], &mut v, &mut z);
            cols[i * h..(i + 1) * h].copy_from_slice(&d[..h]);
        }
        for y in 0..h {
            for i in 0..n {
                grid[y * w + x0 + i] = cols[i * h + y];
            }
        }
    }
    for y in 0..h {
        f[..w].copy_from_slice(&grid[y * w..(y + 1) * w]);
        edt_1d(&f[..w], &mut d[..w], &mut v, &mut z);
        grid[y * w..(y + 1) * w].copy_from_slice(&d[..w]);
    }
    grid
}

fn edt_1d(f: &[f64], d: &mut [f64], v: &mut [usize], z: &mut [f64]) {
    let n = f.len();
    let mut k = 0;
    v[0] = 0;
    z[0] = f64::NEG_INFINITY;
    z[1] = f64::INFINITY;
    for q in 1..n {
        loop {
            let p = v[k];
            let s = ((f[q] + (q * q) as f64) - (f[p] + (p * p) as f64)) / (2.0 * (q - p) as f64);
            if s <= z[k] && k > 0 {
                k -= 1;
                continue;
            }
            if s <= z[k] {
                // k == 0 and the new parabola wins everywhere.
                v[0] = q;
                z[0] = f64::NEG_INFINITY;
                z[1] = f64::INFINITY;
                break;
            }
            k += 1;
            v[k] = q;
            z[k] = s;
            z[k + 1] = f64::INFINITY;
            break;
        }
    }
    k = 0;
    for (q, dq) in d.iter_mut().enumerate() {
        while z[k + 1] < q as f64 {
            k += 1;
        }
        let p = v[k];
        *dq = (q as f64 - p as f64).powi(2) + f[p];
    }
}

/// The fixture fonts (`tests/films/text/fonts`), for the unit tests.
#[cfg(test)]
pub fn test_font(name: &str) -> FontSpec {
    let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/films/text/fonts")
        .join(name);
    FontSpec {
        label: format!("tests/films/text/fonts/{name}"),
        file,
        index: 0,
        variations: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font(name: &str) -> Font {
        Font::load(&test_font(name)).unwrap()
    }

    fn style(size: f32) -> TextStyle {
        TextStyle {
            size,
            spread: 4.0,
            ..TextStyle::default()
        }
    }

    #[test]
    fn a_line_has_the_expected_box_and_metrics() {
        let latin = font("NotoSans-subset.ttf");
        let l = layout("Big Shadow", &[&latin], &style(32.0)).unwrap();
        assert_eq!(l.lines, 1);
        assert!(l.missing.is_empty() && l.warnings.is_empty());
        // 10 glyphs, all on one baseline, left to right.
        assert_eq!(l.glyphs.len(), 10);
        assert!(l.glyphs.windows(2).all(|w| w[1].x >= w[0].x));
        assert!(l.glyphs.iter().all(|g| g.y == l.glyphs[0].y));
        let [em, baseline, pitch, pad] = l.meta;
        assert_eq!((em, pad), (32.0, 4.0));
        assert!((pitch - 38.4).abs() < 1e-4);
        assert_eq!(l.glyphs[0].y, baseline);
        assert_eq!(l.height, (pitch + 8.0).ceil() as usize);
        assert!(l.width > 100 && l.width < 220, "{}", l.width);
    }

    #[test]
    fn wrapping_alignment_and_max_lines() {
        let latin = font("NotoSans-subset.ttf");
        let s = TextStyle {
            width: Some(120.0),
            align: Align::Center,
            ..style(24.0)
        };
        let l = layout("a tiny mouse, a tealight, and a lot of wall", &[&latin], &s).unwrap();
        assert!(l.lines >= 3, "{}", l.lines);
        assert_eq!(l.width, (120.0f32 + 8.0).ceil() as usize);
        // Every line fits the width, and centring leaves equal space on both sides.
        let mut by_line: std::collections::BTreeMap<i32, Vec<f32>> = Default::default();
        for g in &l.glyphs {
            by_line.entry(g.y as i32).or_default().push(g.x);
        }
        assert_eq!(by_line.len(), l.lines);
        for xs in by_line.values() {
            assert!(xs[0] >= 4.0 - 1e-3, "{xs:?}");
        }
        // max_lines refuses a text that wraps further.
        let e = layout(
            "a tiny mouse, a tealight, and a lot of wall",
            &[&latin],
            &TextStyle {
                max_lines: Some(2),
                ..s.clone()
            },
        )
        .unwrap_err();
        assert!(e.to_string().contains("more than max_lines = 2"), "{e}");
        // Newlines always break; trailing spaces don't count toward alignment.
        let right = TextStyle {
            align: Align::Right,
            width: Some(200.0),
            ..style(24.0)
        };
        let a = layout("ab", &[&latin], &right).unwrap();
        let b = layout("ab   ", &[&latin], &right).unwrap();
        assert_eq!(a.glyphs[0].x, b.glyphs[0].x);
        assert_eq!(
            layout("a\nb\n\nc", &[&latin], &style(24.0)).unwrap().lines,
            4
        );
    }

    #[test]
    fn a_word_wider_than_the_line_breaks_between_letters() {
        let latin = font("NotoSans-subset.ttf");
        let s = TextStyle {
            width: Some(60.0),
            ..style(24.0)
        };
        let l = layout("Supercalifragilistic", &[&latin], &s).unwrap();
        assert!(l.lines >= 3);
        assert!(
            l.warnings[0].contains("wider than width = 60"),
            "{:?}",
            l.warnings
        );
    }

    #[test]
    fn right_to_left_and_mixed_paragraphs() {
        let latin = font("NotoSans-subset.ttf");
        let hebrew = font("NotoSansHebrew-subset.ttf");
        let chain = [&latin, &hebrew];
        // "שלום" alone: an RTL paragraph, so start alignment puts it on the right, and
        // the visual order is reversed (the first letter, shin, is rightmost).
        let s = TextStyle {
            width: Some(300.0),
            ..style(32.0)
        };
        let l = layout("שלום", &chain, &s).unwrap();
        assert!(l.missing.is_empty());
        assert!(l.glyphs.iter().all(|g| g.font == 1));
        let max_x = l.glyphs.iter().map(|g| g.x).fold(0.0, f32::max);
        assert!(max_x > 200.0, "right-aligned: {max_x}");
        let shin = hebrew.face().glyph_index('ש').unwrap().0;
        let rightmost = l.glyphs.iter().max_by(|a, b| a.x.total_cmp(&b.x)).unwrap();
        assert_eq!(rightmost.id, shin);
        // Mixed: an LTR paragraph with a Hebrew word keeps the Latin on the left.
        let l = layout("Hi שלום!", &chain, &s).unwrap();
        let h = l.glyphs.iter().find(|g| g.font == 0).unwrap();
        assert!(h.x < 40.0, "{}", h.x);
        // direction = "rtl" forces the base direction of a Latin paragraph.
        let l = layout(
            "abc",
            &chain,
            &TextStyle {
                direction: Direction::Rtl,
                ..s.clone()
            },
        )
        .unwrap();
        assert!(l.glyphs[0].x > 200.0);
    }

    #[test]
    fn arabic_is_joined_and_fallback_is_per_grapheme() {
        let latin = font("NotoSans-subset.ttf");
        let arabic = font("NotoNaskhArabic-subset.ttf");
        let l = layout("مرحبا", &[&latin, &arabic], &style(48.0)).unwrap();
        assert!(l.missing.is_empty());
        assert!(l.glyphs.iter().all(|g| g.font == 1));
        // Joined forms differ from the isolated letters' nominal glyphs.
        let face = arabic.face();
        let nominal: Vec<u16> = "مرحبا"
            .chars()
            .map(|c| face.glyph_index(c).unwrap().0)
            .collect();
        let shaped: Vec<u16> = l.glyphs.iter().map(|g| g.id).collect();
        assert_ne!(
            shaped.iter().rev().copied().collect::<Vec<_>>(),
            nominal,
            "shaping should pick joining forms"
        );
    }

    #[test]
    fn missing_glyphs_are_reported() {
        let latin = font("NotoSans-subset.ttf");
        let l = layout("a→b", &[&latin], &style(24.0)).unwrap();
        assert_eq!(l.missing, vec!['→']);
    }

    #[test]
    fn letter_spacing_and_features() {
        let latin = font("NotoSans-subset.ttf");
        let plain = layout("office", &[&latin], &style(32.0)).unwrap();
        let spaced = layout(
            "office",
            &[&latin],
            &TextStyle {
                letter_spacing: 0.1,
                features: vec!["-liga".into()],
                ..style(32.0)
            },
        )
        .unwrap();
        assert!(spaced.width > plain.width);
        assert!(spaced.glyphs.len() >= plain.glyphs.len());
        let e = layout(
            "x",
            &[&latin],
            &TextStyle {
                features: vec!["not a feature!".into()],
                ..style(32.0)
            },
        )
        .unwrap_err();
        assert!(e.to_string().contains("isn't an OpenType feature"), "{e}");
    }

    #[test]
    fn variations_change_the_outlines() {
        let regular = font("NotoSans-subset.ttf");
        let mut spec = test_font("NotoSans-subset.ttf");
        spec.variations = vec![("wght".into(), 900.0)];
        let black = Font::load(&spec).unwrap();
        let s = style(32.0);
        let ink = |f: &Font| {
            let l = layout("Ink", &[f], &s).unwrap();
            rasterize(&l, &[f], s.spread)
                .rgba
                .chunks(4)
                .map(|p| p[0] as u32)
                .sum::<u32>()
        };
        assert!(ink(&black) > ink(&regular) * 5 / 4);
        spec.variations = vec![("slnt".into(), 1.0)];
        let e = Font::load(&spec).err().unwrap().to_string();
        assert!(
            e.contains("no \"slnt\" axis") && e.contains("\"wght\""),
            "{e}"
        );
    }

    #[test]
    fn unreadable_fonts_say_why() {
        let spec = |label: &str| FontSpec {
            file: label.into(),
            label: label.into(),
            index: 0,
            variations: vec![],
        };
        let e = Font::from_bytes(spec("a.woff2"), b"wOF2....".to_vec())
            .err()
            .unwrap();
        assert!(e.to_string().contains("convert it to TTF or OTF"));
        let e = Font::from_bytes(
            spec("b.ttf"),
            b"version https://git-lfs.github.com/spec/v1\n".to_vec(),
        )
        .err()
        .unwrap();
        assert!(e.to_string().contains("Git LFS pointer"));
        let e = Font::from_bytes(spec("c.ttf"), b"<html>".to_vec())
            .err()
            .unwrap();
        assert!(e.to_string().contains("not a font"));
    }

    #[test]
    fn coverage_and_distance_agree() {
        let latin = font("NotoSans-subset.ttf");
        let s = style(40.0);
        let l = layout("O", &[&latin], &s).unwrap();
        let img = rasterize(&l, &[&latin], s.spread);
        assert_eq!(img.rgba.len(), l.width * l.height * 4);
        let mut solid_in = 0;
        for p in img.rgba.chunks(4) {
            let (cov, sdf) = (p[0], p[1]);
            assert_eq!(p[3], cov);
            assert_eq!(p[2], 0);
            // Fully covered pixels are inside (≥ 0.5), empty ones outside (≤ 0.5).
            if cov == 255 {
                assert!(sdf >= 128, "covered but sdf {sdf}");
                solid_in += 1;
            }
            if cov == 0 {
                assert!(sdf <= 128, "empty but sdf {sdf}");
            }
        }
        assert!(solid_in > 20);
        // The padding ring is empty and far outside.
        assert_eq!(img.rgba[0], 0);
        assert!(img.rgba[1] < 20);
        // The counter of the O (its centre) is empty.
        let (cx, cy) = (l.width / 2, (l.meta[1] - l.meta[0] * 0.35) as usize);
        assert_eq!(img.rgba[(cy * l.width + cx) * 4], 0);
    }

    #[test]
    fn edt_matches_brute_force() {
        let (w, h) = (13, 9);
        let inside: Vec<bool> = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as i32, (i / w) as i32);
                (x - 6).pow(2) + (y - 4).pow(2) < 10 || (x == 1 && y == 7)
            })
            .collect();
        for target in [true, false] {
            let fast = edt(&inside, w, h, target);
            for i in 0..w * h {
                let want = if inside[i] != target {
                    0.0
                } else {
                    (0..w * h)
                        .filter(|&j| inside[j] != target)
                        .map(|j| {
                            let dx = (i % w) as f64 - (j % w) as f64;
                            let dy = (i / w) as f64 - (j / w) as f64;
                            dx * dx + dy * dy
                        })
                        .fold(f64::INFINITY, f64::min)
                };
                assert_eq!(fast[i], want, "target {target}, sample {i}");
            }
        }
    }

    /// The bytes of a small block are pinned: a change here means the raster changed
    /// (a crate upgrade, a maths change). Bump it on purpose, like a golden image.
    #[test]
    fn raster_is_pinned() {
        use sha2::Digest;
        let latin = font("NotoSans-subset.ttf");
        let s = style(24.0);
        let l = layout("Ag", &[&latin], &s).unwrap();
        let img = rasterize(&l, &[&latin], s.spread);
        let hash = sha2::Sha256::digest(&img.rgba);
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!((img.width, img.height), (l.width, l.height));
        assert_eq!(
            hex, "9850511a0030280f94bb488de7b852a744f9055c6fd49c11e4594b3e929b0eba",
            "{}×{}",
            img.width, img.height
        );
    }
}
