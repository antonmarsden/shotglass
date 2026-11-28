//! Footage (#70), CPU side: image sequences (`renders/fog_####.png`, one image per
//! frame). A sequence's pattern, the scan that finds its frames (once, at
//! `Movie::load`), the errors for gaps, padding and case, and `check --deep`'s
//! decode of every frame. Which frame a channel shows when is `movie::FrameMap`;
//! decoding a frame is `texture::load`, as for any image; the renderer streams them.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Frames per footage channel that must fit in the texture budget at once: the
/// frame on screen and the next two (`Movie::load` refuses a shot that can't).
pub const MIN_FRAMES: usize = 3;

/// A sequence's file name split around its run of `#`: `fog_####.png` is prefix
/// `fog_`, 4 digits, suffix `.png`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Pattern {
    pub prefix: String,
    pub digits: usize,
    pub suffix: String,
}

impl Pattern {
    /// From a file name (no folder) with exactly one run of `#`.
    pub fn parse(name: &str) -> Result<Pattern, String> {
        let Some(start) = name.find('#') else {
            return Err(format!(
                "\"{name}\" has no # (write the frame number as #, like fog_####.png)"
            ));
        };
        let digits = name[start..].chars().take_while(|c| *c == '#').count();
        let suffix = &name[start + digits..];
        if suffix.contains('#') {
            return Err(format!(
                "\"{name}\": an image sequence has one run of # (the frame number)"
            ));
        }
        Ok(Pattern {
            prefix: name[..start].to_string(),
            digits,
            suffix: suffix.to_string(),
        })
    }

    /// The file name of frame number `number`: zero-padded to the run's width, wider
    /// when the number needs it (as Blender writes `fog_10000.png` for `####`).
    pub fn name(&self, number: u64) -> String {
        format!(
            "{}{:0width$}{}",
            self.prefix,
            number,
            self.suffix,
            width = self.digits
        )
    }

    /// The frame number of a file name this pattern writes, if it is one.
    pub fn number(&self, name: &str) -> Option<u64> {
        let mid = name
            .strip_prefix(&self.prefix)?
            .strip_suffix(&self.suffix)?;
        if mid.is_empty() || !mid.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        // Exactly the run's width, or wider only when the number needs it.
        if mid.len() < self.digits || (mid.len() > self.digits && mid.starts_with('0')) {
            return None;
        }
        mid.parse().ok()
    }

    /// The pattern as written: `fog_####.png`.
    pub fn written(&self) -> String {
        format!("{}{}{}", self.prefix, "#".repeat(self.digits), self.suffix)
    }
}

/// What the scan found: frames `first .. first + count` (file numbers), contiguous.
#[derive(Clone, Debug, PartialEq)]
pub struct Scan {
    pub dir: PathBuf,
    pub pattern: Pattern,
    /// The file number that is source frame 0.
    pub first: u64,
    pub count: u64,
    /// Source frame 0's size from its header (None if it can't be read: decoding it
    /// reports why). Every frame must have this size.
    pub size: Option<(usize, usize)>,
    /// A video (#73): the clip its frames are decoded from (`pattern` is then its
    /// file name with no digits, `first` 0).
    pub video: Option<std::sync::Arc<crate::video::Source>>,
}

impl Scan {
    /// The file of source frame `k` (0-based): a video's frames are all in one file.
    pub fn path(&self, k: u64) -> PathBuf {
        match &self.video {
            Some(v) => v.path.clone(),
            None => self.dir.join(self.pattern.name(self.first + k)),
        }
    }

    /// Source frame `k`'s name for messages: "fog_1042.png", "surf.webm frame 12".
    pub fn frame_name(&self, k: u64) -> String {
        match &self.video {
            Some(_) => format!("{} frame {k}", self.pattern.prefix),
            None => self.pattern.name(self.first + k),
        }
    }

    /// "fog_1001.png … fog_1100.png", "surf.webm frames 0 … 99".
    pub fn range(&self) -> String {
        if self.video.is_some() {
            return format!("{} frames 0 … {}", self.pattern.prefix, self.count - 1);
        }
        format!(
            "{} … {}",
            self.pattern.name(self.first),
            self.pattern.name(self.first + self.count - 1)
        )
    }
}

/// "1034", "1034–1036".
fn span(a: u64, b: u64) -> String {
    if a == b {
        format!("frame {a} is")
    } else {
        format!("frames {a}–{b} are")
    }
}

/// Find a sequence's frames in `dir`. `first` is the file number of source frame 0
/// (default: the lowest found); frames must be contiguous from there. Trailing frames
/// that aren't there yet just make the sequence shorter; a gap is an error. Names are
/// matched exactly (case-sensitive everywhere, so a film behaves the same on every OS).
pub fn scan(
    dir: &Path,
    label: &str,
    pattern: &Pattern,
    first: Option<u64>,
) -> Result<Scan, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        let folder = label.rsplit_once('/').map_or(".", |(d, _)| d);
        format!("{label}: can't list its folder \"{folder}\" ({e})")
    })?;
    let mut numbers = BTreeSet::new();
    let mut names = vec![];
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        match pattern.number(&name) {
            Some(n) => {
                numbers.insert(n);
            }
            None => names.push(name),
        }
    }
    let Some(&lowest) = numbers.first() else {
        return Err(format!(
            "{label}: matches no files{}",
            hint(pattern, &names)
        ));
    };
    let first = first.unwrap_or(lowest);
    if !numbers.contains(&first) {
        return Err(format!(
            "{label}: first = {first}, but {} isn't there (the files found run {} … {})",
            pattern.name(first),
            pattern.name(lowest),
            pattern.name(*numbers.last().unwrap())
        ));
    }
    let mut count = 0u64;
    let mut gap = None;
    for &n in numbers.range(first..) {
        if n == first + count {
            count += 1;
        } else {
            gap = Some(n);
            break;
        }
    }
    if let Some(next) = gap {
        let last = *numbers.last().unwrap();
        return Err(format!(
            "{label}: {} missing (between {first} and {last}); a sequence's frames must be \
             contiguous",
            span(first + count, next - 1)
        ));
    }
    let scan = Scan {
        dir: dir.to_path_buf(),
        pattern: pattern.clone(),
        first,
        count,
        size: None,
        video: None,
    };
    let size = crate::texture::dimensions(&scan.path(0));
    if let Some((w, h)) = size {
        crate::texture::check_size(&format!("{label} ({})", pattern.name(first)), w, h)?;
    }
    Ok(Scan { size, ..scan })
}

/// A video's frames (#73): its index, read once (`Movie::load`), as a scan.
pub fn scan_video(
    file: &Path,
    label: &str,
    opts: &crate::movie::VideoOptions,
) -> Result<Scan, String> {
    let source = crate::video::Source::open(file, opts).map_err(|e| format!("{label}: {e}"))?;
    let (w, h) = (
        source.clip.track.width as usize,
        source.clip.track.height as usize,
    );
    crate::texture::check_size(label, w, h)?;
    let name = file
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    Ok(Scan {
        dir: file.parent().unwrap_or(Path::new(".")).to_path_buf(),
        pattern: Pattern {
            prefix: name,
            digits: 0,
            suffix: String::new(),
        },
        first: 0,
        count: source.clip.frames.len() as u64,
        size: Some((w, h)),
        video: Some(std::sync::Arc::new(source)),
    })
}

/// Why nothing matched, when the folder suggests it: other padding, other case.
fn hint(p: &Pattern, names: &[String]) -> String {
    for n in names {
        let lower = n.to_ascii_lowercase();
        let lp = p.prefix.to_ascii_lowercase();
        let ls = p.suffix.to_ascii_lowercase();
        let Some(mid) = lower.strip_prefix(&lp).and_then(|r| r.strip_suffix(&ls)) else {
            continue;
        };
        if mid.is_empty() || !mid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if mid.len() != p.digits && n.starts_with(&p.prefix) && n.ends_with(&p.suffix) {
            let other = Pattern {
                digits: mid.len(),
                ..p.clone()
            };
            return format!(
                " (the files have {} digits, like \"{n}\": write \"{}\")",
                mid.len(),
                other.written()
            );
        }
        if mid.len() == p.digits {
            return format!(" (names are case-sensitive; the files are like \"{n}\")");
        }
    }
    String::new()
}

/// Decode every frame (`check --deep`): each must decode and have frame 0's size.
/// Returns the problems, at most `limit` of them.
pub fn check_frames(s: &Scan, label: &str, limit: usize) -> Vec<String> {
    let mut problems = vec![];
    let want = s.size;
    for k in 0..s.count {
        let name = s.pattern.name(s.first + k);
        let shown = match label.rsplit_once('/') {
            Some((d, _)) => format!("{d}/{name}"),
            None => name.clone(),
        };
        match crate::texture::load(&s.path(k), &shown, false) {
            Ok(img) => {
                if let Some((w, h)) = want
                    && (img.width, img.height) != (w, h)
                {
                    problems.push(size_error(&shown, (img.width, img.height), (w, h), s));
                }
            }
            Err(e) => problems.push(e),
        }
        if problems.len() >= limit {
            break;
        }
    }
    problems
}

/// "renders/fog_1042.png: 1280×720, but the sequence is 1920×1080 (frame 1001)".
pub fn size_error(shown: &str, got: (usize, usize), want: (usize, usize), s: &Scan) -> String {
    format!(
        "{shown}: {}×{}, but the sequence is {}×{} (frame {})",
        got.0, got.1, want.0, want.1, s.first
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder per test (removed and recreated), like the other modules' tests.
    struct Dir(PathBuf);
    impl Dir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp() -> Dir {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "shotglass-footage-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }

    fn dir_with(names: &[&str]) -> Dir {
        let d = tmp();
        for n in names {
            std::fs::write(d.path().join(n), b"").unwrap();
        }
        d
    }

    fn pat(s: &str) -> Pattern {
        Pattern::parse(s).unwrap()
    }

    #[test]
    fn pattern_names_and_numbers() {
        let p = pat("fog_####.png");
        assert_eq!(p.name(7), "fog_0007.png");
        assert_eq!(p.name(10000), "fog_10000.png");
        assert_eq!(p.number("fog_0007.png"), Some(7));
        assert_eq!(p.number("fog_10000.png"), Some(10000));
        // Narrower, wider with a leading zero, other text: not this pattern's.
        assert_eq!(p.number("fog_007.png"), None);
        assert_eq!(p.number("fog_00007.png"), None);
        assert_eq!(p.number("fog_00a7.png"), None);
        assert_eq!(p.number("fog_.png"), None);
        assert_eq!(p.number("Fog_0007.png"), None);
        assert_eq!(p.written(), "fog_####.png");
        assert_eq!(pat("#.jpg").name(3), "3.jpg");
        assert!(
            Pattern::parse("a_##_##.png")
                .unwrap_err()
                .contains("one run of #")
        );
        assert!(Pattern::parse("a.png").is_err());
    }

    #[test]
    fn scan_finds_first_and_count() {
        let d = dir_with(&[
            "f_1001.png",
            "f_1002.png",
            "f_1003.png",
            "other.png",
            "f_1001.jpg",
        ]);
        let s = scan(d.path(), "r/f_####.png", &pat("f_####.png"), None).unwrap();
        assert_eq!((s.first, s.count), (1001, 3));
        assert_eq!(s.path(2), d.path().join("f_1003.png"));
        // An explicit first skips what comes before it.
        let s = scan(d.path(), "r/f_####.png", &pat("f_####.png"), Some(1002)).unwrap();
        assert_eq!((s.first, s.count), (1002, 2));
        let e = scan(d.path(), "r/f_####.png", &pat("f_####.png"), Some(1)).unwrap_err();
        assert!(e.contains("first = 1, but f_0001.png isn't there"), "{e}");
        assert!(e.contains("f_1001.png … f_1003.png"), "{e}");
    }

    #[test]
    fn scan_reports_gaps() {
        let d = dir_with(&["f_1001.png", "f_1002.png", "f_1006.png", "f_1007.png"]);
        let e = scan(d.path(), "r/f_####.png", &pat("f_####.png"), None).unwrap_err();
        assert_eq!(
            e,
            "r/f_####.png: frames 1003–1005 are missing (between 1001 and 1007); a \
             sequence's frames must be contiguous"
        );
        let d = dir_with(&["f_1.png", "f_3.png"]);
        let e = scan(d.path(), "f_#.png", &pat("f_#.png"), None).unwrap_err();
        assert!(e.contains("frame 2 is missing"), "{e}");
    }

    #[test]
    fn scan_overflows_padding_like_blender() {
        let d = dir_with(&["f_9998.png", "f_9999.png", "f_10000.png", "f_10001.png"]);
        let s = scan(d.path(), "f_####.png", &pat("f_####.png"), None).unwrap();
        assert_eq!((s.first, s.count), (9998, 4));
        assert_eq!(s.path(3), d.path().join("f_10001.png"));
    }

    #[test]
    fn scan_hints_at_padding_and_case() {
        let d = dir_with(&["f_001.png", "f_002.png"]);
        let e = scan(d.path(), "f_####.png", &pat("f_####.png"), None).unwrap_err();
        assert!(e.contains("matches no files"), "{e}");
        assert!(e.contains("the files have 3 digits, like \"f_00"), "{e}");
        assert!(e.contains("write \"f_###.png\""), "{e}");
        let d = dir_with(&["F_0001.png"]);
        let e = scan(d.path(), "f_####.png", &pat("f_####.png"), None).unwrap_err();
        assert!(
            e.contains("names are case-sensitive; the files are like \"F_0001.png\""),
            "{e}"
        );
        let e = scan(
            &d.path().join("nope"),
            "nope/f_####.png",
            &pat("f_####.png"),
            None,
        )
        .unwrap_err();
        assert!(
            e.starts_with("nope/f_####.png: can't list its folder \"nope\""),
            "{e}"
        );
    }

    #[test]
    fn deep_check_finds_broken_and_odd_sized_frames() {
        let d = tmp();
        let save = |n: &str, w: u32| {
            image::RgbaImage::from_pixel(w, 2, image::Rgba([1, 2, 3, 255]))
                .save(d.path().join(n))
                .unwrap()
        };
        save("f_1.png", 4);
        save("f_2.png", 4);
        save("f_3.png", 5);
        std::fs::write(d.path().join("f_4.png"), b"").unwrap();
        let s = scan(d.path(), "r/f_#.png", &pat("f_#.png"), None).unwrap();
        assert_eq!(s.size, Some((4, 2)));
        let p = check_frames(&s, "r/f_#.png", 10);
        assert_eq!(p.len(), 2, "{p:?}");
        assert_eq!(p[0], "r/f_3.png: 5×2, but the sequence is 4×2 (frame 1)");
        assert!(p[1].starts_with("r/f_4.png: the file is empty"), "{}", p[1]);
    }
}
