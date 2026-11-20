//! Building GLSL sources: Shadertoy-style headers, `#include`, and mapping driver
//! errors back to file:line.
//!
//! Every file gets its own GLSL "source string number" through `#line 1 N`, so a
//! driver message like `3(42) : error` or `0:42(7): error` tells us the file (N) and
//! the line directly.

use anyhow::{Result, anyhow, bail};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug)]
pub struct Dialect {
    pub es: bool,
    pub version: u32,
}

impl Dialect {
    /// Pick a GLSL version from GL_SHADING_LANGUAGE_VERSION / GL_VERSION.
    pub fn detect(glsl_version: &str, gl_version: &str) -> Dialect {
        let es = gl_version.contains("OpenGL ES") || glsl_version.contains("OpenGL ES");
        let num: String = glsl_version
            .split_whitespace()
            .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .unwrap_or("3.30")
            .chars()
            .filter(|c| c.is_ascii_digit())
            .take(3)
            .collect();
        let v: u32 = num.parse().unwrap_or(330);
        if es {
            Dialect {
                es: true,
                version: 300,
            }
        } else {
            let version = if v >= 430 {
                430
            } else if v >= 410 {
                410
            } else {
                330
            };
            Dialect { es: false, version }
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

/// A fully assembled shader plus the names of the files it came from.
#[derive(Clone, Debug)]
pub struct Assembled {
    pub text: String,
    /// Index = GLSL source string number.
    pub files: Vec<String>,
}

pub struct Builder {
    text: String,
    files: Vec<String>,
    root: PathBuf,
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
            files: vec!["<shotglass header>".into()],
            root: root.to_path_buf(),
        }
    }

    pub fn file(mut self, path: &Path) -> Result<Builder> {
        self.push_file(path, 0)?;
        Ok(self)
    }

    pub fn builtin(mut self, name: &str, src: &str) -> Result<Builder> {
        let idx = self.new_string(format!("<builtin {name}>"));
        self.push_text(src, idx, None, 0)?;
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
            files: self.files,
        }
    }

    fn new_string(&mut self, name: String) -> usize {
        self.files.push(name);
        self.files.len() - 1
    }

    fn push_file(&mut self, path: &Path, depth: usize) -> Result<()> {
        if depth > 16 {
            bail!("#include nested too deeply at {}", path.display());
        }
        let src = std::fs::read_to_string(path).map_err(|e| anyhow!("{}: {e}", path.display()))?;
        let idx = self.new_string(display_path(path, &self.root));
        self.push_text(&src, idx, Some(path), depth)
    }

    fn push_text(
        &mut self,
        src: &str,
        idx: usize,
        path: Option<&Path>,
        depth: usize,
    ) -> Result<()> {
        self.text += &format!("#line 1 {idx}\n");
        for (n, line) in src.lines().enumerate() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("#include") {
                let name = rest
                    .trim()
                    .trim_matches(|c| c == '"' || c == '<' || c == '>');
                let here = path
                    .and_then(Path::parent)
                    .unwrap_or(&self.root)
                    .to_path_buf();
                let candidate = [
                    normalize(&here.join(name)),
                    normalize(&self.root.join(name)),
                ]
                .into_iter()
                .find(|p| p.exists());
                let Some(inc) = candidate else {
                    bail!(
                        "{}:{}: can't find #include \"{name}\"",
                        self.files[idx],
                        n + 1
                    );
                };
                self.push_file(&inc, depth + 1)?;
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
        let d = Dialect {
            es: false,
            version: 330,
        };
        let a = Builder::new(d, "uniform float iTime;", &dir)
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

    #[test]
    fn detects_dialect() {
        assert_eq!(Dialect::detect("4.60 NVIDIA", "4.6.0 NVIDIA").version, 430);
        assert_eq!(
            Dialect::detect("4.50", "4.5 (Core Profile) Mesa").version,
            430
        );
        assert_eq!(Dialect::detect("4.10", "4.1 Metal").version, 410);
        assert!(Dialect::detect("OpenGL ES GLSL ES 3.00", "OpenGL ES 3.0").es);
    }
}
