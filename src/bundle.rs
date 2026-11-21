//! `shotglass licenses` and `shotglass bundle` (issue #3).
//!
//! - `licenses` lists every library a film actually includes, with the SPDX licences from
//!   its `library.toml`, and flags non-commercial ones.
//! - `bundle` writes a self-contained copy of a film: its folder, plus exactly the locked
//!   library files it includes (and each library's `library.toml` and licence files) in
//!   `shotglass-libs/<name>/`, with `shotglass.lock` pointing at them (`path`). The bundle
//!   loads with no cache, no network and no overrides, and never changes.

use crate::library::{self, Lock, Origin};
use crate::movie::Movie;
use crate::shader::Dialect;
use crate::validate;
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Where a bundle keeps its libraries: `shotglass-libs/<local name>/`.
pub const LIBS_DIR: &str = "shotglass-libs";
/// The licence summary a bundle carries at its root.
pub const NOTICES_FILE: &str = "LIBRARY-LICENSES.md";

/// What a film includes from its libraries: per local name, the files (relative to the
/// library root, forward slashes) reached by `#include` from any of its shaders, and the
/// library textures its shots bind. Shaders that can't be assembled (a missing file) are
/// returned as messages.
pub fn used_files(m: &Movie) -> (BTreeMap<String, BTreeSet<String>>, Vec<String>) {
    let mut used: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut errors = vec![];
    // Includes are expanded textually (not under #if), so one dialect finds them all.
    for s in validate::movie_sources(m, Dialect::desktop(m.glsl)) {
        let a = match s.src {
            Ok(a) => a,
            Err(e) => {
                errors.push(format!("{}: {e:#}", s.what));
                continue;
            }
        };
        // Library files are labelled `<local name>/<path in the library>`.
        for label in &a.files {
            let Some((name, rest)) = label.split_once('/') else {
                continue;
            };
            if let Some(l) = m.libraries.iter().find(|l| l.name == name)
                && l.root.join(rest).is_file()
            {
                used.entry(name.to_string())
                    .or_default()
                    .insert(rest.to_string());
            }
        }
    }
    for c in m.shots.iter().flat_map(|s| s.channels.iter().flatten()) {
        if let Some(lib) = &c.library {
            used.entry(lib.clone()).or_default().insert(c.rel.clone());
        }
    }
    (used, errors)
}

/// One library in `licenses`' list.
#[derive(Debug)]
pub struct LicenceRow {
    pub name: String,
    pub canonical: String,
    pub version: String,
    pub licences: Vec<String>,
    pub non_commercial: bool,
    /// Where the files come from: the source and pin, a folder in the film, an override.
    pub source: String,
    /// Library files the film includes.
    pub files: usize,
}

#[derive(Debug)]
pub struct LicenceReport {
    /// Libraries the film includes something from, in `[libraries]` order.
    pub rows: Vec<LicenceRow>,
    /// Declared in `[libraries]` but never included.
    pub unused: Vec<String>,
    /// Shaders that couldn't be assembled (the list may be incomplete).
    pub errors: Vec<String>,
}

pub fn licence_report(m: &Movie) -> Result<LicenceReport> {
    let lock = Lock::load(&m.root)?;
    let (used, errors) = used_files(m);
    let mut rows = vec![];
    let mut unused = vec![];
    for l in &m.libraries {
        let Some(files) = used.get(&l.name) else {
            unused.push(l.name.clone());
            continue;
        };
        let entry = lock.as_ref().and_then(|k| k.get(&l.name));
        let pinned = |e: &library::LockEntry| {
            format!(
                "{}{} {}",
                e.source,
                e.subdir
                    .as_ref()
                    .map_or(String::new(), |s| format!(" (subdir {s})")),
                e.pin()
            )
        };
        let source = match (&l.origin, entry) {
            (Origin::Locked, Some(e)) => pinned(e),
            (Origin::Bundled(p), Some(e)) => format!("{}, bundled in {p}", pinned(e)),
            (Origin::Path, _) => format!("in the film ({})", rel_to(&l.root, &m.root)),
            (Origin::Env(v), _) => format!("override {v}: {}", l.root.display()),
            (Origin::Local, _) => {
                format!("override in {}: {}", library::LOCAL_FILE, l.root.display())
            }
            (_, None) => l.root.display().to_string(),
        };
        rows.push(LicenceRow {
            name: l.name.clone(),
            canonical: l.canonical.clone(),
            version: l.version.to_string(),
            non_commercial: l.licences.iter().any(|x| library::is_non_commercial(x)),
            licences: l.licences.clone(),
            source,
            files: files.len(),
        });
    }
    Ok(LicenceReport {
        rows,
        unused,
        errors,
    })
}

/// `shotglass licenses <movie>`.
pub fn print_licences(movie: &Path) -> Result<()> {
    let m = crate::load_movie(movie)?;
    let r = licence_report(&m)?;
    for e in &r.errors {
        eprintln!("warning: {e}");
    }
    if !r.errors.is_empty() {
        eprintln!("warning: some shaders couldn't be read, so this list may be incomplete");
    }
    if r.rows.is_empty() {
        println!("{} uses no libraries", m.title);
    } else {
        println!(
            "{} uses {} librar{}:",
            m.title,
            r.rows.len(),
            if r.rows.len() == 1 { "y" } else { "ies" }
        );
    }
    for row in &r.rows {
        println!(
            "\n{}  ({} {})\n    licences: {}{}\n    source:   {}\n    files:    {} included",
            row.name,
            row.canonical,
            row.version,
            if row.licences.is_empty() {
                "none declared".to_string()
            } else {
                row.licences.join(", ")
            },
            if row.non_commercial {
                "  [NON-COMMERCIAL]"
            } else {
                ""
            },
            row.source,
            row.files
        );
    }
    if !r.unused.is_empty() {
        println!(
            "\nnot listed (in [libraries] but nothing is included): {}",
            r.unused.join(", ")
        );
    }
    let nc: Vec<String> = r
        .rows
        .iter()
        .filter(|x| x.non_commercial)
        .map(|x| format!("{} ({})", x.name, x.licences.join(", ")))
        .collect();
    if !nc.is_empty() {
        println!(
            "\nwarning: non-commercial licence: {}; the film can't be used commercially \
             while it includes {}",
            nc.join(", "),
            if nc.len() == 1 { "it" } else { "them" }
        );
    }
    let none: Vec<&str> = r
        .rows
        .iter()
        .filter(|x| x.licences.is_empty())
        .map(|x| x.name.as_str())
        .collect();
    if !none.is_empty() {
        println!(
            "\nwarning: no licence declared: {}; ask the author before you publish the film",
            none.join(", ")
        );
    }
    Ok(())
}

/// What `bundle` wrote.
#[derive(Debug)]
pub struct Bundled {
    /// Files copied from the film folder.
    pub film_files: usize,
    /// Per bundled library: local name and the number of files written.
    pub libraries: Vec<(String, usize)>,
}

/// Write a self-contained copy of the film at `movie` to `out` (new or empty, outside the
/// film): the film folder (minus hidden files and `movie.local.toml`), each locked library's
/// included files in `shotglass-libs/<name>/`, a lock pointing there, and a licence
/// summary. Libraries are taken as locked from `cache`; overrides are ignored.
pub fn bundle(movie: &Path, out: &Path, cache: &Path) -> Result<Bundled> {
    let m = Movie::load_locked(movie, cache)?;
    if let Some(s) = m.library_specs.iter().find(|s| {
        s.path
            .as_deref()
            .is_some_and(|p| p.trim_start_matches("./").starts_with(LIBS_DIR))
    }) {
        bail!(
            "library \"{}\" lives in {LIBS_DIR}/, which bundles use for locked libraries; \
             move it to another folder",
            s.name
        );
    }
    let (used, errors) = used_files(&m);
    if !errors.is_empty() {
        bail!(
            "can't bundle a film with shaders that don't assemble:\n{}",
            errors.join("\n")
        );
    }
    if out.exists() && std::fs::read_dir(out)?.next().is_some() {
        bail!("{} already exists and isn't empty", out.display());
    }
    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let (film_abs, out_abs) = (m.root.canonicalize()?, out.canonicalize()?);
    if out_abs.starts_with(&film_abs) {
        let _ = std::fs::remove_dir(out);
        bail!(
            "--out {} is inside the film folder; write the bundle somewhere else",
            out.display()
        );
    }

    // The film itself.
    let skip = [
        library::LOCAL_FILE,
        library::LOCK_FILE,
        LIBS_DIR,
        NOTICES_FILE,
    ];
    let mut film_files = 0;
    copy_tree(&m.root, out, &skip, &mut film_files)?;

    // Locked libraries: exactly the included files, plus library.toml and licence files.
    let lock = Lock::load(&m.root)?.unwrap_or_default();
    let mut new_lock = Lock::default();
    let mut libraries = vec![];
    let mut notices = String::new();
    notices += &format!(
        "# Library licences\n\nLibraries included by \"{}\", as written by `shotglass bundle`. \
         Each library's licence files are next to its files.\n",
        m.title
    );
    for l in &m.libraries {
        let nc = l.licences.iter().any(|x| library::is_non_commercial(x));
        let licences = if l.licences.is_empty() {
            "none declared".to_string()
        } else {
            l.licences.join(", ")
        };
        let mut section = format!(
            "\n## {} ({} {})\n\n- Licences: {licences}{}\n",
            l.name,
            l.canonical,
            l.version,
            if nc { " (non-commercial)" } else { "" }
        );
        let entry = match &l.origin {
            Origin::Locked | Origin::Bundled(_) => lock.get(&l.name).cloned(),
            _ => None,
        };
        let Some(mut entry) = entry else {
            // A library inside the film folder: copied with the film.
            let rel = rel_to(&l.root, &m.root);
            section += &format!("- In the film: `{rel}/`\n");
            for (name, _) in library::notice_files(&l.root, &l.root) {
                section += &format!("- `{rel}/{name}`\n");
            }
            notices += &section;
            continue;
        };
        let top = match &l.origin {
            Origin::Locked => library::source_root(&l.root, entry.subdir.as_deref()),
            _ => l.root.clone(),
        };
        let rel_dir = format!("{LIBS_DIR}/{}", l.name);
        let dest = out.join(&rel_dir);
        let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
        files.insert(
            library::LIBRARY_FILE.to_string(),
            l.root.join(library::LIBRARY_FILE),
        );
        for f in used.get(&l.name).into_iter().flatten() {
            files.insert(f.clone(), l.root.join(f));
        }
        let mut notice_names = vec![];
        for (name, path) in library::notice_files(&l.root, &top) {
            notice_names.push(name.clone());
            files.entry(name).or_insert(path);
        }
        for (rel, from) in &files {
            let to = dest.join(rel);
            std::fs::create_dir_all(to.parent().expect("inside dest"))?;
            std::fs::copy(from, &to)
                .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
        }
        section += &format!(
            "- Source: {}{} {}\n- Bundled in: `{rel_dir}/`\n",
            entry.source,
            entry
                .subdir
                .as_ref()
                .map_or(String::new(), |s| format!(" (subdir {s})")),
            entry.pin()
        );
        for name in &notice_names {
            section += &format!("- `{rel_dir}/{name}`\n");
        }
        if notice_names.is_empty() {
            section += "- (the library has no licence file)\n";
        }
        notices += &section;
        entry.path = Some(rel_dir);
        entry.sha256 = library::hash_dir(&dest)?;
        new_lock.libraries.push(entry);
        libraries.push((l.name.clone(), files.len()));
    }
    if !new_lock.libraries.is_empty() {
        let p = out.join(library::LOCK_FILE);
        std::fs::write(
            &p,
            format!(
                "# Bundled by `shotglass bundle`: libraries are read from {LIBS_DIR}/.\n{}",
                // Replaces to_text's "written by fetch" header.
                new_lock
                    .to_text()
                    .split_once('\n')
                    .map_or("", |(_, body)| body)
            ),
        )
        .with_context(|| format!("writing {}", p.display()))?;
    }
    if !m.libraries.is_empty() {
        std::fs::write(out.join(NOTICES_FILE), notices)?;
    }

    // The bundle must stand on its own: no cache, no overrides.
    let check = Movie::load_locked(out, &out.join(".no-cache"))
        .context("the bundle doesn't load on its own (a bug in shotglass bundle)")?;
    let (_, errors) = used_files(&check);
    if !errors.is_empty() {
        bail!(
            "the bundle is missing files (a bug in shotglass bundle):\n{}",
            errors.join("\n")
        );
    }
    Ok(Bundled {
        film_files,
        libraries,
    })
}

/// `shotglass bundle <movie> --out <dir>`.
pub fn run_bundle(movie: &Path, out: &Path) -> Result<()> {
    let m = Movie::read(movie)?;
    if m.library_specs.iter().any(|s| s.source.is_some()) {
        // Make sure every locked library is downloaded, overridden ones included.
        crate::fetch_libraries(
            movie,
            library::FetchMode::Missing {
                skip: Default::default(),
            },
            false,
        )?;
    }
    let b = bundle(movie, out, &library::cache_dir())?;
    println!("{}: {} film file(s)", out.display(), b.film_files);
    for (name, n) in &b.libraries {
        println!("  {LIBS_DIR}/{name}: {n} file(s)");
    }
    if !m.library_specs.is_empty() {
        println!("  licences: {}", out.join(NOTICES_FILE).display());
    }
    println!(
        "it loads with no network, cache or overrides: shotglass play {}",
        out.display()
    );
    Ok(())
}

/// Copy `from` into `to` recursively, skipping hidden entries and, at the top level, the
/// names in `skip`.
fn copy_tree(from: &Path, to: &Path, skip: &[&str], count: &mut usize) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || skip.contains(&name.as_str()) {
            continue;
        }
        let ty = e.file_type()?;
        if ty.is_dir() {
            copy_tree(&e.path(), &to.join(&name), &[], count)?;
        } else if ty.is_file() {
            std::fs::copy(e.path(), to.join(&name))
                .with_context(|| format!("copying {}", e.path().display()))?;
            *count += 1;
        }
    }
    Ok(())
}

/// `p` relative to `root`, forward slashes (or `p` itself if it isn't inside).
fn rel_to(p: &Path, root: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{Fetcher, LockEntry};

    const SOURCE: &str = "https://example.com/kit";
    const OK: &str = "#include <kit/noise.glsl>\n#include <nc/grain.glsl>\n\
        void mainImage(out vec4 c, in vec2 p) {\n  \
        c = vec4(kit_noise(p) * nc_grain(), 0.0, 0.0, 1.0);\n}\n";

    fn tmp(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("shotglass-bundle-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &Path, files: &[(&str, &str)]) {
        for (n, t) in files {
            let p = dir.join(n);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, t).unwrap();
        }
    }

    /// A film with a locked library (`kit`, from a fake cache, in a subdir with the licence
    /// files at the repository root), a non-commercial one inside the film (`nc`) and one
    /// it declares but never includes (`spare`). Returns (film, cache).
    fn fixture(name: &str) -> (PathBuf, PathBuf) {
        let base = tmp(name);
        let (film, cache) = (base.join("film"), base.join("cache"));
        let repo = library::cache_path(&cache, SOURCE, "0.4.1");
        write(
            &repo,
            &[
                ("LICENSE-MIT", "MIT licence text"),
                ("NOTICE", "kit notice"),
                ("README.md", "not a licence"),
                (
                    "core/library.toml",
                    "name = \"kit/core\"\nversion = \"0.4.1\"\nlicences = [\"MIT\"]\n",
                ),
                (
                    "core/noise.glsl",
                    "#include \"hash.glsl\"\nfloat kit_noise(vec2 p) { return kit_hash(p); }\n",
                ),
                (
                    "core/hash.glsl",
                    "float kit_hash(vec2 p) { return fract(sin(dot(p, vec2(12.9898, 78.233))) * 43758.5453); }\n",
                ),
                ("core/unused.glsl", "float kit_unused() { return 1.0; }\n"),
                (
                    "other/library.toml",
                    "name = \"kit/other\"\nversion = \"0.4.1\"\n",
                ),
            ],
        );
        let core = repo.join("core");
        write(
            &film,
            &[
                (
                    "movie.toml",
                    "[libraries]\n\
                     kit = { version = \"0.4\", source = \"https://example.com/kit\", subdir = \"core\" }\n\
                     nc = { path = \"lib/nc\" }\n\
                     spare = { path = \"lib/spare\" }\n\
                     [[shot]]\nid = \"a\"\nscene = \"scenes/a.glsl\"\nduration = 1\n",
                ),
                ("scenes/a.glsl", OK),
                ("notes.txt", "the film's own file"),
                (".hidden", "skipped"),
                (
                    library::LOCAL_FILE,
                    "[libraries.kit]\npath = \"does/not/exist\"\n",
                ),
                (
                    "lib/nc/library.toml",
                    "name = \"someone/grain\"\nversion = \"1.0.0\"\nlicence = \"CC-BY-NC-SA-4.0\"\n",
                ),
                ("lib/nc/grain.glsl", "float nc_grain() { return 0.5; }\n"),
                ("lib/nc/LICENSE", "CC BY-NC-SA"),
                (
                    "lib/spare/library.toml",
                    "name = \"me/spare\"\nversion = \"0.1.0\"\nlicences = [\"CC0-1.0\"]\n",
                ),
            ],
        );
        let lock = Lock {
            version: 1,
            libraries: vec![LockEntry {
                name: "kit".into(),
                version: "0.4.1".into(),
                source: SOURCE.into(),
                tag: "v0.4.1".into(),
                rev: None,
                subdir: Some("core".into()),
                path: None,
                sha256: library::hash_dir(&core).unwrap(),
            }],
        };
        std::fs::write(film.join(library::LOCK_FILE), lock.to_text()).unwrap();
        (film, cache)
    }

    /// Fails every network call.
    struct Offline;
    impl Fetcher for Offline {
        fn tags(&self, _: &str) -> Result<Vec<String>> {
            bail!("offline")
        }
        fn download(&self, _: &str, _: &str, _: &Path) -> Result<()> {
            bail!("offline")
        }
        fn head(&self, _: &str, _: &str) -> Result<String> {
            bail!("offline")
        }
        fn download_rev(&self, _: &str, _: &str, _: &Path) -> Result<()> {
            bail!("offline")
        }
    }

    #[test]
    fn non_commercial_licences_are_recognised() {
        for nc in [
            "CC-BY-NC-4.0",
            "CC-BY-NC-SA-3.0",
            "cc-by-nc-nd-4.0",
            "MIT OR CC-BY-NC-4.0",
            "(CC-BY-NC-SA-4.0)",
        ] {
            assert!(library::is_non_commercial(nc), "{nc}");
        }
        for ok in [
            "MIT",
            "Apache-2.0",
            "CC-BY-4.0",
            "CC-BY-SA-4.0",
            "CC0-1.0",
            "NCSA",
        ] {
            assert!(!library::is_non_commercial(ok), "{ok}");
        }
    }

    #[test]
    fn licenses_lists_what_the_film_includes() {
        let (film, cache) = fixture("licenses");
        let m = Movie::load_locked(&film, &cache).unwrap();
        let r = licence_report(&m).unwrap();
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let names: Vec<&str> = r.rows.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["kit", "nc"]);
        assert_eq!(r.unused, ["spare"]);
        let kit = &r.rows[0];
        assert_eq!(
            (kit.canonical.as_str(), kit.version.as_str()),
            ("kit/core", "0.4.1")
        );
        assert_eq!(kit.licences, ["MIT"]);
        assert!(!kit.non_commercial);
        assert_eq!(kit.files, 2, "noise.glsl and the hash.glsl it includes");
        assert!(
            kit.source.contains(SOURCE) && kit.source.contains("0.4.1"),
            "{}",
            kit.source
        );
        let nc = &r.rows[1];
        assert_eq!(nc.licences, ["CC-BY-NC-SA-4.0"]);
        assert!(nc.non_commercial);
        assert_eq!(nc.source, "in the film (lib/nc)");
        std::fs::remove_dir_all(film.parent().unwrap()).ok();
    }

    #[test]
    fn a_bundle_loads_and_checks_with_no_cache_network_or_overrides() {
        let (film, cache) = fixture("bundle");
        let out = film.parent().unwrap().join("site");
        let b = bundle(&film, &out, &cache).unwrap();
        assert_eq!(b.libraries.len(), 1);

        let libs = out.join(LIBS_DIR).join("kit");
        for f in [
            "library.toml",
            "noise.glsl",
            "hash.glsl",
            "LICENSE-MIT",
            "NOTICE",
        ] {
            assert!(libs.join(f).is_file(), "{f} is bundled");
        }
        for f in ["unused.glsl", "README.md"] {
            assert!(!libs.join(f).exists(), "{f} isn't bundled");
        }
        assert!(!out.join(LIBS_DIR).join("other").exists());
        assert!(out.join("notes.txt").is_file());
        assert!(out.join("lib/nc/grain.glsl").is_file());
        assert!(!out.join(".hidden").exists());
        assert!(
            !out.join(library::LOCAL_FILE).exists(),
            "no overrides in a bundle"
        );

        let lock = Lock::load(&out).unwrap().unwrap();
        let e = lock.get("kit").unwrap();
        assert_eq!(e.path.as_deref(), Some("shotglass-libs/kit"));
        assert_eq!(
            (e.source.as_str(), e.version.as_str()),
            (SOURCE, "0.4.1"),
            "provenance kept"
        );
        assert_eq!(e.sha256, library::hash_dir(&libs).unwrap());

        let notices = std::fs::read_to_string(out.join(NOTICES_FILE)).unwrap();
        for want in [
            "kit/core 0.4.1",
            "shotglass-libs/kit/LICENSE-MIT",
            "CC-BY-NC-SA-4.0 (non-commercial)",
            "lib/nc/LICENSE",
        ] {
            assert!(notices.contains(want), "{want} in:\n{notices}");
        }

        // Loading never touches the network or the cache, and the auto-fetch before
        // commands has nothing to download.
        let none = out.join("no-such-cache");
        let specs = Movie::read(&out).unwrap().library_specs;
        let (again, log) = library::fetch(
            &specs,
            Some(&lock),
            &Offline,
            &none,
            &library::FetchMode::Missing {
                skip: Default::default(),
            },
        )
        .unwrap();
        assert_eq!(again, lock, "{log:?}");
        assert!(!none.exists());
        let m = Movie::load_locked(&out, &none).unwrap();
        assert!(matches!(m.libraries[0].origin, Origin::Bundled(_)));
        assert_eq!(m.libraries[0].root, libs);
        // The normal load path (overrides looked up, none set) finds the bundled copy too.
        let mut m2 = Movie::read(&out).unwrap();
        m2.resolve_libraries(&|_| None, &none).unwrap();
        // The strict check `check --no-gpu` runs.
        if validate::AVAILABLE {
            for d in [Dialect::BASELINE, Dialect::WEB] {
                let errors = validate::validate_movie(&m, d);
                assert!(errors.is_empty(), "{d}: {errors:?}");
            }
        }

        // An edited bundled file is caught like an edited cache.
        std::fs::write(
            libs.join("noise.glsl"),
            "float kit_noise(vec2 p) { return 0.0; }\n",
        )
        .unwrap();
        let e = format!("{:#}", Movie::load_locked(&out, &none).unwrap_err());
        assert!(e.contains("bundled copy was edited"), "{e}");

        // Refusals: a non-empty --out, an --out inside the film.
        let e = bundle(&film, &out, &cache).unwrap_err().to_string();
        assert!(e.contains("isn't empty"), "{e}");
        let e = bundle(&film, &film.join("site"), &cache)
            .unwrap_err()
            .to_string();
        assert!(e.contains("inside the film folder"), "{e}");
        assert!(!film.join("site").exists());
        std::fs::remove_dir_all(film.parent().unwrap()).ok();
    }

    #[test]
    fn a_bundle_can_be_bundled_again() {
        let (film, cache) = fixture("rebundle");
        let base = film.parent().unwrap().to_path_buf();
        bundle(&film, &base.join("one"), &cache).unwrap();
        bundle(&base.join("one"), &base.join("two"), &base.join("none")).unwrap();
        let a = Lock::load(&base.join("one")).unwrap().unwrap();
        let b = Lock::load(&base.join("two")).unwrap().unwrap();
        assert_eq!(a, b);
        std::fs::remove_dir_all(&base).ok();
    }
}
