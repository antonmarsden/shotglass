//! `shotglass fonts add`: fetch font families from Google Fonts (the google/fonts
//! repository) into a film, ready for `[fonts]`.
//!
//! The network is touched only here: the files are copied into the film's
//! `fonts/<family>/` folder with their licence, the family's `METADATA.pb` and a
//! `SOURCE.txt` naming the commit they came from, and `movie.toml` gets a `[fonts]`
//! entry per file with `licence` filled in. From then on the film plays, exports,
//! bundles and freezes from its own folder, offline, and every machine draws the same
//! glyphs.
//!
//! Fetching uses git like `shotglass fetch` (same proxy, certificate and credential
//! setup): a shallow, blobless, sparse clone of only the families asked for (a few MB,
//! not the multi-GB repository). google/fonts sorts families by licence, so the folder
//! tells it: `ofl/` (SIL OFL 1.1, nearly all), `apache/` (Apache 2.0), `ufl/` (Ubuntu
//! Font Licence). All three allow embedding and redistributing the fonts unmodified.

use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};

use crate::library::Git;

/// The repository `fonts add` reads (`SHOTGLASS_GOOGLE_FONTS` overrides it, for
/// mirrors and tests).
pub const GOOGLE_FONTS: &str = "https://github.com/google/fonts";

/// google/fonts' licence folders and their SPDX ids.
const LICENCE_DIRS: &[(&str, &str)] = &[
    ("ofl", "OFL-1.1"),
    ("apache", "Apache-2.0"),
    ("ufl", "Ubuntu-font-1.0"),
];

/// The licence files a family folder may carry.
const LICENCE_FILES: &[&str] = &["OFL.txt", "LICENSE.txt", "UFL.txt"];

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

/// What `fonts add` needs from a family's `METADATA.pb` (protobuf text format).
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
    let mut font = FontFile {
        filename: String::new(),
        italic: false,
        weight: 400,
    };
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
                "fonts" => {
                    font = FontFile {
                        filename: String::new(),
                        italic: false,
                        weight: 400,
                    }
                }
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

/// The name a font file gets in the film: google/fonts' variable fonts carry their axes
/// in brackets (`Inter[opsz,wght].ttf`), awkward in URLs and shells, so they become
/// `Inter-Variable.ttf` (`Inter-Italic[opsz,wght].ttf` → `Inter-Italic-Variable.ttf`).
pub fn local_name(filename: &str) -> String {
    match (filename.find('['), filename.rfind(']')) {
        (Some(a), Some(b)) if a < b => {
            let stem = filename[..a].trim_end_matches('-');
            format!("{stem}-Variable{}", &filename[b + 1..])
        }
        _ => filename.to_string(),
    }
}

/// The `[fonts]` name for a file: the family slug for the upright regular (or the
/// upright variable font), else slug + weight name (+ `_italic`).
pub fn font_key(slug: &str, f: &FontFile, variable: bool) -> String {
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
    let mut key = slug.to_string();
    if !variable && !weight.is_empty() {
        key += "_";
        key += weight;
    }
    if f.italic {
        key += "_italic";
    }
    // A [fonts] name is an identifier: it can't start with a digit.
    if key.starts_with(|c: char| c.is_ascii_digit()) {
        key.insert_str(0, "font_");
    }
    key
}

/// What one family came to: its folder in the film and the `[fonts]` entries.
#[derive(Debug, PartialEq)]
pub struct Added {
    pub family: String,
    pub folder: String,
    pub licence: String,
    /// (key, film-relative path).
    pub entries: Vec<(String, String)>,
    pub axes: Vec<(String, f32, f32)>,
}

/// Fetch `families` from `source` (at `rev`, or its default branch) into `film`'s
/// `fonts/` folder and add `[fonts]` entries to its `movie.toml`. Returns what was
/// added and the commit it came from.
pub fn add(
    film: &Path,
    families: &[String],
    source: &str,
    rev: Option<&str>,
) -> Result<(Vec<Added>, String)> {
    if families.is_empty() {
        bail!("name at least one family, as on fonts.google.com (e.g. \"Lobster\")");
    }
    let movie = film.join(crate::project::MOVIE_FILE);
    let toml_text =
        std::fs::read_to_string(&movie).with_context(|| format!("reading {}", movie.display()))?;
    let mut doc: toml_edit::DocumentMut = toml_text
        .parse()
        .with_context(|| format!("parsing {}", movie.display()))?;
    let slugs: Vec<String> = families.iter().map(|f| slug(f)).collect();
    for (f, s) in families.iter().zip(&slugs) {
        if s.is_empty() {
            bail!("\"{f}\" isn't a family name");
        }
        let dest = film.join("fonts").join(s);
        if dest.exists() {
            bail!(
                "{} is already there; delete it to fetch \"{f}\" again",
                dest.display()
            );
        }
    }
    let tmp = std::env::temp_dir().join(format!(
        "shotglass-fonts-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    let result = (|| {
        let commit = sparse_fetch(source, rev, &slugs, &tmp)?;
        let mut added = vec![];
        for (family, s) in families.iter().zip(&slugs) {
            added.push(copy_family(&tmp, family, s, source, &commit, film)?);
        }
        Ok::<_, anyhow::Error>((added, commit))
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    let (added, commit) = result?;
    // [fonts] entries, keeping the rest of movie.toml (comments, order) as it is.
    let fonts = doc
        .entry("fonts")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| anyhow!("{}: fonts isn't a table", movie.display()))?;
    for a in &added {
        for (key, path) in &a.entries {
            if fonts.contains_key(key) {
                continue;
            }
            let mut entry = toml_edit::InlineTable::new();
            entry.insert("file", path.as_str().into());
            entry.insert("licence", a.licence.as_str().into());
            fonts.insert(key, toml_edit::value(entry));
        }
    }
    std::fs::write(&movie, doc.to_string())
        .with_context(|| format!("writing {}", movie.display()))?;
    Ok((added, commit))
}

/// A sparse, blobless, shallow checkout of the families' candidate folders (every
/// licence folder) into `dest`. Returns the commit.
fn sparse_fetch(source: &str, rev: Option<&str>, slugs: &[String], dest: &Path) -> Result<String> {
    let d = dest.to_string_lossy().into_owned();
    let git = |args: &[&str]| -> Result<String> {
        let mut a = vec!["-C", d.as_str(), "-c", "core.autocrlf=false"];
        a.extend_from_slice(args);
        Git::run(&a)
    };
    std::fs::create_dir_all(dest)?;
    Git::run(&["init", "--quiet", "--", &d])?;
    git(&["remote", "add", "origin", source])?;
    let mut patterns = vec!["sparse-checkout", "set", "--no-cone"];
    let dirs: Vec<String> = slugs
        .iter()
        .flat_map(|s| LICENCE_DIRS.iter().map(move |(l, _)| format!("/{l}/{s}/")))
        .collect();
    patterns.extend(dirs.iter().map(String::as_str));
    git(&patterns)?;
    let what = rev.unwrap_or("HEAD");
    git(&[
        "fetch",
        "--quiet",
        "--depth",
        "1",
        "--filter=blob:none",
        "origin",
        what,
    ])
    .with_context(|| format!("fetching {what} of {source}"))?;
    git(&[
        "-c",
        "advice.detachedHead=false",
        "checkout",
        "--quiet",
        "FETCH_HEAD",
    ])?;
    Ok(git(&["rev-parse", "HEAD"])?.trim().to_string())
}

fn copy_family(
    checkout: &Path,
    family: &str,
    s: &str,
    source: &str,
    commit: &str,
    film: &Path,
) -> Result<Added> {
    let Some((dir, licence)) = LICENCE_DIRS
        .iter()
        .find(|(l, _)| checkout.join(l).join(s).join("METADATA.pb").is_file())
    else {
        bail!(
            "no family \"{family}\" in google/fonts (looked for {}); use the name as \
             fonts.google.com shows it",
            LICENCE_DIRS
                .iter()
                .map(|(l, _)| format!("{l}/{s}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    };
    let from = checkout.join(dir).join(s);
    let meta_text = std::fs::read_to_string(from.join("METADATA.pb"))?;
    let meta = parse_metadata(&meta_text).with_context(|| format!("{dir}/{s}/METADATA.pb"))?;
    let to: PathBuf = film.join("fonts").join(s);
    std::fs::create_dir_all(&to)?;
    let mut entries: Vec<(String, String)> = vec![];
    let mut renamed = vec![];
    for f in &meta.fonts {
        let local = local_name(&f.filename);
        let rel = format!("fonts/{s}/{local}");
        if entries.iter().any(|(_, p)| *p == rel) {
            continue; // a variable font listed once per named style
        }
        std::fs::copy(from.join(&f.filename), to.join(&local))
            .with_context(|| format!("{dir}/{s}/{} is listed but missing", f.filename))?;
        if local != f.filename {
            renamed.push(format!("{} → {local}", f.filename));
        }
        let variable = f.filename.contains('[');
        let mut key = font_key(s, f, variable);
        while entries.iter().any(|(k, _)| *k == key) {
            key += "_2";
        }
        entries.push((key, rel));
    }
    let mut licence_file = None;
    for l in LICENCE_FILES {
        if from.join(l).is_file() {
            std::fs::copy(from.join(l), to.join(l))?;
            licence_file = Some(*l);
        }
    }
    if licence_file.is_none() {
        bail!(
            "{dir}/{s} has no licence file ({})",
            LICENCE_FILES.join(", ")
        );
    }
    std::fs::copy(from.join("METADATA.pb"), to.join("METADATA.pb"))?;
    std::fs::write(
        to.join("SOURCE.txt"),
        format!(
            "{} ({licence}), fetched by `shotglass fonts add` from\n{source} at commit {commit},\n\
             folder {dir}/{s}/. The font files are unmodified{}.\n",
            meta.name,
            if renamed.is_empty() {
                String::new()
            } else {
                format!(" (renamed: {})", renamed.join(", "))
            }
        ),
    )?;
    Ok(Added {
        family: meta.name,
        folder: format!("fonts/{s}"),
        licence: licence.to_string(),
        entries,
        axes: meta.axes,
    })
}

/// `shotglass fonts add`: fetch, copy, add entries, and say what happened.
pub fn run_add(film: &Path, families: &[String], rev: Option<&str>) -> Result<()> {
    let source = std::env::var("SHOTGLASS_GOOGLE_FONTS").unwrap_or_else(|_| GOOGLE_FONTS.into());
    let (added, commit) = add(film, families, &source, rev)?;
    for a in &added {
        println!("{} ({}) → {}/", a.family, a.licence, a.folder);
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
        "from {source} at {}; commit the fonts/ folder with the film",
        &commit[..commit.len().min(12)]
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
date_added: "2010-05-10"
fonts {
  name: "Lobster"
  style: "normal"
  weight: 400
  filename: "Lobster-Regular.ttf"
  post_script_name: "Lobster-Regular"
  full_name: "Lobster Regular"
  copyright: "Copyright 2010 The Lobster Project Authors"
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
    fn slugs_match_google_fonts_folders() {
        assert_eq!(slug("Lobster"), "lobster");
        assert_eq!(slug("Atkinson Hyperlegible"), "atkinsonhyperlegible");
        assert_eq!(slug("Noto Sans JP"), "notosansjp");
        assert_eq!(slug("M PLUS 1p"), "mplus1p");
        assert_eq!(slug("  "), "");
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
    fn names_and_keys() {
        assert_eq!(local_name("Inter[opsz,wght].ttf"), "Inter-Variable.ttf");
        assert_eq!(
            local_name("Inter-Italic[opsz,wght].ttf"),
            "Inter-Italic-Variable.ttf"
        );
        assert_eq!(local_name("Lobster-Regular.ttf"), "Lobster-Regular.ttf");
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
        assert_eq!(font_key("21cent", &f(400, false), false), "font_21cent");
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
        // Something big elsewhere that a sparse fetch must not bring along.
        std::fs::create_dir_all(dir.join("ofl/other")).unwrap();
        std::fs::write(dir.join("ofl/other/Other.ttf"), vec![7u8; 4096]).unwrap();
        let d = dir.to_string_lossy().into_owned();
        let git = |args: &[&str]| {
            let mut a = vec!["-C", d.as_str()];
            a.extend_from_slice(args);
            Git::run(&a).unwrap();
        };
        Git::run(&["init", "--quiet", "--", &d]).unwrap();
        git(&["config", "uploadpack.allowFilter", "true"]);
        git(&["config", "uploadpack.allowAnySHA1InWant", "true"]);
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ]);
        format!("file://{}", dir.display())
    }

    #[test]
    fn add_copies_families_and_writes_entries() {
        if Git::run(&["--version"]).is_err() {
            eprintln!("skipped: no git");
            return;
        }
        let base =
            std::env::temp_dir().join(format!("shotglass-fonts-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let source = fixture_repo(&base.join("repo"));
        let film = base.join("film");
        std::fs::create_dir_all(&film).unwrap();
        std::fs::write(
            film.join("movie.toml"),
            "# my film\ntitle = \"t\"\n\n[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n",
        )
        .unwrap();
        let (added, commit) = add(
            &film,
            &["Test Sans".to_string(), "Test Mono".to_string()],
            &source,
            None,
        )
        .unwrap();
        assert_eq!(commit.len(), 40);
        assert_eq!(added[0].licence, "OFL-1.1");
        assert_eq!(
            added[0].entries,
            vec![(
                "testsans".to_string(),
                "fonts/testsans/TestSans-Variable.ttf".to_string()
            )]
        );
        assert_eq!(added[1].licence, "Apache-2.0");
        let keys: Vec<&str> = added[1].entries.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["testmono", "testmono_bold"]);
        // Files, licence, metadata and provenance are in the film; nothing else came.
        let f = film.join("fonts/testsans");
        for name in [
            "TestSans-Variable.ttf",
            "OFL.txt",
            "METADATA.pb",
            "SOURCE.txt",
        ] {
            assert!(f.join(name).is_file(), "{name}");
        }
        let source_txt = std::fs::read_to_string(f.join("SOURCE.txt")).unwrap();
        assert!(source_txt.contains(&commit) && source_txt.contains("ofl/testsans/"));
        assert!(source_txt.contains("TestSans[wdth,wght].ttf → TestSans-Variable.ttf"));
        assert!(film.join("fonts/testmono/LICENSE.txt").is_file());
        // movie.toml keeps its comment and gains [fonts]; the film loads and lays out.
        let toml = std::fs::read_to_string(film.join("movie.toml")).unwrap();
        assert!(toml.starts_with("# my film\n"), "{toml}");
        assert!(toml.contains("testsans = { file = \"fonts/testsans/TestSans-Variable.ttf\", licence = \"OFL-1.1\" }"), "{toml}");
        std::fs::write(
            film.join("movie.toml"),
            format!("{toml}\n[texts]\nt = {{ text = \"Hi\", font = \"testsans\" }}\n"),
        )
        .unwrap();
        let toml = std::fs::read_to_string(film.join("movie.toml")).unwrap();
        let toml = toml.replace("duration = 1\n", "duration = 1\nchannels = [\"t\"]\n");
        std::fs::write(film.join("movie.toml"), toml).unwrap();
        std::fs::write(
            film.join("a.glsl"),
            "void mainImage(out vec4 c, in vec2 p) { c = vec4(0.0); }\n",
        )
        .unwrap();
        let m = crate::movie::Movie::load(&film).unwrap();
        let t = m.shots[0].channels[0].as_ref().unwrap().text().unwrap();
        assert_eq!(t.fonts[0].licence.as_deref(), Some("OFL-1.1"));
        // Asking again for a family that's there is refused; an unknown family is named.
        let e = add(&film, &["Test Sans".to_string()], &source, None).unwrap_err();
        assert!(e.to_string().contains("is already there"), "{e}");
        let e = add(&film, &["Nope Font".to_string()], &source, None).unwrap_err();
        assert!(format!("{e:#}").contains("no family \"Nope Font\" in google/fonts (looked for ofl/nopefont, apache/nopefont, ufl/nopefont)"), "{e:#}");
        assert!(!film.join("fonts/nopefont").exists());
        // A pinned commit fetches the same files.
        let pinned = base.join("film2");
        std::fs::create_dir_all(&pinned).unwrap();
        std::fs::write(pinned.join("movie.toml"), "[fonts]\n").unwrap();
        let (_, c2) = add(&pinned, &["Test Mono".to_string()], &source, Some(&commit)).unwrap();
        assert_eq!(c2, commit);
        assert!(pinned.join("fonts/testmono/TestMono-Bold.ttf").is_file());
        let _ = std::fs::remove_dir_all(&base);
    }
}
