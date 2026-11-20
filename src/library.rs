//! Libraries: shared GLSL that a film declares by name in `[libraries]` (issue #3).
//!
//! - `movie.toml` names each library locally (`kit = { version = "0.4", source = "https://…", subdir = "core" }`),
//!   and shaders include `<kit/noise.glsl>`. The film picks the name, so two publishers'
//!   "kit"s never clash.
//! - Each library root has a `library.toml` with its canonical name (`shotglass-kit/core`),
//!   version, the engine contract it needs, peer requirements and licences. A library's own
//!   includes of other libraries use canonical names, which map to the film's local names.
//! - `shotglass fetch` resolves version ranges against git tags, downloads into a per-user
//!   cache and writes `shotglass.lock` (exact version, tag, subdir, sha256 of the files).
//!   Loading a film only reads the lock and the cache; it never touches the network.
//! - Per-machine overrides (`SHOTGLASS_LIB_<NAME>`, `movie.local.toml`) point a library at a
//!   local folder without putting machine paths in the committed film.

use anyhow::{Context, Result, anyhow, bail};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The engine contract version: uniforms, entry points, the transition interface and cue
/// defines. A film's `shotglass = "x.y"` and a library's `requires_shotglass` are checked
/// against it.
pub const CONTRACT: &str = env!("CARGO_PKG_VERSION");

pub const LOCK_FILE: &str = "shotglass.lock";
pub const LOCAL_FILE: &str = "movie.local.toml";
pub const LIBRARY_FILE: &str = "library.toml";

pub fn contract() -> Version {
    Version::parse(CONTRACT).expect("CARGO_PKG_VERSION is semver")
}

pub fn parse_req(s: &str) -> Result<VersionReq> {
    VersionReq::parse(s.trim()).map_err(|e| anyhow!("\"{s}\" is not a version range ({e})"))
}

/// `shotglass = "0.4"` in `movie.toml`: refuse with a clear message if this engine's
/// contract doesn't satisfy it.
pub fn check_engine(req: &str) -> Result<()> {
    let r = parse_req(req).context("shotglass")?;
    let have = contract();
    if r.matches(&have) {
        return Ok(());
    }
    // Newer than every version the range names → this engine is too old.
    let too_old = r.comparators.iter().any(|c| {
        let want = Version::new(c.major, c.minor.unwrap_or(0), c.patch.unwrap_or(0));
        want > have
    });
    if too_old {
        bail!(
            "this film needs shotglass {req} (engine contract), but this is shotglass {CONTRACT}; \
             install a newer shotglass"
        );
    }
    bail!(
        "this film was made for shotglass {req}, but this is shotglass {CONTRACT}, whose \
         engine contract is not compatible; use a matching shotglass or update the film"
    )
}

// ------------------------------------------------------------------ path rules

/// Web-friendly relative path (forward slashes, no absolute paths, drive letters, URLs,
/// or `..` that climbs above the folder it is relative to). The same `movie.toml` has to
/// work on desktop and in the web player.
pub fn check_rel_path(p: &str) -> std::result::Result<(), String> {
    if p.is_empty() {
        return Err("empty path".into());
    }
    if p.contains('\\') {
        return Err(format!("\"{p}\" uses a backslash; use forward slashes"));
    }
    if p.contains("://") {
        return Err(format!(
            "\"{p}\" is a URL; URLs are only allowed in a library's source"
        ));
    }
    let b = p.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return Err(format!(
            "\"{p}\" starts with a drive letter; paths must be relative to the film folder"
        ));
    }
    if p.starts_with('/') {
        return Err(format!(
            "\"{p}\" is absolute; paths must be relative to the film folder"
        ));
    }
    let mut depth = 0i32;
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return Err(format!("\"{p}\" climbs above the film folder"));
                }
            }
            _ => depth += 1,
        }
    }
    Ok(())
}

/// If `rel` (from `base`) only exists when ignoring case, the name as it is on disk.
/// Case-insensitive file systems (Windows, macOS) find such files; Linux and the web don't.
pub fn case_mismatch(base: &Path, rel: &str) -> Option<String> {
    let mut dir = base.to_path_buf();
    let mut actual: Vec<String> = vec![];
    let mut differs = false;
    for seg in rel.split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                dir.push("..");
                actual.push("..".into());
                continue;
            }
            _ => {}
        }
        let mut exact = false;
        let mut other = None;
        for e in std::fs::read_dir(&dir).ok()?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name == seg {
                exact = true;
                break;
            }
            if name.eq_ignore_ascii_case(seg) {
                other = Some(name);
            }
        }
        let name = if exact {
            seg.to_string()
        } else {
            differs = true;
            other?
        };
        dir.push(&name);
        actual.push(name);
    }
    differs.then(|| actual.join("/"))
}

fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A branch name we're willing to hand to git: no options, whitespace, `..` or ref syntax.
fn is_branch_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(['-', '/'])
        && !s.ends_with(['/', '.'])
        && !s.contains("..")
        && !s.contains("//")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
}

/// A full git commit id (sha1 or sha256), lowercase hex.
fn is_commit_id(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

/// A canonical name: one or more name segments joined by `/` (`shotglass-kit/core`).
fn is_canonical(s: &str) -> bool {
    !s.is_empty() && s.split('/').all(is_name)
}

// ------------------------------------------------------------------ declarations

/// One entry of `[libraries]` in `movie.toml`.
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct LibraryDecl {
    /// Semver range, resolved against the source's git tags.
    pub version: Option<String>,
    /// https URL of a git repository.
    pub source: Option<String>,
    /// Branch to follow when there's no version (default: the repository's default branch).
    pub branch: Option<String>,
    /// Folder of the source that is the library root.
    pub subdir: Option<String>,
    /// A library inside the film's own folder (relative, may not climb out).
    pub path: Option<String>,
}

/// A validated `[libraries]` entry.
#[derive(Clone, Debug, PartialEq)]
pub struct LibrarySpec {
    /// Local name: the include prefix in this film.
    pub name: String,
    pub version: Option<VersionReq>,
    pub source: Option<String>,
    /// Set (with a source) only when there's no version: the branch to follow, `None` for
    /// the default branch. Fetching pins the branch's newest commit.
    pub branch: Option<String>,
    pub subdir: Option<String>,
    pub path: Option<String>,
}

impl LibrarySpec {
    /// Follows a branch (no version range) rather than tagged releases?
    pub fn tracks_branch(&self) -> bool {
        self.source.is_some() && self.version.is_none()
    }

    /// The ref a branch-following spec resolves: `HEAD` (the default branch) or
    /// `refs/heads/<branch>`.
    fn branch_ref(&self) -> String {
        match &self.branch {
            Some(b) => format!("refs/heads/{b}"),
            None => "HEAD".to_string(),
        }
    }
}

impl LibrarySpec {
    pub fn parse(name: &str, d: &LibraryDecl) -> Result<LibrarySpec> {
        if !is_name(name) {
            bail!("library name \"{name}\" may only use letters, digits, _ and -");
        }
        let ctx = || format!("library \"{name}\"");
        let version = match &d.version {
            Some(v) => Some(parse_req(v).with_context(ctx)?),
            None => None,
        };
        match (&d.source, &d.path) {
            (Some(_), Some(_)) => bail!("{}: give either source or path, not both", ctx()),
            (None, None) => bail!(
                "{}: needs a source (https git URL + version) or a path inside the film",
                ctx()
            ),
            (Some(src), None) => {
                if !src.starts_with("https://")
                    || src.len() <= "https://".len()
                    || src.contains(char::is_whitespace)
                {
                    bail!("{}: source must be an https URL, got \"{src}\"", ctx());
                }
                if let Some(b) = &d.branch {
                    if version.is_some() {
                        bail!(
                            "{}: give either version (tagged releases) or branch, not both",
                            ctx()
                        );
                    }
                    if !is_branch_name(b) {
                        bail!("{}: \"{b}\" is not a usable branch name", ctx());
                    }
                }
            }
            (None, Some(p)) => {
                check_rel_path(p).map_err(|e| anyhow!("{}: path {e}", ctx()))?;
                if d.subdir.is_some() {
                    bail!(
                        "{}: subdir only goes with source; put the folder in path",
                        ctx()
                    );
                }
                if d.branch.is_some() {
                    bail!("{}: branch only goes with source", ctx());
                }
            }
        }
        if let Some(s) = &d.subdir {
            check_rel_path(s).map_err(|e| anyhow!("{}: subdir {e}", ctx()))?;
            if s.split('/').any(|x| x == "..") {
                bail!("{}: subdir may not contain ..", ctx());
            }
        }
        Ok(LibrarySpec {
            name: name.to_string(),
            version,
            source: d.source.clone(),
            branch: d.branch.clone(),
            subdir: d
                .subdir
                .as_ref()
                .map(|s| s.trim_matches('/').to_string())
                .filter(|s| !s.is_empty()),
            path: d.path.clone(),
        })
    }
}

// ------------------------------------------------------------------ library.toml

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LibraryToml {
    name: String,
    version: String,
    requires_shotglass: Option<String>,
    #[serde(default)]
    requires: BTreeMap<String, String>,
    #[serde(alias = "licenses", alias = "licence", alias = "license")]
    licences: Option<OneOrMany>,
    /// GLSL version the library needs beyond the engine baseline (checked by #6).
    glsl: Option<u32>,
    #[allow(dead_code)] // for people and listings; not used by the engine
    description: Option<String>,
}

/// Where a library's files came from (for messages).
#[derive(Clone, Debug, PartialEq)]
pub enum Origin {
    /// `path = "…"` in movie.toml.
    Path,
    /// `SHOTGLASS_LIB_<NAME>`.
    Env(String),
    /// `movie.local.toml`.
    Local,
    /// The cache, as pinned by shotglass.lock.
    Locked,
}

/// A resolved library: a folder on disk with a valid `library.toml`.
#[derive(Clone, Debug)]
pub struct Library {
    pub name: String,
    pub canonical: String,
    pub version: Version,
    pub root: PathBuf,
    pub licences: Vec<String>,
    pub requires: Vec<(String, VersionReq)>,
    pub requires_shotglass: Option<VersionReq>,
    pub glsl: Option<u32>,
    pub origin: Origin,
}

impl Library {
    pub fn read(name: &str, root: &Path, origin: Origin) -> Result<Library> {
        let file = root.join(LIBRARY_FILE);
        let text = std::fs::read_to_string(&file).with_context(|| {
            format!(
                "library \"{name}\": can't read {} (every library root needs a {LIBRARY_FILE})",
                file.display()
            )
        })?;
        Library::parse(name, root, origin, &text)
            .with_context(|| format!("library \"{name}\": in {}", file.display()))
    }

    fn parse(name: &str, root: &Path, origin: Origin, text: &str) -> Result<Library> {
        let t: LibraryToml = toml::from_str(text)?;
        if !is_canonical(&t.name) {
            bail!(
                "name \"{}\" must be name segments joined by /, e.g. \"shotglass-kit/core\"",
                t.name
            );
        }
        let version = Version::parse(t.version.trim())
            .map_err(|e| anyhow!("version \"{}\" is not semver ({e})", t.version))?;
        let mut requires = vec![];
        for (k, v) in &t.requires {
            if !is_canonical(k) {
                bail!("requires: \"{k}\" is not a canonical library name");
            }
            requires.push((
                k.clone(),
                parse_req(v).with_context(|| format!("requires {k}"))?,
            ));
        }
        Ok(Library {
            name: name.to_string(),
            canonical: t.name,
            version,
            root: root.to_path_buf(),
            licences: match t.licences {
                None => vec![],
                Some(OneOrMany::One(s)) => vec![s],
                Some(OneOrMany::Many(v)) => v,
            },
            requires,
            requires_shotglass: match &t.requires_shotglass {
                Some(r) => Some(parse_req(r).context("requires_shotglass")?),
                None => None,
            },
            glsl: t.glsl,
            origin,
        })
    }

    /// "kit (shotglass-kit/core 0.4.2)".
    pub fn describe(&self) -> String {
        format!("{} ({} {})", self.name, self.canonical, self.version)
    }
}

// ------------------------------------------------------------------ overrides

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalFile {
    #[serde(default)]
    libraries: BTreeMap<String, LocalLib>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalLib {
    path: String,
}

/// `SHOTGLASS_LIB_KIT` for a library called `kit` (`my-lib` → `SHOTGLASS_LIB_MY_LIB`).
pub fn env_var(name: &str) -> String {
    format!(
        "SHOTGLASS_LIB_{}",
        name.to_ascii_uppercase().replace('-', "_")
    )
}

/// Per-machine overrides: `SHOTGLASS_LIB_<NAME>` beats `movie.local.toml`. Override paths
/// may be absolute; relative ones are relative to the film folder.
pub fn overrides(
    film_root: &Path,
    specs: &[LibrarySpec],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<BTreeMap<String, (PathBuf, Origin)>> {
    let mut out = BTreeMap::new();
    let local = film_root.join(LOCAL_FILE);
    if local.exists() {
        let text = std::fs::read_to_string(&local)
            .with_context(|| format!("reading {}", local.display()))?;
        let f: LocalFile =
            toml::from_str(&text).with_context(|| format!("in {}", local.display()))?;
        for (name, l) in f.libraries {
            if !specs.iter().any(|s| s.name == name) {
                bail!(
                    "{LOCAL_FILE} overrides library \"{name}\", but movie.toml has no such library"
                );
            }
            out.insert(name, (film_root.join(&l.path), Origin::Local));
        }
    }
    for s in specs {
        let var = env_var(&s.name);
        if let Some(p) = env(&var).filter(|p| !p.is_empty()) {
            out.insert(s.name.clone(), (film_root.join(&p), Origin::Env(var)));
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ lock file

/// `shotglass.lock`: every source library pinned to an exact version and file hash.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    pub version: u32,
    #[serde(rename = "library", default)]
    pub libraries: Vec<LockEntry>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LockEntry {
    /// Local name from movie.toml.
    pub name: String,
    /// Exact version (for a followed branch: what its `library.toml` said when pinned).
    pub version: String,
    /// The git URL; together with `tag` (or `rev`) this is the resolved location.
    pub source: String,
    /// The release tag, or for a followed branch the ref: `HEAD` or `refs/heads/<branch>`.
    pub tag: String,
    /// The pinned commit when following a branch instead of tagged releases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
    /// sha256 of the library root's files (see `hash_dir`).
    pub sha256: String,
}

impl Default for Lock {
    fn default() -> Lock {
        Lock {
            version: 1,
            libraries: vec![],
        }
    }
}

impl Lock {
    pub fn parse(text: &str) -> Result<Lock> {
        let l: Lock = toml::from_str(text)?;
        if l.version != 1 {
            bail!(
                "lock file version {} is not supported by this shotglass",
                l.version
            );
        }
        for e in &l.libraries {
            if let Some(r) = &e.rev
                && !is_commit_id(r)
            {
                bail!(
                    "library \"{}\": rev \"{r}\" is not a full commit id",
                    e.name
                );
            }
        }
        Ok(l)
    }

    pub fn to_text(&self) -> String {
        let mut l = self.clone();
        l.libraries.sort_by(|a, b| a.name.cmp(&b.name));
        format!(
            "# Written by `shotglass fetch`/`update`. Commit it with the film.\n{}",
            toml::to_string(&l).expect("lock serialises")
        )
    }

    pub fn load(film_root: &Path) -> Result<Option<Lock>> {
        let p = film_root.join(LOCK_FILE);
        if !p.exists() {
            return Ok(None);
        }
        let text =
            std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        Lock::parse(&text)
            .map(Some)
            .with_context(|| format!("in {}", p.display()))
    }

    pub fn get(&self, name: &str) -> Option<&LockEntry> {
        self.libraries.iter().find(|e| e.name == name)
    }
}

impl LockEntry {
    /// Still valid for this spec (same source and subdir, version in range)?
    fn fits(&self, s: &LibrarySpec) -> bool {
        s.source.as_deref() == Some(self.source.as_str())
            && s.subdir == self.subdir
            && match (&s.version, &self.rev) {
                (Some(r), None) => Version::parse(&self.version).is_ok_and(|v| r.matches(&v)),
                (None, Some(_)) => self.tag == s.branch_ref(),
                _ => false,
            }
    }

    /// The cache folder name under the source: the version, or the commit.
    fn cache_key(&self) -> &str {
        self.rev.as_deref().unwrap_or(&self.version)
    }

    /// For messages: `0.4.2`, or `main@1a2b3c4 (0.1.0)`.
    pub fn pin(&self) -> String {
        match &self.rev {
            Some(r) => format!(
                "{}@{} ({})",
                self.tag.strip_prefix("refs/heads/").unwrap_or(&self.tag),
                &r[..7],
                self.version
            ),
            None => self.version.clone(),
        }
    }
}

// ------------------------------------------------------------------ hashing and cache

/// sha256 over a library's files, the same on every OS: files sorted by their path relative
/// to the root (forward slashes), hidden files and folders (`.git`, …) skipped, CRLF turned
/// into LF in UTF-8 files. For each file: path bytes, a 0 byte, the content length as
/// 8 little-endian bytes, the content. Lowercase hex.
pub fn hash_dir(root: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut files = vec![];
    collect_files(root, "", &mut files)?;
    files.sort();
    let mut h = Sha256::new();
    for rel in files {
        let bytes = std::fs::read(root.join(&rel))?;
        let bytes = match String::from_utf8(bytes) {
            Ok(s) => s.replace("\r\n", "\n").into_bytes(),
            Err(e) => e.into_bytes(),
        };
        h.update(rel.as_bytes());
        h.update([0u8]);
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn collect_files(dir: &Path, prefix: &str, out: &mut Vec<String>) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for e in entries {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let ty = e.file_type()?;
        if ty.is_dir() {
            collect_files(&e.path(), &rel, out)?;
        } else if ty.is_file() {
            out.push(rel);
        }
    }
    Ok(())
}

/// Per-user download cache: `SHOTGLASS_CACHE` if set, else `%LOCALAPPDATA%\shotglass\libs`,
/// `~/Library/Caches/shotglass/libs` or `$XDG_CACHE_HOME`/`~/.cache/shotglass/libs`.
pub fn cache_dir() -> PathBuf {
    let var = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if let Some(p) = var("SHOTGLASS_CACHE") {
        return p;
    }
    let base = if cfg!(windows) {
        var("LOCALAPPDATA")
    } else if cfg!(target_os = "macos") {
        var("HOME").map(|h| h.join("Library/Caches"))
    } else {
        var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|h| h.join(".cache")))
    };
    base.unwrap_or_else(std::env::temp_dir)
        .join("shotglass")
        .join("libs")
}

/// Cache folder for one source at one version, shared by every library (subdir) in it.
pub fn cache_path(cache: &Path, source: &str, version: &str) -> PathBuf {
    let s = source.strip_prefix("https://").unwrap_or(source);
    let s = s.trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    let mut p = cache.to_path_buf();
    for seg in s.split('/').filter(|x| !x.is_empty()) {
        let clean: String = seg
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        // never "." or ".." as a folder name
        p.push(if clean.chars().all(|c| c == '.') {
            "_".to_string()
        } else {
            clean
        });
    }
    p.push(version);
    p
}

// ------------------------------------------------------------------ resolving

/// Turn a film's declarations into library folders on disk, checking `library.toml`,
/// versions, the engine contract and peer requirements. Never touches the network.
pub fn resolve(
    film_root: &Path,
    specs: &[LibrarySpec],
    overrides: &BTreeMap<String, (PathBuf, Origin)>,
    lock: Option<&Lock>,
    cache: &Path,
) -> Result<Vec<Library>> {
    let mut libs = vec![];
    for s in specs {
        let (root, origin) = if let Some((p, o)) = overrides.get(&s.name) {
            (p.clone(), o.clone())
        } else if let Some(p) = &s.path {
            (film_root.join(p), Origin::Path)
        } else {
            let fetch = "run `shotglass fetch` on the film";
            let e = lock.and_then(|l| l.get(&s.name)).ok_or_else(|| {
                anyhow!("library \"{}\" isn't in {LOCK_FILE} yet; {fetch}", s.name)
            })?;
            if !e.fits(s) {
                bail!(
                    "library \"{}\" in movie.toml no longer matches {LOCK_FILE} ({} {}); \
                     run `shotglass update` on the film",
                    s.name,
                    e.source,
                    e.pin()
                );
            }
            let dir = cache_path(cache, &e.source, e.cache_key());
            let root = match &e.subdir {
                Some(sub) => dir.join(sub),
                None => dir.clone(),
            };
            if !root.is_dir() {
                bail!(
                    "library \"{}\" {} isn't downloaded on this machine; {fetch}",
                    s.name,
                    e.pin()
                );
            }
            let got = hash_dir(&root)?;
            if got != e.sha256 {
                bail!(
                    "library \"{}\" {}: files in {} don't match {LOCK_FILE} (sha256 {got}, \
                     locked {}); the cache was edited or the tag moved",
                    s.name,
                    e.pin(),
                    root.display(),
                    e.sha256
                );
            }
            (root, Origin::Locked)
        };
        let lib = Library::read(&s.name, &root, origin)?;
        if let Some(r) = &s.version
            && !r.matches(&lib.version)
        {
            bail!(
                "library \"{}\": movie.toml asks for version {r}, but {} is {} {}",
                s.name,
                root.display(),
                lib.canonical,
                lib.version
            );
        }
        libs.push(lib);
    }
    check_set(&libs)?;
    Ok(libs)
}

/// Engine contract, duplicate canonical names, and peer requirements.
pub fn check_set(libs: &[Library]) -> Result<()> {
    let have = contract();
    for l in libs {
        if let Some(r) = &l.requires_shotglass
            && !r.matches(&have)
        {
            bail!(
                "library {} needs shotglass {r}, but this is shotglass {CONTRACT}",
                l.describe()
            );
        }
    }
    for (i, a) in libs.iter().enumerate() {
        if let Some(b) = libs[..i].iter().find(|b| b.canonical == a.canonical) {
            bail!(
                "libraries \"{}\" and \"{}\" are both {}; list each library once",
                b.name,
                a.name,
                a.canonical
            );
        }
    }
    for l in libs {
        for (peer, req) in &l.requires {
            match libs.iter().find(|p| &p.canonical == peer) {
                None => bail!(
                    "library {} needs {peer} {req}; add it to [libraries] in movie.toml \
                     (peer libraries are checked, not installed)",
                    l.describe()
                ),
                Some(p) if !req.matches(&p.version) => bail!(
                    "library {} needs {peer} {req}, but \"{}\" is {}",
                    l.describe(),
                    p.name,
                    p.version
                ),
                Some(_) => {}
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ fetching

/// Where library releases come from. `Git` is the real one; tests use a fake.
pub trait Fetcher {
    /// Tag names published at `source`.
    fn tags(&self, source: &str) -> Result<Vec<String>>;
    /// Put the files of `tag` into `dest`, which doesn't exist yet.
    fn download(&self, source: &str, tag: &str, dest: &Path) -> Result<()>;
    /// The commit `refname` (`HEAD` or `refs/heads/<branch>`) points at now.
    fn head(&self, source: &str, refname: &str) -> Result<String>;
    /// Put the files of commit `rev` into `dest`, which doesn't exist yet.
    fn download_rev(&self, source: &str, rev: &str, dest: &Path) -> Result<()>;
}

/// `v1.2.3` or `1.2.3`.
pub fn tag_version(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

/// The newest tag whose version is in range.
pub fn pick(req: &VersionReq, tags: &[String]) -> Option<(Version, String)> {
    tags.iter()
        .filter_map(|t| tag_version(t).map(|v| (v, t.clone())))
        .filter(|(v, _)| req.matches(v))
        .max_by(|a, b| a.0.cmp(&b.0))
}

#[derive(Clone, Debug)]
pub enum FetchMode {
    /// Fetch what isn't locked or downloaded; keep every existing pin. Libraries in `skip`
    /// (overridden locally) aren't touched.
    Missing { skip: BTreeSet<String> },
    /// Re-resolve these libraries (all if empty) to the newest version in range.
    Update(Vec<String>),
}

/// Bring the lock file and the cache up to date. Returns the new lock (unchanged entries
/// keep their place) and a line per library.
pub fn fetch(
    specs: &[LibrarySpec],
    old: Option<&Lock>,
    fetcher: &dyn Fetcher,
    cache: &Path,
    mode: &FetchMode,
) -> Result<(Lock, Vec<String>)> {
    if let FetchMode::Update(names) = mode {
        for n in names {
            if !specs.iter().any(|s| &s.name == n) {
                bail!("movie.toml has no library \"{n}\"");
            }
        }
    }
    let mut lock = Lock::default();
    let mut log = vec![];
    for s in specs {
        let Some(source) = &s.source else {
            log.push(format!(
                "{}: local folder {}",
                s.name,
                s.path.as_deref().unwrap_or("")
            ));
            continue;
        };
        let prev = old.and_then(|l| l.get(&s.name)).filter(|e| e.fits(s));
        let refresh = match mode {
            FetchMode::Missing { skip } => {
                if skip.contains(&s.name) {
                    if let Some(e) = old.and_then(|l| l.get(&s.name)) {
                        lock.libraries.push(e.clone());
                    }
                    log.push(format!("{}: overridden locally", s.name));
                    continue;
                }
                false
            }
            FetchMode::Update(names) => names.is_empty() || names.contains(&s.name),
        };
        let entry = match prev {
            Some(e) if !refresh => {
                let dir = cache_path(cache, &e.source, e.cache_key());
                let fresh = !dir.exists();
                if fresh {
                    download(fetcher, source, &e.tag, e.rev.as_deref(), &dir)?;
                }
                let root = lib_root(&dir, &e.subdir, &s.name)?;
                let got = hash_dir(&root)?;
                if got != e.sha256 {
                    bail!(
                        "library \"{}\" {} ({} tag {}): sha256 {got} doesn't match {LOCK_FILE} \
                         ({}); {}",
                        s.name,
                        e.pin(),
                        e.source,
                        e.rev.as_deref().unwrap_or(&e.tag),
                        e.sha256,
                        if fresh {
                            "the tag was moved or re-published. Check the source, then \
                             `shotglass update` to accept the new files"
                        } else {
                            "the cached copy was changed; delete it and fetch again"
                        }
                    );
                }
                log.push(format!(
                    "{}: {} {}{}",
                    s.name,
                    e.pin(),
                    if fresh { "downloaded" } else { "cached" },
                    " (locked)"
                ));
                e.clone()
            }
            _ if s.tracks_branch() => {
                let refname = s.branch_ref();
                let rev = fetcher.head(source, &refname).with_context(|| {
                    format!("library \"{}\": looking up {refname} of {source}", s.name)
                })?;
                if !is_commit_id(&rev) {
                    bail!(
                        "library \"{}\": {source} {refname} gave \"{rev}\", not a commit id",
                        s.name
                    );
                }
                let dir = cache_path(cache, source, &rev);
                if !dir.exists() {
                    download(fetcher, source, &refname, Some(&rev), &dir)?;
                }
                let root = lib_root(&dir, &s.subdir, &s.name)?;
                let lib = Library::read(&s.name, &root, Origin::Locked)?;
                let e = LockEntry {
                    name: s.name.clone(),
                    version: lib.version.to_string(),
                    source: source.clone(),
                    tag: refname,
                    rev: Some(rev),
                    subdir: s.subdir.clone(),
                    sha256: hash_dir(&root)?,
                };
                log.push(pin_change(&s.name, prev, &e));
                e
            }
            _ => {
                let req = s
                    .version
                    .as_ref()
                    .expect("validated: a source without a version follows a branch");
                let tags = fetcher
                    .tags(source)
                    .with_context(|| format!("library \"{}\": listing tags of {source}", s.name))?;
                let (v, tag) = pick(req, &tags).ok_or_else(|| {
                    let mut vs: Vec<Version> = tags.iter().filter_map(|t| tag_version(t)).collect();
                    vs.sort();
                    anyhow!(
                        "library \"{}\": no release of {source} matches {req} (versions: {})",
                        s.name,
                        if vs.is_empty() {
                            "none".to_string()
                        } else {
                            vs.iter()
                                .map(|v| v.to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    )
                })?;
                let dir = cache_path(cache, source, &v.to_string());
                if !dir.exists() {
                    download(fetcher, source, &tag, None, &dir)?;
                }
                let root = lib_root(&dir, &s.subdir, &s.name)?;
                let e = LockEntry {
                    name: s.name.clone(),
                    version: v.to_string(),
                    source: source.clone(),
                    tag,
                    rev: None,
                    subdir: s.subdir.clone(),
                    sha256: hash_dir(&root)?,
                };
                log.push(pin_change(&s.name, prev, &e));
                e
            }
        };
        lock.libraries.push(entry);
    }
    Ok((lock, log))
}

/// A log line for a (re-)resolved pin.
fn pin_change(name: &str, prev: Option<&LockEntry>, e: &LockEntry) -> String {
    match prev {
        Some(p) if p.pin() != e.pin() => format!("{name}: {} → {}", p.pin(), e.pin()),
        Some(_) => format!("{name}: {} (up to date)", e.pin()),
        None => format!("{name}: {} (new)", e.pin()),
    }
}

fn lib_root(dir: &Path, subdir: &Option<String>, name: &str) -> Result<PathBuf> {
    let root = match subdir {
        Some(s) => dir.join(s),
        None => dir.to_path_buf(),
    };
    if !root.join(LIBRARY_FILE).is_file() {
        bail!(
            "library \"{name}\": {} has no {LIBRARY_FILE}{}",
            root.display(),
            if subdir.is_none() {
                " (does it need a subdir?)"
            } else {
                ""
            }
        );
    }
    Ok(root)
}

/// Download into a temporary sibling, then rename, so a half-finished download never looks
/// like a cached release.
fn download(
    fetcher: &dyn Fetcher,
    source: &str,
    tag: &str,
    rev: Option<&str>,
    dir: &Path,
) -> Result<()> {
    let parent = dir.parent().expect("cache path has a parent");
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        dir.file_name().unwrap().to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    match rev {
        Some(r) => fetcher
            .download_rev(source, r, &tmp)
            .with_context(|| format!("downloading {source} {tag} at {r}")),
        None => fetcher
            .download(source, tag, &tmp)
            .with_context(|| format!("downloading {source} {tag}")),
    }?;
    if let Err(e) = std::fs::rename(&tmp, dir) {
        let _ = std::fs::remove_dir_all(&tmp);
        if !dir.exists() {
            return Err(e.into());
        }
    }
    Ok(())
}

/// Fetch with the `git` command line (no credentials prompts; LF line endings kept).
pub struct Git;

impl Git {
    fn run(args: &[&str]) -> Result<String> {
        let out = std::process::Command::new("git")
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|e| anyhow!("can't run git ({e}); fetching libraries needs git installed"))?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

impl Fetcher for Git {
    fn tags(&self, source: &str) -> Result<Vec<String>> {
        let out = Git::run(&["ls-remote", "--tags", "--refs", source])?;
        Ok(out
            .lines()
            .filter_map(|l| l.split('\t').nth(1))
            .filter_map(|r| r.strip_prefix("refs/tags/"))
            .map(str::to_string)
            .collect())
    }

    fn download(&self, source: &str, tag: &str, dest: &Path) -> Result<()> {
        let d = dest.to_string_lossy();
        Git::run(&[
            "-c",
            "core.autocrlf=false",
            "-c",
            "advice.detachedHead=false",
            "clone",
            "--quiet",
            "--depth",
            "1",
            "--branch",
            tag,
            "--",
            source,
            &d,
        ])?;
        std::fs::remove_dir_all(dest.join(".git"))?;
        Ok(())
    }

    fn head(&self, source: &str, refname: &str) -> Result<String> {
        let out = Git::run(&["ls-remote", source, refname])?;
        out.lines()
            .filter_map(|l| l.split_once('\t'))
            .find(|(_, r)| *r == refname)
            .map(|(sha, _)| sha.to_string())
            .ok_or_else(|| anyhow!("{source} has no {refname}"))
    }

    fn download_rev(&self, source: &str, rev: &str, dest: &Path) -> Result<()> {
        if !is_commit_id(rev) {
            bail!("\"{rev}\" is not a full commit id");
        }
        let d = dest.to_string_lossy();
        Git::run(&["init", "--quiet", "--", &d])?;
        let in_dest = |args: &[&str]| {
            let mut a = vec!["-C", &*d, "-c", "core.autocrlf=false"];
            a.extend_from_slice(args);
            Git::run(&a)
        };
        // Servers (GitHub included) hand out a reachable commit by id; fall back to all
        // branches for ones that don't.
        if in_dest(&["fetch", "--quiet", "--depth", "1", source, rev]).is_err() {
            in_dest(&[
                "fetch",
                "--quiet",
                source,
                "+refs/heads/*:refs/remotes/src/*",
            ])?;
        }
        in_dest(&[
            "-c",
            "advice.detachedHead=false",
            "checkout",
            "--quiet",
            rev,
        ])?;
        std::fs::remove_dir_all(dest.join(".git"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("shotglass-lib-{name}-{}", std::process::id()));
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

    /// `toml_src` is the inside of an inline table: `version = "1", path = "x"`.
    fn spec(name: &str, toml_src: &str) -> Result<LibrarySpec> {
        let t: BTreeMap<String, LibraryDecl> = toml::from_str(&format!("x = {{ {toml_src} }}"))?;
        let d = &t["x"];
        LibrarySpec::parse(name, d)
    }

    fn libtoml(name: &str, version: &str, extra: &str) -> String {
        format!("name = \"{name}\"\nversion = \"{version}\"\nlicences = [\"MIT\"]\n{extra}")
    }

    #[test]
    fn engine_check() {
        let have = contract();
        assert!(check_engine(&format!("{}.{}", have.major, have.minor)).is_ok());
        let e = check_engine("99.0").unwrap_err().to_string();
        assert!(
            e.contains("needs shotglass 99.0") && e.contains("newer"),
            "{e}"
        );
        // An engine whose contract moved on (0.x minor bumps are breaking).
        let older = format!("{}.{}", have.major, have.minor.wrapping_sub(1));
        if have.minor > 0 {
            let e = check_engine(&older).unwrap_err().to_string();
            assert!(e.contains("not compatible"), "{e}");
        }
        assert!(check_engine("not a version").is_err());
    }

    #[test]
    fn path_rules() {
        for ok in [
            "a.glsl",
            "scenes/a.glsl",
            "scenes/../a.glsl",
            "./x/y.glsl",
            "lib/kit",
        ] {
            assert!(check_rel_path(ok).is_ok(), "{ok}");
        }
        for (bad, why) in [
            ("/etc/x.glsl", "absolute"),
            ("C:/films/x.glsl", "drive letter"),
            ("c:x.glsl", "drive letter"),
            ("scenes\\a.glsl", "backslash"),
            ("../x.glsl", "climbs"),
            ("a/../../x.glsl", "climbs"),
            ("https://x.org/a.glsl", "URL"),
            ("", "empty"),
        ] {
            let e = check_rel_path(bad).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
    }

    #[test]
    fn case_only_matches_are_found() {
        let d = tmp("case");
        write(&d, &[("Scenes/Moon.glsl", "x")]);
        assert_eq!(case_mismatch(&d, "Scenes/Moon.glsl"), None);
        assert_eq!(
            case_mismatch(&d, "scenes/moon.glsl").as_deref(),
            Some("Scenes/Moon.glsl")
        );
        assert_eq!(case_mismatch(&d, "scenes/missing.glsl"), None);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn spec_validation() {
        let s = spec(
            "kit",
            r#"version = "0.4", source = "https://github.com/antonmarsden/shotglass-kit", subdir = "core/""#,
        )
        .unwrap();
        assert_eq!(s.subdir.as_deref(), Some("core"));
        assert!(s.version.unwrap().matches(&Version::new(0, 4, 9)));
        assert!(spec("mine", r#"path = "lib/mykit""#).is_ok());
        // No version: follow the default branch, or the one named.
        let s = spec("kit", r#"source = "https://x.org/k""#).unwrap();
        assert!(s.tracks_branch());
        assert_eq!(s.branch_ref(), "HEAD");
        let s = spec("kit", r#"source = "https://x.org/k", branch = "dev/next""#).unwrap();
        assert_eq!(s.branch_ref(), "refs/heads/dev/next");

        for (name, src, why) in [
            ("kit", r#"path = "lib", source = "https://x""#, "either"),
            ("kit", r#"version = "1""#, "needs a source"),
            (
                "kit",
                r#"source = "https://x.org/k", version = "1", branch = "main""#,
                "not both",
            ),
            (
                "kit",
                r#"source = "https://x.org/k", branch = "--upload-pack=x""#,
                "branch name",
            ),
            (
                "kit",
                r#"source = "https://x.org/k", branch = "a..b""#,
                "branch name",
            ),
            ("kit", r#"path = "lib", branch = "main""#, "branch only"),
            (
                "kit",
                r#"source = "http://x.org/k", version = "1""#,
                "https",
            ),
            ("kit", r#"source = "file:///k", version = "1""#, "https"),
            ("kit", r#"path = "../shared""#, "climbs"),
            ("kit", r#"path = "C:/kit""#, "drive"),
            ("kit", r#"path = "lib", subdir = "x""#, "subdir only"),
            (
                "kit",
                r#"source = "https://x.org/k", version = "1", subdir = "../x""#,
                "climbs",
            ),
            (
                "kit",
                r#"source = "https://x.org/k", version = "one""#,
                "version range",
            ),
            ("k/it", r#"path = "lib""#, "letters"),
        ] {
            let e = format!("{:#}", spec(name, src).unwrap_err());
            assert!(e.contains(why), "{src}: {e}");
        }
        assert!(spec("kit", r#"path = "lib", pth = "x""#).is_err());
    }

    #[test]
    fn library_toml() {
        let l = Library::parse(
            "kit",
            Path::new("k"),
            Origin::Path,
            &libtoml(
                "shotglass-kit/core",
                "0.4.2",
                "requires_shotglass = \">=0.1\"\nglsl = 330\n[requires]\n\"shotglass-kit/sounds\" = \">=0.3\"\n",
            ),
        )
        .unwrap();
        assert_eq!(l.canonical, "shotglass-kit/core");
        assert_eq!(l.version, Version::new(0, 4, 2));
        assert_eq!(l.licences, vec!["MIT"]);
        assert_eq!(l.requires[0].0, "shotglass-kit/sounds");
        let one = Library::parse(
            "kit",
            Path::new("k"),
            Origin::Path,
            "name = \"a\"\nversion = \"1.0.0\"\nlicense = \"CC-BY-NC-4.0\"\n",
        )
        .unwrap();
        assert_eq!(one.licences, vec!["CC-BY-NC-4.0"]);
        for bad in [
            "name = \"a b\"\nversion = \"1.0.0\"",
            "name = \"a\"\nversion = \"1.0\"",
            "name = \"a\"\nversion = \"1.0.0\"\nscript = \"x\"",
        ] {
            assert!(
                Library::parse("k", Path::new("k"), Origin::Path, bad).is_err(),
                "{bad}"
            );
        }
    }

    fn lib(name: &str, canonical: &str, version: &str, requires: &[(&str, &str)]) -> Library {
        Library {
            name: name.into(),
            canonical: canonical.into(),
            version: Version::parse(version).unwrap(),
            root: PathBuf::from(name),
            licences: vec![],
            requires: requires
                .iter()
                .map(|(c, r)| (c.to_string(), parse_req(r).unwrap()))
                .collect(),
            requires_shotglass: None,
            glsl: None,
            origin: Origin::Path,
        }
    }

    #[test]
    fn peer_requirements_are_checked() {
        let toon = lib(
            "toon",
            "someone/toon",
            "1.2.0",
            &[("shotglass-kit/core", ">=0.3")],
        );
        let e = check_set(std::slice::from_ref(&toon))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("needs shotglass-kit/core >=0.3") && e.contains("add it"),
            "{e}"
        );
        // The film's local name doesn't matter; the canonical name does.
        let old = lib("basics", "shotglass-kit/core", "0.2.9", &[]);
        let e = check_set(&[toon.clone(), old]).unwrap_err().to_string();
        assert!(e.contains("\"basics\" is 0.2.9"), "{e}");
        let ok = lib("basics", "shotglass-kit/core", "0.4.0", &[]);
        assert!(check_set(&[toon, ok.clone()]).is_ok());
        let dup = lib("kit", "shotglass-kit/core", "0.4.0", &[]);
        let e = check_set(&[ok, dup]).unwrap_err().to_string();
        assert!(e.contains("both shotglass-kit/core"), "{e}");
        let mut future = lib("f", "a/f", "1.0.0", &[]);
        future.requires_shotglass = Some(parse_req("99").unwrap());
        assert!(
            check_set(&[future])
                .unwrap_err()
                .to_string()
                .contains("needs shotglass ^99")
        );
    }

    #[test]
    fn version_ranges_pick_the_newest_match() {
        let tags: Vec<String> = [
            "v0.3.9",
            "v0.4.0",
            "0.4.2",
            "v0.4.10",
            "v0.5.0",
            "v0.4.11-beta.1",
            "latest",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let p = |r: &str| pick(&parse_req(r).unwrap(), &tags).map(|(v, t)| (v.to_string(), t));
        assert_eq!(p("0.4"), Some(("0.4.10".into(), "v0.4.10".into())));
        assert_eq!(p("=0.4.2"), Some(("0.4.2".into(), "0.4.2".into())));
        assert_eq!(p(">=0.3, <0.4"), Some(("0.3.9".into(), "v0.3.9".into())));
        assert_eq!(p("0.5"), Some(("0.5.0".into(), "v0.5.0".into())));
        assert_eq!(p("1"), None);
        assert_eq!(tag_version("latest"), None);
    }

    #[test]
    fn lock_round_trip() {
        let l = Lock {
            version: 1,
            libraries: vec![
                LockEntry {
                    name: "kit".into(),
                    version: "0.1.0".into(),
                    source: "https://github.com/antonmarsden/shotglass-kit".into(),
                    tag: "HEAD".into(),
                    rev: Some("e".repeat(40)),
                    subdir: Some("core".into()),
                    sha256: "ef".repeat(32),
                },
                LockEntry {
                    name: "sfx".into(),
                    version: "0.3.1".into(),
                    source: "https://github.com/antonmarsden/shotglass-kit".into(),
                    tag: "v0.3.1".into(),
                    rev: None,
                    subdir: Some("sounds".into()),
                    sha256: "ab".repeat(32),
                },
                LockEntry {
                    name: "toon".into(),
                    version: "1.2.0".into(),
                    source: "https://github.com/someone/toon-kit".into(),
                    tag: "1.2.0".into(),
                    rev: None,
                    subdir: None,
                    sha256: "cd".repeat(32),
                },
            ],
        };
        let text = l.to_text();
        assert!(text.starts_with("# Written by"), "{text}");
        assert_eq!(Lock::parse(&text).unwrap(), l);
        assert!(Lock::parse("version = 2\n").is_err());
        assert!(Lock::parse("version = 1\n[[library]]\nname = \"x\"\n").is_err());
        let bad_rev = text.replace(&"e".repeat(40), "--upload-pack=x");
        assert!(format!("{:#}", Lock::parse(&bad_rev).unwrap_err()).contains("commit id"));
    }

    #[test]
    fn hash_is_stable_across_line_endings_and_ignores_hidden_files() {
        let a = tmp("hash-a");
        let b = tmp("hash-b");
        write(
            &a,
            &[
                ("library.toml", "name = \"x\"\n"),
                ("fx/n.glsl", "float f();\nfloat g();\n"),
            ],
        );
        write(
            &b,
            &[
                ("library.toml", "name = \"x\"\r\n"),
                ("fx/n.glsl", "float f();\r\nfloat g();\r\n"),
                (".git/HEAD", "ref"),
            ],
        );
        assert_eq!(hash_dir(&a).unwrap(), hash_dir(&b).unwrap());
        assert_eq!(hash_dir(&a).unwrap().len(), 64);
        write(&b, &[("fx/n.glsl", "float f();\n")]);
        assert_ne!(hash_dir(&a).unwrap(), hash_dir(&b).unwrap());
        // renaming a file changes the hash too
        write(&b, &[("fx/n.glsl", "float f();\nfloat g();\n")]);
        assert_eq!(hash_dir(&a).unwrap(), hash_dir(&b).unwrap());
        std::fs::rename(b.join("fx/n.glsl"), b.join("fx/m.glsl")).unwrap();
        assert_ne!(hash_dir(&a).unwrap(), hash_dir(&b).unwrap());
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn cache_paths_are_tidy() {
        let c = Path::new("/c");
        assert_eq!(
            cache_path(c, "https://github.com/a/kit.git/", "0.4.2"),
            Path::new("/c/github.com/a/kit/0.4.2")
        );
        assert_eq!(
            cache_path(c, "https://x.org/../a b", "1.0.0"),
            Path::new("/c/x.org/_/a_b/1.0.0")
        );
    }

    /// tag → (path, content) files.
    type Releases = BTreeMap<String, Vec<(String, String)>>;

    /// In-memory releases: source → tag → files.
    #[derive(Default)]
    struct Fake {
        repos: RefCell<BTreeMap<String, Releases>>,
        /// (source, ref) → commit; commits are stored in `repos` like tags.
        heads: RefCell<BTreeMap<(String, String), String>>,
        downloads: RefCell<usize>,
    }

    impl Fake {
        fn publish(&self, source: &str, tag: &str, files: &[(&str, &str)]) {
            self.repos
                .borrow_mut()
                .entry(source.into())
                .or_default()
                .insert(
                    tag.into(),
                    files
                        .iter()
                        .map(|(a, b)| (a.to_string(), b.to_string()))
                        .collect(),
                );
        }

        /// A new commit on `refname` (`HEAD` or `refs/heads/…`).
        fn push(&self, source: &str, refname: &str, rev: &str, files: &[(&str, &str)]) {
            self.publish(source, rev, files);
            self.heads
                .borrow_mut()
                .insert((source.into(), refname.into()), rev.into());
        }
    }

    impl Fetcher for Fake {
        fn head(&self, source: &str, refname: &str) -> Result<String> {
            self.heads
                .borrow()
                .get(&(source.to_string(), refname.to_string()))
                .cloned()
                .ok_or_else(|| anyhow!("no {refname}"))
        }
        fn download_rev(&self, source: &str, rev: &str, dest: &Path) -> Result<()> {
            self.download(source, rev, dest)
        }
        fn tags(&self, source: &str) -> Result<Vec<String>> {
            Ok(self
                .repos
                .borrow()
                .get(source)
                .ok_or_else(|| anyhow!("no repo {source}"))?
                .keys()
                .cloned()
                .collect())
        }
        fn download(&self, source: &str, tag: &str, dest: &Path) -> Result<()> {
            *self.downloads.borrow_mut() += 1;
            let repos = self.repos.borrow();
            let files = repos
                .get(source)
                .and_then(|r| r.get(tag))
                .ok_or_else(|| anyhow!("no tag"))?;
            for (n, t) in files {
                let p = dest.join(n);
                std::fs::create_dir_all(p.parent().unwrap())?;
                std::fs::write(p, t)?;
            }
            Ok(())
        }
    }

    const KIT: &str = "https://example.com/shotglass-kit";

    fn kit_release(f: &Fake, v: &str, noise: &str) {
        f.publish(
            KIT,
            &format!("v{v}"),
            &[
                ("core/library.toml", &libtoml("shotglass-kit/core", v, "")),
                ("core/noise.glsl", noise),
                (
                    "sounds/library.toml",
                    &libtoml(
                        "shotglass-kit/sounds",
                        v,
                        "[requires]\n\"shotglass-kit/core\" = \">=0.3\"\n",
                    ),
                ),
                ("sounds/bell.glsl", "vec2 bell(float t);\n"),
                ("README.md", "kit"),
            ],
        );
    }

    fn specs(src: &str) -> Vec<LibrarySpec> {
        let t: BTreeMap<String, LibraryDecl> = toml::from_str(src).unwrap();
        t.iter()
            .map(|(n, d)| LibrarySpec::parse(n, d).unwrap())
            .collect()
    }

    #[test]
    fn fetch_locks_resolves_and_updates() {
        let cache = tmp("fetch-cache");
        let film = tmp("fetch-film");
        let f = Fake::default();
        kit_release(&f, "0.3.0", "float n03();\n");
        kit_release(&f, "0.4.0", "float n040();\n");
        let sp = specs(&format!(
            "kit = {{ version = \"0.4\", source = \"{KIT}\", subdir = \"core\" }}\n\
             sfx = {{ version = \"0.3\", source = \"{KIT}\", subdir = \"sounds\" }}\n"
        ));
        let missing = FetchMode::Missing {
            skip: BTreeSet::new(),
        };
        let (lock, log) = fetch(&sp, None, &f, &cache, &missing).unwrap();
        assert_eq!(lock.get("kit").unwrap().version, "0.4.0");
        assert_eq!(lock.get("sfx").unwrap().version, "0.3.0");
        assert_eq!(lock.get("sfx").unwrap().subdir.as_deref(), Some("sounds"));
        assert!(
            log.iter().any(|l| l.contains("kit: 0.4.0 (new)")),
            "{log:?}"
        );
        assert_eq!(*f.downloads.borrow(), 2);

        // Resolving uses only the lock and the cache; local names map to canonical names
        // and subdirs pick the library root.
        let libs = resolve(&film, &sp, &BTreeMap::new(), Some(&lock), &cache).unwrap();
        let kit = libs.iter().find(|l| l.name == "kit").unwrap();
        assert_eq!(kit.canonical, "shotglass-kit/core");
        assert!(
            kit.root.ends_with("example.com/shotglass-kit/0.4.0/core"),
            "{:?}",
            kit.root
        );
        let sfx = libs.iter().find(|l| l.name == "sfx").unwrap();
        assert_eq!(sfx.canonical, "shotglass-kit/sounds");
        assert!(sfx.root.ends_with("0.3.0/sounds"));

        // A new release doesn't move the lock unless asked.
        kit_release(&f, "0.4.1", "float n041();\n");
        let (same, _) = fetch(&sp, Some(&lock), &f, &cache, &missing).unwrap();
        assert_eq!(same, lock);
        assert_eq!(*f.downloads.borrow(), 2, "already cached: no downloads");
        let (newer, log) = fetch(
            &sp,
            Some(&lock),
            &f,
            &cache,
            &FetchMode::Update(vec!["kit".into()]),
        )
        .unwrap();
        assert_eq!(newer.get("kit").unwrap().version, "0.4.1");
        assert_eq!(newer.get("sfx").unwrap(), lock.get("sfx").unwrap());
        assert!(log.iter().any(|l| l.contains("0.4.0 → 0.4.1")), "{log:?}");
        assert!(
            fetch(
                &sp,
                Some(&lock),
                &f,
                &cache,
                &FetchMode::Update(vec!["nope".into()])
            )
            .is_err()
        );

        // Lock written by an older movie.toml (range moved) must be re-resolved, not used.
        let sp5 = specs(&format!(
            "kit = {{ version = \"0.3\", source = \"{KIT}\", subdir = \"core\" }}\n"
        ));
        let e = resolve(&film, &sp5, &BTreeMap::new(), Some(&lock), &cache).unwrap_err();
        assert!(e.to_string().contains("no longer matches"), "{e}");
        let e = resolve(&film, &sp5, &BTreeMap::new(), None, &cache).unwrap_err();
        assert!(e.to_string().contains("shotglass fetch"), "{e}");
        let (l3, _) = fetch(&sp5, Some(&lock), &f, &cache, &missing).unwrap();
        assert_eq!(l3.get("kit").unwrap().version, "0.3.0");
        assert_eq!(
            l3.libraries.len(),
            1,
            "entries for removed libraries are dropped"
        );

        let e = fetch(
            &specs(&format!(
                "kit = {{ version = \"2\", source = \"{KIT}\", subdir = \"core\" }}\n"
            )),
            None,
            &f,
            &cache,
            &missing,
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("no release") && e.contains("0.3.0, 0.4.0, 0.4.1"),
            "{e}"
        );
        let e = fetch(
            &specs(&format!(
                "kit = {{ version = \"0.4\", source = \"{KIT}\" }}\n"
            )),
            None,
            &f,
            &cache,
            &missing,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("does it need a subdir"), "{e}");
        std::fs::remove_dir_all(&cache).ok();
        std::fs::remove_dir_all(&film).ok();
    }

    #[test]
    fn hash_mismatches_are_rejected() {
        let cache = tmp("hash-cache");
        let film = tmp("hash-film");
        let f = Fake::default();
        kit_release(&f, "0.4.0", "float a();\n");
        let sp = specs(&format!(
            "kit = {{ version = \"0.4\", source = \"{KIT}\", subdir = \"core\" }}\n"
        ));
        let missing = FetchMode::Missing {
            skip: BTreeSet::new(),
        };
        let (lock, _) = fetch(&sp, None, &f, &cache, &missing).unwrap();

        // Someone edits the cached copy.
        let noise = cache_path(&cache, KIT, "0.4.0").join("core/noise.glsl");
        std::fs::write(&noise, "float b();\n").unwrap();
        let e = resolve(&film, &sp, &BTreeMap::new(), Some(&lock), &cache).unwrap_err();
        assert!(e.to_string().contains("don't match shotglass.lock"), "{e}");
        let e = fetch(&sp, Some(&lock), &f, &cache, &missing).unwrap_err();
        assert!(e.to_string().contains("cached copy was changed"), "{e}");

        // The tag is moved upstream: a fresh download no longer matches the lock.
        std::fs::remove_dir_all(cache_path(&cache, KIT, "0.4.0")).unwrap();
        kit_release(&f, "0.4.0", "float moved();\n");
        let e = fetch(&sp, Some(&lock), &f, &cache, &missing).unwrap_err();
        assert!(e.to_string().contains("tag was moved"), "{e}");
        // `update` accepts the new files deliberately.
        let (l2, _) = fetch(&sp, Some(&lock), &f, &cache, &FetchMode::Update(vec![])).unwrap();
        assert_ne!(
            l2.get("kit").unwrap().sha256,
            lock.get("kit").unwrap().sha256
        );
        std::fs::remove_dir_all(&cache).ok();
        std::fs::remove_dir_all(&film).ok();
    }

    #[test]
    fn overrides_and_local_paths() {
        let film = tmp("ovr-film");
        let dev = tmp("ovr-dev");
        write(
            &film,
            &[("lib/mine/library.toml", &libtoml("me/mine", "0.1.0", ""))],
        );
        write(
            &dev,
            &[(
                "library.toml",
                &libtoml("shotglass-kit/core", "0.5.0-dev", ""),
            )],
        );
        let sp = specs(&format!(
            "kit = {{ version = \"0.4\", source = \"{KIT}\", subdir = \"core\" }}\nmine = {{ path = \"lib/mine\" }}\n"
        ));
        let none = |_: &str| None;
        // Not fetched, not overridden → asks for a fetch.
        assert!(
            resolve(
                &film,
                &sp,
                &overrides(&film, &sp, &none).unwrap(),
                None,
                &film
            )
            .is_err()
        );

        // movie.local.toml points kit at a working copy (absolute paths are fine there).
        write(
            &film,
            &[(
                LOCAL_FILE,
                &format!("[libraries.kit]\npath = '{}'\n", dev.display()),
            )],
        );
        let o = overrides(&film, &sp, &none).unwrap();
        assert_eq!(o["kit"].1, Origin::Local);
        // ...but the version must still be in range.
        let e = resolve(&film, &sp, &o, None, &film)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("asks for version ^0.4") && e.contains("0.5.0-dev"),
            "{e}"
        );
        write(
            &dev,
            &[("library.toml", &libtoml("shotglass-kit/core", "0.4.9", ""))],
        );
        let libs = resolve(&film, &sp, &o, None, &film).unwrap();
        assert_eq!(libs[0].root, dev);
        assert_eq!(libs[1].root, film.join("lib/mine"));
        assert_eq!(libs[1].origin, Origin::Path);

        // The environment beats movie.local.toml; relative to the film folder.
        let env = |k: &str| (k == "SHOTGLASS_LIB_KIT").then(|| "lib/mine".to_string());
        let o = overrides(&film, &sp, &env).unwrap();
        assert_eq!(
            o["kit"],
            (
                film.join("lib/mine"),
                Origin::Env("SHOTGLASS_LIB_KIT".into())
            )
        );
        assert_eq!(env_var("my-lib"), "SHOTGLASS_LIB_MY_LIB");

        write(&film, &[(LOCAL_FILE, "[libraries.other]\npath = 'x'\n")]);
        let e = overrides(&film, &sp, &none).unwrap_err().to_string();
        assert!(e.contains("no such library"), "{e}");
        write(&film, &[(LOCAL_FILE, "[libraries.kit]\npth = 'x'\n")]);
        assert!(overrides(&film, &sp, &none).is_err());

        // Missing library.toml is a clear error.
        let sp2 = specs("bare = { path = \"lib\" }\n");
        let e = format!(
            "{:#}",
            resolve(&film, &sp2, &BTreeMap::new(), None, &film).unwrap_err()
        );
        assert!(e.contains("needs a library.toml"), "{e}");
        std::fs::remove_dir_all(&film).ok();
        std::fs::remove_dir_all(&dev).ok();
    }

    #[test]
    fn a_source_without_version_follows_a_branch() {
        let cache = tmp("branch-cache");
        let film = tmp("branch-film");
        let f = Fake::default();
        let (c1, c2) = ("1".repeat(40), "2".repeat(40));
        let core = |v: &str, body: &str| {
            vec![
                (
                    "core/library.toml".to_string(),
                    libtoml("shotglass-kit/core", v, ""),
                ),
                ("core/noise.glsl".to_string(), body.to_string()),
            ]
        };
        let push = |r: &str, c: &str, files: Vec<(String, String)>| {
            let v: Vec<(&str, &str)> = files
                .iter()
                .map(|(a, b)| (a.as_str(), b.as_str()))
                .collect();
            f.push(KIT, r, c, &v);
        };
        push("HEAD", &c1, core("0.1.0", "float n1();\n"));
        let sp = specs(&format!(
            "kit = {{ source = \"{KIT}\", subdir = \"core\" }}\n"
        ));
        let missing = FetchMode::Missing {
            skip: BTreeSet::new(),
        };
        let (lock, log) = fetch(&sp, None, &f, &cache, &missing).unwrap();
        let e = lock.get("kit").unwrap();
        assert_eq!(e.rev.as_deref(), Some(c1.as_str()));
        assert_eq!((e.tag.as_str(), e.version.as_str()), ("HEAD", "0.1.0"));
        assert!(
            log.iter().any(|l| l.contains("HEAD@1111111 (0.1.0) (new)")),
            "{log:?}"
        );
        let text = lock.to_text();
        assert!(text.contains(&format!("rev = \"{c1}\"")), "{text}");
        assert_eq!(Lock::parse(&text).unwrap(), lock);

        let libs = resolve(&film, &sp, &BTreeMap::new(), Some(&lock), &cache).unwrap();
        assert!(libs[0].root.ends_with(format!("shotglass-kit/{c1}/core")));

        // New commits don't move the pin until `update`, which takes the newest one.
        push("HEAD", &c2, core("0.2.0", "float n2();\n"));
        let (same, _) = fetch(&sp, Some(&lock), &f, &cache, &missing).unwrap();
        assert_eq!(same, lock);
        let (newer, log) = fetch(&sp, Some(&lock), &f, &cache, &FetchMode::Update(vec![])).unwrap();
        assert_eq!(newer.get("kit").unwrap().rev.as_deref(), Some(c2.as_str()));
        assert!(
            log.iter()
                .any(|l| l.contains("HEAD@1111111 (0.1.0) → HEAD@2222222 (0.2.0)")),
            "{log:?}"
        );

        // Naming a branch, or adding a version, no longer fits the pin.
        let named = specs(&format!(
            "kit = {{ source = \"{KIT}\", subdir = \"core\", branch = \"next\" }}\n"
        ));
        let e = format!(
            "{:#}",
            resolve(&film, &named, &BTreeMap::new(), Some(&lock), &cache).unwrap_err()
        );
        assert!(e.contains("shotglass update"), "{e}");
        let versioned = specs(&format!(
            "kit = {{ version = \"0.1\", source = \"{KIT}\", subdir = \"core\" }}\n"
        ));
        assert!(resolve(&film, &versioned, &BTreeMap::new(), Some(&lock), &cache).is_err());
        push("refs/heads/next", &c2, core("0.2.0", "float n2();\n"));
        let (l2, _) = fetch(&named, Some(&lock), &f, &cache, &missing).unwrap();
        assert_eq!(l2.get("kit").unwrap().tag, "refs/heads/next");
        assert!(l2.get("kit").unwrap().pin().starts_with("next@2222222"));

        // A fresh machine downloads the pinned commit, not the branch's newest.
        let cache2 = tmp("branch-cache2");
        let n = *f.downloads.borrow();
        let (again, log) = fetch(&sp, Some(&lock), &f, &cache2, &missing).unwrap();
        assert_eq!(again, lock);
        assert_eq!(*f.downloads.borrow(), n + 1);
        assert!(
            log.iter()
                .any(|l| l.contains("1111111") && l.contains("downloaded")),
            "{log:?}"
        );
        for d in [cache, cache2, film] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[test]
    fn git_fetcher_reads_tags_and_downloads() {
        let git = |dir: &Path, args: &[&str]| {
            std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "tag.gpgsign=false",
                ])
                .args(args)
                .current_dir(dir)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        let repo = tmp("git-repo");
        if !git(&repo, &["init", "-q"]) {
            eprintln!("git not available; skipping");
            return;
        }
        write(
            &repo,
            &[
                ("core/library.toml", &libtoml("a/core", "1.0.0", "")),
                ("core/x.glsl", "float x();\n"),
            ],
        );
        assert!(git(&repo, &["add", "."]));
        assert!(git(&repo, &["commit", "-q", "-m", "one"]));
        assert!(git(&repo, &["tag", "v1.0.0"]));
        // file:///tmp/x on Unix, file:///C:/x on Windows
        let p = repo.to_string_lossy().replace('\\', "/");
        let url = format!("file://{}{p}", if p.starts_with('/') { "" } else { "/" });
        let tags = Git.tags(&url).unwrap();
        assert_eq!(tags, vec!["v1.0.0"]);
        let dest = tmp("git-dest").join("out");
        Git.download(&url, "v1.0.0", &dest).unwrap();
        assert!(dest.join("core/x.glsl").is_file());
        assert!(!dest.join(".git").exists());
        assert_eq!(
            hash_dir(&dest.join("core")).unwrap(),
            hash_dir(&repo.join("core")).unwrap()
        );

        // Following a branch: the pinned commit stays downloadable after the branch moves.
        let first = Git.head(&url, "HEAD").unwrap();
        assert!(is_commit_id(&first), "{first}");
        write(&repo, &[("core/x.glsl", "float x2();\n")]);
        assert!(git(&repo, &["commit", "-q", "-a", "-m", "two"]));
        let second = Git.head(&url, "HEAD").unwrap();
        assert_ne!(first, second);
        assert!(Git.head(&url, "refs/heads/nope").is_err());
        let old = tmp("git-dest-rev").join("out");
        Git.download_rev(&url, &first, &old).unwrap();
        assert_eq!(
            std::fs::read_to_string(old.join("core/x.glsl")).unwrap(),
            "float x();\n"
        );
        assert!(!old.join(".git").exists());
        assert!(Git.download_rev(&url, "--upload-pack=x", &old).is_err());

        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(dest.parent().unwrap()).ok();
        std::fs::remove_dir_all(old.parent().unwrap()).ok();
    }
}
