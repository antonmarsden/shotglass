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
//! | 0 | format version (u32, `FORMAT`) |
//! | 1 | glyph count (u32) |
//! | 2 | line count (u32) |
//! | 3 | first line record texel (u32) |
//! | 4 | first glyph record texel (u32) |
//! | 5 | layout em (u16 .4), atlas em (u16 .4) |
//! | 6 | atlas spread (u16 .8), line pitch (u16 .4) |
//! | 7 | line record texels (u16), glyph record texels (u16) |
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
//! | 3 | tile left (s16 .4), tile top (s16 .4), atlas texels: the tile's top-left corner is at (pen x + left, baseline − top) · layout em / atlas em |
//! | 4 | grapheme index (u16), word index (u16), in logical order |
//! | 5 | line (u16), x offset from the pen, right positive (s16 .4): the glyph's origin is at pen x + x offset |
//!
//! The glyph's index in the block is its record number. Grapheme and word
//! indices count from the start of the text (UAX #29), so a stagger follows
//! reading order whatever the direction.

#![allow(dead_code)] // wired into `[texts]` in the next #71 step

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use toodee::TooDee;
use unicode_segmentation::UnicodeSegmentation;

use crate::msdf::{self, GlyphKey};
use crate::text::{Font, Layout};

pub const FORMAT: u32 = 1;
pub const TABLE_WIDTH: usize = 1024;
pub const HEADER_TEXELS: usize = 8;
pub const LINE_TEXELS: usize = 4;
pub const GLYPH_TEXELS: usize = 6;
/// Indices are 16-bit.
pub const MAX_GLYPHS: usize = 65_535;

pub struct GlyphRun {
    pub atlas: msdf::Atlas,
    /// `TABLE_WIDTH` texels per row, top row first.
    pub table: TooDee<[u8; 4]>,
}

/// Builds the atlas and table for `layout`, the layout of `text` with `fonts`.
pub fn build(
    text: &str,
    layout: &Layout,
    fonts: &[&Font],
    atlas_em: f32,
    spread: f32,
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
    t.u32(FORMAT);
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
        match tile {
            Some(tile) => {
                t.u16s(tile.x as u16, tile.y as u16);
                t.u16s(tile.w as u16, tile.h as u16);
                t.u16s(sfixed(tile.left, "a tile")?, sfixed(tile.top, "a tile")?);
            }
            None => {
                t.u32(0);
                t.u32(0);
                t.u32(0);
            }
        }
        let at = g.cluster as usize;
        t.u16s(index_of(&graphemes, at) as u16, index_of(&words, at) as u16);
        t.u16s(g.line as u16, sfixed(g.x_offset, "a glyph's x offset")?);
    }
    let table_height = t.0.len().div_ceil(TABLE_WIDTH).max(1);
    if table_height > crate::msdf::MAX_SIDE {
        bail!("the glyph table is over {} rows", crate::msdf::MAX_SIDE);
    }
    t.0.resize(table_height * TABLE_WIDTH, [0; 4]);
    Ok(GlyphRun {
        atlas,
        table: TooDee::from_vec(TABLE_WIDTH, table_height, t.0),
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
}

/// The table read back, as a shader would (tests and `check`).
#[derive(Debug, PartialEq)]
pub struct Decoded {
    pub em: f32,
    pub atlas_em: f32,
    pub spread: f32,
    pub pitch: f32,
    pub lines: Vec<DecodedLine>,
    pub glyphs: Vec<DecodedGlyph>,
}

#[derive(Debug, PartialEq)]
pub struct DecodedLine {
    pub baseline: f32,
    pub first: usize,
    pub count: usize,
    pub x0: f32,
    pub x1: f32,
}

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
}

pub fn decode(table: &[[u8; 4]]) -> Result<Decoded> {
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
    if u32_at(0)? != FORMAT {
        bail!("glyph table format {} (want {FORMAT})", u32_at(0)?);
    }
    let (n, lines) = (u32_at(1)? as usize, u32_at(2)? as usize);
    let (line_at, glyph_at) = (u32_at(3)? as usize, u32_at(4)? as usize);
    let (em, atlas_em) = u16s_at(5)?;
    let (spread, pitch) = u16s_at(6)?;
    let (lt, gt) = u16s_at(7)?;
    let (lt, gt) = (lt as usize, gt as usize);
    let mut out = Decoded {
        em: em as f32 / 16.0,
        atlas_em: atlas_em as f32 / 16.0,
        spread: spread as f32 / 256.0,
        pitch: pitch as f32 / 16.0,
        lines: vec![],
        glyphs: vec![],
    };
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
        out.glyphs.push(DecodedGlyph {
            x: x as f32 / 16.0,
            dy: s(dy),
            tile: [tx, ty, tw, th],
            left: s(left),
            top: s(top),
            grapheme,
            word,
            line,
            dx: s(dx),
        });
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
