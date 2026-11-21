//! Projects (#59): the folder that owns a set of films' libraries.
//!
//! - `shotglass.toml` at a project's root declares the engine contract (`shotglass = "…"`)
//!   and `[libraries]` once for every film under it. `movie.toml` has neither.
//! - A film's project is the nearest folder at or above the film that has a
//!   `shotglass.toml`. A film with none above it is a project of its own with no libraries.
//! - Everything library-related is per project: `path` libraries are relative to the
//!   project root and may not climb out of it, and there is one `shotglass.lock` and one
//!   `shotglass.local.toml` (overrides) per project, next to `shotglass.toml`.
//! - Freezing a film (`shotglass freeze`, see bundle.rs) makes it a project of its own:
//!   it gets a `shotglass.toml` marked frozen, a lock pointing into `shotglass-libs/`, and
//!   the library files it includes.

use crate::library::{self, LibraryDecl, LibrarySpec};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const PROJECT_FILE: &str = "shotglass.toml";
pub const MOVIE_FILE: &str = "movie.toml";
/// First line of the `shotglass.toml` that `freeze` and `bundle` write.
pub const FROZEN_HEADER: &str = "# Frozen by shotglass";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectFile {
    /// Engine contract the project's films need, e.g. "0.1".
    shotglass: Option<String>,
    #[serde(default)]
    libraries: BTreeMap<String, LibraryDecl>,
}

/// A project: a root folder, the engine contract and the declared libraries.
#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    pub root: PathBuf,
    /// Has a `shotglass.toml` (false: a lone film with nothing above it).
    pub has_file: bool,
    /// Written by `freeze` or `bundle`: libraries come only from `shotglass-libs/`.
    pub frozen: bool,
    /// `shotglass = "…"`: the engine contract range the films were made for.
    pub shotglass: Option<String>,
    /// `[libraries]`, validated but not looked up on disk.
    pub specs: Vec<LibrarySpec>,
}

impl Project {
    /// A film folder with no `shotglass.toml` at or above it.
    pub fn bare(root: &Path) -> Project {
        Project {
            root: root.to_path_buf(),
            has_file: false,
            frozen: false,
            shotglass: None,
            specs: vec![],
        }
    }

    /// Parse `shotglass.toml`'s text. The engine contract is checked first: a newer
    /// project may use fields this engine doesn't know, and "upgrade" is the useful message.
    pub fn parse(text: &str, root: &Path) -> Result<Project> {
        match toml::from_str::<toml::Table>(text)
            .ok()
            .and_then(|t| t.get("shotglass").cloned())
        {
            Some(toml::Value::String(v)) => library::check_engine(&v)?,
            Some(_) => bail!("shotglass must be a version string, e.g. shotglass = \"0.1\""),
            None => {}
        }
        let f: ProjectFile = toml::from_str(text)?;
        let mut specs = vec![];
        for (name, d) in &f.libraries {
            specs.push(LibrarySpec::parse(name, d)?);
        }
        Ok(Project {
            root: root.to_path_buf(),
            has_file: true,
            frozen: text.starts_with(FROZEN_HEADER),
            shotglass: f.shotglass,
            specs,
        })
    }

    /// Read `<root>/shotglass.toml`.
    pub fn read(root: &Path) -> Result<Project> {
        let file = root.join(PROJECT_FILE);
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        Project::parse(&text, root).with_context(|| format!("in {}", file.display()))
    }

    /// The project a film folder belongs to.
    pub fn for_film(film: &Path) -> Result<Project> {
        match find_root(film) {
            Some(root) => Project::read(&root),
            None => Ok(Project::bare(film)),
        }
    }

    /// Is `film` this project's root (a one-film project)?
    pub fn is_film_root(&self, film: &Path) -> bool {
        same_dir(&self.root, film)
    }

    /// The films of the project: the root itself if it has a `movie.toml`, then every
    /// folder directly under the root that has one (frozen films included), by name.
    pub fn films(&self) -> Result<Vec<PathBuf>> {
        films_in(&self.root)
    }
}

/// The films directly in `root` (see `Project::films`).
pub fn films_in(root: &Path) -> Result<Vec<PathBuf>> {
    let mut v = vec![];
    if root.join(MOVIE_FILE).is_file() {
        v.push(root.to_path_buf());
    }
    let mut subs = vec![];
    for e in std::fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with('.') && e.path().join(MOVIE_FILE).is_file() {
            subs.push(root.join(name));
        }
    }
    subs.sort();
    v.extend(subs);
    Ok(v)
}

/// The nearest folder at or above `film` with a `shotglass.toml`. Relative paths stay
/// relative (to the working directory) when the project is below it.
pub fn find_root(film: &Path) -> Option<PathBuf> {
    if film.join(PROJECT_FILE).is_file() {
        return Some(film.to_path_buf());
    }
    let abs = std::path::absolute(film).ok()?;
    let found = abs
        .ancestors()
        .skip(1)
        .find(|a| a.join(PROJECT_FILE).is_file())?;
    Some(tidy(found))
}

/// `p` relative to the working directory if it's inside it ("." for the directory itself).
fn tidy(p: &Path) -> PathBuf {
    let Ok(cwd) = std::env::current_dir() else {
        return p.to_path_buf();
    };
    match p.strip_prefix(&cwd) {
        Ok(r) if r.as_os_str().is_empty() => PathBuf::from("."),
        Ok(r) => r.to_path_buf(),
        Err(_) => p.to_path_buf(),
    }
}

/// Same folder, however it was spelled.
pub fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// What a command's path names: a film, or a project root without a `movie.toml`.
pub enum Target {
    Film(PathBuf),
    Project(Project),
}

/// `path` is a film folder (or its `movie.toml`), or a project root.
pub fn target(path: &Path) -> Result<Target> {
    if path.is_file() {
        return Ok(Target::Film(
            path.parent().map(Path::to_path_buf).unwrap_or_default(),
        ));
    }
    if path.join(MOVIE_FILE).is_file() {
        return Ok(Target::Film(path.to_path_buf()));
    }
    if path.join(PROJECT_FILE).is_file() {
        return Ok(Target::Project(Project::read(path)?));
    }
    if !path.exists() {
        bail!("{} doesn't exist", path.display());
    }
    bail!(
        "{} has no {MOVIE_FILE} (a film) or {PROJECT_FILE} (a project)",
        path.display()
    )
}

/// A film for commands that work on one film (play, still, export, …): a project root
/// is refused with the list of its films.
pub fn film(path: &Path) -> Result<PathBuf> {
    match target(path)? {
        Target::Film(f) => Ok(f),
        Target::Project(p) => {
            let films = p.films()?;
            Err(anyhow!(
                "{} is a project ({PROJECT_FILE}), not a film; name one of its films{}",
                path.display(),
                if films.is_empty() {
                    " (it has none yet)".to_string()
                } else {
                    format!(
                        ": {}",
                        films
                            .iter()
                            .map(|f| f.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            ))
        }
    }
}

/// Split one fixture text into (`shotglass.toml`, `movie.toml`): the top-level
/// `shotglass = …` line and the `[libraries]` table (with its `[libraries.x]` sub-tables)
/// go to the project. For tests and selftest fixtures, which write a film as one text.
pub fn split_fixture(text: &str) -> (String, String) {
    let (mut project, mut movie) = (String::new(), String::new());
    let mut in_libs = false;
    let mut top = true;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with('[') {
            top = false;
            in_libs = t.starts_with("[libraries]") || t.starts_with("[libraries.");
        }
        let to_project = in_libs || (top && t.starts_with("shotglass") && t.contains('='));
        let out = if to_project { &mut project } else { &mut movie };
        out.push_str(line);
        out.push('\n');
    }
    (project, movie)
}

/// Write a fixture film (one text, see `split_fixture`) as `movie.toml` plus a
/// `shotglass.toml` in `dir`, so the film is its own project whatever is above it.
pub fn write_fixture(dir: &Path, text: &str) -> Result<()> {
    let (project, movie) = split_fixture(text);
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(PROJECT_FILE), project)?;
    std::fs::write(dir.join(MOVIE_FILE), movie)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("shotglass-project-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn fixtures_split_into_project_and_movie() {
        let (p, m) = split_fixture(
            "shotglass = \"0.1\"\ntitle = \"x\"\n[libraries]\nkit = { path = \"k\" }\n\
             [libraries.fx]\npath = \"f\"\n[[shot]]\nid = \"a\"\n",
        );
        assert_eq!(
            p,
            "shotglass = \"0.1\"\n[libraries]\nkit = { path = \"k\" }\n[libraries.fx]\npath = \"f\"\n"
        );
        assert_eq!(m, "title = \"x\"\n[[shot]]\nid = \"a\"\n");
    }

    #[test]
    fn the_nearest_project_file_wins_and_films_are_listed() {
        let root = tmp("find");
        std::fs::write(
            root.join(PROJECT_FILE),
            "[libraries]\nlib = { path = \"lib\" }\n",
        )
        .unwrap();
        for f in ["b-film", "a-film", ".hidden", "frozen"] {
            std::fs::create_dir_all(root.join(f)).unwrap();
            std::fs::write(root.join(f).join(MOVIE_FILE), "").unwrap();
        }
        std::fs::create_dir_all(root.join("lib")).unwrap();
        std::fs::write(
            root.join("frozen").join(PROJECT_FILE),
            format!("{FROZEN_HEADER}\n[libraries]\n"),
        )
        .unwrap();

        let p = Project::for_film(&root.join("a-film")).unwrap();
        assert!(same_dir(&p.root, &root) && p.has_file && !p.frozen);
        assert_eq!(p.specs.len(), 1);
        assert!(!p.is_film_root(&root.join("a-film")));
        let fz = Project::for_film(&root.join("frozen")).unwrap();
        assert!(fz.frozen && fz.is_film_root(&root.join("frozen")));
        let names: Vec<String> = p
            .films()
            .unwrap()
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a-film", "b-film", "frozen"]);

        assert!(matches!(target(&root).unwrap(), Target::Project(_)));
        assert!(matches!(
            target(&root.join("a-film")).unwrap(),
            Target::Film(_)
        ));
        let e = format!("{:#}", film(&root).unwrap_err());
        assert!(e.contains("is a project") && e.contains("a-film"), "{e}");
        let e = format!("{:#}", target(&root.join("lib")).err().unwrap());
        assert!(e.contains("has no movie.toml"), "{e}");

        // No project file anywhere above: the film is a bare project of its own.
        let lone = std::env::temp_dir().join(format!("shotglass-lone-{}", std::process::id()));
        std::fs::create_dir_all(&lone).unwrap();
        if find_root(&lone).is_none() {
            assert_eq!(Project::for_film(&lone).unwrap(), Project::bare(&lone));
        }
        let _ = std::fs::remove_dir_all(&lone);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn project_files_are_validated() {
        let r = Path::new(".");
        let e = format!(
            "{:#}",
            Project::parse("shotglass = \"99.1\"\nnew = 1\n", r).unwrap_err()
        );
        assert!(e.contains("needs shotglass 99.1"), "{e}");
        let e = format!("{:#}", Project::parse("title = \"x\"\n", r).unwrap_err());
        assert!(e.contains("unknown field"), "{e}");
        let e = format!(
            "{:#}",
            Project::parse("[libraries]\nkit = { path = \"../kit\" }\n", r).unwrap_err()
        );
        assert!(e.contains("climbs above the project folder"), "{e}");
    }
}
