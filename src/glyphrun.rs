//! Glyph runs (#71): a laid-out text as an MTSDF atlas (`msdf`) plus a table
//! texture that says where every glyph goes, so a shader can draw, and move,
//! each glyph on its own.
//!
//! # Table format (engine contract)
//!
//! RGBA8 texels, `TABLE_WIDTH` per row, read with `texelFetch` (never filtered).
//! Record texel `i` is at (`i % TABLE_WIDTH`, `i / TABLE_WIDTH`), top row first.
//! A texel holds one u32 (R = most significant byte) or two u16 (R,G and B,A,
//! each most significant byte first). Fixed-point: `.4` = 1/16 px, `.8` = 1/256
//! px; `s` = two's complement. All lengths are texels of the layout (the text's
//! `size` em) unless they say atlas.
//!
//! Header, `HEADER_TEXELS` texels from texel 0:
//!
//! | texel | contents |
//! |---|---|
//! | 0 | format version (u16, `FORMAT`), kind (u16: `KIND_RUN` 0, `KIND_SET` 1) |
//! | 1 | glyph count (u32) |
//! | 2 | line count (u32) |
//! | 3 | first line record texel (u32) |
//! | 4 | first glyph record texel (u32) |
//! | 5 | layout em (u16 .4), atlas em (u16 .4) |
//! | 6 | atlas spread (u16 .8), line pitch (u16 .4) |
//! | 7 | line record texels (u16), glyph record texels (u16) |
//! | 8 | entry count (u32; 0 unless the text is subtitles) |
//! | 9 | first entry record texel (u32) |
//! | 10 | entry record texels (u16), reserved (u16, 0) |
//! | 11 | reserved (0) |
//!
//! Line record (`LINE_TEXELS`), one per visual line, top to bottom:
//!
//! | texel | contents |
//! |---|---|
//! | 0 | baseline y from the top of the block (u32 .8) |
//! | 1 | first glyph (u16), glyph count (u16) |
//! | 2 | pen x where the line starts (u16 .4), where it ends (u16 .4), after alignment: the end includes the last glyph's advance |
//! | 3 | reserved (0) |
//!
//! Glyph record (`GLYPH_TEXELS`), per glyph, lines in order and each line's
//! glyphs in visual order (left to right, after bidi), so pen x never decreases
//! within a line (search on it; the x offset is separate):
//!
//! | texel | contents |
//! |---|---|
//! | 0 | pen x (u16 .4), y offset from the baseline, down positive (s16 .4) |
//! | 1 | atlas tile x (u16), atlas tile y (u16), in atlas texels |
//! | 2 | atlas tile width (u16), height (u16); 0 × 0 for a glyph with no ink |
//! | 3 | tile left (s16 .4), tile top (s16 .4), atlas texels: with k = layout em / atlas em, the tile's top-left corner is at (pen x + x offset + left · k, baseline + y offset − top · k), and it is (width, height) · k big |
//! | 4 | grapheme index (u16), word index (u16), in logical order |
//! | 5 | line (u16), x offset from the pen, right positive (s16 .4): the glyph's origin is at pen x + x offset |
//!
//! Entry record (`ENTRY_TEXELS`), subtitles only, one per cue in time order:
//!
//! | texel | contents |
//! |---|---|
//! | 0 | start (u32, milliseconds of film time) |
//! | 1 | end (u32, milliseconds; shown while start ≤ t < end) |
//! | 2 | first line (u16), line count (u16) |
//! | 3 | reserved (0) |
//!
//! Every cue is laid out as its own paragraph(s) of one block, so a cue's lines
//! follow each other; to place a cue, offset by its first line's baseline.
//!
//! The glyph's index in the block is its record number. Grapheme and word
//! indices count from the start of the text (UAX #29), so a stagger follows
//! reading order whatever the direction.
//!
//! # Glyph sets
//!
//! A glyph set (`set = true`) is a table of kind `KIND_SET` with no lines: one
//! glyph record per character, sorted by code point (binary search from GLSL),
//! for strings a shader computes (counters, timecode). Each character is shaped
//! alone (no kerning or ligatures between set glyphs). Texels 0–3 are as above
//! with pen x 0 and y offset 0; texel 4 is the code point (u32); texel 5 is the
//! advance (u16 .4) and the x offset (s16 .4).

use anyhow::{Result, bail};
use std::collections::BTreeMap;
use toodee::TooDee;
use unicode_segmentation::UnicodeSegmentation;

use crate::msdf::{self, GlyphKey};
use crate::text::{Font, Layout, PlacedGlyph, TextStyle, layout};

pub const FORMAT: u16 = 1;
pub const KIND_RUN: u16 = 0;
pub const KIND_SET: u16 = 1;
pub const TABLE_WIDTH: usize = 1024;
pub const HEADER_TEXELS: usize = 12;
pub const ENTRY_TEXELS: usize = 4;
pub const LINE_TEXELS: usize = 4;
pub const GLYPH_TEXELS: usize = 6;
/// Indices are 16-bit.
pub const MAX_GLYPHS: usize = 65_535;

pub struct GlyphRun {
    pub atlas: msdf::Atlas,
    /// `TABLE_WIDTH` texels per row, top row first.
    pub table: TooDee<[u8; 4]>,
    /// The layout's warnings (a set has none), so the texture loader needn't lay
    /// the text out again for them.
    pub warnings: Vec<String>,
}

/// Builds the atlas and table for `layout`, the layout of `text` with `fonts`.
pub fn build(
    text: &str,
    layout: &Layout,
    fonts: &[&Font],
    atlas_em: f32,
    spread: f32,
) -> Result<GlyphRun> {
    build_with(text, layout, fonts, atlas_em, spread, &[])
}

/// A subtitle cue placed in the block: its time and its lines.
#[derive(Clone, Copy, Debug, PartialEq)]
struct EntryLines {
    start: u32,
    end: u32,
    first_line: usize,
    lines: usize,
}

/// Builds a subtitle glyph run: the cues' texts laid out as one block, one cue
/// after another, plus the entry table. `entries` must be in time order.
pub fn build_subtitles(
    entries: &[crate::subtitles::Entry],
    fonts: &[&Font],
    style: &TextStyle,
    atlas_em: f32,
    spread: f32,
) -> Result<(GlyphRun, Layout)> {
    if entries.windows(2).any(|w| w[1].start < w[0].end) {
        bail!("subtitle cues must be in time order without overlaps");
    }
    let text = entries
        .iter()
        .map(|e| e.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let l = layout(&text, fonts, style)?;
    // Which lines each cue took: by the byte ranges of the cues in `text`.
    let mut placed = vec![];
    let mut at = 0usize;
    for e in entries {
        let range = at..at + e.text.len();
        at = range.end + 1;
        let lines: Vec<u32> = l
            .glyphs
            .iter()
            .filter(|g| range.contains(&(g.cluster as usize)))
            .map(|g| g.line)
            .collect();
        let (Some(&a), Some(&b)) = (lines.iter().min(), lines.iter().max()) else {
            bail!(
                "the subtitle at {} has nothing to draw",
                crate::subtitles::stamp(e.start)
            );
        };
        placed.push(EntryLines {
            start: e.start,
            end: e.end,
            first_line: a as usize,
            lines: (b - a + 1) as usize,
        });
    }
    let run = build_with(&text, &l, fonts, atlas_em, spread, &placed)?;
    Ok((run, l))
}

fn build_with(
    text: &str,
    layout: &Layout,
    fonts: &[&Font],
    atlas_em: f32,
    spread: f32,
    entries: &[EntryLines],
) -> Result<GlyphRun> {
    let n = layout.glyphs.len();
    if n > MAX_GLYPHS {
        bail!("the text has {n} glyphs, more than {MAX_GLYPHS} in one glyph run");
    }
    // Each distinct glyph once, in a fixed order.
    let mut index: BTreeMap<GlyphKey, usize> = BTreeMap::new();
    for g in &layout.glyphs {
        index
            .entry(GlyphKey {
                font: g.font,
                id: g.id as u32,
            })
            .or_default();
    }
    let keys: Vec<GlyphKey> = index.keys().copied().collect();
    for (i, v) in index.values_mut().enumerate() {
        *v = i;
    }
    let atlas = msdf::build(&keys, fonts, atlas_em, spread)?;

    let graphemes: Vec<usize> = text.grapheme_indices(true).map(|(i, _)| i).collect();
    let words: Vec<usize> = text
        .split_word_bound_indices()
        .filter(|(_, w)| w.chars().any(char::is_alphanumeric))
        .map(|(i, _)| i)
        .collect();

    let em = layout.meta[0];
    let pitch = layout.meta[2];
    let lines = layout.lines;
    let mut t = Table::default();
    let line_at = HEADER_TEXELS;
    let glyph_at = line_at + lines * LINE_TEXELS;
    let entry_at = glyph_at + n * GLYPH_TEXELS;
    t.u16s(FORMAT, KIND_RUN);
    t.u32(n as u32);
    t.u32(lines as u32);
    t.u32(line_at as u32);
    t.u32(glyph_at as u32);
    t.u16s(
        fixed(em, 16.0, "the text size")?,
        fixed(atlas_em, 16.0, "the atlas size")?,
    );
    t.u16s(
        fixed(spread, 256.0, "the spread")?,
        fixed(pitch, 16.0, "the line pitch")?,
    );
    t.u16s(LINE_TEXELS as u16, GLYPH_TEXELS as u16);
    t.u32(entries.len() as u32);
    t.u32(if entries.is_empty() {
        0
    } else {
        entry_at as u32
    });
    t.u16s(ENTRY_TEXELS as u16, 0);
    t.u32(0);

    // Lines: glyphs are grouped by line, in order, already.
    let mut first = 0usize;
    for line in 0..lines {
        let count = layout.glyphs[first..]
            .iter()
            .take_while(|g| g.line as usize == line)
            .count();
        let baseline = first_baseline(layout) + line as f32 * pitch;
        let (x0, x1) = layout.line_spans[line];
        t.u32(fixed32(baseline, 256.0, "a baseline")?);
        t.u16s(first as u16, count as u16);
        t.u16s(
            fixed(x0, 16.0, "a line's x")?,
            fixed(x1, 16.0, "a line's x")?,
        );
        t.u32(0);
        first += count;
    }
    if first != n {
        bail!("glyph runs: glyphs aren't grouped by line (engine bug)");
    }

    for g in &layout.glyphs {
        let line_baseline = first_baseline(layout) + g.line as f32 * pitch;
        let tile = atlas.tiles[index[&GlyphKey {
            font: g.font,
            id: g.id as u32,
        }]];
        t.u16s(
            fixed(g.x - g.x_offset, 16.0, "a glyph's x")?,
            sfixed(g.y - line_baseline, "a glyph's y offset")?,
        );
        t.tile(tile)?;
        let at = g.cluster as usize;
        t.u16s(index_of(&graphemes, at) as u16, index_of(&words, at) as u16);
        t.u16s(g.line as u16, sfixed(g.x_offset, "a glyph's x offset")?);
    }
    for e in entries {
        t.u32(e.start);
        t.u32(e.end);
        t.u16s(e.first_line as u16, e.lines as u16);
        t.u32(0);
    }
    Ok(GlyphRun {
        atlas,
        table: t.finish()?,
        warnings: layout.warnings.clone(),
    })
}

/// Builds a glyph set: each distinct character of `chars` once, laid out alone
/// with `fonts` and `style` (its size, fallback and letter spacing apply).
pub fn build_set(
    chars: &str,
    fonts: &[&Font],
    style: &TextStyle,
    atlas_em: f32,
    spread: f32,
) -> Result<GlyphRun> {
    let mut set: Vec<char> = chars.chars().collect();
    set.sort();
    set.dedup();
    if set.is_empty() {
        bail!("a glyph set needs at least one character");
    }
    if let Some(c) = set.iter().find(|c| c.is_control()) {
        bail!(
            "a glyph set can't hold the control character U+{:04X}",
            *c as u32
        );
    }
    if set.len() > MAX_GLYPHS {
        bail!("a glyph set has at most {MAX_GLYPHS} characters");
    }
    // One layout per character: its glyph, advance and metrics.
    let mut placed = vec![];
    let mut meta = [0f32; 4];
    for &c in &set {
        let l = layout(&c.to_string(), fonts, style)?;
        meta = l.meta;
        if c.is_whitespace() {
            // Layout trims whitespace: shape it alone with the first font that has it.
            let mut found = None;
            for (fi, f) in fonts.iter().enumerate() {
                if let Some((id, adv)) = f.shape_alone(c, style.size, &style.features)? {
                    found = Some((fi, id, adv));
                    break;
                }
            }
            let Some((font, id, adv)) = found else {
                bail!("no font has U+{:04X} for the glyph set", c as u32);
            };
            let g = PlacedGlyph {
                font,
                id,
                x: 0.0,
                y: 0.0,
                x_offset: 0.0,
                line: 0,
                cluster: 0,
            };
            placed.push((c, g, adv + style.letter_spacing * style.size, 0.0));
            continue;
        }
        if let Some(m) = l.missing.first() {
            bail!("no font has U+{:04X} ({m}) for the glyph set", *m as u32);
        }
        let [g] = l.glyphs[..] else {
            bail!(
                "'{c}' is {} glyphs in this font; glyph sets take one glyph per character",
                l.glyphs.len()
            );
        };
        // Layout leaves the spacing off after the last letter; a set character is
        // followed by another, so it gets it back (as whitespace does above).
        let (x0, x1) = l.line_spans[0];
        let baseline = l.meta[1];
        placed.push((
            c,
            g,
            x1 - x0 + style.letter_spacing * style.size,
            g.y - baseline,
        ));
    }
    let mut index: BTreeMap<GlyphKey, usize> = BTreeMap::new();
    for (_, g, ..) in &placed {
        index
            .entry(GlyphKey {
                font: g.font,
                id: g.id as u32,
            })
            .or_default();
    }
    let keys: Vec<GlyphKey> = index.keys().copied().collect();
    for (i, v) in index.values_mut().enumerate() {
        *v = i;
    }
    let atlas = msdf::build(&keys, fonts, atlas_em, spread)?;

    let mut t = Table::default();
    t.u16s(FORMAT, KIND_SET);
    t.u32(set.len() as u32);
    t.u32(0);
    t.u32(HEADER_TEXELS as u32);
    t.u32(HEADER_TEXELS as u32);
    t.u16s(
        fixed(meta[0], 16.0, "the text size")?,
        fixed(atlas_em, 16.0, "the atlas size")?,
    );
    t.u16s(
        fixed(spread, 256.0, "the spread")?,
        fixed(meta[2], 16.0, "the line pitch")?,
    );
    t.u16s(LINE_TEXELS as u16, GLYPH_TEXELS as u16);
    t.u32(0);
    t.u32(0);
    t.u16s(ENTRY_TEXELS as u16, 0);
    t.u32(0);
    for (c, g, advance, dy) in &placed {
        let tile = atlas.tiles[index[&GlyphKey {
            font: g.font,
            id: g.id as u32,
        }]];
        t.u16s(0, sfixed(*dy, "a glyph's y offset")?);
        t.tile(tile)?;
        t.u32(*c as u32);
        t.u16s(
            fixed(*advance, 16.0, "an advance")?,
            sfixed(g.x_offset, "a glyph's x offset")?,
        );
    }
    Ok(GlyphRun {
        atlas,
        table: t.finish()?,
        warnings: vec![],
    })
}

/// The first baseline from the block's top, in layout texels.
fn first_baseline(layout: &Layout) -> f32 {
    layout.meta[1]
}

/// How many of `starts` are at or before `at`, minus one (0 before the first).
fn index_of(starts: &[usize], at: usize) -> usize {
    starts.partition_point(|&s| s <= at).saturating_sub(1)
}

fn fixed(v: f32, scale: f32, what: &str) -> Result<u16> {
    let q = (v * scale).round();
    if !(0.0..=u16::MAX as f32).contains(&q) {
        bail!("{what} ({v}) is out of range for a glyph run");
    }
    Ok(q as u16)
}

fn fixed32(v: f32, scale: f32, what: &str) -> Result<u32> {
    let q = (v as f64 * scale as f64).round();
    if !(0.0..=u32::MAX as f64).contains(&q) {
        bail!("{what} ({v}) is out of range for a glyph run");
    }
    Ok(q as u32)
}

/// Signed .4 fixed point, as two's complement u16.
fn sfixed(v: f32, what: &str) -> Result<u16> {
    let q = (v * 16.0).round();
    if !(i16::MIN as f32..=i16::MAX as f32).contains(&q) {
        bail!("{what} ({v}) is out of range for a glyph run");
    }
    Ok(q as i16 as u16)
}

#[derive(Default)]
struct Table(Vec<[u8; 4]>);

impl Table {
    fn u32(&mut self, v: u32) {
        self.0.push(v.to_be_bytes());
    }
    fn u16s(&mut self, a: u16, b: u16) {
        let (a, b) = (a.to_be_bytes(), b.to_be_bytes());
        self.0.push([a[0], a[1], b[0], b[1]]);
    }
    /// Glyph record texels 1–3: the atlas tile and bearing (zeros: no ink).
    fn tile(&mut self, tile: Option<msdf::Tile>) -> Result<()> {
        match tile {
            Some(tile) => {
                self.u16s(tile.x as u16, tile.y as u16);
                self.u16s(tile.w as u16, tile.h as u16);
                self.u16s(sfixed(tile.left, "a tile")?, sfixed(tile.top, "a tile")?);
            }
            None => {
                self.u32(0);
                self.u32(0);
                self.u32(0);
            }
        }
        Ok(())
    }
    /// Pads to whole rows of `TABLE_WIDTH`.
    fn finish(mut self) -> Result<TooDee<[u8; 4]>> {
        let rows = self.0.len().div_ceil(TABLE_WIDTH).max(1);
        if rows > msdf::MAX_SIDE {
            bail!("the glyph table is over {} rows", msdf::MAX_SIDE);
        }
        self.0.resize(rows * TABLE_WIDTH, [0; 4]);
        Ok(TooDee::from_vec(TABLE_WIDTH, rows, self.0))
    }
}

/// The table read back, as a shader would (tests).
#[cfg(test)]
#[derive(Debug, PartialEq)]
pub struct Decoded {
    pub kind: u16,
    pub em: f32,
    pub atlas_em: f32,
    pub spread: f32,
    pub pitch: f32,
    pub lines: Vec<DecodedLine>,
    pub glyphs: Vec<DecodedGlyph>,
    /// (start ms, end ms, first line, line count)
    pub entries: Vec<(u32, u32, usize, usize)>,
}

#[cfg(test)]
#[derive(Debug, PartialEq)]
pub struct DecodedLine {
    pub baseline: f32,
    pub first: usize,
    pub count: usize,
    pub x0: f32,
    pub x1: f32,
}

#[cfg(test)]
#[derive(Debug, PartialEq)]
pub struct DecodedGlyph {
    pub x: f32,
    pub dy: f32,
    pub tile: [u16; 4],
    pub left: f32,
    pub top: f32,
    pub grapheme: u16,
    pub word: u16,
    pub line: u16,
    pub dx: f32,
    /// Glyph sets only (else 0): the character and its advance.
    pub code: u32,
    pub advance: f32,
}

#[cfg(test)]
pub fn decode(table: &[[u8; 4]]) -> Result<Decoded> {
    use anyhow::Context;
    let u32_at = |i: usize| -> Result<u32> {
        Ok(u32::from_be_bytes(
            *table.get(i).context("glyph table too short")?,
        ))
    };
    let u16s_at = |i: usize| -> Result<(u16, u16)> {
        let t = table.get(i).context("glyph table too short")?;
        Ok((
            u16::from_be_bytes([t[0], t[1]]),
            u16::from_be_bytes([t[2], t[3]]),
        ))
    };
    let s = |v: u16| v as i16 as f32 / 16.0;
    let (format, kind) = u16s_at(0)?;
    if format != FORMAT {
        bail!("glyph table format {format} (want {FORMAT})");
    }
    if kind != KIND_RUN && kind != KIND_SET {
        bail!("glyph table kind {kind}");
    }
    let set = kind == KIND_SET;
    let (n, lines) = (u32_at(1)? as usize, u32_at(2)? as usize);
    let (line_at, glyph_at) = (u32_at(3)? as usize, u32_at(4)? as usize);
    let (em, atlas_em) = u16s_at(5)?;
    let (spread, pitch) = u16s_at(6)?;
    let (lt, gt) = u16s_at(7)?;
    let (lt, gt) = (lt as usize, gt as usize);
    let mut out = Decoded {
        kind,
        em: em as f32 / 16.0,
        atlas_em: atlas_em as f32 / 16.0,
        spread: spread as f32 / 256.0,
        pitch: pitch as f32 / 16.0,
        lines: vec![],
        glyphs: vec![],
        entries: vec![],
    };
    let (entries, entry_at) = (u32_at(8)? as usize, u32_at(9)? as usize);
    let (et, _) = u16s_at(10)?;
    for e in 0..entries {
        let at = entry_at + e * et as usize;
        let (first, count) = u16s_at(at + 2)?;
        out.entries
            .push((u32_at(at)?, u32_at(at + 1)?, first as usize, count as usize));
    }
    for l in 0..lines {
        let at = line_at + l * lt;
        let (first, count) = u16s_at(at + 1)?;
        let (x0, x1) = u16s_at(at + 2)?;
        out.lines.push(DecodedLine {
            baseline: u32_at(at)? as f32 / 256.0,
            first: first as usize,
            count: count as usize,
            x0: x0 as f32 / 16.0,
            x1: x1 as f32 / 16.0,
        });
    }
    for g in 0..n {
        let at = glyph_at + g * gt;
        let (x, dy) = u16s_at(at)?;
        let (tx, ty) = u16s_at(at + 1)?;
        let (tw, th) = u16s_at(at + 2)?;
        let (left, top) = u16s_at(at + 3)?;
        let (grapheme, word) = u16s_at(at + 4)?;
        let (line, dx) = u16s_at(at + 5)?;
        let mut g = DecodedGlyph {
            x: x as f32 / 16.0,
            dy: s(dy),
            tile: [tx, ty, tw, th],
            left: s(left),
            top: s(top),
            grapheme,
            word,
            line,
            dx: s(dx),
            code: 0,
            advance: 0.0,
        };
        if set {
            g.code = u32_at(at + 4)?;
            g.advance = line as f32 / 16.0;
            (g.grapheme, g.word, g.line) = (0, 0, 0);
        }
        out.glyphs.push(g);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::{TextStyle, layout, test_font};
    use toodee::TooDeeOps;

    fn font(name: &str) -> Font {
        Font::load(&test_font(name)).unwrap()
    }

    fn style(size: f32, width: Option<f32>) -> TextStyle {
        TextStyle {
            size,
            width,
            spread: 4.0,
            ..TextStyle::default()
        }
    }

    #[test]
    fn the_table_round_trips() {
        let f = font("NotoSans-subset.ttf");
        let text = "Big Shadow\nover the wall";
        let l = layout(text, &[&f], &style(40.0, None)).unwrap();
        let run = build(text, &l, &[&f], 48.0, 4.0).unwrap();
        assert_eq!(run.table.num_cols(), TABLE_WIDTH);
        let d = decode(run.table.data()).unwrap();
        assert_eq!((d.em, d.atlas_em, d.spread), (40.0, 48.0, 4.0));
        assert_eq!(d.lines.len(), 2);
        assert_eq!(d.glyphs.len(), l.glyphs.len());
        for (g, p) in d.glyphs.iter().zip(&l.glyphs) {
            assert!((g.x + g.dx - p.x).abs() <= 1.0 / 16.0, "{} vs {}", g.x, p.x);
            assert_eq!(g.line as u32, p.line);
            let line = &d.lines[g.line as usize];
            assert!((line.baseline + g.dy - p.y).abs() <= 1.0 / 16.0);
        }
        // Every glyph with ink points at its tile in the atlas.
        let tiles: Vec<_> = run.atlas.tiles.iter().flatten().collect();
        for g in d.glyphs.iter().filter(|g| g.tile[2] > 0) {
            assert!(
                tiles
                    .iter()
                    .any(|t| [t.x, t.y, t.w, t.h] == g.tile.map(|v| v as usize)
                        && (t.left - g.left).abs() <= 1.0 / 32.0
                        && (t.top - g.top).abs() <= 1.0 / 32.0)
            );
        }
        // Lines cover the glyphs in order.
        assert_eq!(d.lines[0].first, 0);
        assert_eq!(d.lines[1].first, d.lines[0].count);
        assert_eq!(d.lines[0].count + d.lines[1].count, d.glyphs.len());
        assert!(d.lines[1].baseline - d.lines[0].baseline == d.pitch);
    }

    #[test]
    fn each_distinct_glyph_is_in_the_atlas_once() {
        let f = font("NotoSans-subset.ttf");
        let text = "aaaa bbbb";
        let l = layout(text, &[&f], &style(40.0, None)).unwrap();
        let run = build(text, &l, &[&f], 48.0, 4.0).unwrap();
        // a, space, b
        assert_eq!(run.atlas.tiles.len(), 3);
    }

    #[test]
    fn glyphs_are_in_visual_order_and_indices_in_logical_order() {
        let latin = font("NotoSans-subset.ttf");
        let hebrew = font("NotoSansHebrew-subset.ttf");
        // An RTL word inside an LTR line: visual order reverses it, logical
        // grapheme indices don't.
        let text = "ab שלום cd";
        let l = layout(text, &[&latin, &hebrew], &style(40.0, None)).unwrap();
        let run = build(text, &l, &[&latin, &hebrew], 32.0, 4.0).unwrap();
        let d = decode(run.table.data()).unwrap();
        for w in d.glyphs.windows(2) {
            assert!(w[1].x >= w[0].x, "pen x decreases: {w:?}");
        }
        let hebrew_glyphs: Vec<_> = d
            .glyphs
            .iter()
            .zip(&l.glyphs)
            .filter(|(_, p)| p.font == 1)
            .map(|(g, _)| g.grapheme)
            .collect();
        assert_eq!(hebrew_glyphs.len(), 4);
        assert!(
            hebrew_glyphs.windows(2).all(|w| w[1] < w[0]),
            "Hebrew graphemes should run right to left visually: {hebrew_glyphs:?}"
        );
        // Words: ab = 0, שלום = 1, cd = 2.
        let words: Vec<u16> = d.glyphs.iter().map(|g| g.word).collect();
        assert_eq!(words.first(), Some(&0));
        assert_eq!(words.last(), Some(&2));
    }

    #[test]
    fn wrapped_lines_get_their_own_records() {
        let f = font("NotoSans-subset.ttf");
        let text = "one two three four five six";
        let l = layout(text, &[&f], &style(40.0, Some(200.0))).unwrap();
        assert!(l.lines > 2);
        let run = build(text, &l, &[&f], 48.0, 4.0).unwrap();
        let d = decode(run.table.data()).unwrap();
        assert_eq!(d.lines.len(), l.lines);
        for (i, line) in d.lines.iter().enumerate() {
            assert!(line.count > 0);
            assert!(
                d.glyphs[line.first..line.first + line.count]
                    .iter()
                    .all(|g| g.line as usize == i)
            );
            assert!(line.x0 <= line.x1);
        }
    }

    #[test]
    fn line_spans_include_the_last_glyphs_advance() {
        let f = font("NotoSans-subset.ttf");
        let text = "WW";
        let l = layout(text, &[&f], &style(40.0, None)).unwrap();
        let run = build(text, &l, &[&f], 48.0, 4.0).unwrap();
        let d = decode(run.table.data()).unwrap();
        let line = &d.lines[0];
        let last = d.glyphs.last().unwrap();
        // The line ends a W's advance (most of an em) after the last W's pen x,
        // and the span is the whole content width (no wrap width, left aligned).
        assert!(line.x1 - last.x > 0.5 * d.em, "{line:?} {last:?}");
        assert!((line.x0 - 4.0).abs() < 0.1 && (line.x1 - (l.width as f32 - 4.0)).abs() < 1.0);
    }

    #[test]
    fn a_mark_keeps_the_pen_order() {
        let f = font("NotoNaskhArabic-subset.ttf");
        // Beh + fatha, lam + fatha: each fatha is placed back over its letter.
        let text = "\u{628}\u{64e}\u{644}\u{64e}\u{645}";
        let l = layout(text, &[&f], &style(40.0, None)).unwrap();
        let run = build(text, &l, &[&f], 48.0, 4.0).unwrap();
        let d = decode(run.table.data()).unwrap();
        for w in d.glyphs.windows(2) {
            assert!(w[1].x >= w[0].x, "pen x decreases: {w:?}");
        }
        assert!(
            d.glyphs.iter().any(|g| g.dx != 0.0),
            "no glyph has an x offset: {:?}",
            d.glyphs
        );
        // A fatha shares its letter's grapheme: beh+fatha, lam+fatha, meem.
        let mut graphemes: Vec<u16> = d.glyphs.iter().map(|g| g.grapheme).collect();
        graphemes.sort();
        // (The font may draw a letter with more than one glyph.)
        let count = |i| graphemes.iter().filter(|&&g| g == i).count();
        assert!(
            count(0) >= 2 && count(1) >= 2 && count(2) >= 1,
            "{graphemes:?}"
        );
        graphemes.dedup();
        assert_eq!(graphemes, [0, 1, 2]);
    }

    #[test]
    fn tight_letter_spacing_and_leading_marks_still_build() {
        let f = font("NotoNaskhArabic-subset.ttf");
        // A mark with no letter before it is offset from the pen, and negative
        // letter spacing pulls pens together; with almost no padding either could
        // put a glyph's x below 0. The stored pen x must stay valid.
        let text = "\u{64e}\u{628}\u{644}\u{645}";
        let st = TextStyle {
            letter_spacing: -0.2,
            spread: 0.1,
            ..style(40.0, None)
        };
        let l = layout(text, &[&f], &st).unwrap();
        let run = build(text, &l, &[&f], 48.0, 4.0).unwrap();
        let d = decode(run.table.data()).unwrap();
        assert_eq!(d.glyphs.len(), l.glyphs.len());
        for (g, p) in d.glyphs.iter().zip(&l.glyphs) {
            assert!((g.x + g.dx - p.x).abs() <= 1.0 / 16.0);
        }
    }

    #[test]
    fn a_glyph_set_is_sorted_by_code_point_with_advances() {
        let f = font("NotoSans-subset.ttf");
        // Unsorted, with a duplicate and a space.
        let run = build_set("9:0 1.0", &[&f], &style(40.0, None), 48.0, 4.0).unwrap();
        let d = decode(run.table.data()).unwrap();
        assert_eq!(d.kind, KIND_SET);
        assert!(d.lines.is_empty());
        let codes: Vec<u32> = d.glyphs.iter().map(|g| g.code).collect();
        assert_eq!(codes, [' ', '.', '0', '1', '9', ':'].map(|c| c as u32));
        // Noto Sans has tabular-width digits: every digit advances the same, and
        // more than the full stop.
        let adv = |c: char| {
            d.glyphs
                .iter()
                .find(|g| g.code == c as u32)
                .unwrap()
                .advance
        };
        assert!(adv('0') > 10.0 && adv('0') == adv('1') && adv('1') == adv('9'));
        assert!(adv('.') < adv('0'));
        // A space has an advance and no ink; digits have ink.
        let space = &d.glyphs[0];
        assert!(space.advance > 0.0 && space.tile[2] == 0);
        assert!(d.glyphs[2].tile[2] > 0);
        // Same tiles as the atlas says.
        assert_eq!(run.atlas.tiles.len(), 6);
    }

    #[test]
    fn a_run_carries_its_layouts_warnings() {
        // The texture loader takes them from the run instead of laying out again.
        let f = font("NotoSans-subset.ttf");
        let st = style(40.0, Some(60.0));
        let l = layout("unbreakable", &[&f], &st).unwrap();
        assert!(!l.warnings.is_empty(), "the fixture should warn");
        let run = build("unbreakable", &l, &[&f], 48.0, 4.0).unwrap();
        assert_eq!(run.warnings, l.warnings);
    }

    #[test]
    fn a_glyph_set_advance_includes_letter_spacing_for_every_character() {
        let f = font("NotoSans-subset.ttf");
        let plain = decode(
            build_set("0 ", &[&f], &style(40.0, None), 48.0, 4.0)
                .unwrap()
                .table
                .data(),
        )
        .unwrap();
        let st = TextStyle {
            letter_spacing: 0.25,
            ..style(40.0, None)
        };
        let spaced = decode(build_set("0 ", &[&f], &st, 48.0, 4.0).unwrap().table.data()).unwrap();
        // Both the space and the digit gain 0.25 em = 10 px.
        for (a, b) in plain.glyphs.iter().zip(&spaced.glyphs) {
            assert!((b.advance - a.advance - 10.0).abs() < 0.1, "{a:?} {b:?}");
        }
    }

    #[test]
    fn a_glyph_set_falls_back_and_refuses_what_no_font_has() {
        let latin = font("NotoSans-subset.ttf");
        let hebrew = font("NotoSansHebrew-subset.ttf");
        let run = build_set("1ש", &[&latin, &hebrew], &style(40.0, None), 48.0, 4.0).unwrap();
        assert_eq!(decode(run.table.data()).unwrap().glyphs.len(), 2);
        let err = build_set("1ש", &[&latin], &style(40.0, None), 48.0, 4.0)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("U+05E9"), "{err}");
        assert!(build_set("", &[&latin], &style(40.0, None), 48.0, 4.0).is_err());
        assert!(build_set("1\n2", &[&latin], &style(40.0, None), 48.0, 4.0).is_err());
    }

    #[test]
    fn a_run_table_is_not_a_set() {
        let f = font("NotoSans-subset.ttf");
        let l = layout("12", &[&f], &style(40.0, None)).unwrap();
        let run = build("12", &l, &[&f], 48.0, 4.0).unwrap();
        let d = decode(run.table.data()).unwrap();
        assert_eq!(d.kind, KIND_RUN);
        assert!(d.glyphs.iter().all(|g| g.code == 0));
    }

    #[test]
    fn subtitles_get_an_entry_per_cue_with_its_lines() {
        use crate::subtitles::{Entry, Format, parse};
        let f = font("NotoSans-subset.ttf");
        let cues = parse(
            "1\n00:00:01,000 --> 00:00:02,500\nBig Shadow\n\n\
             2\n00:00:03,000 --> 00:00:04,000\nover the\nwall\n\n\
             3\n00:00:04,000 --> 00:00:05,000\none two three four five six seven\n",
            Format::Srt,
            "subs.srt",
        )
        .unwrap();
        let (run, l) = build_subtitles(&cues, &[&f], &style(40.0, Some(300.0)), 48.0, 4.0).unwrap();
        let d = decode(run.table.data()).unwrap();
        assert_eq!(d.entries.len(), 3);
        assert_eq!(d.entries[0], (1000, 2500, 0, 1));
        assert_eq!((d.entries[1].0, d.entries[1].1), (3000, 4000));
        assert_eq!((d.entries[1].2, d.entries[1].3), (1, 2));
        // The third cue wraps at 300 px into several lines, after the second's.
        assert_eq!(d.entries[2].2, 3);
        assert!(d.entries[2].3 >= 2);
        assert_eq!(d.entries[2].2 + d.entries[2].3, l.lines);
        // Lines of a cue hold only that cue's glyphs.
        let second: String = "over the\nwall".into();
        let wanted = l
            .glyphs
            .iter()
            .filter(|g| g.line == 1 || g.line == 2)
            .count();
        assert_eq!(wanted, second.chars().filter(|&c| c != '\n').count());
        // Overlaps are refused here too.
        let bad = [
            Entry {
                start: 0,
                end: 2000,
                text: "a".into(),
            },
            Entry {
                start: 1000,
                end: 3000,
                text: "b".into(),
            },
        ];
        assert!(build_subtitles(&bad, &[&f], &style(40.0, None), 48.0, 4.0).is_err());
    }

    #[test]
    fn a_plain_run_has_no_entries() {
        let f = font("NotoSans-subset.ttf");
        let l = layout("hi", &[&f], &style(40.0, None)).unwrap();
        let d = decode(build("hi", &l, &[&f], 48.0, 4.0).unwrap().table.data()).unwrap();
        assert!(d.entries.is_empty());
    }

    #[test]
    fn out_of_range_values_are_errors() {
        assert!(fixed(-1.0, 16.0, "x").is_err());
        assert!(fixed(4096.0, 16.0, "x").is_err());
        assert_eq!(fixed(4095.9375, 16.0, "x").unwrap(), u16::MAX);
        assert_eq!(sfixed(-1.5, "y").unwrap() as i16, -24);
        assert!(sfixed(3000.0, "y").is_err());
    }

    #[test]
    fn indices_count_from_the_start() {
        assert_eq!(index_of(&[0, 3, 7], 0), 0);
        assert_eq!(index_of(&[0, 3, 7], 4), 1);
        assert_eq!(index_of(&[0, 3, 7], 9), 2);
        assert_eq!(index_of(&[2], 0), 0);
    }
}
