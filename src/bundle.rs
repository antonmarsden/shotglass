//! `shotglass licenses`, `freeze`, `unfreeze` and `bundle` (issues #3, #59).
//!
//! - `licenses` lists every library a film actually includes, with the SPDX licences from
//!   its `library.toml`, and flags non-commercial ones.
//! - `freeze` makes a film a project of its own that never changes: exactly the locked
//!   library files it includes (and each library's `library.toml` and licence files) go in
//!   `<film>/shotglass-libs/<name>/`, with a `shotglass.toml` marked frozen and a
//!   `shotglass.lock` pointing at them (`path`). It then loads with no project, cache,
//!   network or overrides. `unfreeze` removes those, and the film follows its project again.
//! - `bundle` copies the film folder somewhere else and writes the same frozen files there.

use crate::library::{self, Lock, LockEntry, Origin};
use crate::movie::Movie;
use crate::project::{self, Project};
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
    for f in m.channels().flat_map(crate::movie::Channel::files) {
        if let Some(lib) = &f.library {
            used.entry(lib.clone()).or_default().insert(f.rel.clone());
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
    let lock = Lock::load(&m.project)?;
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
            if e.source.is_empty() {
                return "a frozen copy of a project folder".to_string();
            }
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
            (Origin::Bundled(p), Some(e)) => format!("{}, frozen in {p}", pinned(e)),
            (Origin::Path, _) if l.root.starts_with(&m.root) => {
                format!("in the film ({})", rel_to(&l.root, &m.root))
            }
            (Origin::Path, _) => format!("in the project ({})", rel_to(&l.root, &m.project)),
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

/// What `freeze` or `bundle` wrote.
#[derive(Debug)]
pub struct Bundled {
    /// Files copied from the film folder (0 for `freeze`).
    pub film_files: usize,
    /// Per frozen library: local name and the number of files written.
    pub libraries: Vec<(String, usize)>,
}

/// Write the frozen form of `m`'s libraries into `dest` (the film folder for `freeze`, the
/// bundle for `bundle`): `shotglass-libs/<name>/` with the files the film includes from
/// each library outside the film folder, a `shotglass.toml` marked frozen that declares
/// only the libraries the film includes, and a `shotglass.lock` pointing at the copies.
/// `m` must be loaded with `load_locked` (no overrides). Returns the libraries written and
/// the licence summary.
fn write_frozen(
    m: &Movie,
    used: &BTreeMap<String, BTreeSet<String>>,
    dest: &Path,
    what: &str,
) -> Result<(Vec<(String, usize)>, String)> {
    let lock = Lock::load(&m.project)?.unwrap_or_default();
    let mut new_lock = Lock::default();
    let mut decls = toml::Table::new();
    let mut libraries = vec![];
    let mut notices = format!(
        "# Library licences\n\nLibraries included by \"{}\", as written by `shotglass {what}`. \
         Each library's licence files are next to its files.\n",
        m.title
    );
    // What the film includes, plus the peers those libraries require (transitively):
    // loading checks peer requirements, so a frozen film must still declare them.
    let mut keep: BTreeSet<&str> = used.keys().map(String::as_str).collect();
    loop {
        let before = keep.len();
        for l in &m.libraries {
            if keep.contains(l.name.as_str()) {
                for (canonical, _) in &l.requires {
                    if let Some(p) = m.libraries.iter().find(|x| &x.canonical == canonical) {
                        keep.insert(p.name.as_str());
                    }
                }
            }
        }
        if keep.len() == before {
            break;
        }
    }
    for l in &m.libraries {
        if !keep.contains(l.name.as_str()) {
            continue; // declared by the project, but this film doesn't need it
        }
        let spec = m
            .library_specs
            .iter()
            .find(|s| s.name == l.name)
            .expect("every library has a spec");
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
        let mut decl = toml::Table::new();
        if matches!(l.origin, Origin::Path) && l.root.starts_with(&m.root) {
            // A library inside the film folder travels with the film.
            let rel = rel_to(&l.root, &m.root);
            decl.insert("path".into(), rel.clone().into());
            if let Some(v) = &spec.version {
                decl.insert("version".into(), v.to_string().into());
            }
            decls.insert(l.name.clone(), decl.into());
            section += &format!("- In the film: `{rel}/`\n");
            for (name, _) in library::notice_files(&l.root, &l.root) {
                section += &format!("- `{rel}/{name}`\n");
            }
            notices += &section;
            continue;
        }
        let rel_dir = format!("{LIBS_DIR}/{}", l.name);
        let mut entry = match (&l.origin, lock.get(&l.name)) {
            (Origin::Locked | Origin::Bundled(_), Some(e)) => e.clone(),
            (Origin::Path, _) => LockEntry {
                name: l.name.clone(),
                version: l.version.to_string(),
                source: String::new(),
                tag: String::new(),
                rev: None,
                subdir: None,
                path: None,
                sha256: String::new(),
            },
            _ => bail!(
                "library \"{}\" isn't in {} (a bug: load_locked resolves every library)",
                l.name,
                library::LOCK_FILE
            ),
        };
        match &spec.source {
            Some(src) => {
                decl.insert("source".into(), src.clone().into());
                if let Some(v) = &spec.version {
                    decl.insert("version".into(), v.to_string().into());
                }
                if let Some(b) = &spec.branch {
                    decl.insert("branch".into(), b.clone().into());
                }
                if let Some(sd) = &spec.subdir {
                    decl.insert("subdir".into(), sd.clone().into());
                }
            }
            None => {
                decl.insert("path".into(), rel_dir.clone().into());
            }
        }
        decls.insert(l.name.clone(), decl.into());
        let top = match &l.origin {
            Origin::Locked => library::source_root(&l.root, entry.subdir.as_deref()),
            _ => l.root.clone(),
        };
        let to_dir = dest.join(&rel_dir);
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
            let to = to_dir.join(rel);
            std::fs::create_dir_all(to.parent().expect("inside dest"))?;
            std::fs::copy(from, &to)
                .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
        }
        if entry.source.is_empty() {
            section += &format!(
                "- Source: the project folder `{}`\n",
                rel_to(&l.root, &m.project)
            );
        } else {
            section += &format!(
                "- Source: {}{} {}\n",
                entry.source,
                entry
                    .subdir
                    .as_ref()
                    .map_or(String::new(), |s| format!(" (subdir {s})")),
                entry.pin()
            );
        }
        section += &format!("- Frozen in: `{rel_dir}/`\n");
        for name in &notice_names {
            section += &format!("- `{rel_dir}/{name}`\n");
        }
        if notice_names.is_empty() {
            section += "- (the library has no licence file)\n";
        }
        notices += &section;
        entry.path = Some(rel_dir);
        entry.sha256 = library::hash_dir(&to_dir)?;
        new_lock.libraries.push(entry);
        libraries.push((l.name.clone(), files.len()));
    }

    let mut proj = toml::Table::new();
    if let Some(v) = &m.shotglass {
        proj.insert("shotglass".into(), v.clone().into());
    }
    proj.insert("libraries".into(), decls.into());
    let header = format!(
        "{}: this film reads its libraries from {LIBS_DIR}/ (exactly the files it\n\
         # includes), never from a project, cache or override, so it never changes.\n",
        project::FROZEN_HEADER
    );
    let unfreeze = if what == "freeze" {
        "# `shotglass unfreeze <film>` removes this file, shotglass.lock and shotglass-libs/,\n\
         # and the film follows its project again.\n"
    } else {
        ""
    };
    let p = dest.join(project::PROJECT_FILE);
    std::fs::write(
        &p,
        format!(
            "{header}{unfreeze}{}",
            toml::to_string(&proj).expect("project file serialises")
        ),
    )
    .with_context(|| format!("writing {}", p.display()))?;
    if !new_lock.libraries.is_empty() {
        let p = dest.join(library::LOCK_FILE);
        std::fs::write(
            &p,
            format!(
                "# Written by `shotglass {what}`: libraries are read from {LIBS_DIR}/.\n{}",
                // Replaces to_text's "written by fetch" header.
                new_lock
                    .to_text()
                    .split_once('\n')
                    .map_or("", |(_, body)| body)
            ),
        )
        .with_context(|| format!("writing {}", p.display()))?;
    }
    Ok((libraries, notices))
}

/// A film that `write_frozen` wrote must load on its own: no project, cache or overrides.
fn check_standalone(dir: &Path, what: &str) -> Result<()> {
    let check = Movie::load_locked(dir, &dir.join(".no-cache")).with_context(|| {
        format!("the {what} doesn't load on its own (a bug in shotglass {what})")
    })?;
    if !project::same_dir(&check.project, dir) {
        bail!("the {what} still belongs to a project (a bug in shotglass {what})");
    }
    let (_, errors) = used_files(&check);
    if !errors.is_empty() {
        bail!(
            "the {what} is missing files (a bug in shotglass {what}):\n{}",
            errors.join("\n")
        );
    }
    Ok(())
}

/// Load a film for `freeze`/`bundle`: libraries exactly as locked, every shader assembling.
fn load_for_freezing(
    movie: &Path,
    cache: &Path,
) -> Result<(Movie, BTreeMap<String, BTreeSet<String>>)> {
    let m = Movie::load_locked(movie, cache)?;
    if let Some(s) = m.library_specs.iter().find(|s| {
        s.path
            .as_deref()
            .is_some_and(|p| p.trim_start_matches("./").starts_with(LIBS_DIR))
    }) && !Project::for_film(&m.root)?.frozen
    {
        bail!(
            "library \"{}\" lives in {LIBS_DIR}/, which frozen films use for their libraries; \
             move it to another folder",
            s.name
        );
    }
    let (used, errors) = used_files(&m);
    if !errors.is_empty() {
        bail!(
            "can't freeze or bundle a film with shaders that don't assemble:\n{}",
            errors.join("\n")
        );
    }
    Ok((m, used))
}

/// Freeze the film at `movie` in place (see the module docs). Libraries are taken as
/// locked from `cache`; overrides are ignored.
pub fn freeze(movie: &Path, cache: &Path) -> Result<Bundled> {
    let p = Project::for_film(movie)?;
    if p.frozen {
        bail!("{} is already frozen", movie.display());
    }
    if p.is_film_root(movie) {
        bail!(
            "{} is a project of its own ({} is in the film folder), so its {} already pins \
             its libraries and nothing outside the film can change it. freeze is for a film \
             in a project of several films; to host this one, use `shotglass bundle`",
            movie.display(),
            project::PROJECT_FILE,
            library::LOCK_FILE
        );
    }
    let (m, used) = load_for_freezing(movie, cache)?;
    if m.root.join(LIBS_DIR).exists() {
        bail!(
            "{} already has a {LIBS_DIR}/ folder; move it away first",
            m.root.display()
        );
    }
    let (libraries, _) = write_frozen(&m, &used, &m.root, "freeze")?;
    if let Err(e) = check_standalone(&m.root, "freeze") {
        let _ = remove_frozen(&m.root);
        return Err(e);
    }
    Ok(Bundled {
        film_files: 0,
        libraries,
    })
}

/// Remove what `write_frozen` wrote into a film folder.
fn remove_frozen(film: &Path) -> Result<()> {
    for f in [project::PROJECT_FILE, library::LOCK_FILE, NOTICES_FILE] {
        let p = film.join(f);
        if p.exists() {
            std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
        }
    }
    let libs = film.join(LIBS_DIR);
    if libs.exists() {
        std::fs::remove_dir_all(&libs).with_context(|| format!("removing {}", libs.display()))?;
    }
    Ok(())
}

/// Undo `freeze`: the film follows the project above it again. Refuses a film that isn't
/// frozen, a bundle (no project above it), and a film whose movie.toml the project can't
/// load (it names a library the project no longer declares, say), before removing anything.
pub fn unfreeze(movie: &Path) -> Result<()> {
    let p = Project::for_film(movie)?;
    if !p.frozen || !p.is_film_root(movie) {
        bail!("{} isn't frozen", movie.display());
    }
    let parent = std::path::absolute(movie)?
        .parent()
        .and_then(project::find_root)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{} isn't inside a project (no {} above it), so there is nothing to go back \
                 to; a bundle stays frozen",
                movie.display(),
                project::PROJECT_FILE
            )
        })?;
    let outer = Project::read(&parent)?;
    let text = std::fs::read_to_string(movie.join(project::MOVIE_FILE))?;
    Movie::parse_in(&text, movie, &outer).with_context(|| {
        format!(
            "{} can't follow the project at {} (nothing was changed)",
            movie.display(),
            parent.display()
        )
    })?;
    remove_frozen(movie)
}

/// Write a self-contained copy of the film at `movie` to `out` (new or empty, outside the
/// film): the film folder (minus hidden files, project and lock files and frozen
/// libraries), then the frozen libraries and a licence summary. Libraries are taken as
/// locked from `cache`; overrides are ignored.
pub fn bundle(movie: &Path, out: &Path, cache: &Path) -> Result<Bundled> {
    let (m, used) = load_for_freezing(movie, cache)?;
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
    let skip = [
        project::PROJECT_FILE,
        library::LOCAL_FILE,
        library::LOCK_FILE,
        LIBS_DIR,
        NOTICES_FILE,
    ];
    let mut film_files = 0;
    copy_tree(&m.root, out, &skip, &mut film_files)?;
    let (libraries, notices) = write_frozen(&m, &used, out, "bundle")?;
    if !libraries.is_empty() || m.libraries.iter().any(|l| used.contains_key(&l.name)) {
        std::fs::write(out.join(NOTICES_FILE), notices)?;
    }
    check_standalone(out, "bundle")?;
    Ok(Bundled {
        film_files,
        libraries,
    })
}

/// Download every locked library of the film's project (overridden ones included), so
/// `freeze`/`bundle` can copy the locked files.
fn fetch_all(movie: &Path) -> Result<Movie> {
    let m = Movie::read(movie)?;
    if m.library_specs.iter().any(|s| s.source.is_some()) {
        crate::fetch_libraries(
            &m.project,
            library::FetchMode::Missing {
                skip: Default::default(),
            },
            false,
        )?;
    }
    Ok(m)
}

/// `shotglass bundle <movie> --out <dir>`.
pub fn run_bundle(movie: &Path, out: &Path) -> Result<()> {
    let m = fetch_all(movie)?;
    let b = bundle(movie, out, &library::cache_dir())?;
    println!("{}: {} film file(s)", out.display(), b.film_files);
    for (name, n) in &b.libraries {
        println!("  {LIBS_DIR}/{name}: {n} file(s)");
    }
    if out.join(NOTICES_FILE).exists() {
        println!("  licences: {}", out.join(NOTICES_FILE).display());
    }
    let _ = m;
    println!(
        "it loads with no project, network, cache or overrides: shotglass play {}",
        out.display()
    );
    Ok(())
}

/// `shotglass freeze <movie>`.
pub fn run_freeze(movie: &Path) -> Result<()> {
    fetch_all(movie)?;
    let b = freeze(movie, &library::cache_dir())?;
    println!("froze {}:", movie.display());
    for (name, n) in &b.libraries {
        println!("  {LIBS_DIR}/{name}: {n} file(s)");
    }
    println!(
        "  {} (marked frozen) and {}: commit them with the film.\n\
         It no longer follows the project's libraries; `shotglass unfreeze {}` undoes this.",
        project::PROJECT_FILE,
        library::LOCK_FILE,
        movie.display()
    );
    Ok(())
}

/// `shotglass unfreeze <movie>`.
pub fn run_unfreeze(movie: &Path) -> Result<()> {
    unfreeze(movie)?;
    println!(
        "unfroze {}: it follows its project's libraries again (check it: shotglass check {})",
        movie.display(),
        movie.display()
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
                    project::PROJECT_FILE,
                    "[libraries]\n\
                     kit = { version = \"0.4\", source = \"https://example.com/kit\", subdir = \"core\" }\n\
                     nc = { path = \"lib/nc\" }\n\
                     spare = { path = \"lib/spare\" }\n",
                ),
                (
                    project::MOVIE_FILE,
                    "[[shot]]\nid = \"a\"\nscene = \"scenes/a.glsl\"\nduration = 1\n",
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

        let proj = Project::read(&out).unwrap();
        assert!(proj.frozen);
        let names: Vec<&str> = proj.specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["kit", "nc"], "only what the film includes");
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
        assert!(e.contains("frozen copy was edited"), "{e}");

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

    /// A project of two films sharing `lib/` (`films`) and the cached kit. Returns
    /// (project root, cache).
    fn project_fixture(name: &str) -> (PathBuf, PathBuf) {
        let (one, cache) = fixture(name);
        let root = one.parent().unwrap().join("films");
        let scene = "#include <kit/noise.glsl>\n#include <films/ring.glsl>\n\
            void mainImage(out vec4 c, in vec2 p) {\n  \
            c = vec4(kit_noise(p) * films_ring(), 0.0, 0.0, 1.0);\n}\n";
        let shot = "[[shot]]\nid = \"a\"\nscene = \"scenes/a.glsl\"\nduration = 1\n";
        write(
            &root,
            &[
                (
                    project::PROJECT_FILE,
                    "shotglass = \"0.1\"\n[libraries]\n\
                     kit = { version = \"0.4\", source = \"https://example.com/kit\", subdir = \"core\" }\n\
                     films = { path = \"lib\" }\n\
                     peer = { path = \"peer\" }\n",
                ),
                (
                    "lib/library.toml",
                    "name = \"films/lib\"\nversion = \"0.1.0\"\nlicences = [\"CC0-1.0\"]\n\
                     [requires]\n\"films/peer\" = \"0.1\"\n",
                ),
                (
                    "peer/library.toml",
                    "name = \"films/peer\"\nversion = \"0.1.0\"\n",
                ),
                ("peer/unused.glsl", "float peer_unused() { return 0.0; }\n"),
                ("lib/ring.glsl", "float films_ring() { return 0.5; }\n"),
                ("lib/spare.glsl", "float films_spare() { return 0.5; }\n"),
                ("a/movie.toml", shot),
                ("a/scenes/a.glsl", scene),
                ("b/movie.toml", shot),
                ("b/scenes/a.glsl", scene),
            ],
        );
        std::fs::copy(one.join(library::LOCK_FILE), root.join(library::LOCK_FILE)).unwrap();
        (root, cache)
    }

    #[test]
    fn a_frozen_film_never_changes_and_unfreezes() {
        let (root, cache) = project_fixture("freeze");
        let a = root.join("a");
        let m = Movie::load_locked(&a, &cache).unwrap();
        assert!(project::same_dir(&m.project, &root));
        assert_eq!(m.libraries.len(), 3);

        // A one-film project has nothing to freeze; a frozen film can't freeze again.
        let e = format!(
            "{:#}",
            freeze(&root.parent().unwrap().join("film"), &cache).unwrap_err()
        );
        assert!(
            e.contains("a project of its own") && e.contains("bundle"),
            "{e}"
        );
        let b = freeze(&a, &cache).unwrap();
        // films/lib requires films/peer: frozen too (library.toml only), though nothing
        // from it is included, or the frozen film would fail the peer check.
        assert_eq!(b.libraries.len(), 3);
        assert!(a.join(LIBS_DIR).join("peer/library.toml").is_file());
        assert!(!a.join(LIBS_DIR).join("peer/unused.glsl").exists());
        let e = format!("{:#}", freeze(&a, &cache).unwrap_err());
        assert!(e.contains("already frozen"), "{e}");

        let fz = Project::read(&a).unwrap();
        assert!(fz.frozen);
        assert_eq!(fz.shotglass.as_deref(), Some("0.1"));
        let libs = a.join(LIBS_DIR);
        for f in [
            "kit/noise.glsl",
            "kit/hash.glsl",
            "kit/LICENSE-MIT",
            "films/ring.glsl",
        ] {
            assert!(libs.join(f).is_file(), "{f} is frozen");
        }
        assert!(!libs.join("films/spare.glsl").exists());
        let lock = Lock::load(&a).unwrap().unwrap();
        let films = lock.get("films").unwrap();
        assert_eq!(
            (films.source.as_str(), films.path.as_deref()),
            ("", Some("shotglass-libs/films"))
        );
        assert_eq!(lock.get("kit").unwrap().source, SOURCE, "provenance kept");

        // Changing the project's lib/ changes film b, never film a, whatever overrides say.
        std::fs::write(
            root.join("lib/ring.glsl"),
            "float films_ring() { return 0.25; }\n",
        )
        .unwrap();
        let elsewhere = root.join("lib").display().to_string();
        let env = |k: &str| k.starts_with("SHOTGLASS_LIB_").then(|| elsewhere.clone());
        let none = root.join("no-cache");
        let mut fa = Movie::read(&a).unwrap();
        fa.resolve_libraries(&env, &none).unwrap();
        assert!(project::same_dir(&fa.project, &a));
        for l in &fa.libraries {
            assert!(matches!(l.origin, Origin::Bundled(_)), "{l:?}");
        }
        let ring = |m: &Movie| {
            let l = m.libraries.iter().find(|l| l.name == "films").unwrap();
            std::fs::read_to_string(l.root.join("ring.glsl")).unwrap()
        };
        assert!(ring(&fa).contains("0.5"));
        let fb = Movie::load_locked(&root.join("b"), &cache).unwrap();
        assert!(ring(&fb).contains("0.25"));
        let r = licence_report(&fa).unwrap();
        assert!(
            r.rows.iter().all(|x| x.source.contains("frozen in")),
            "{:?}",
            r.rows
        );

        // A bundle of a film in a project stands alone too, and can't be unfrozen.
        let out = root.parent().unwrap().join("site-b");
        bundle(&root.join("b"), &out, &cache).unwrap();
        assert!(Project::read(&out).unwrap().frozen);
        let e = format!("{:#}", unfreeze(&out).unwrap_err());
        assert!(e.contains("isn't inside a project"), "{e}");

        // Unfreeze: back to the project's lib/.
        let e = format!("{:#}", unfreeze(&root.join("b")).unwrap_err());
        assert!(e.contains("isn't frozen"), "{e}");
        unfreeze(&a).unwrap();
        for f in [project::PROJECT_FILE, library::LOCK_FILE, LIBS_DIR] {
            assert!(!a.join(f).exists(), "{f} removed");
        }
        let fa = Movie::load_locked(&a, &cache).unwrap();
        assert!(project::same_dir(&fa.project, &root));
        assert!(ring(&fa).contains("0.25"));
        std::fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    #[test]
    fn unfreeze_refuses_a_film_the_project_cant_load() {
        let (root, cache) = project_fixture("unfreeze-refused");
        let a = root.join("a");
        freeze(&a, &cache).unwrap();
        // The project drops the library the film includes... a transition from it, say.
        std::fs::write(
            a.join("movie.toml"),
            "[[shot]]\nid = \"a\"\nscene = \"scenes/a.glsl\"\nduration = 1\n\
             [[shot]]\nid = \"b\"\nscene = \"scenes/a.glsl\"\nduration = 1\n\
             transition = { type = \"films/ring\", duration = 0.5 }\n",
        )
        .unwrap();
        std::fs::write(root.join(project::PROJECT_FILE), "[libraries]\n").unwrap();
        let e = format!("{:#}", unfreeze(&a).unwrap_err());
        assert!(e.contains("nothing was changed"), "{e}");
        assert!(a.join(LIBS_DIR).is_dir() && a.join(project::PROJECT_FILE).is_file());
        std::fs::remove_dir_all(root.parent().unwrap()).ok();
    }
}
