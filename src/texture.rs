//! iChannel textures, CPU side: decoding PNG and JPEG files to RGBA8 (flipped for
//! `vflip`), size limits, memory accounting and the error messages. No OpenGL here; the
//! renderer (`gfx.rs`) uploads what this produces. The same code decodes on every
//! platform (including wasm), so every machine uploads identical texels.

use std::path::Path;

/// Largest side of an image: every desktop GPU and practically every WebGL 2
/// implementation supports 4096.
pub const MAX_SIZE: usize = 4096;
/// The WebGL 2 minimum for `MAX_TEXTURE_SIZE` (some phones): `check --web` warns above.
pub const WEB_MAX_SIZE: usize = 2048;
/// Texture memory the renderer keeps resident (RGBA8 with mipmaps).
pub const BUDGET: usize = 256 << 20;

/// How a channel is sampled when minified/magnified (`filter` in movie.toml).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Filter {
    /// Trilinear: `LINEAR_MIPMAP_LINEAR` / `LINEAR` (the default, as on Shadertoy).
    Mipmap,
    Linear,
    Nearest,
}

/// What a channel does outside 0..1 (`wrap` in movie.toml), on both axes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Wrap {
    Repeat,
    Clamp,
    Mirror,
}

impl Filter {
    pub const NAMES: &[&str] = &["mipmap", "linear", "nearest"];
    pub fn parse(s: &str) -> Option<Filter> {
        match s {
            "mipmap" => Some(Filter::Mipmap),
            "linear" => Some(Filter::Linear),
            "nearest" => Some(Filter::Nearest),
            _ => None,
        }
    }
}

impl Wrap {
    pub const NAMES: &[&str] = &["repeat", "clamp", "mirror"];
    pub fn parse(s: &str) -> Option<Wrap> {
        match s {
            "repeat" => Some(Wrap::Repeat),
            "clamp" => Some(Wrap::Clamp),
            "mirror" => Some(Wrap::Mirror),
            _ => None,
        }
    }
}

/// An image ready to upload: RGBA8, straight alpha, rows in upload order (the first
/// row is texture row 0, which `texture()` samples at v = 0).
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedImage {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    /// Things worth saying that don't stop the image from being used (16-bit data, an
    /// extension that doesn't match the content).
    pub warnings: Vec<String>,
    /// `iChannelMeta` for this texture: a text block's (em, first baseline, line pitch,
    /// padding) in texels; zeros for images.
    pub meta: [f32; 4],
}

/// The extensions shotglass reads (any case).
pub fn has_image_extension(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg"]
        .iter()
        .any(|e| lower.ends_with(e) && lower.len() > e.len())
}

/// GPU memory of a `w` × `h` RGBA8 texture with a full mipmap chain.
pub fn gpu_bytes(w: usize, h: usize) -> usize {
    let (mut w, mut h) = (w.max(1), h.max(1));
    let mut total = 0;
    loop {
        total += w * h * 4;
        if w == 1 && h == 1 {
            return total;
        }
        w = (w / 2).max(1);
        h = (h / 2).max(1);
    }
}

/// "85 MB", "5.3 MB", "21 KB".
pub fn describe_bytes(b: usize) -> String {
    let mb = b as f64 / (1 << 20) as f64;
    if mb >= 10.0 {
        format!("{mb:.0} MB")
    } else if mb >= 0.1 {
        format!("{mb:.1} MB")
    } else {
        format!("{} KB", b.div_ceil(1024))
    }
}

/// Refuse an image larger than `MAX_SIZE` on a side.
pub fn check_size(label: &str, w: usize, h: usize) -> Result<(), String> {
    if w > MAX_SIZE || h > MAX_SIZE {
        return Err(format!(
            "{label}: {w}×{h} is larger than {MAX_SIZE}×{MAX_SIZE}, the most every GPU and \
             browser supports; scale it down"
        ));
    }
    if w == 0 || h == 0 {
        return Err(format!("{label}: the image is empty ({w}×{h})"));
    }
    Ok(())
}

/// Read and decode a file (see `decode`).
pub fn load(path: &Path, label: &str, vflip: bool) -> Result<DecodedImage, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{label}: {e}"))?;
    decode(&bytes, label, vflip)
}

/// Decode PNG or JPEG bytes to RGBA8. Grey becomes (g, g, g, 1), RGB gets a = 1,
/// palettes are expanded and 16-bit channels are rounded to 8 bits (with a warning).
/// Colour-space chunks and ICC profiles are ignored: texels are the stored numbers.
/// With `vflip` the rows are reversed, so the image is upright when sampled with
/// `fragCoord / iResolution.xy` (Shadertoy's default).
pub fn decode(bytes: &[u8], label: &str, vflip: bool) -> Result<DecodedImage, String> {
    let format = sniff(bytes, label)?;
    let name = match format {
        image::ImageFormat::Png => "PNG",
        _ => "JPEG",
    };
    let mut warnings = vec![];
    let lower = label.to_ascii_lowercase();
    let ext_png = lower.ends_with(".png");
    if ext_png != (format == image::ImageFormat::Png) && has_image_extension(label) {
        warnings.push(format!(
            "{label} is a {name} file; its extension says otherwise (it is read as {name})"
        ));
    }
    let reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    // Check the size from the header first: no point decoding a 16k image to refuse it.
    let header = image::ImageReader::with_format(std::io::Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|e| decode_error(label, name, &e))?;
    check_size(label, header.0 as usize, header.1 as usize)?;
    let mut reader = reader;
    reader.no_limits();
    let img = reader.decode().map_err(|e| decode_error(label, name, &e))?;
    if img.color().bytes_per_pixel() / img.color().channel_count() > 1 {
        warnings.push(format!(
            "{label} is 16-bit; shotglass keeps 8 bits per channel for now"
        ));
    }
    let rgba = img.into_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    let mut data = rgba.into_raw();
    if vflip {
        flip_rows(&mut data, w, h);
    }
    Ok(DecodedImage {
        width: w,
        height: h,
        rgba: data,
        warnings,
        meta: [0.0; 4],
    })
}

/// Reverse the rows of an RGBA8 image in place (top row first ↔ bottom row first).
fn flip_rows(data: &mut [u8], w: usize, h: usize) {
    let row = w * 4;
    for y in 0..h / 2 {
        let (top, bottom) = data.split_at_mut((h - 1 - y) * row);
        top[y * row..(y + 1) * row].swap_with_slice(&mut bottom[..row]);
    }
}

/// Lay out and rasterise a text block (#68), oriented like an image (`vflip` puts the
/// top line at v = 1, upright with `fragCoord / iResolution.xy`). Runs on the decode
/// worker: tens of milliseconds for a large block.
pub fn load_text(t: &crate::movie::TextSource, vflip: bool) -> Result<DecodedImage, String> {
    let (layout, fonts) = t.layout().map_err(|e| format!("{e:#}"))?;
    let refs: Vec<&crate::text::Font> = fonts.iter().collect();
    let img = crate::text::rasterize(&layout, &refs, t.style.spread);
    let mut rgba = img.rgba;
    if vflip {
        flip_rows(&mut rgba, img.width, img.height);
    }
    Ok(DecodedImage {
        width: img.width,
        height: img.height,
        rgba,
        warnings: layout
            .warnings
            .iter()
            .map(|w| format!("text \"{}\": {w}", t.name))
            .collect(),
        meta: layout.meta,
    })
}

/// Width and height from the file's header, without decoding the pixels.
pub fn dimensions(path: &Path) -> Option<(usize, usize)> {
    let r = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?;
    let (w, h) = r.into_dimensions().ok()?;
    Some((w as usize, h as usize))
}

/// Texture memory a shot's channels need at once (each texture once: image sizes from
/// their headers, text blocks from their layout), and a description per texture for
/// messages. Unreadable files count 0: their error is reported where they're used.
pub fn shot_memory(
    channels: &[Option<crate::movie::Channel>; crate::movie::N_CHANNELS],
) -> (usize, Vec<String>) {
    let mut seen: Vec<&crate::movie::Source> = vec![];
    let mut total = 0;
    let mut parts = vec![];
    for c in channels.iter().flatten() {
        if seen.contains(&&c.source) {
            continue;
        }
        seen.push(&c.source);
        let size = match &c.source {
            crate::movie::Source::Image(f) => dimensions(&f.file),
            crate::movie::Source::Text(t) => t.layout().ok().map(|(l, _)| (l.width, l.height)),
            _ => None, // another pass's picture, not a texture
        };
        if let Some((w, h)) = size {
            let b = gpu_bytes(w, h);
            total += b;
            parts.push(format!(
                "{} {} ({w}×{h} with mipmaps)",
                c.label(),
                describe_bytes(b)
            ));
        }
    }
    (total, parts)
}

/// The format from the content. Explains the usual mistakes: an HTML page or a Git
/// LFS pointer saved instead of the image.
fn sniff(bytes: &[u8], label: &str) -> Result<image::ImageFormat, String> {
    if bytes.starts_with(b"version https://git-lfs") {
        return Err(format!(
            "{label}: this is a Git LFS pointer, not the image; run `git lfs pull`"
        ));
    }
    match image::guess_format(bytes) {
        Ok(f @ (image::ImageFormat::Png | image::ImageFormat::Jpeg)) => Ok(f),
        Ok(other) => Err(format!(
            "{label}: this is a {other:?} image; shotglass reads PNG and JPEG images"
        )),
        Err(_) if bytes.is_empty() => {
            Err(format!("{label}: the file is empty (still being saved?)"))
        }
        Err(_) => {
            let start: String = String::from_utf8_lossy(&bytes[..bytes.len().min(9)])
                .chars()
                .filter(|c| !c.is_control())
                .collect();
            let html = start.trim_start().starts_with('<');
            Err(format!(
                "{label}: not a PNG or JPEG (it starts with \"{start}\"{})",
                if html {
                    "; an HTML page saved by mistake?"
                } else {
                    ""
                }
            ))
        }
    }
}

fn decode_error(label: &str, name: &str, e: &image::ImageError) -> String {
    let msg = e.to_string();
    let lower = msg.to_ascii_lowercase();
    if lower.contains("eof") || lower.contains("end of") || lower.contains("unexpected end") {
        format!("{label}: unexpected end of {name} data (still being saved?)")
    } else {
        format!("{label}: couldn't decode the {name} data ({msg})")
    }
}

/// Edit distance of at most one (one character inserted, deleted or replaced)?
fn within_one_edit(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (short, long) = if a.len() <= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    if long.len() - short.len() > 1 {
        return false;
    }
    if short.len() == long.len() {
        return short
            .iter()
            .zip(long.iter())
            .filter(|(x, y)| x != y)
            .count()
            <= 1;
    }
    let prefix = short
        .iter()
        .zip(long.iter())
        .take_while(|(x, y)| x == y)
        .count();
    short[prefix..] == long[prefix + 1..]
}

/// For a missing `rel` (relative to `base`, forward slashes): a file in the same folder
/// whose name differs only in case or by one edit.
pub fn did_you_mean(base: &Path, rel: &str) -> Option<String> {
    let (dir, name) = match rel.rsplit_once('/') {
        Some((d, n)) => (Some(d), n),
        None => (None, rel),
    };
    let folder = dir.map_or(base.to_path_buf(), |d| base.join(d));
    let mut best: Option<String> = None;
    for e in std::fs::read_dir(folder).ok()?.flatten() {
        let other = e.file_name().to_string_lossy().into_owned();
        if other == name {
            continue;
        }
        if other.eq_ignore_ascii_case(name) || within_one_edit(&other, name) {
            let full = dir.map_or(other.clone(), |d| format!("{d}/{other}"));
            if best.as_ref().is_none_or(|b| full < *b) {
                best = Some(full);
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(color: image::ExtendedColorType, w: u32, h: u32, data: &[u8]) -> Vec<u8> {
        use image::ImageEncoder;
        let mut out = vec![];
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(data, w, h, color)
            .unwrap();
        out
    }

    #[test]
    fn decodes_every_png_kind_to_rgba8() {
        use image::ExtendedColorType as C;
        // 2x1 images: left pixel, right pixel.
        let grey = decode(&png(C::L8, 2, 1, &[10, 200]), "g.png", false).unwrap();
        assert_eq!(grey.rgba, [10, 10, 10, 255, 200, 200, 200, 255]);
        let ga = decode(&png(C::La8, 2, 1, &[10, 50, 200, 0]), "ga.png", false).unwrap();
        assert_eq!(ga.rgba, [10, 10, 10, 50, 200, 200, 200, 0]);
        let rgb = decode(&png(C::Rgb8, 2, 1, &[1, 2, 3, 4, 5, 6]), "c.png", false).unwrap();
        assert_eq!(rgb.rgba, [1, 2, 3, 255, 4, 5, 6, 255]);
        let rgba = decode(
            &png(C::Rgba8, 2, 1, &[1, 2, 3, 4, 5, 6, 7, 8]),
            "c.png",
            false,
        )
        .unwrap();
        assert_eq!(rgba.rgba, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(rgba.warnings.is_empty(), "{:?}", rgba.warnings);
        // 16-bit (the encoder takes native-endian samples): rounded to 8 bits, with a warning.
        let samples: Vec<u8> = [0xFFFFu16, 0x8000]
            .iter()
            .flat_map(|v| v.to_ne_bytes())
            .collect();
        let wide = decode(&png(C::L16, 2, 1, &samples), "h.png", false).unwrap();
        assert_eq!(wide.rgba[0], 255);
        assert_eq!(wide.rgba[4], 128);
        assert!(wide.warnings[0].contains("16-bit"), "{:?}", wide.warnings);
    }

    #[test]
    fn palette_pngs_are_expanded() {
        // A 2x1 indexed PNG written by hand with the png crate: palette red, blue.
        let mut out = vec![];
        {
            let mut e = png::Encoder::new(&mut out, 2, 1);
            e.set_color(png::ColorType::Indexed);
            e.set_depth(png::BitDepth::Eight);
            e.set_palette(vec![255, 0, 0, 0, 0, 255]);
            let mut w = e.write_header().unwrap();
            w.write_image_data(&[1, 0]).unwrap();
        }
        let img = decode(&out, "p.png", false).unwrap();
        assert_eq!(img.rgba, [0, 0, 255, 255, 255, 0, 0, 255]);
    }

    #[test]
    fn jpegs_decode() {
        use image::ImageEncoder;
        let mut out = vec![];
        let data = [200u8; 8 * 8 * 3];
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 95)
            .write_image(&data, 8, 8, image::ExtendedColorType::Rgb8)
            .unwrap();
        let img = decode(&out, "t.jpg", false).unwrap();
        assert_eq!((img.width, img.height), (8, 8));
        assert!(
            img.rgba
                .chunks(4)
                .all(|p| p[0].abs_diff(200) <= 2 && p[3] == 255)
        );
        // The content decides the format; a wrong extension is a warning.
        let img = decode(&out, "t.png", false).unwrap();
        assert!(img.warnings[0].contains("JPEG"), "{:?}", img.warnings);
    }

    #[test]
    fn vflip_reverses_rows() {
        use image::ExtendedColorType as C;
        // 1x3, top to bottom: 1, 2, 3.
        let bytes = png(C::L8, 1, 3, &[1, 2, 3]);
        let rows = |img: DecodedImage| img.rgba.chunks(4).map(|p| p[0]).collect::<Vec<_>>();
        assert_eq!(rows(decode(&bytes, "a.png", false).unwrap()), [1, 2, 3]);
        assert_eq!(rows(decode(&bytes, "a.png", true).unwrap()), [3, 2, 1]);
        let bytes = png(C::L8, 1, 4, &[1, 2, 3, 4]);
        assert_eq!(rows(decode(&bytes, "a.png", true).unwrap()), [4, 3, 2, 1]);
    }

    #[test]
    fn explains_bad_files() {
        let e = decode(b"<!DOCTYPE html><html>", "t/b.png", true).unwrap_err();
        assert!(
            e.contains("not a PNG or JPEG") && e.contains("<!DOCTYPE") && e.contains("HTML"),
            "{e}"
        );
        let e = decode(
            b"version https://git-lfs.github.com/spec/v1\noid sha256:abc\n",
            "t/b.png",
            true,
        )
        .unwrap_err();
        assert!(
            e.contains("Git LFS pointer") && e.contains("git lfs pull"),
            "{e}"
        );
        let full = png(image::ExtendedColorType::Rgb8, 8, 8, &[7; 192]);
        let e = decode(&full[..full.len() / 2], "t/b.png", true).unwrap_err();
        assert!(
            e.starts_with("t/b.png: ") && e.contains("still being saved"),
            "{e}"
        );
        assert!(decode(b"", "t/b.png", true).unwrap_err().contains("empty"));
        // GIF is recognised, but not read.
        let e = decode(b"GIF89a\x01\x00\x01\x00", "t/b.png", true).unwrap_err();
        assert!(e.contains("PNG and JPEG"), "{e}");
    }

    #[test]
    fn size_limits() {
        assert!(check_size("a.png", 4096, 4096).is_ok());
        let e = check_size("textures/sky.png", 8192, 4096).unwrap_err();
        assert_eq!(
            e,
            "textures/sky.png: 8192×4096 is larger than 4096×4096, the most every GPU and \
             browser supports; scale it down"
        );
        // The header is checked before decoding.
        let mut big = vec![];
        {
            let mut e = png::Encoder::new(&mut big, 5000, 1);
            e.set_color(png::ColorType::Grayscale);
            let mut w = e.write_header().unwrap();
            w.write_image_data(&[0; 5000]).unwrap();
        }
        assert!(decode(&big, "w.png", false).unwrap_err().contains("5000×1"));
        assert_eq!(gpu_bytes(1, 1), 4);
        assert_eq!(gpu_bytes(2, 2), 20);
        assert_eq!(gpu_bytes(4, 1), 16 + 8 + 4);
        // 4096² with mips is about 85 MB; 1024² about 5.3 MB.
        assert_eq!(describe_bytes(gpu_bytes(4096, 4096)), "85 MB");
        assert_eq!(describe_bytes(gpu_bytes(1024, 1024)), "5.3 MB");
    }

    #[test]
    fn names_and_suggestions() {
        assert!(has_image_extension("a/b.PNG") && has_image_extension("x.jpeg"));
        assert!(!has_image_extension("a.webp") && !has_image_extension(".png"));
        assert!(Filter::parse("point").is_none() && Wrap::parse("mirror").is_some());
        assert!(within_one_edit("stars.png", "strs.png"));
        assert!(within_one_edit("stars.png", "stbrs.png"));
        assert!(within_one_edit("stars.png", "stars.pnh"));
        assert!(!within_one_edit("stars.png", "sky.png"));
        let dir = std::env::temp_dir().join(format!("shotglass-tex-dym-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("textures")).unwrap();
        std::fs::write(dir.join("textures/stars.png"), "").unwrap();
        assert_eq!(
            did_you_mean(&dir, "textures/strs.png").as_deref(),
            Some("textures/stars.png")
        );
        assert_eq!(
            did_you_mean(&dir, "textures/Stars.png").as_deref(),
            Some("textures/stars.png")
        );
        assert_eq!(did_you_mean(&dir, "textures/moon.png"), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
