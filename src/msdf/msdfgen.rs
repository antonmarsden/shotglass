use anyhow::Result;
use toodee::TooDee;

use super::{SEED, Seg};

#[repr(C)]
struct Edge {
    points: [[f64; 2]; 4],
    order: i32,
}

unsafe extern "C" {
    fn sg_msdfgen_generate(
        edges: *const Edge,
        contour_ends: *const usize,
        contour_count: usize,
        width: i32,
        height: i32,
        spread: f64,
        seed: u64,
        pixels: *mut u8,
    ) -> i32;
}

pub(super) fn generate(
    contours: &[Vec<Seg>],
    to_px: impl Fn((f64, f64)) -> (f64, f64),
    width: u32,
    height: u32,
    spread: f64,
) -> Result<TooDee<[u8; 4]>> {
    let mut edges = Vec::new();
    let mut contour_ends = Vec::with_capacity(contours.len());
    for contour in contours {
        for segment in contour {
            let (order, points) = match *segment {
                Seg::Line(a, b) => (1, [a, b, a, a]),
                Seg::Quad(a, b, c) => (2, [a, b, c, a]),
                Seg::Cubic(a, b, c, d) => (3, [a, b, c, d]),
            };
            let edge = Edge {
                order,
                points: points.map(|p| to_px(p).into()),
            };
            edges.push(edge);
        }
        contour_ends.push(edges.len());
    }
    let mut image = TooDee::init(width as usize, height as usize, [0; 4]);
    // The shim borrows the edge arrays and writes exactly width * height RGBA pixels.
    let status = unsafe {
        sg_msdfgen_generate(
            edges.as_ptr(),
            contour_ends.as_ptr(),
            contour_ends.len(),
            width as i32,
            height as i32,
            spread,
            SEED,
            image.data_mut().as_mut_ptr().cast(),
        )
    };
    anyhow::ensure!(status == 0, "msdfgen could not rasterize the glyph outline");
    Ok(image)
}
