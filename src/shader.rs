//! Building GLSL sources: Shadertoy-style headers, `#include`, and mapping driver
//! errors back to file:line.
//!
//! Every file gets its own GLSL "source string number" through `#line 1 N`, so a
//! driver message like `3(42) : error` or `0:42(7): error` tells us the file (N) and
//! the line directly.

use crate::library::Library;
use anyhow::{Result, anyhow, bail};
use std::collections::{HashMap, HashSet};
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
    /// Highest desktop GLSL version declared by an included library.
    pub library_glsl: Option<u32>,
    /// Index = GLSL source string number.
    pub files: Vec<String>,
    /// Portability problems that didn't stop the build (e.g. an include that only
    /// matches a file when ignoring case).
    pub warnings: Vec<String>,
    /// Functions or structs defined in more than one file (e.g. by two libraries). Only
    /// reported when the shader fails to compile (see [`Assembled::explain`]), because
    /// preprocessor tricks can make a duplicate harmless.
    pub clashes: Vec<String>,
}

impl Assembled {
    /// Compile errors, led by any definition clashes: "both `toon` and `kit` define
    /// `hash21(vec2)`" says more than the driver's "redefinition" at one of the two places.
    pub fn explain(&self, errors: Vec<String>) -> Vec<String> {
        self.clashes.iter().cloned().chain(errors).collect()
    }
}

/// A top-level function or struct defined by one file of a shader.
#[derive(Clone, Debug)]
struct Definition {
    /// `hash21(vec2)` (name and parameter types: overloads don't clash) or `struct Ray`.
    key: String,
    /// Source string number (index into `files`).
    file: usize,
    line: usize,
    scope: Scope,
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
    library_glsl: Option<u32>,
    /// Absolute paths already in this shader: each file is included at most once.
    included: HashSet<PathBuf>,
    warnings: Vec<String>,
    defs: Vec<Definition>,
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
            library_glsl: None,
            included: HashSet::new(),
            warnings: vec![],
            defs: vec![],
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

    /// A file of the film whose text the caller already has (e.g. rewritten): includes
    /// resolve from its folder, and messages name it like [`Builder::file`] would.
    pub fn file_text(mut self, path: &Path, src: &str) -> Result<Builder> {
        self.included.insert(absolute(path));
        let idx = self.new_string(self.display(path, Scope::Film));
        self.push_text(src, idx, Some(path), Scope::Film, 0)?;
        Ok(self)
    }

    pub fn builtin(self, name: &str, src: &str) -> Result<Builder> {
        self.generated(&format!("<builtin {name}>"), src)
    }

    /// Film-level code the harness writes (e.g. the `#include` for a library transition),
    /// shown as `label` in messages.
    pub fn generated(mut self, label: &str, src: &str) -> Result<Builder> {
        let idx = self.new_string(label.to_string());
        self.push_text(src, idx, None, Scope::Film, 0)?;
        Ok(self)
    }

    /// A pass named by `kind`, as films spell transitions, layer passes, blends and
    /// metrics: a film file (`fx/iris.glsl`, relative to `root`) or a library's
    /// (`transitions/iris` → `#include <transitions/iris.glsl>`, shown as
    /// `<what> "<kind>"`). A bare name is refused with `bare(name)`.
    pub fn pass(
        self,
        root: &Path,
        what: &str,
        kind: &str,
        bare: impl FnOnce(&str) -> anyhow::Error,
    ) -> Result<Builder> {
        match crate::movie::transition_kind(kind) {
            crate::movie::TransitionKind::File(p) => self.file(&root.join(p)),
            crate::movie::TransitionKind::Library(lib, file) => self.generated(
                &format!("{what} \"{kind}\""),
                &format!(
                    "#include <{lib}/{file}>
"
                ),
            ),
            crate::movie::TransitionKind::Name(n) => Err(bare(n)),
        }
    }

    /// Generated code that follows the user's code (e.g. `main()`).
    pub fn footer(mut self, src: &str) -> Builder {
        let idx = self.new_string("<shotglass footer>".into());
        self.text += &format!("#line 1 {idx}\n{src}\n");
        self
    }

    pub fn build(self) -> Assembled {
        let clashes = self.clashes();
        Assembled {
            text: self.text,
            dialect: self.dialect,
            library_glsl: self.library_glsl,
            files: self.files,
            warnings: self.warnings,
            clashes,
        }
    }

    /// One message per definition that a different file already made.
    fn clashes(&self) -> Vec<String> {
        let mut first: HashMap<&str, &Definition> = HashMap::new();
        let mut out = vec![];
        for d in &self.defs {
            let Some(f) = first.get(d.key.as_str()) else {
                first.insert(&d.key, d);
                continue;
            };
            if f.file == d.file {
                continue; // same file: the driver's own message points at it
            }
            let at = |x: &Definition| format!("{}:{}", self.files[x.file], x.line);
            let owner = |x: &Definition| match x.scope {
                Scope::Film => "the film".to_string(),
                Scope::Lib(i) => format!("library \"{}\"", self.libs[i].name),
            };
            let advice = match (f.scope, d.scope) {
                (Scope::Lib(a), Scope::Lib(b)) if a != b => {
                    "; the libraries clash, so one of them has to rename it (public library \
                     functions should start with the library's own prefix)"
                }
                (Scope::Lib(_), Scope::Film) | (Scope::Film, Scope::Lib(_)) => {
                    "; rename the film's one, or use the library's"
                }
                _ => "; keep one of them",
            };
            out.push(format!(
                "error: `{}` is defined by both {} ({}) and {} ({}){advice}",
                d.key,
                owner(f),
                at(f),
                owner(d),
                at(d),
            ));
        }
        out
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
        if let Scope::Lib(i) = scope {
            self.library_glsl = self.library_glsl.max(self.libs[i].glsl);
        }
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
            let inside_root = absolute(&p).starts_with(absolute(root));
            let is_file = inside_root && p.is_file();
            let inside_resolved_root = !is_file || {
                // Keep the written path for nested includes, diagnostics and case checks.
                // Use canonical paths only to check where the file actually resides.
                p.canonicalize()
                    .and_then(|file| {
                        normalize(root)
                            .canonicalize()
                            .map(|root| file.starts_with(root))
                    })
                    .map_err(|e| format!("can't resolve #include \"{name}\" at {label}: {e}"))?
            };
            if !inside_root || !inside_resolved_root {
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
            if is_file {
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
        // Definitions are recorded in compile order: a file's own ones up to each include
        // go before the included file's.
        let mut defs = top_level_definitions(src).into_iter().peekable();
        self.text += &format!("#line 1 {idx}\n");
        for (n, line) in src.lines().enumerate() {
            while let Some((key, l)) = defs.next_if(|d| d.1 <= n + 1) {
                self.defs.push(Definition {
                    key,
                    file: idx,
                    line: l,
                    scope,
                });
            }
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
        for (key, l) in defs {
            self.defs.push(Definition {
                key,
                file: idx,
                line: l,
                scope,
            });
        }
        Ok(())
    }
}

/// Top-level functions and structs that `src` defines (with a body), as
/// (`name(param types)` or `struct Name`, line). Comments are skipped, and so is code
/// inside `#if`/`#ifdef` blocks (other than include guards), which may never be compiled.
/// A heuristic for error messages, not a parser: anything odd is simply not reported.
pub(crate) fn top_level_definitions(src: &str) -> Vec<(String, usize)> {
    // Blank comments and preprocessor lines, keeping every newline (line numbers) and
    // byte offset.
    let mut code: Vec<u8> = src.as_bytes().to_vec();
    let mut i = 0;
    while i < code.len() {
        if code[i..].starts_with(b"//") {
            while i < code.len() && code[i] != b'\n' {
                code[i] = b' ';
                i += 1;
            }
        } else if code[i..].starts_with(b"/*") {
            while i < code.len() && !code[i..].starts_with(b"*/") {
                if code[i] != b'\n' {
                    code[i] = b' ';
                }
                i += 1;
            }
            for _ in 0..2.min(code.len() - i) {
                code[i] = b' ';
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    let text = String::from_utf8_lossy(&code).into_owned();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    // Conditional blocks: true = an include guard (#ifndef X / #define X), which counts
    // as always compiled.
    let mut conds: Vec<bool> = vec![];
    for n in 0..lines.len() {
        let t = lines[n].trim_start().to_string();
        let skipped = conds.iter().any(|g| !g);
        if let Some(d) = t.strip_prefix('#') {
            let mut w = d.split_whitespace();
            match w.next() {
                Some("if" | "ifdef") => conds.push(false),
                Some("ifndef") => {
                    let name = w.next().unwrap_or("");
                    let next = lines
                        .get(n + 1)
                        .map(|l| l.split_whitespace().collect::<Vec<_>>());
                    let guard = !name.is_empty()
                        && next.is_some_and(|v| {
                            (v.first() == Some(&"#define") && v.get(1) == Some(&name))
                                || (v.first() == Some(&"#")
                                    && v.get(1) == Some(&"define")
                                    && v.get(2) == Some(&name))
                        });
                    conds.push(guard);
                }
                Some("endif") => {
                    conds.pop();
                }
                _ => {}
            }
            lines[n].clear();
        } else if skipped {
            lines[n].clear();
        }
    }
    let code = lines.join("\n");
    let bytes = code.as_bytes();
    let mut out = vec![];
    let (mut depth, mut start) = (0usize, 0usize);
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'{' => {
                if depth == 0
                    && let Some((key, at)) = definition(&code[start..i])
                {
                    let line = code[..start + at].matches('\n').count() + 1;
                    out.push((key, line));
                }
                depth += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    start = i + 1;
                }
            }
            b';' if depth == 0 => start = i + 1,
            _ => {}
        }
    }
    out
}

/// The text before a top-level `{`: a function header (`float f(vec2 p)`) or a struct
/// (`struct Ray`)? Returns the key and the byte offset of the name.
fn definition(head: &str) -> Option<(String, usize)> {
    let is_ident = |s: &str| {
        s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    let t = head.trim_end();
    if let Some(close) = t.strip_suffix(')') {
        let open = close.rfind('(')?;
        let before = close[..open].trim_end();
        let name_start = before
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map_or(0, |i| i + 1);
        let name = &before[name_start..];
        // A return type must come first (and `layout(...)` etc. aren't functions).
        let ret = before[..name_start].trim();
        if !is_ident(name) || ret.is_empty() || name == "layout" {
            return None;
        }
        const QUALIFIERS: &[&str] = &[
            "in", "out", "inout", "const", "highp", "mediump", "lowp", "precise",
        ];
        let mut types = vec![];
        for p in close[open + 1..].split(',') {
            let ty = p
                .split(|c: char| c.is_whitespace() || c == '[')
                .find(|w| !w.is_empty() && !QUALIFIERS.contains(w))
                .unwrap_or("");
            if ty.is_empty() || ty == "void" {
                continue;
            }
            types.push(if p.contains('[') {
                format!("{ty}[]")
            } else {
                ty.to_string()
            });
        }
        return Some((format!("{name}({})", types.join(", ")), name_start));
    }
    let words: Vec<&str> = t.split_whitespace().collect();
    if let [.., "struct", name] = words.as_slice()
        && is_ident(name)
    {
        let at = t.rfind(name)?;
        return Some((format!("struct {name}"), at));
    }
    None
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

/// `p` relative to `root`, with forward slashes. Both are compared in normalised absolute
/// form: a root like `film/../../core` (a `movie.local.toml` override) must still match
/// the normalised file paths that `resolve` returns.
fn display_path(p: &Path, root: &Path) -> String {
    let (abs, abs_root) = (absolute(p), absolute(root));
    abs.strip_prefix(&abs_root)
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

/// A param's GLSL value: one number is a float, 2–4 numbers a `vecN`.
pub fn define_value(v: &[f32]) -> String {
    use crate::movie::glsl_float;
    match v {
        [x] => glsl_float(*x),
        _ => format!(
            "vec{}({})",
            v.len(),
            v.iter()
                .map(|x| glsl_float(*x))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Whether `text` uses `name` as a whole identifier (not part of a longer one).
pub fn mentions_ident(text: &str, name: &str) -> bool {
    let ident = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    text.match_indices(name).any(|(i, _)| {
        !ident(text[..i].chars().next_back()) && !ident(text[i + name.len()..].chars().next())
    })
}

/// Compile and link a vertex and a fragment shader. Err = messages: the vertex
/// shader's log (prefixed), the fragment shader's mapped to file:line through
/// `fs_files` (the `#line` source strings), or the link log.
pub unsafe fn link(
    gl: &eframe::glow::Context,
    vs: &str,
    fs: &str,
    fs_files: &[String],
) -> std::result::Result<eframe::glow::Program, Vec<String>> {
    use crate::gfx::Tracked;
    use eframe::glow::{self, HasContext};
    unsafe {
        let v = gl.new_shader(glow::VERTEX_SHADER).map_err(|e| vec![e])?;
        gl.shader_source(v, vs);
        gl.compile_shader(v);
        let f = gl.new_shader(glow::FRAGMENT_SHADER).map_err(|e| vec![e])?;
        gl.shader_source(f, fs);
        gl.compile_shader(f);
        let prog = gl.new_program().map_err(|e| vec![e])?;
        gl.attach_shader(prog, v);
        gl.attach_shader(prog, f);
        gl.link_program(prog);
        let mut errors = vec![];
        if !gl.get_program_link_status(prog) {
            if !gl.get_shader_compile_status(v) {
                errors.push(format!("vertex shader: {}", gl.get_shader_info_log(v)));
            }
            if !gl.get_shader_compile_status(f) {
                errors.extend(remap_log(&gl.get_shader_info_log(f), fs_files));
            }
            if errors.is_empty() {
                errors.push(format!("link: {}", gl.get_program_info_log(prog)));
            }
        }
        for sh in [v, f] {
            gl.detach_shader(prog, sh);
            gl.free_shader(sh);
        }
        if errors.is_empty() {
            Ok(prog)
        } else {
            gl.free_program(prog);
            Err(errors)
        }
    }
}

/// [`link`] an assembled fragment shader with the shared full-screen vertex shader.
pub unsafe fn compile(
    gl: &eframe::glow::Context,
    src: &Assembled,
) -> std::result::Result<eframe::glow::Program, Vec<String>> {
    unsafe {
        link(
            gl,
            &crate::gfx::vertex_source(src.dialect),
            &src.text,
            &src.files,
        )
    }
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
    fn override_outside_the_film_shows_local_names() {
        use crate::library::Origin;
        // Like a `movie.local.toml` override `path = "../../core"`: the root keeps its `..`.
        let top = tmp("outside");
        let film = top.join("tests/film");
        write(
            &top,
            &[
                (
                    "core/library.toml",
                    "name = \"shotglass-kit/core\"\nversion = \"0.4.0\"\n",
                ),
                ("core/ease.glsl", "#include \"fx/step.glsl\"\nfloat e;\n"),
                ("core/fx/step.glsl", "float s;\n"),
                ("tests/film/scenes/a.glsl", "#include <kit/ease.glsl>\n"),
            ],
        );
        let libs = vec![Library::read("kit", &film.join("../../core"), Origin::Path).unwrap()];
        let a = Builder::new(D, "", &film)
            .libraries(&libs)
            .file(&film.join("scenes/a.glsl"))
            .unwrap()
            .build();
        assert_eq!(
            a.files,
            vec![
                "<shotglass header>",
                "scenes/a.glsl",
                "kit/ease.glsl",
                "kit/fx/step.glsl",
            ]
        );
        std::fs::remove_dir_all(&top).ok();
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

    #[test]
    fn finds_top_level_definitions() {
        let src = "// float commented(){ }\n\
                   /* float block(){ }\n */\n\
                   float hash21(vec2 p) { return 0.0; }\n\
                   vec3 shade(in vec3 p, const float k[4], out vec2 uv)\n{\n  if (k[0] > 0.0) { uv = p.xy; }\n  return p;\n}\n\
                   void main(void){}\n\
                   struct Ray { vec3 o; };\n\
                   layout(std140) uniform Block { float x; };\n\
                   float proto(float x);\n\
                   #ifdef FANCY\nfloat maybe(){ return 1.0; }\n#endif\n\
                   #ifndef GUARD\n#define GUARD\nfloat guarded(){ return 1.0; }\n#endif\n";
        let d = top_level_definitions(src);
        let keys: Vec<(&str, usize)> = d.iter().map(|(k, l)| (k.as_str(), *l)).collect();
        assert_eq!(
            keys,
            vec![
                ("hash21(vec2)", 4),
                ("shade(vec3, float[], vec2)", 5),
                ("main()", 10),
                ("struct Ray", 11),
                ("guarded()", 19),
            ]
        );
    }

    #[test]
    fn clashes_name_both_owners() {
        let (dir, mut libs) = film("clash");
        // toon also defines kit's kit_hash(); the film defines toon_outline() itself.
        write(
            &libs[1].root,
            &[(
                "fx/hash.glsl",
                "\nfloat kit_hash(){ return 0.25; }\nfloat kit_hash(float s){ return s; }\n",
            )],
        );
        libs[1] = Library::read("toon", &libs[1].root, crate::library::Origin::Path).unwrap();
        write(
            &dir,
            &[(
                "scenes/a.glsl",
                "#include <kit/noise.glsl>\n#include <toon/fx/hash.glsl>\n\
                 #include <toon/fx/outline.glsl>\nfloat toon_outline(){ return 1.0; }\n",
            )],
        );
        let a = Builder::new(D, "", &dir)
            .libraries(&libs)
            .file(&dir.join("scenes/a.glsl"))
            .unwrap()
            .build();
        assert_eq!(a.clashes.len(), 2, "{:#?}", a.clashes);
        assert!(
            a.clashes[0].contains("`kit_hash()` is defined by both library \"kit\" (kit/hash.glsl:1) and library \"toon\" (toon/fx/hash.glsl:2)")
                && a.clashes[0].contains("libraries clash"),
            "{}",
            a.clashes[0]
        );
        assert!(
            a.clashes[1].contains(
                "library \"toon\" (toon/fx/outline.glsl:2) and the film (scenes/a.glsl:4)"
            ),
            "{}",
            a.clashes[1]
        );
        // The overload kit_hash(float) is fine. Clashes lead the compile errors.
        let e = a.explain(vec!["kit/hash.glsl:1: error: redefinition".into()]);
        assert_eq!(e.len(), 3);
        assert!(e[2].contains("redefinition"));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&libs[1].root).ok();
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

    #[cfg(any(unix, windows))]
    fn link_dir(target: &Path, link: &Path) {
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_dir(target, link);
        #[cfg(windows)]
        if matches!(&result, Err(e) if e.raw_os_error() == Some(1314)) {
            // Junctions can be created without the Windows symlink privilege.
            let output = std::process::Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-Command",
                    "New-Item -ItemType Junction -Path $env:SHOTGLASS_TEST_LINK -Target $env:SHOTGLASS_TEST_TARGET -ErrorAction Stop",
                ])
                .env("SHOTGLASS_TEST_LINK", link)
                .env("SHOTGLASS_TEST_TARGET", target)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        result.unwrap();
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn include_links_stay_within_their_scope() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("film");
        let library = dir.join("vendor/kit");
        write(&dir, &[("local/helper.glsl", "float film_helper;\n")]);
        write(
            &library,
            &[
                ("library.toml", "name = \"kit/core\"\nversion = \"0.4.0\"\n"),
                ("local/helper.glsl", "float kit_helper;\n"),
            ],
        );
        let outside = temp.path().join("film-other");
        write(&outside, &[("helper.glsl", "float outside_helper;\n")]);
        link_dir(&dir.join("local"), &dir.join("inside"));
        link_dir(&outside, &dir.join("outside"));
        link_dir(&library.join("local"), &library.join("inside"));
        // This target is inside the film, but outside the library.
        link_dir(&dir.join("local"), &library.join("outside"));
        let libs = [Library::read("kit", &library, crate::library::Origin::Path).unwrap()];
        let film_link = temp.path().join("film-link");
        link_dir(&dir, &film_link);
        let dir = film_link;
        let entry = dir.join("main.glsl");
        std::fs::write(
            &entry,
            "#include \"inside/helper.glsl\"\n#include <kit/inside/helper.glsl>\n",
        )
        .unwrap();
        let source = Builder::new(D, "", &dir)
            .libraries(&libs)
            .file(&entry)
            .unwrap()
            .build();
        assert!(source.text.contains("float film_helper;"));
        assert!(source.text.contains("float kit_helper;"));
        assert!(source.files.iter().any(|f| f == "inside/helper.glsl"));
        assert!(source.files.iter().any(|f| f == "kit/inside/helper.glsl"));

        let error = include_error(&dir, &libs, "#include \"outside/helper.glsl\"\n");
        assert!(
            error.contains("outside/helper.glsl (outside the film folder, not allowed)"),
            "{error}"
        );
        let error = include_error(&dir, &libs, "#include <kit/outside/helper.glsl>\n");
        assert!(
            error.contains("outside the library \"kit\", not allowed"),
            "{error}"
        );
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
