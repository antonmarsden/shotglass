//! Glyph atlases for glyph runs (#71): each distinct glyph once, as a
//! multi-channel + true signed distance field (MTSDF), shelf-packed into one
//! RGBA8 image.
//!
//! Tile format: RGB = multi-channel distance (the median is the distance; corners
//! stay sharp), A = true distance. A signed distance d px (inside positive) is
//! stored as ⌊255 · (d / (2 · spread) + 0.5)⌋ (fdsm truncates), so the outline
//! lies between 127 and 128: threshold at 127.5 / 255. Values saturate `spread`
//! px from the edge.
//! Rows are top first, glyphs upright (y down), like every texture we decode.
//!
//! Generated with the `fdsm` crate. Output is the same on every run on one
//! machine; across platforms its cubic solver uses the platform's `acos`/`cbrt`,
//! so tests compare against a reference with a one-level tolerance, not a hash.

// Not wired into `[texts]` yet (the next #71 step); the tests use all of it.
#![allow(dead_code)]

use anyhow::{Context, Result, bail};
use fdsm::bezier::scanline::FillRule;
use fdsm::bezier::{Point, Segment};
use fdsm::generate::generate_mtsdf;
use fdsm::render::correct_sign_mtsdf;
use fdsm::shape::{Contour, Shape};
use image::RgbaImage;
use skrifa::font::glyph::outline::PathSink;
use toodee::{CopyOps, TooDee, TooDeeOps, TooDeeOpsMut};

use crate::text::Font;

/// Largest atlas side, as for any texture.
pub const MAX_SIDE: usize = 4096;
/// Empty texels around every tile (atlas edges included), so linear filtering
/// at a tile's edge never reads a neighbour, whatever the wrap mode.
const GUTTER: usize = 1;
/// Corner threshold for edge colouring (sine of the angle), fdsm's default.
const CORNER_SIN: f64 = 0.03;
/// Edge-colouring seed: fixed, so the atlas is reproducible.
const SEED: u64 = 69441337420;

/// One glyph to put in an atlas: glyph `id` of `fonts[font]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GlyphKey {
    pub font: usize,
    pub id: u32,
}

/// Where a glyph's tile is, and how it sits on the pen position. All in atlas
/// pixels at the atlas em size. The tile's top-left corner is at
/// (pen x + `left`, baseline − `top`), y down.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tile {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub left: f32,
    pub top: f32,
}

pub struct Atlas {
    /// RGBA8 texels, top row first.
    pub image: TooDee<[u8; 4]>,
    /// Per requested glyph, in order; `None` for a glyph with no outline (a space).
    pub tiles: Vec<Option<Tile>>,
    pub em: f32,
    pub spread: f32,
}

/// A glyph's distance field, before packing.
struct Field {
    image: TooDee<[u8; 4]>,
    left: f32,
    top: f32,
}

/// Builds the atlas for `glyphs` (each should appear once) at `em` px per em.
pub fn build(glyphs: &[GlyphKey], fonts: &[&Font], em: f32, spread: f32) -> Result<Atlas> {
    if !(em > 0.0 && spread > 0.0) {
        bail!("atlas em size and spread must be positive");
    }
    let fields = glyphs
        .iter()
        .map(|g| field(fonts[g.font], g.id, em as f64, spread as f64))
        .collect::<Result<Vec<Option<Field>>>>()?;
    let sizes: Vec<Option<(usize, usize)>> = fields
        .iter()
        .map(|f| f.as_ref().map(|f| (f.image.num_cols(), f.image.num_rows())))
        .collect();
    let (width, height, places) = pack(&sizes)?;
    let mut atlas = TooDee::init(width, height, [0u8; 4]);
    let mut tiles = Vec::with_capacity(glyphs.len());
    for (f, place) in fields.iter().zip(&places) {
        let (Some(f), Some((x, y))) = (f, place) else {
            tiles.push(None);
            continue;
        };
        let (w, h) = (f.image.num_cols(), f.image.num_rows());
        atlas
            .view_mut((*x, *y), (x + w, y + h))
            .copy_from_toodee(&f.image);
        tiles.push(Some(Tile {
            x: *x,
            y: *y,
            w,
            h,
            left: f.left,
            top: f.top,
        }));
    }
    Ok(Atlas {
        image: atlas,
        tiles,
        em,
        spread,
    })
}

/// Shelf packing: tallest first (ties by width, then request order), rows left
/// to right. The width is about the square root of the area, so atlases are
/// roughly square. Pure function of the sizes.
#[allow(clippy::type_complexity)]
fn pack(sizes: &[Option<(usize, usize)>]) -> Result<(usize, usize, Vec<Option<(usize, usize)>>)> {
    let mut order: Vec<usize> = (0..sizes.len()).filter(|&i| sizes[i].is_some()).collect();
    order.sort_by_key(|&i| {
        let (w, h) = sizes[i].unwrap();
        (std::cmp::Reverse(h), std::cmp::Reverse(w), i)
    });
    let area: usize = order
        .iter()
        .map(|&i| {
            let (w, h) = sizes[i].unwrap();
            (w + GUTTER) * (h + GUTTER)
        })
        .sum();
    // Every tile carries a gutter on its right and bottom; one more column and
    // row on the left and top.
    let widest = order
        .iter()
        .map(|&i| sizes[i].unwrap().0 + GUTTER)
        .max()
        .unwrap_or(1)
        + GUTTER;
    let width = ((area as f64).sqrt().ceil() as usize + GUTTER)
        .max(widest)
        .min(MAX_SIDE);
    if widest > MAX_SIDE {
        bail!("a glyph is wider than {MAX_SIDE} px at this atlas size");
    }
    let mut places = vec![None; sizes.len()];
    let (mut x, mut y, mut shelf) = (GUTTER, GUTTER, 0);
    for i in order {
        let (w, h) = sizes[i].unwrap();
        if x + w + GUTTER > width {
            x = GUTTER;
            y += shelf;
            shelf = 0;
        }
        places[i] = Some((x, y));
        x += w + GUTTER;
        shelf = shelf.max(h + GUTTER);
    }
    let height = (y + shelf).max(1);
    if height > MAX_SIDE {
        bail!(
            "the glyph atlas would be {width}×{height} px, over {MAX_SIDE}: use a smaller atlas size or fewer glyphs"
        );
    }
    Ok((width, height, places))
}

/// The MTSDF of one glyph, `None` when it has no outline (a space), an error
/// when its outline is broken.
fn field(font: &Font, id: u32, em: f64, spread: f64) -> Result<Option<Field>> {
    let scale = em / font.units_per_em() as f64;
    let mut pen = Pen::default();
    if !font
        .draw_outline(id, &mut pen)
        .context("can't build the glyph atlas")?
    {
        return Ok(None);
    }
    pen.finish();
    let Some((x0, y0, x1, y1)) = pen.bounds else {
        return Ok(None);
    };
    // Tile: the control-point box (which contains the outline), plus `spread`.
    let w = ((x1 - x0) * scale + 2.0 * spread).ceil() as u32;
    let h = ((y1 - y0) * scale + 2.0 * spread).ceil() as u32;
    let left = x0 * scale - spread;
    let top = y1 * scale + spread;
    // Font units, y up → tile pixels, y down.
    let to_px = |(x, y): (f64, f64)| Point::new(x * scale - left, top - y * scale);
    let contours = pen
        .contours
        .iter()
        .map(|c| Contour {
            segments: c
                .iter()
                .map(|s| match *s {
                    Seg::Line(a, b) => Segment::line(to_px(a), to_px(b)),
                    Seg::Quad(a, b, c) => Segment::quad(to_px(a), to_px(b), to_px(c)),
                    Seg::Cubic(a, b, c, d) => {
                        Segment::cubic(to_px(a), to_px(b), to_px(c), to_px(d))
                    }
                })
                .collect(),
        })
        .collect();
    let colored = Shape::edge_coloring_simple(Shape { contours }, CORNER_SIN, SEED);
    let prepared = colored.prepare();
    let mut image = RgbaImage::new(w, h);
    generate_mtsdf(&prepared, 2.0 * spread, &mut image);
    correct_sign_mtsdf(&mut image, &prepared, FillRule::Nonzero);
    let px = image.pixels().map(|p| p.0).collect();
    Ok(Some(Field {
        image: TooDee::from_vec(w as usize, h as usize, px),
        left: left as f32,
        top: top as f32,
    }))
}

#[derive(Clone, Copy)]
enum Seg {
    Line((f64, f64), (f64, f64)),
    Quad((f64, f64), (f64, f64), (f64, f64)),
    Cubic((f64, f64), (f64, f64), (f64, f64), (f64, f64)),
}

/// Collects an outline as closed contours of segments (font units).
#[derive(Default)]
struct Pen {
    contours: Vec<Vec<Seg>>,
    current: Vec<Seg>,
    start: (f64, f64),
    last: (f64, f64),
    bounds: Option<(f64, f64, f64, f64)>,
}

impl Pen {
    fn grow(&mut self, (x, y): (f64, f64)) {
        let b = self.bounds.get_or_insert((x, y, x, y));
        *b = (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y));
    }
    fn push(&mut self, s: Seg, to: (f64, f64)) {
        self.current.push(s);
        self.last = to;
    }
    fn finish(&mut self) {
        if self.last != self.start && !self.current.is_empty() {
            self.current.push(Seg::Line(self.last, self.start));
        }
        self.last = self.start;
        let c = std::mem::take(&mut self.current);
        if !c.is_empty() {
            self.contours.push(c);
        }
    }
}

impl PathSink for Pen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.finish();
        let p = (x as f64, y as f64);
        self.grow(p);
        self.start = p;
        self.last = p;
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let p = (x as f64, y as f64);
        if p != self.last {
            self.grow(p);
            self.push(Seg::Line(self.last, p), p);
        }
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (c, p) = ((x1 as f64, y1 as f64), (x as f64, y as f64));
        // Degenerate curves (a control point on an end point) are lines, or
        // nothing: fdsm can't take a zero-length segment.
        if c == self.last || c == p {
            return self.line_to(x, y);
        }
        self.grow(c);
        self.grow(p);
        self.push(Seg::Quad(self.last, c, p), p);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (c1, c2, p) = (
            (x1 as f64, y1 as f64),
            (x2 as f64, y2 as f64),
            (x as f64, y as f64),
        );
        if (c1 == self.last || c1 == p) && (c2 == self.last || c2 == p) {
            return self.line_to(x, y);
        }
        self.grow(c1);
        self.grow(c2);
        self.grow(p);
        self.push(Seg::Cubic(self.last, c1, c2, p), p);
    }
    fn close(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::test_font;

    fn latin() -> Font {
        Font::load(&test_font("NotoSans-subset.ttf")).unwrap()
    }

    /// Glyph id of `c` in `font` (cmap), for tests.
    fn gid(font: &Font, c: char) -> u32 {
        font.glyph_for(c).expect("glyph in the fixture font")
    }

    fn texel(a: &Atlas, x: usize, y: usize) -> [u8; 4] {
        a.image[(x, y)]
    }

    fn median(p: [u8; 4]) -> u8 {
        let [r, g, b, _] = p;
        r.max(g).min(r.min(g).max(b))
    }

    #[test]
    fn tiles_are_disjoint_and_inside() {
        let f = latin();
        let keys: Vec<_> = "AHOgjW.,"
            .chars()
            .map(|c| GlyphKey {
                font: 0,
                id: gid(&f, c),
            })
            .collect();
        let a = build(&keys, &[&f], 48.0, 4.0).unwrap();
        let t: Vec<Tile> = a.tiles.iter().map(|t| t.unwrap()).collect();
        for (i, p) in t.iter().enumerate() {
            assert!(p.x + p.w <= a.image.num_cols() && p.y + p.h <= a.image.num_rows());
            for q in &t[i + 1..] {
                let apart = p.x + p.w + GUTTER <= q.x
                    || q.x + q.w + GUTTER <= p.x
                    || p.y + p.h + GUTTER <= q.y
                    || q.y + q.h + GUTTER <= p.y;
                assert!(apart, "{p:?} overlaps {q:?}");
            }
        }
    }

    #[test]
    fn tiles_keep_a_gutter_from_the_atlas_edges() {
        let sizes = vec![Some((5, 7)), Some((3, 3)), None, Some((6, 2))];
        let (w, h, places) = pack(&sizes).unwrap();
        for (s, p) in sizes.iter().zip(&places) {
            let (Some((sw, sh)), Some((x, y))) = (s, p) else {
                assert!(s.is_none() && p.is_none());
                continue;
            };
            assert!(*x >= GUTTER && *y >= GUTTER, "{x},{y}");
            assert!(x + sw + GUTTER <= w && y + sh + GUTTER <= h);
        }
    }

    #[test]
    fn degenerate_curves_become_lines_or_nothing() {
        let mut pen = Pen::default();
        pen.move_to(0.0, 0.0);
        pen.quad_to(0.0, 0.0, 0.0, 0.0); // a point: nothing
        pen.quad_to(0.0, 0.0, 10.0, 0.0); // control on the start: a line
        pen.quad_to(10.0, 10.0, 10.0, 10.0); // control on the end: a line
        pen.curve_to(10.0, 10.0, 0.0, 10.0, 0.0, 10.0); // controls on the ends
        pen.curve_to(0.0, 10.0, 0.0, 10.0, 0.0, 10.0); // a point: nothing
        pen.close();
        let c = &pen.contours[0];
        assert_eq!(c.len(), 4, "three lines plus the closing one");
        assert!(c.iter().all(|s| matches!(s, Seg::Line(a, b) if a != b)));
    }

    #[test]
    fn a_space_has_no_tile() {
        let f = latin();
        let keys = [
            GlyphKey {
                font: 0,
                id: gid(&f, ' '),
            },
            GlyphKey {
                font: 0,
                id: gid(&f, 'I'),
            },
        ];
        let a = build(&keys, &[&f], 48.0, 4.0).unwrap();
        assert!(a.tiles[0].is_none());
        assert!(a.tiles[1].is_some());
    }

    #[test]
    fn inside_is_above_half_and_outside_below() {
        let f = latin();
        let a = build(
            &[GlyphKey {
                font: 0,
                id: gid(&f, 'I'),
            }],
            &[&f],
            48.0,
            4.0,
        )
        .unwrap();
        let t = a.tiles[0].unwrap();
        // The middle of an I's stem is inside; the tile's corner (spread px of
        // padding) is outside.
        let mid = texel(&a, t.x + t.w / 2, t.y + t.h / 2);
        let corner = texel(&a, t.x, t.y);
        assert!(median(mid) >= 128 && mid[3] >= 128, "{mid:?}");
        assert!(median(corner) <= 127 && corner[3] <= 127, "{corner:?}");
        // Upright: an I's tile sits on the baseline, so its top is about the cap
        // height plus the spread above it.
        assert!(t.top > 30.0 && t.top < 40.0, "top {}", t.top);
    }

    #[test]
    fn the_same_glyphs_give_the_same_atlas() {
        let f = latin();
        let keys: Vec<_> = "Shadow"
            .chars()
            .map(|c| GlyphKey {
                font: 0,
                id: gid(&f, c),
            })
            .collect();
        let a = build(&keys, &[&f], 40.0, 4.0).unwrap();
        let b = build(&keys, &[&f], 40.0, 4.0).unwrap();
        assert_eq!(a.image.num_cols(), b.image.num_cols());
        assert!(a.image == b.image);
    }

    #[test]
    fn too_many_glyphs_is_an_error() {
        let sizes = vec![Some((1000, 1000)); 40];
        assert!(pack(&sizes).is_err());
    }

    /// Pins the atlas of a short string against `tests/golden/msdf-atlas.png`,
    /// allowing one level per channel (the cubic solver's maths library differs
    /// across platforms). `SHOTGLASS_BLESS=1` rewrites it; look at it after.
    #[test]
    fn atlas_matches_its_reference() {
        let f = latin();
        let mut keys: Vec<_> = "Big Shadow, AVW 0123456789:."
            .chars()
            .map(|c| GlyphKey {
                font: 0,
                id: gid(&f, c),
            })
            .collect();
        keys.sort();
        keys.dedup();
        let a = build(&keys, &[&f], 48.0, 4.0).unwrap();
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/msdf-atlas.png");
        let (w, h) = (a.image.num_cols() as u32, a.image.num_rows() as u32);
        let bytes = a.image.data().iter().flatten().copied().collect();
        let img = RgbaImage::from_raw(w, h, bytes).unwrap();
        if std::env::var_os("SHOTGLASS_BLESS").is_some() {
            img.save(&path).unwrap();
            return;
        }
        let want = image::open(&path)
            .expect("tests/golden/msdf-atlas.png (SHOTGLASS_BLESS=1 cargo test msdf to write it)")
            .to_rgba8();
        assert_eq!(want.dimensions(), img.dimensions());
        let worst = want
            .as_raw()
            .iter()
            .zip(img.as_raw())
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0);
        assert!(
            worst <= 1,
            "atlas differs from its reference by up to {worst} levels"
        );
    }
}
