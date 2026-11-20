//! Building GLSL sources: Shadertoy-style headers, `#include`, and mapping driver
//! errors back to file:line.
//!
//! Every file gets its own GLSL "source string number" through `#line 1 N`, so a
//! driver message like `3(42) : error` or `0:42(7): error` tells us the file (N) and
//! the line directly.

use crate::library::Library;
use anyhow::{Result, anyhow, bail};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The GLSL language a shader is written in: the `#version` line the harness emits.
///
/// Films get a fixed baseline whatever the driver offers (`330 core` on desktop,
/// `300 es` on OpenGL ES / the web), so every machine compiles the same language.
/// A film may opt in to a later desktop version with `glsl = "410"` or `"430"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Dialect {
    pub es: bool,
    pub version: u32,
}

/// Desktop GLSL versions a film may ask for in `movie.toml` (`glsl = "..."`).
pub const DESKTOP_VERSIONS: &[u32] = &[330, 410, 430];

impl Dialect {
    /// The portable desktop baseline: OpenGL 3.3 core.
    pub const BASELINE: Dialect = Dialect {
        es: false,
        version: 330,
    };
    /// GLSL ES 3.00: OpenGL ES 3.0 and WebGL 2.
    pub const WEB: Dialect = Dialect {
        es: true,
        version: 300,
    };

    /// Desktop GLSL at `version` (`None` = the baseline).
    pub fn desktop(version: Option<u32>) -> Dialect {
        Dialect {
            es: false,
            version: version.unwrap_or(Self::BASELINE.version),
        }
    }

    pub fn version_line(&self) -> String {
        if self.es {
            "#version 300 es\nprecision highp float;\nprecision highp int;\nprecision highp sampler2D;\n".into()
        } else {
            format!("#version {} core\n", self.version)
        }
    }
}

impl std::fmt::Display for Dialect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.es {
            write!(
                f,
                "GLSL ES {}.{:02}",
                self.version / 100,
                self.version % 100
            )
        } else {
            write!(f, "GLSL {} core", self.version)
        }
    }
}

/// What the driver can compile, from GL_SHADING_LANGUAGE_VERSION / GL_VERSION.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriverGlsl {
    pub es: bool,
    /// Highest GLSL version the driver accepts, e.g. 460 for "4.60 NVIDIA".
    pub max: u32,
}

impl DriverGlsl {
    pub fn detect(glsl_version: &str, gl_version: &str) -> DriverGlsl {
        let es = gl_version.contains("OpenGL ES") || glsl_version.contains("OpenGL ES");
        let num: String = glsl_version
            .split_whitespace()
            .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .unwrap_or(if es { "3.00" } else { "3.30" })
            .chars()
            .filter(|c| c.is_ascii_digit())
            .take(3)
            .collect();
        let max = num.parse().unwrap_or(if es { 300 } else { 330 });
        DriverGlsl { es, max }
    }

    /// The dialect a film compiles in on this driver: the baseline, or the film's
    /// opt-in (`glsl` in movie.toml). Refuses an opt-in the driver can't provide.
    pub fn dialect(&self, opt_in: Option<u32>) -> std::result::Result<Dialect, String> {
        match (self.es, opt_in) {
            (true, None) => Ok(Dialect::WEB),
            (true, Some(v)) => Err(format!(
                "this film asks for desktop GLSL {v} (glsl = \"{v}\" in movie.toml), but this \
                 driver is OpenGL ES and only runs GLSL ES 3.00; remove the glsl line to use \
                 the portable baseline"
            )),
            (false, v) => {
                let d = Dialect::desktop(v);
                if self.max < d.version {
                    Err(format!(
                        "this film asks for GLSL {} (glsl = \"{}\" in movie.toml), but this driver \
                         only goes up to GLSL {}; remove the glsl line to use the portable \
                         baseline (330)",
                        d.version, d.version, self.max
                    ))
                } else {
                    Ok(d)
                }
            }
        }
    }
}

/// A fully assembled shader plus the names of the files it came from.
#[derive(Clone, Debug)]
pub struct Assembled {
    pub text: String,
    /// The language `text` declares in its `#version` line.
    pub dialect: Dialect,
    /// Index = GLSL source string number.
    pub files: Vec<String>,
    /// Portability problems that didn't stop the build (e.g. an include that only
    /// matches a file when ignoring case).
    pub warnings: Vec<String>,
}

/// Which tree a file belongs to: the film, or one of its libraries (index into `libs`).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Scope {
    Film,
    Lib(usize),
}

pub struct Builder {
    text: String,
    dialect: Dialect,
    files: Vec<String>,
    root: PathBuf,
    libs: Vec<Library>,
    /// Absolute paths already in this shader: each file is included at most once.
    included: HashSet<PathBuf>,
    warnings: Vec<String>,
}

impl Builder {
    /// `header` is generated code (uniform declarations, defines). It is source string 0.
    pub fn new(dialect: Dialect, header: &str, root: &Path) -> Builder {
        let mut text = dialect.version_line();
        text += header;
        if !text.ends_with('\n') {
            text.push('\n');
        }
        Builder {
            text,
            dialect,
            files: vec!["<shotglass header>".into()],
            root: root.to_path_buf(),
            libs: vec![],
            included: HashSet::new(),
            warnings: vec![],
        }
    }

    /// The film's libraries: `#include <name/file.glsl>` resolves through their local names.
    pub fn libraries(mut self, libs: &[Library]) -> Builder {
        self.libs = libs.to_vec();
        self
    }

    pub fn file(mut self, path: &Path) -> Result<Builder> {
        self.push_file(path, Scope::Film, 0)?;
        Ok(self)
    }

    pub fn builtin(mut self, name: &str, src: &str) -> Result<Builder> {
        let idx = self.new_string(format!("<builtin {name}>"));
        self.push_text(src, idx, None, Scope::Film, 0)?;
        Ok(self)
    }

    /// Generated code that follows the user's code (e.g. `main()`).
    pub fn footer(mut self, src: &str) -> Builder {
        let idx = self.new_string("<shotglass footer>".into());
        self.text += &format!("#line 1 {idx}\n{src}\n");
        self
    }

    pub fn build(self) -> Assembled {
        Assembled {
            text: self.text,
            dialect: self.dialect,
            files: self.files,
            warnings: self.warnings,
        }
    }

    fn new_string(&mut self, name: String) -> usize {
        self.files.push(name);
        self.files.len() - 1
    }

    fn scope_root(&self, scope: Scope) -> &Path {
        match scope {
            Scope::Film => &self.root,
            Scope::Lib(i) => &self.libs[i].root,
        }
    }

    /// `scenes/a.glsl` for film files, `kit/noise.glsl` (local name) for library files.
    fn display(&self, path: &Path, scope: Scope) -> String {
        match scope {
            Scope::Film => display_path(path, &self.root),
            Scope::Lib(i) => format!(
                "{}/{}",
                self.libs[i].name,
                display_path(path, &self.libs[i].root)
            ),
        }
    }

    fn push_file(&mut self, path: &Path, scope: Scope, depth: usize) -> Result<()> {
        if depth > 16 {
            bail!("#include nested too deeply at {}", path.display());
        }
        self.included.insert(absolute(path));
        let src = std::fs::read_to_string(path).map_err(|e| anyhow!("{}: {e}", path.display()))?;
        let idx = self.new_string(self.display(path, scope));
        self.push_text(&src, idx, Some(path), scope, depth)
    }

    /// Where `#include` (`<…>` if `angle`) from a file in `scope` in folder `here` points.
    /// Err = the message for the include line.
    fn resolve(
        &mut self,
        name: &str,
        angle: bool,
        here: &Path,
        scope: Scope,
    ) -> std::result::Result<(PathBuf, Scope), String> {
        if name.contains('\\') || name.starts_with('/') || name.contains(':') {
            return Err(format!(
                "#include \"{name}\": use a relative path with forward slashes"
            ));
        }
        // Library prefixes: local names in the film, canonical names inside libraries.
        let lib = match scope {
            Scope::Film => name.split_once('/').and_then(|(first, rest)| {
                let i = self.libs.iter().position(|l| l.name == first)?;
                Some((i, rest))
            }),
            Scope::Lib(_) => self
                .libs
                .iter()
                .enumerate()
                .filter_map(|(i, l)| {
                    let rest = name.strip_prefix(&l.canonical)?.strip_prefix('/')?;
                    Some((i, rest, l.canonical.len()))
                })
                .max_by_key(|x| x.2)
                .map(|(i, rest, _)| (i, rest)),
        };
        let lib_candidate =
            lib.map(|(i, rest)| (self.libs[i].root.join(rest), Scope::Lib(i), rest));
        let mut candidates: Vec<(PathBuf, Scope, String)> = vec![];
        if let (true, Some((p, s, rest))) = (angle, &lib_candidate) {
            candidates.push((p.clone(), *s, rest.to_string()));
        } else {
            let root = self.scope_root(scope).to_path_buf();
            candidates.push((here.join(name), scope, name.to_string()));
            if normalize(here) != normalize(&root) {
                candidates.push((root.join(name), scope, name.to_string()));
            }
            if let Some((p, s, rest)) = &lib_candidate {
                candidates.push((p.clone(), *s, rest.to_string()));
            }
        }
        let mut searched = vec![];
        let mut hints = vec![];
        for (p, s, _) in &candidates {
            let p = normalize(p);
            let root = self.scope_root(*s);
            let label = match s {
                Scope::Film => display_path(&p, root),
                Scope::Lib(_) => format!("{} ({})", self.display(&p, *s), p.display()),
            };
            if !absolute(&p).starts_with(absolute(root)) {
                searched.push(format!(
                    "{label} (outside the {}, not allowed)",
                    match s {
                        Scope::Film => "film folder".to_string(),
                        Scope::Lib(i) => format!("library \"{}\"", self.libs[*i].name),
                    }
                ));
                continue;
            }
            let rel = display_path(&p, root);
            if p.is_file() {
                if let Some(actual) = crate::library::case_mismatch(root, &rel) {
                    self.warnings.push(format!(
                        "#include \"{name}\" only matches {actual} when ignoring case; \
                         it won't be found on Linux or the web"
                    ));
                }
                return Ok((p, *s));
            }
            if let Some(actual) = crate::library::case_mismatch(root, &rel) {
                hints.push(format!("{actual} exists, but differs in case"));
            }
            searched.push(label);
        }
        let mut msg = format!(
            "can't find #include {}; searched:",
            if angle {
                format!("<{name}>")
            } else {
                format!("\"{name}\"")
            }
        );
        for s in &searched {
            msg += &format!("\n    {s}");
        }
        for h in &hints {
            msg += &format!("\n    ({h})");
        }
        if scope != Scope::Film && angle && lib.is_none() {
            msg += "\n    (a library includes other libraries by canonical name, e.g. \
                    <shotglass-kit/core/noise.glsl>, and the film must list them in [libraries])";
        } else if scope == Scope::Film && angle && lib.is_none() && name.contains('/') {
            let names: Vec<&str> = self.libs.iter().map(|l| l.name.as_str()).collect();
            msg += &format!(
                "\n    (no library called \"{}\" in [libraries]{})",
                name.split('/').next().unwrap_or(""),
                if names.is_empty() {
                    String::new()
                } else {
                    format!("; the film has {}", names.join(", "))
                }
            );
        }
        Err(msg)
    }

    fn push_text(
        &mut self,
        src: &str,
        idx: usize,
        path: Option<&Path>,
        scope: Scope,
        depth: usize,
    ) -> Result<()> {
        self.text += &format!("#line 1 {idx}\n");
        for (n, line) in src.lines().enumerate() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("#include") {
                let rest = rest.trim();
                let angle = rest.starts_with('<');
                let name = rest.trim_matches(|c| c == '"' || c == '<' || c == '>');
                let here = path
                    .and_then(Path::parent)
                    .unwrap_or(self.scope_root(scope))
                    .to_path_buf();
                let (inc, inc_scope) = self
                    .resolve(name, angle, &here, scope)
                    .map_err(|m| anyhow!("{}:{}: {m}", self.files[idx], n + 1))?;
                if self.included.contains(&absolute(&inc)) {
                    // Include-once: already in this shader. Keep the line count.
                    self.text.push('\n');
                    continue;
                }
                self.push_file(&inc, inc_scope, depth + 1)?;
                // Resume numbering in this file on the line after the include.
                self.text += &format!("#line {} {idx}\n", n + 2);
                continue;
            }
            if trimmed.starts_with("#version") {
                // The harness supplies the version line; keep line numbers aligned.
                self.text.push('\n');
                continue;
            }
            self.text += line;
            self.text.push('\n');
        }
        Ok(())
    }
}

/// Normalised absolute path, for include-once and "stays inside its root" checks.
fn absolute(p: &Path) -> PathBuf {
    let p = if p.as_os_str().is_empty() {
        Path::new(".")
    } else {
        p
    };
    normalize(&std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()))
}

/// Resolve `.` and `..` without touching the file system (canonicalize adds \\?\ on Windows).
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

fn display_path(p: &Path, root: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Turn a raw driver log into "file:line: message" lines.
pub fn remap_log(log: &str, files: &[String]) -> Vec<String> {
    log.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| match parse_location(l) {
            Some((file, line, msg)) => {
                let name = files.get(file).map(String::as_str).unwrap_or("?");
                format!("{name}:{line}: {msg}")
            }
            None => l.to_string(),
        })
        .collect()
}

/// Understands the common driver formats:
///   NVIDIA      `0(12) : error C1008: ...`
///   Mesa        `0:12(5): error: ...`
///   AMD/Intel   `ERROR: 0:12: '...' : ...`
fn parse_location(line: &str) -> Option<(usize, usize, String)> {
    let mut s = line;
    let mut severity = "";
    for p in ["ERROR: ", "WARNING: ", "error: ", "warning: "] {
        if let Some(r) = s.strip_prefix(p) {
            severity = if p.to_ascii_lowercase().starts_with("error") {
                "error: "
            } else {
                "warning: "
            };
            s = r;
        }
    }
    let file_end = s.find(|c: char| !c.is_ascii_digit())?;
    if file_end == 0 {
        return None;
    }
    let file: usize = s[..file_end].parse().ok()?;
    let rest = &s[file_end..];
    let (line_no, rest) = if let Some(r) = rest.strip_prefix('(') {
        let end = r.find(')')?;
        (r[..end].parse().ok()?, &r[end + 1..])
    } else {
        let r = rest.strip_prefix(':')?;
        let end = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
        if end == 0 {
            return None;
        }
        let n = r[..end].parse().ok()?;
        let mut r = &r[end..];
        // Mesa adds a column: 0:12(5)
        if let Some(c) = r.strip_prefix('(')
            && let Some(e) = c.find(')')
        {
            r = &c[e + 1..];
        }
        (n, r)
    };
    let msg = rest.trim_start_matches([':', ' ']).trim();
    Some((file, line_no, format!("{severity}{msg}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_driver_formats() {
        let files = vec![
            "<h>".to_string(),
            "scenes/a.glsl".to_string(),
            "common.glsl".to_string(),
        ];
        assert_eq!(
            remap_log("0:12(5): error: `foo' undeclared", &files),
            vec!["<h>:12: error: `foo' undeclared"]
        );
        assert_eq!(
            remap_log("1(7) : error C1008: undefined variable \"x\"", &files),
            vec!["scenes/a.glsl:7: error C1008: undefined variable \"x\""]
        );
        assert_eq!(
            remap_log("ERROR: 2:3: 'y' : undeclared identifier", &files),
            vec!["common.glsl:3: error: 'y' : undeclared identifier"]
        );
        assert_eq!(
            remap_log("some other text", &files),
            vec!["some other text"]
        );
    }

    #[test]
    fn includes_get_their_own_source_numbers() {
        let dir = std::env::temp_dir().join(format!("shotglass-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("scenes")).unwrap();
        std::fs::write(
            dir.join("common.glsl"),
            "float common_fn(){ return 1.0; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("scenes/a.glsl"),
            "#include \"../common.glsl\"\nvoid f(){}\n",
        )
        .unwrap();
        let a = Builder::new(Dialect::BASELINE, "uniform float iTime;", &dir)
            .file(&dir.join("scenes/a.glsl"))
            .unwrap()
            .build();
        assert_eq!(
            a.files,
            vec!["<shotglass header>", "scenes/a.glsl", "common.glsl"]
        );
        assert!(a.text.contains("#line 1 1\n#line 1 2\nfloat common_fn"));
        assert!(a.text.contains("#line 2 1\nvoid f(){}"));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("shotglass-inc-{name}-{}", std::process::id()));
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

    const D: Dialect = Dialect {
        es: false,
        version: 330,
    };

    /// A film with two libraries: `kit` (canonical shotglass-kit/core) and `toon`
    /// (someone/toon), which includes the kit by its canonical name.
    fn film(name: &str) -> (PathBuf, Vec<Library>) {
        use crate::library::Origin;
        let film = tmp(name);
        let kit = film.join("vendor/kit-core");
        let toon = tmp(&format!("{name}-toon"));
        write(
            &kit,
            &[
                (
                    "library.toml",
                    "name = \"shotglass-kit/core\"\nversion = \"0.4.0\"\n",
                ),
                (
                    "noise.glsl",
                    "#include \"hash.glsl\"\nfloat kit_noise(){ return kit_hash(); }\n",
                ),
                ("hash.glsl", "float kit_hash(){ return 0.5; }\n"),
            ],
        );
        write(
            &toon,
            &[
                (
                    "library.toml",
                    "name = \"someone/toon\"\nversion = \"1.2.0\"\n",
                ),
                (
                    "fx/outline.glsl",
                    "#include <shotglass-kit/core/noise.glsl>\nfloat toon_outline(){ return kit_noise(); }\n",
                ),
            ],
        );
        let libs = vec![
            Library::read("kit", &kit, Origin::Path).unwrap(),
            Library::read("toon", &toon, Origin::Path).unwrap(),
        ];
        (film, libs)
    }

    #[test]
    fn library_includes_use_local_and_canonical_names() {
        let (dir, libs) = film("names");
        write(
            &dir,
            &[(
                "scenes/a.glsl",
                "#include <toon/fx/outline.glsl>\n#include <kit/noise.glsl>\n#include \"kit/hash.glsl\"\nvoid f(){}\n",
            )],
        );
        let a = Builder::new(D, "", &dir)
            .libraries(&libs)
            .file(&dir.join("scenes/a.glsl"))
            .unwrap()
            .build();
        // Library files show their local name; each file appears once (include-once),
        // even when reached by a canonical name and a local name.
        assert_eq!(
            a.files,
            vec![
                "<shotglass header>",
                "scenes/a.glsl",
                "toon/fx/outline.glsl",
                "kit/noise.glsl",
                "kit/hash.glsl",
            ]
        );
        assert_eq!(a.text.matches("float kit_hash()").count(), 1);
        // Line numbers in a.glsl stay right after skipped includes.
        assert!(a.text.contains("#line 2 1\n\n\nvoid f(){}"), "{}", a.text);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&libs[1].root).ok();
    }

    #[test]
    fn include_once_per_shader() {
        let dir = tmp("once");
        write(
            &dir,
            &[
                ("common.glsl", "float c(){ return 1.0; }\n"),
                (
                    "sfx/x.glsl",
                    "#include \"../common.glsl\"\nfloat x(){ return c(); }\n",
                ),
                (
                    "s.glsl",
                    "#include \"common.glsl\"\n#include \"sfx/x.glsl\"\n#include \"./common.glsl\"\nvoid f(){}\n",
                ),
            ],
        );
        let a = Builder::new(D, "", &dir)
            .file(&dir.join("s.glsl"))
            .unwrap()
            .build();
        assert_eq!(a.text.matches("float c()").count(), 1);
        assert_eq!(
            a.files,
            vec!["<shotglass header>", "s.glsl", "common.glsl", "sfx/x.glsl"]
        );
        // A second shader gets its own copy.
        let b = Builder::new(D, "", &dir)
            .file(&dir.join("sfx/x.glsl"))
            .unwrap()
            .build();
        assert_eq!(b.text.matches("float c()").count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn include_error(dir: &Path, libs: &[Library], src: &str) -> String {
        write(dir, &[("scenes/e.glsl", src)]);
        let e = Builder::new(D, "", dir)
            .libraries(libs)
            .file(&dir.join("scenes/e.glsl"))
            .err()
            .expect("should fail");
        format!("{e:#}")
    }

    #[test]
    fn missing_includes_say_where_they_looked() {
        let (dir, libs) = film("missing");
        let e = include_error(&dir, &libs, "// x\n#include <kit/nosie.glsl>\n");
        assert!(
            e.starts_with("scenes/e.glsl:2: can't find #include <kit/nosie.glsl>; searched:"),
            "{e}"
        );
        assert!(e.contains("kit/nosie.glsl ("), "{e}");

        let e = include_error(&dir, &libs, "#include \"helpers.glsl\"\n");
        assert!(
            e.contains("scenes/helpers.glsl") && e.contains("\n    helpers.glsl"),
            "{e}"
        );

        let e = include_error(&dir, &libs, "#include <kat/noise.glsl>\n");
        assert!(
            e.contains("no library called \"kat\"") && e.contains("kit, toon"),
            "{e}"
        );

        // The film uses local names, not canonical ones.
        let e = include_error(&dir, &libs, "#include <shotglass-kit/core/noise.glsl>\n");
        assert!(e.contains("no library called \"shotglass-kit\""), "{e}");

        // Library files can't reach out of their library, nor film files out of the film.
        let e = include_error(&dir, &libs, "#include <kit/../../../etc/passwd>\n");
        assert!(e.contains("outside the library \"kit\""), "{e}");
        let e = include_error(&dir, &libs, "#include \"../../outside.glsl\"\n");
        assert!(e.contains("outside the film folder"), "{e}");
        let e = include_error(&dir, &libs, "#include \"C:\\\\x.glsl\"\n");
        assert!(e.contains("forward slashes"), "{e}");

        // Errors inside a library point at the library file by its local name.
        write(
            &libs[0].root,
            &[("broken.glsl", "\n#include \"gone.glsl\"\n")],
        );
        let e = include_error(&dir, &libs, "#include <kit/broken.glsl>\n");
        assert!(
            e.starts_with("kit/broken.glsl:2: can't find #include \"gone.glsl\""),
            "{e}"
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&libs[1].root).ok();
    }

    #[test]
    fn case_only_include_matches_are_reported() {
        let dir = tmp("case");
        write(
            &dir,
            &[
                ("Common.glsl", "float c(){ return 1.0; }\n"),
                ("s.glsl", "#include \"common.glsl\"\n"),
            ],
        );
        match Builder::new(D, "", &dir).file(&dir.join("s.glsl")) {
            // case-sensitive file system: not found, with a hint
            Err(e) => {
                let e = format!("{e:#}");
                assert!(e.contains("Common.glsl exists, but differs in case"), "{e}");
            }
            // case-insensitive: found, with a warning
            Ok(b) => {
                let a = b.build();
                assert!(
                    a.warnings[0].contains("only matches Common.glsl"),
                    "{:?}",
                    a.warnings
                );
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detects_driver() {
        assert_eq!(
            DriverGlsl::detect("4.60 NVIDIA", "4.6.0 NVIDIA"),
            DriverGlsl {
                es: false,
                max: 460
            }
        );
        assert_eq!(
            DriverGlsl::detect("4.50", "4.5 (Core Profile) Mesa").max,
            450
        );
        assert_eq!(DriverGlsl::detect("4.10", "4.1 Metal").max, 410);
        assert!(DriverGlsl::detect("OpenGL ES GLSL ES 3.00", "OpenGL ES 3.0").es);
    }

    #[test]
    fn baseline_whatever_the_driver_offers() {
        for (glsl, gl) in [
            ("4.60 NVIDIA", "4.6.0 NVIDIA"),
            ("4.10", "4.1 Metal"),
            ("3.30", "3.3 Mesa"),
        ] {
            let d = DriverGlsl::detect(glsl, gl).dialect(None).unwrap();
            assert_eq!(d, Dialect::BASELINE);
            assert!(d.version_line().starts_with("#version 330 core\n"));
        }
        let es = DriverGlsl::detect("OpenGL ES GLSL ES 3.20", "OpenGL ES 3.2");
        assert_eq!(es.dialect(None).unwrap(), Dialect::WEB);
        assert!(Dialect::WEB.version_line().starts_with("#version 300 es\n"));
    }

    #[test]
    fn opt_in_needs_a_driver_that_provides_it() {
        let nv = DriverGlsl::detect("4.60 NVIDIA", "4.6.0 NVIDIA");
        assert_eq!(nv.dialect(Some(430)).unwrap(), Dialect::desktop(Some(430)));
        assert_eq!(nv.dialect(Some(410)).unwrap().version, 410);
        let mac = DriverGlsl::detect("4.10", "4.1 Metal");
        assert_eq!(mac.dialect(Some(410)).unwrap().version, 410);
        let e = mac.dialect(Some(430)).unwrap_err();
        assert!(
            e.contains("GLSL 430") && e.contains("up to GLSL 410"),
            "{e}"
        );
        let arm = DriverGlsl::detect("3.30", "3.3 compatibility pack");
        assert!(arm.dialect(Some(410)).is_err());
        let es = DriverGlsl::detect("OpenGL ES GLSL ES 3.20", "OpenGL ES 3.2");
        assert!(es.dialect(Some(410)).unwrap_err().contains("OpenGL ES"));
    }
}
