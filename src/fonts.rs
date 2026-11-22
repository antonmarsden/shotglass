//! Google Fonts families as libraries, and `shotglass fonts add`.
//!
//! A family is declared in the project like any library and its files are named in
//! `[fonts]` like any library file:
//!
//! ```toml
//! # shotglass.toml
//! [libraries]
//! lobster = { google = "Lobster" }
//! # movie.toml
//! [fonts]
//! lobster = "<lobster/Lobster-Regular.ttf>"
//! ```
//!
//! So fonts get everything libraries have: fetched when a command needs them (git, a
//! shallow, blobless, sparse checkout of just that family's folder in google/fonts),
//! pinned in `shotglass.lock` (the commit, the folder and a sha256 of the files), kept
//! in the shared cache, moved on purpose by `shotglass update`, overridable with
//! `SHOTGLASS_LIB_<NAME>`, and copied into the film by `freeze`/`bundle`, exactly the
//! files it uses plus the licence. The library code is in `library.rs`; this module has
//! the Google Fonts specifics (folder names, `METADATA.pb`, licences) and `fonts add`,
//! which writes both declarations and fetches.
//!
//! google/fonts sorts families by licence: `ofl/` (SIL OFL 1.1, nearly all), `apache/`
//! (Apache 2.0), `ufl/` (Ubuntu Font Licence). All three allow embedding and
//! redistributing the fonts unmodified.

use anyhow::{Context, Result, anyhow, bail};
use std::path::Path;

use crate::library::{self, Fetcher};

/// Where families come from (`SHOTGLASS_GOOGLE_FONTS` overrides it, for mirrors).
pub const GOOGLE_FONTS: &str = "https://github.com/google/fonts";

pub fn google_source() -> String {
    std::env::var("SHOTGLASS_GOOGLE_FONTS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| GOOGLE_FONTS.to_string())
}

/// google/fonts' licence folders and their SPDX ids.
pub const LICENCE_DIRS: &[(&str, &str)] = &[
    ("ofl", "OFL-1.1"),
    ("apache", "Apache-2.0"),
    ("ufl", "Ubuntu-font-1.0"),
];

/// The SPDX id for `METADATA.pb`'s `license` field.
pub fn spdx(license: &str) -> Option<String> {
    match license {
        "OFL" => Some("OFL-1.1"),
        "APACHE2" => Some("Apache-2.0"),
        "UFL" => Some("Ubuntu-font-1.0"),
        _ => None,
    }
    .map(str::to_string)
}

/// A family name as google/fonts names its folder: lowercase letters and digits only
/// ("Atkinson Hyperlegible" → "atkinsonhyperlegible", "Noto Sans JP" → "notosansjp").
pub fn slug(family: &str) -> String {
    family
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// One font file in a family's `METADATA.pb`.
#[derive(Clone, Debug, PartialEq)]
pub struct FontFile {
    pub filename: String,
    pub italic: bool,
    pub weight: u32,
}

/// What we use from a family's `METADATA.pb` (protobuf text format).
#[derive(Clone, Debug, PartialEq)]
pub struct Metadata {
    pub name: String,
    pub license: String,
    pub fonts: Vec<FontFile>,
    /// Variable-font axes: (tag, min, max).
    pub axes: Vec<(String, f32, f32)>,
}

/// Parse the parts of `METADATA.pb` we use. Unknown fields are skipped; nested blocks
/// are tracked by brace depth.
pub fn parse_metadata(text: &str) -> Result<Metadata> {
    let mut m = Metadata {
        name: String::new(),
        license: String::new(),
        fonts: vec![],
        axes: vec![],
    };
    let mut block: Vec<String> = vec![];
    let new_font = || FontFile {
        filename: String::new(),
        italic: false,
        weight: 400,
    };
    let mut font = new_font();
    let mut axis = (String::new(), 0.0f32, 0.0f32);
    let unquote = |v: &str| v.trim().trim_matches('"').to_string();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_suffix('{') {
            let name = name.trim().to_string();
            match name.as_str() {
                "fonts" => font = new_font(),
                "axes" => axis = (String::new(), 0.0, 0.0),
                _ => {}
            }
            block.push(name);
            continue;
        }
        if line == "}" {
            match block.pop().as_deref() {
                Some("fonts") if block.is_empty() => m.fonts.push(font.clone()),
                Some("axes") if block.is_empty() => m.axes.push(axis.clone()),
                _ => {}
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        match (block.as_slice(), key) {
            ([], "name") => m.name = unquote(value),
            ([], "license") => m.license = unquote(value),
            ([b], "filename") if b == "fonts" => font.filename = unquote(value),
            ([b], "style") if b == "fonts" => font.italic = unquote(value) == "italic",
            ([b], "weight") if b == "fonts" => font.weight = value.trim().parse().unwrap_or(400),
            ([b], "tag") if b == "axes" => axis.0 = unquote(value),
            ([b], "min_value") if b == "axes" => axis.1 = value.trim().parse().unwrap_or(0.0),
            ([b], "max_value") if b == "axes" => axis.2 = value.trim().parse().unwrap_or(0.0),
            _ => {}
        }
    }
    if m.fonts.is_empty() {
        bail!("METADATA.pb lists no fonts");
    }
    Ok(m)
}

/// The `[fonts]` name for a file: the library name for the upright regular (or the
/// upright variable font), else name + weight (+ `_italic`).
pub fn font_key(name: &str, f: &FontFile, variable: bool) -> String {
    let weight = match f.weight {
        100 => "thin",
        200 => "extralight",
        300 => "light",
        500 => "medium",
        600 => "semibold",
        700 => "bold",
        800 => "extrabold",
        900 => "black",
        _ => "",
    };
    let mut key = name.to_string();
    if !variable && !weight.is_empty() {
        key += "_";
        key += weight;
    }
    if f.italic {
        key += "_italic";
    }
    key
}

/// A library name for a family: its slug, made a valid name (`[fonts]` names and
/// library names start with a letter).
fn library_name(family: &str) -> String {
    let s = slug(family);
    if s.starts_with(|c: char| c.is_ascii_digit()) {
        format!("font_{s}")
    } else {
        s
    }
}

/// What one family came to.
#[derive(Debug, PartialEq)]
pub struct Added {
    pub family: String,
    pub library: String,
    pub licence: String,
    /// The pin, for messages: `google/fonts@1a2b3c4`.
    pub pin: String,
    /// (key, `<library/file>`).
    pub entries: Vec<(String, String)>,
    pub axes: Vec<(String, f32, f32)>,
}

/// Declare `families` as libraries in the film's project (creating `shotglass.toml`
/// next to the film if it has none), fetch and pin them, and add a `[fonts]` entry per
/// file to `movie.toml`. Both files keep their comments and order.
pub fn add(
    film: &Path,
    families: &[String],
    fetcher: &dyn Fetcher,
    cache: &Path,
) -> Result<Vec<Added>> {
    if families.is_empty() {
        bail!("name at least one family, as on fonts.google.com (e.g. \"Lobster\")");
    }
    let movie_file = film.join(crate::project::MOVIE_FILE);
    let movie_text = std::fs::read_to_string(&movie_file)
        .with_context(|| format!("reading {}", movie_file.display()))?;
    let mut movie: toml_edit::DocumentMut = movie_text
        .parse()
        .with_context(|| format!("parsing {}", movie_file.display()))?;
    let project = crate::project::Project::for_film(film)?;
    if project.frozen {
        bail!(
            "{} is frozen; `shotglass unfreeze` it to add fonts",
            project.root.display()
        );
    }
    let proj_root = if project.has_file {
        project.root.clone()
    } else {
        film.to_path_buf()
    };
    let proj_file = proj_root.join(crate::project::PROJECT_FILE);
    let proj_text = if project.has_file {
        std::fs::read_to_string(&proj_file)
            .with_context(|| format!("reading {}", proj_file.display()))?
    } else {
        format!("shotglass = \"{}\"\n", library::CONTRACT)
    };
    let mut proj: toml_edit::DocumentMut = proj_text
        .parse()
        .with_context(|| format!("parsing {}", proj_file.display()))?;
    // 1. [libraries] name = { google = "Family" } (kept if it's already there).
    let libs = proj
        .entry("libraries")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| anyhow!("{}: libraries isn't a table", proj_file.display()))?;
    let mut names = vec![];
    for family in families {
        if slug(family).is_empty() {
            bail!("\"{family}\" isn't a family name");
        }
        let name = library_name(family);
        match libs.get(&name) {
            None => {
                let mut decl = toml_edit::InlineTable::new();
                decl.insert("google", family.as_str().into());
                libs.insert(&name, toml_edit::value(decl));
            }
            Some(existing) => {
                let same = existing
                    .as_table_like()
                    .and_then(|t| t.get("google"))
                    .and_then(|g| g.as_str())
                    .is_some_and(|g| slug(g) == slug(family));
                if !same {
                    bail!(
                        "{} already has a library called \"{name}\"; rename it (or the font) first",
                        proj_file.display()
                    );
                }
            }
        }
        names.push(name);
    }
    let new_proj_text = proj.to_string();
    // 2. Fetch and pin, before writing anything: a typo in a family name changes nothing.
    let parsed = crate::project::Project::parse(&new_proj_text, &proj_root)?;
    let old_lock = library::Lock::load(&proj_root)?;
    let (lock, _) = library::fetch(
        &parsed.specs,
        old_lock.as_ref(),
        fetcher,
        cache,
        &library::FetchMode::Missing {
            skip: Default::default(),
        },
    )?;
    let mut added = vec![];
    for (family, name) in families.iter().zip(&names) {
        let e = lock
            .get(name)
            .ok_or_else(|| anyhow!("library \"{name}\" wasn't pinned (a bug)"))?;
        let root = match &e.path {
            Some(p) => proj_root.join(p),
            None => library::cache_path(cache, &e.source, &e.cache_key())
                .join(e.subdir.as_deref().unwrap_or("")),
        };
        let meta_text = std::fs::read_to_string(root.join("METADATA.pb"))?;
        let meta = parse_metadata(&meta_text)?;
        let mut entries: Vec<(String, String)> = vec![];
        for f in &meta.fonts {
            let path = format!("<{name}/{}>", f.filename);
            if entries.iter().any(|(_, p)| *p == path) {
                continue; // a variable font listed once per named style
            }
            let mut key = font_key(name, f, f.filename.contains('['));
            while entries.iter().any(|(k, _)| *k == key) {
                key += "_2";
            }
            entries.push((key, path));
        }
        added.push(Added {
            family: if meta.name.is_empty() {
                family.clone()
            } else {
                meta.name.clone()
            },
            library: name.clone(),
            licence: spdx(&meta.license).unwrap_or_else(|| meta.license.clone()),
            pin: e.pin(),
            entries,
            axes: meta.axes,
        });
    }
    // 3. [fonts] entries (existing names are left alone).
    let fonts = movie
        .entry("fonts")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| anyhow!("{}: fonts isn't a table", movie_file.display()))?;
    for a in &added {
        for (key, path) in &a.entries {
            if !fonts.contains_key(key) {
                fonts.insert(key, toml_edit::value(path.as_str()));
            }
        }
    }
    std::fs::write(&proj_file, new_proj_text)
        .with_context(|| format!("writing {}", proj_file.display()))?;
    if old_lock.as_ref() != Some(&lock) {
        let f = proj_root.join(library::LOCK_FILE);
        std::fs::write(&f, lock.to_text()).with_context(|| format!("writing {}", f.display()))?;
    }
    std::fs::write(&movie_file, movie.to_string())
        .with_context(|| format!("writing {}", movie_file.display()))?;
    Ok(added)
}

/// `shotglass fonts add`: declare, fetch, add entries, and say what happened.
pub fn run_add(film: &Path, families: &[String]) -> Result<()> {
    let added = add(film, families, &library::Git, &library::cache_dir())?;
    for a in &added {
        println!(
            "{} ({}): library \"{}\", {}",
            a.family, a.licence, a.library, a.pin
        );
        for (key, path) in &a.entries {
            println!("  [fonts] {key} = \"{path}\"");
        }
        if !a.axes.is_empty() {
            let axes: Vec<String> = a
                .axes
                .iter()
                .map(|(t, lo, hi)| format!("{t} {lo}–{hi}"))
                .collect();
            let example = a.axes.iter().find(|x| x.0 == "wght").unwrap_or(&a.axes[0]);
            println!(
                "  variable: {} (e.g. variations = {{ {} = {} }})",
                axes.join(", "),
                example.0,
                example.2
            );
        }
    }
    println!(
        "pinned in {}; commit it with shotglass.toml and movie.toml",
        library::LOCK_FILE
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOBSTER: &str = r#"name: "Lobster"
designer: "Impallari Type"
license: "OFL"
category: "DISPLAY"
fonts {
  name: "Lobster"
  style: "normal"
  weight: 400
  filename: "Lobster-Regular.ttf"
  post_script_name: "Lobster-Regular"
}
subsets: "latin"
source {
  repository_url: "https://github.com/impallari/The-Lobster-Font"
}
"#;

    const INTER: &str = r#"name: "Inter"
license: "OFL"
fonts {
  name: "Inter"
  style: "normal"
  weight: 400
  filename: "Inter[opsz,wght].ttf"
}
fonts {
  name: "Inter"
  style: "italic"
  weight: 400
  filename: "Inter-Italic[opsz,wght].ttf"
}
axes {
  tag: "opsz"
  min_value: 14.0
  max_value: 32.0
}
axes {
  tag: "wght"
  min_value: 100.0
  max_value: 900.0
}
"#;

    #[test]
    fn slugs_and_licences() {
        assert_eq!(slug("Lobster"), "lobster");
        assert_eq!(slug("Atkinson Hyperlegible"), "atkinsonhyperlegible");
        assert_eq!(slug("Noto Sans JP"), "notosansjp");
        assert_eq!(slug("M PLUS 1p"), "mplus1p");
        assert_eq!(library_name("21 Cent"), "font_21cent");
        assert_eq!(spdx("OFL").as_deref(), Some("OFL-1.1"));
        assert_eq!(spdx("APACHE2").as_deref(), Some("Apache-2.0"));
        assert_eq!(spdx("UFL").as_deref(), Some("Ubuntu-font-1.0"));
        assert_eq!(spdx("??"), None);
    }

    #[test]
    fn metadata_is_read() {
        let m = parse_metadata(LOBSTER).unwrap();
        assert_eq!((m.name.as_str(), m.license.as_str()), ("Lobster", "OFL"));
        assert_eq!(
            m.fonts,
            vec![FontFile {
                filename: "Lobster-Regular.ttf".into(),
                italic: false,
                weight: 400
            }]
        );
        assert!(m.axes.is_empty());
        let m = parse_metadata(INTER).unwrap();
        assert_eq!(m.fonts.len(), 2);
        assert!(m.fonts[1].italic);
        assert_eq!(
            m.axes,
            vec![("opsz".into(), 14.0, 32.0), ("wght".into(), 100.0, 900.0)]
        );
        assert!(parse_metadata("name: \"x\"\n").is_err());
    }

    #[test]
    fn keys() {
        let f = |weight, italic| FontFile {
            filename: String::new(),
            italic,
            weight,
        };
        assert_eq!(font_key("lobster", &f(400, false), false), "lobster");
        assert_eq!(font_key("roboto", &f(700, false), false), "roboto_bold");
        assert_eq!(
            font_key("roboto", &f(300, true), false),
            "roboto_light_italic"
        );
        assert_eq!(font_key("inter", &f(400, true), true), "inter_italic");
        assert_eq!(font_key("inter", &f(400, false), true), "inter");
    }

    /// The real git fetcher, reading Google Fonts families from `source`.
    struct GitAt(String);

    impl Fetcher for GitAt {
        fn tags(&self, source: &str) -> Result<Vec<String>> {
            library::Git.tags(source)
        }
        fn download(&self, source: &str, tag: &str, dest: &Path) -> Result<()> {
            library::Git.download(source, tag, dest)
        }
        fn head(&self, source: &str, refname: &str) -> Result<String> {
            library::Git.head(source, refname)
        }
        fn download_rev(&self, source: &str, rev: &str, dest: &Path) -> Result<()> {
            library::Git.download_rev(source, rev, dest)
        }
        fn download_sparse(
            &self,
            source: &str,
            rev: &str,
            dirs: &[String],
            dest: &Path,
        ) -> Result<()> {
            library::Git.download_sparse(source, rev, dirs, dest)
        }
        fn google_source(&self) -> String {
            self.0.clone()
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let d = dir.to_string_lossy().into_owned();
        let mut a = vec!["-C", d.as_str()];
        a.extend_from_slice(args);
        library::Git::run(&a).unwrap();
    }

    /// A tiny stand-in for google/fonts: a local repository with an `ofl/` family (a
    /// real font, so the film can lay text out with it) and an `apache/` one.
    fn fixture_repo(dir: &Path) -> String {
        let font = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/films/text/fonts/NotoSans-subset.ttf"),
        )
        .unwrap();
        let fam = dir.join("ofl/testsans");
        std::fs::create_dir_all(&fam).unwrap();
        std::fs::write(fam.join("TestSans[wdth,wght].ttf"), &font).unwrap();
        std::fs::write(fam.join("OFL.txt"), "SIL Open Font License 1.1\n").unwrap();
        std::fs::write(fam.join("DESCRIPTION.en_us.html"), "<p>test</p>\n").unwrap();
        std::fs::write(
            fam.join("METADATA.pb"),
            "name: \"Test Sans\"\nlicense: \"OFL\"\nfonts {\n  style: \"normal\"\n  weight: 400\n  filename: \"TestSans[wdth,wght].ttf\"\n}\naxes {\n  tag: \"wght\"\n  min_value: 100.0\n  max_value: 900.0\n}\n",
        )
        .unwrap();
        let fam = dir.join("apache/testmono");
        std::fs::create_dir_all(&fam).unwrap();
        std::fs::write(fam.join("TestMono-Regular.ttf"), &font).unwrap();
        std::fs::write(fam.join("TestMono-Bold.ttf"), &font).unwrap();
        std::fs::write(fam.join("LICENSE.txt"), "Apache License 2.0\n").unwrap();
        std::fs::write(
            fam.join("METADATA.pb"),
            "name: \"Test Mono\"\nlicense: \"APACHE2\"\nfonts {\n  style: \"normal\"\n  weight: 400\n  filename: \"TestMono-Regular.ttf\"\n}\nfonts {\n  style: \"normal\"\n  weight: 700\n  filename: \"TestMono-Bold.ttf\"\n}\n",
        )
        .unwrap();
        // Something elsewhere that a sparse fetch must not bring along.
        std::fs::create_dir_all(dir.join("ofl/other")).unwrap();
        std::fs::write(dir.join("ofl/other/Other.ttf"), vec![7u8; 4096]).unwrap();
        library::Git::run(&["init", "--quiet", "--", &dir.to_string_lossy()]).unwrap();
        git(dir, &["config", "uploadpack.allowFilter", "true"]);
        git(dir, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
        git(dir, &["add", "."]);
        git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        format!("file://{}", dir.display())
    }

    #[test]
    fn add_declares_fetches_pins_and_freezes() {
        if library::Git::run(&["--version"]).is_err() {
            eprintln!("skipped: no git");
            return;
        }
        let base =
            std::env::temp_dir().join(format!("shotglass-fonts-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let source = fixture_repo(&base.join("repo"));
        let fetcher = GitAt(source.clone());
        let cache = base.join("cache");
        let film = base.join("film");
        std::fs::create_dir_all(&film).unwrap();
        std::fs::write(
            film.join("movie.toml"),
            "# my film\ntitle = \"t\"\n\n[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n",
        )
        .unwrap();
        std::fs::write(
            film.join("a.glsl"),
            "void mainImage(out vec4 c, in vec2 p) { c = vec4(0.0); }\n",
        )
        .unwrap();
        let added = add(
            &film,
            &["Test Sans".to_string(), "Test Mono".to_string()],
            &fetcher,
            &cache,
        )
        .unwrap();
        assert_eq!(added[0].library, "testsans");
        assert_eq!(added[0].licence, "OFL-1.1");
        assert!(
            added[0].pin.starts_with("google/fonts@"),
            "{}",
            added[0].pin
        );
        assert_eq!(
            added[0].entries,
            vec![(
                "testsans".to_string(),
                "<testsans/TestSans[wdth,wght].ttf>".to_string()
            )]
        );
        assert_eq!(added[1].licence, "Apache-2.0");
        let keys: Vec<&str> = added[1].entries.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["testmono", "testmono_bold"]);
        // The project declares the families, the lock pins them, movie.toml names files.
        let proj = std::fs::read_to_string(film.join("shotglass.toml")).unwrap();
        assert!(
            proj.contains("testsans = { google = \"Test Sans\" }"),
            "{proj}"
        );
        let lock = library::Lock::load(&film).unwrap().unwrap();
        let e = lock.get("testsans").unwrap();
        assert_eq!(e.subdir.as_deref(), Some("ofl/testsans"));
        assert_eq!(e.google.as_deref(), Some("Test Sans"));
        assert_eq!(
            lock.get("testmono").unwrap().subdir.as_deref(),
            Some("apache/testmono")
        );
        let toml = std::fs::read_to_string(film.join("movie.toml")).unwrap();
        assert!(toml.starts_with("# my film\n"), "{toml}");
        assert!(
            toml.contains("testsans = \"<testsans/TestSans[wdth,wght].ttf>\""),
            "{toml}"
        );
        // Only the family folders were fetched; nothing went into the film folder.
        let cached = library::cache_path(&cache, &e.source, &e.cache_key());
        assert!(
            cached
                .join("ofl/testsans/TestSans[wdth,wght].ttf")
                .is_file()
        );
        assert!(!cached.join("ofl/other").exists());
        assert!(!film.join("fonts").exists());
        // A text with it loads, lays out, and reports the family's licence.
        let toml = format!("{toml}\n[texts]\nt = {{ text = \"Hi\", font = \"testsans\" }}\n")
            .replace("duration = 1\n", "duration = 1\nchannels = [\"t\"]\n");
        std::fs::write(film.join("movie.toml"), &toml).unwrap();
        let m = crate::movie::Movie::load_locked(&film, &cache).unwrap();
        let lib = m.libraries.iter().find(|l| l.name == "testsans").unwrap();
        assert_eq!(lib.licences, vec!["OFL-1.1".to_string()]);
        assert_eq!(lib.describe(), "testsans (Google Fonts \"Test Sans\")");
        // A deleted cache comes back at the pinned commit (what any command does).
        std::fs::remove_dir_all(&cache).unwrap();
        let old = library::Lock::load(&film).unwrap();
        let (again, log) = library::fetch(
            &m.library_specs,
            old.as_ref(),
            &fetcher,
            &cache,
            &library::FetchMode::Missing {
                skip: Default::default(),
            },
        )
        .unwrap();
        assert_eq!(Some(&again), old.as_ref());
        assert!(
            log.iter().any(|l| l.contains("downloaded (locked)")),
            "{log:?}"
        );
        // Bundling copies the font it uses, its licence and METADATA.pb, nothing else.
        let used = crate::bundle::used_files(&m).0;
        assert_eq!(
            used.get("testsans")
                .map(|s| s.iter().cloned().collect::<Vec<_>>()),
            Some(vec!["TestSans[wdth,wght].ttf".to_string()])
        );
        let frozen = base.join("frozen");
        crate::bundle::bundle(&film, &frozen, &cache).unwrap();
        let copy = frozen.join("shotglass-libs/testsans");
        for f in ["TestSans[wdth,wght].ttf", "OFL.txt", "METADATA.pb"] {
            assert!(copy.join(f).is_file(), "{f}");
        }
        assert!(!copy.join("DESCRIPTION.en_us.html").exists());
        // Test Mono is named in [fonts] but used by no text: declared, no font files.
        let mono = frozen.join("shotglass-libs/testmono");
        assert!(mono.join("METADATA.pb").is_file() && mono.join("LICENSE.txt").is_file());
        assert!(!mono.join("TestMono-Regular.ttf").exists());
        let fm = crate::movie::Movie::load_locked(&frozen, &base.join("no-cache")).unwrap();
        let lic = |name: &str| {
            fm.libraries
                .iter()
                .find(|l| l.name == name)
                .map(|l| l.licences.clone())
        };
        assert_eq!(lic("testsans"), Some(vec!["OFL-1.1".to_string()]));
        assert_eq!(lic("testmono"), Some(vec!["Apache-2.0".to_string()]));
        // Asking again is harmless; an unknown family is named, and changes nothing.
        add(&film, &["Test Sans".to_string()], &fetcher, &cache).unwrap();
        let before = std::fs::read_to_string(film.join("shotglass.toml")).unwrap();
        let e = add(&film, &["Nope Font".to_string()], &fetcher, &cache).unwrap_err();
        assert!(
            format!("{e:#}").contains(
                "no family \"Nope Font\" in Google Fonts (looked for ofl/nopefont, apache/nopefont, ufl/nopefont"
            ),
            "{e:#}"
        );
        assert_eq!(
            std::fs::read_to_string(film.join("shotglass.toml")).unwrap(),
            before
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
