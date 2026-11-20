//! Where a movie's files come from: the local disk (desktop) or a set of files
//! downloaded over HTTP before playback starts (web player).
//!
//! Everything that reads a movie file — `movie.toml`, scenes, sounds, the mix,
//! custom transitions and `#include`s — goes through a [`FileSource`], so the
//! web player can resolve the same paths against a URL instead of a folder.

use crate::shader::{include_candidates, include_name, normalize};
use std::collections::{BTreeSet, HashMap};
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub trait FileSource: Send + Sync + Debug {
    /// The file's text. Errors read like "path: reason".
    fn read(&self, path: &Path) -> std::io::Result<String>;
    fn exists(&self, path: &Path) -> bool;
}

/// The local file system (the desktop player and CLI).
#[derive(Debug, Default)]
pub struct Disk;

impl FileSource for Disk {
    fn read(&self, path: &Path) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
}

pub fn disk() -> Arc<dyn FileSource> {
    Arc::new(Disk)
}

/// Files held in memory, keyed by their path relative to the movie root
/// (e.g. downloaded over HTTP by the web player).
#[derive(Debug, Default, Clone)]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // web player + tests
pub struct Memory {
    files: HashMap<PathBuf, String>,
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
impl Memory {
    pub fn insert(&mut self, path: &Path, text: String) {
        self.files.insert(normalize(path), text);
    }
    pub fn len(&self) -> usize {
        self.files.len()
    }
}

impl FileSource for Memory {
    fn read(&self, path: &Path) -> std::io::Result<String> {
        self.files.get(&normalize(path)).cloned().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "not found (not downloaded)")
        })
    }
    fn exists(&self, path: &Path) -> bool {
        self.files.contains_key(&normalize(path))
    }
}

/// Works out which files a movie needs when they can only be fetched one by one
/// (HTTP): start from the files `movie.toml` names, then follow every `#include`,
/// trying the same candidates in the same order as the shader builder does.
///
/// Drive it with [`Preloader::wanted`] (paths to fetch next) and
/// [`Preloader::provide`] (the text, or `None` if the fetch failed or 404'd) until
/// `wanted` is empty, then take the files with [`Preloader::finish`].
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // web player + tests
pub struct Preloader {
    root: PathBuf,
    files: Memory,
    /// Paths already asked for (found or not).
    asked: BTreeSet<PathBuf>,
    /// Paths to hand out next.
    queue: Vec<PathBuf>,
    /// Includes still being resolved: remaining candidates, tried in order.
    includes: Vec<Vec<PathBuf>>,
    /// Paths that were asked for but couldn't be fetched.
    pub missing: Vec<PathBuf>,
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
impl Preloader {
    pub fn new(root: &Path, files: impl IntoIterator<Item = PathBuf>) -> Preloader {
        let mut p = Preloader {
            root: root.to_path_buf(),
            files: Memory::default(),
            asked: BTreeSet::new(),
            queue: vec![],
            includes: vec![],
            missing: vec![],
        };
        for f in files {
            p.want(normalize(&f));
        }
        p
    }

    /// Add a file that's already been fetched (e.g. `movie.toml` itself).
    pub fn insert(&mut self, path: &Path, text: String) {
        let path = normalize(path);
        self.asked.insert(path.clone());
        self.files.insert(&path, text);
    }

    fn want(&mut self, path: PathBuf) {
        if self.asked.insert(path.clone()) {
            self.queue.push(path);
        }
    }

    /// Paths to fetch now (may be fetched in parallel). Empty when done.
    pub fn wanted(&mut self) -> Vec<PathBuf> {
        self.advance_includes();
        std::mem::take(&mut self.queue)
    }

    /// The result of fetching `path`: its text, or `None` if it doesn't exist.
    pub fn provide(&mut self, path: &Path, text: Option<String>) {
        let path = normalize(path);
        match text {
            Some(text) => {
                let here = path.parent().unwrap_or(&self.root).to_path_buf();
                for line in text.lines() {
                    if let Some(name) = include_name(line) {
                        let mut c: Vec<PathBuf> = vec![];
                        for p in include_candidates(&here, &self.root, name) {
                            if !c.contains(&p) {
                                c.push(p);
                            }
                        }
                        self.includes.push(c);
                    }
                }
                self.files.insert(&path, text);
            }
            None => self.missing.push(path),
        }
    }

    /// For each unresolved include: done if a candidate exists; otherwise ask for
    /// the first candidate not yet asked about; give up when all are missing (the
    /// shader builder then reports "can't find #include").
    fn advance_includes(&mut self) {
        let includes = std::mem::take(&mut self.includes);
        for mut cands in includes {
            while let Some(first) = cands.first().cloned() {
                if self.files.exists(&first) {
                    break; // resolved
                }
                if !self.asked.contains(&first) {
                    self.want(first);
                    self.includes.push(cands);
                    break;
                }
                if !self.missing.contains(&first) {
                    // Asked but not answered yet: check again next round.
                    self.includes.push(cands);
                    break;
                }
                cands.remove(0);
            }
        }
    }

    pub fn finish(self) -> Memory {
        self.files
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a preloader against an in-memory "server", one round at a time.
    fn preload(server: &HashMap<&str, &str>, start: &[&str]) -> (Memory, Vec<PathBuf>, usize) {
        let mut p = Preloader::new(Path::new(""), start.iter().map(PathBuf::from));
        let mut fetched = 0;
        loop {
            let want = p.wanted();
            if want.is_empty() {
                break;
            }
            for w in want {
                fetched += 1;
                let key = w.to_string_lossy().replace('\\', "/");
                p.provide(&w, server.get(key.as_str()).map(|s| s.to_string()));
            }
        }
        let missing = p.missing.clone();
        (p.finish(), missing, fetched)
    }

    #[test]
    fn follows_includes_like_the_shader_builder() {
        let server: HashMap<&str, &str> = [
            (
                "scenes/a.glsl",
                "#include \"../common.glsl\"\n#include \"lib/noise.glsl\"\n",
            ),
            ("common.glsl", "#include \"deep.glsl\"\nfloat c;\n"),
            ("deep.glsl", "float d;\n"),
            // Not next to the scene: found at the movie root instead.
            ("lib/noise.glsl", "float n;\n"),
            ("scenes/b.glsl", "#include \"../common.glsl\"\n"),
        ]
        .into_iter()
        .collect();
        let (files, missing, fetched) = preload(&server, &["scenes/a.glsl", "scenes/b.glsl"]);
        for f in [
            "scenes/a.glsl",
            "scenes/b.glsl",
            "common.glsl",
            "deep.glsl",
            "lib/noise.glsl",
        ] {
            assert!(files.exists(Path::new(f)), "{f} wasn't fetched");
        }
        assert_eq!(files.len(), 5);
        // scenes/lib/noise.glsl was tried first (and missing), like the builder does.
        assert_eq!(missing, vec![PathBuf::from("scenes/lib/noise.glsl")]);
        // common.glsl is shared but fetched once.
        assert_eq!(fetched, 6);
    }

    #[test]
    fn missing_files_and_includes_are_left_for_the_builder_to_report() {
        let server: HashMap<&str, &str> = [("a.glsl", "#include \"nope.glsl\"\n")]
            .into_iter()
            .collect();
        let (files, missing, _) = preload(&server, &["a.glsl", "gone.glsl"]);
        assert!(files.exists(Path::new("a.glsl")));
        assert!(!files.exists(Path::new("gone.glsl")));
        assert!(missing.contains(&PathBuf::from("gone.glsl")));
        assert!(missing.contains(&PathBuf::from("nope.glsl")));
    }

    #[test]
    fn memory_paths_are_normalised() {
        let mut m = Memory::default();
        m.insert(Path::new("scenes/../common.glsl"), "x".into());
        assert!(m.exists(Path::new("./common.glsl")));
        assert_eq!(m.read(Path::new("common.glsl")).unwrap(), "x");
        assert!(m.read(Path::new("other.glsl")).is_err());
    }
}
