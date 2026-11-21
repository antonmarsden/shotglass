//! Strict, driver-independent GLSL validation for `shotglass check`.
//!
//! Drivers differ: NVIDIA accepts built-ins from GLSL versions newer than the
//! `#version` line and some outright invalid code, so "it compiles here" proves
//! little. This module runs every shader a film compiles through Khronos glslang, the
//! GLSL reference front end, at a fixed dialect (the desktop baseline, the film's
//! opt-in, or GLSL ES 3.00 for the web), and maps its messages back to file:line.
//!
//! The entry point for other tools (the web player, #2) is [`validate_movie`].

use crate::gfx::{ShaderError, scene_source, transition_source, vertex_source};
use crate::movie::Movie;
use crate::shader::{Assembled, Dialect, remap_log};
use crate::sound::{mix_source, music_source, shot_sound_source};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Vertex,
    Fragment,
}

/// One shader a film compiles.
pub struct Source {
    /// What it is, for messages: `shot "moon" (scenes/moon.glsl)`.
    pub what: String,
    pub stage: Stage,
    /// The assembled text, or why it couldn't be assembled (a missing file).
    pub src: anyhow::Result<Assembled>,
}

/// Every shader `m` compiles, assembled in dialect `d`: the shared vertex shader, the
/// copy pass, each shot's scene and incoming transition, and the sound shaders
/// (music, per-shot sounds, mix). Identical sources appear once.
pub fn movie_sources(m: &Movie, d: Dialect) -> Vec<Source> {
    let cues = m.cue_defines();
    let rel = |p: &std::path::Path| {
        p.strip_prefix(&m.root)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/")
    };
    let mut out = vec![Source {
        what: "vertex shader".into(),
        stage: Stage::Vertex,
        src: Ok(Assembled {
            text: vertex_source(d),
            dialect: d,
            files: vec!["<shotglass vertex shader>".into()],
            warnings: vec![],
            clashes: vec![],
        }),
    }];
    let mut frag = |what: String, src| {
        out.push(Source {
            what,
            stage: Stage::Fragment,
            src,
        })
    };
    frag(
        "copy pass".into(),
        transition_source(d, "copy", &[], &cues, m),
    );
    for shot in &m.shots {
        if let Some(t) = &shot.transition_in {
            frag(
                format!("transition into \"{}\" ({})", shot.id, t.kind),
                transition_source(d, &t.kind, &t.params, &cues, m),
            );
        }
        frag(
            format!("shot \"{}\" ({})", shot.id, rel(&shot.scene)),
            scene_source(d, shot, &cues, m),
        );
    }
    if let Some(p) = &m.music {
        frag(format!("music ({})", rel(p)), music_source(d, m, &cues, p));
    }
    for (i, shot) in m.shots.iter().enumerate() {
        if let Some(snd) = &shot.sound {
            frag(
                format!("sound for \"{}\" ({})", shot.id, rel(&snd.path)),
                shot_sound_source(d, m, &cues, i),
            );
        }
    }
    if let Some(p) = &m.mix {
        frag(format!("mix ({})", rel(p)), mix_source(d, m, &cues, p));
    }
    let mut seen = HashSet::new();
    out.retain(|s| match &s.src {
        Ok(a) => seen.insert(a.text.clone()),
        Err(_) => true,
    });
    out
}

/// Validate every shader of `m` as dialect `d` (e.g. [`Dialect::BASELINE`],
/// `Dialect::desktop(m.glsl)` or [`Dialect::WEB`]), without a GPU. Returns one
/// entry per failing shader, messages as `file:line: error: ...`; empty = all good.
pub fn validate_movie(m: &Movie, d: Dialect) -> Vec<ShaderError> {
    validate_sources(&movie_sources(m, d))
}

/// Whether this build can validate (the `strict-glsl` feature).
pub const AVAILABLE: bool = cfg!(feature = "strict-glsl");

/// Validate sources from [`movie_sources`]; one entry per failing shader.
pub fn validate_sources(sources: &[Source]) -> Vec<ShaderError> {
    sources
        .iter()
        .filter_map(|s| {
            let messages = match &s.src {
                Ok(a) => a.explain(explain_later_versions(
                    a,
                    s.stage,
                    validate(a, s.stage).err()?,
                )),
                Err(e) => vec![format!("{e:#}")],
            };
            Some(ShaderError {
                what: s.what.clone(),
                messages,
            })
        })
        .collect()
}

/// Point out errors that go away at the newest desktop version a film may opt in to:
/// they're almost always built-ins from a later GLSL (`textureQueryLevels`, ...).
fn explain_later_versions(src: &Assembled, stage: Stage, messages: Vec<String>) -> Vec<String> {
    let newest = Dialect::desktop(crate::shader::DESKTOP_VERSIONS.last().copied());
    let old_line = src.dialect.version_line();
    if src.dialect.es || src.dialect.version >= newest.version || !src.text.starts_with(&old_line) {
        return messages;
    }
    let newer = Assembled {
        text: format!("{}{}", newest.version_line(), &src.text[old_line.len()..]),
        dialect: newest,
        files: src.files.clone(),
        warnings: vec![],
        clashes: vec![],
    };
    let still = validate(&newer, stage).err().unwrap_or_default();
    messages
        .into_iter()
        .map(|m| {
            if still.contains(&m) || m.contains("not available in") {
                m
            } else {
                format!(
                    "{m} (valid in {newest}: this film compiles as {}, the portable baseline)",
                    src.dialect
                )
            }
        })
        .collect()
}

/// Run one assembled shader through the reference front end. Err = messages mapped
/// to file:line through the `#line` markers the assembler wrote.
#[cfg(feature = "strict-glsl")]
pub fn validate(src: &Assembled, stage: Stage) -> Result<(), Vec<String>> {
    use glslang::{
        Compiler, CompilerOptions, ShaderInput, ShaderMessage, ShaderSource, ShaderStage,
        SourceLanguage, Target,
    };
    let compiler = Compiler::acquire()
        .ok_or_else(|| vec!["couldn't start the GLSL reference compiler (glslang)".into()])?;
    if src.text.contains('\0') {
        return Err(vec!["shader source contains a NUL byte".into()]);
    }
    let source = ShaderSource::from(src.text.as_str());
    let options = CompilerOptions {
        source_language: SourceLanguage::GLSL,
        // No client API and no SPIR-V: plain GLSL rules, as an OpenGL driver applies them.
        target: Target::None(None),
        version_profile: None, // taken from the #version line
        messages: ShaderMessage::DEFAULT,
    };
    let stage = match stage {
        Stage::Vertex => ShaderStage::Vertex,
        Stage::Fragment => ShaderStage::Fragment,
    };
    let input = ShaderInput::new(
        &source,
        stage,
        &options,
        None::<&[(&str, Option<&str>)]>,
        None,
    )
    .map_err(|e| vec![format!("glslang: {e}")])?;
    match compiler.create_shader(input) {
        Ok(_) => Ok(()),
        Err(glslang::error::GlslangError::ParseError(log))
        | Err(glslang::error::GlslangError::PreprocessError(log)) => {
            let messages = tidy_log(&log.log, src);
            Err(if messages.is_empty() {
                vec!["rejected by glslang (no message)".into()]
            } else {
                messages
            })
        }
        Err(e) => Err(vec![format!("glslang: {e}")]),
    }
}

#[cfg(not(feature = "strict-glsl"))]
pub fn validate(_src: &Assembled, _stage: Stage) -> Result<(), Vec<String>> {
    Err(vec![
        "this shotglass was built without the strict-glsl feature, so it can't validate shaders"
            .into(),
    ])
}

/// glslang's log → `file:line: error: ...` lines. Drops its summary lines and turns
/// "required extension not requested" (a built-in from a later version) into advice
/// that fits the fixed-version model.
#[cfg_attr(not(feature = "strict-glsl"), allow(dead_code))]
fn tidy_log(log: &str, src: &Assembled) -> Vec<String> {
    let mut kept: Vec<String> = vec![];
    for line in log.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let is_msg = line.starts_with("ERROR:") || line.starts_with("WARNING:");
        if !is_msg {
            // Continuation lines: the extension names after "Possible extensions include:".
            continue;
        }
        if line.ends_with("compilation terminated")
            || line.contains("compilation errors.")
            || line.contains("No code generated.")
        {
            continue;
        }
        let line = match line.find("required extension not requested") {
            Some(i) => format!(
                "{}not available in {} (it's from a later GLSL version or an extension)",
                &line[..i],
                src.dialect
            ),
            None => line.to_string(),
        };
        kept.push(line);
    }
    remap_log(&kept.join("\n"), &src.files)
}

#[cfg(all(test, feature = "strict-glsl"))]
mod tests {
    use super::*;
    use crate::shader::Builder;
    use std::path::Path;

    fn dir(name: &str) -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("shotglass-validate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("scenes")).unwrap();
        d
    }

    fn scene(d: Dialect, root: &Path, body: &str) -> Result<(), Vec<String>> {
        std::fs::write(root.join("scenes/a.glsl"), body).unwrap();
        let src = Builder::new(d, "uniform vec3 iResolution;\nout vec4 o;\n", root)
            .file(&root.join("scenes/a.glsl"))
            .unwrap()
            .footer("void main(){ vec4 c; mainImage(c, gl_FragCoord.xy); o = c; }")
            .build();
        validate(&src, Stage::Fragment)
    }

    const OK: &str =
        "void mainImage(out vec4 c, in vec2 p) {\n  c = vec4(p / iResolution.xy, 0.0, 1.0);\n}\n";

    #[test]
    fn later_builtins_are_flagged_at_the_baseline() {
        let root = dir("builtins");
        assert_eq!(scene(Dialect::BASELINE, &root, OK), Ok(()));
        let src = "void mainImage(out vec4 c, in vec2 p) {\n  int i = int(p.x);\n  c = vec4(float(bitfieldExtract(i, 0, 2)));\n}\n";
        let e = scene(Dialect::BASELINE, &root, src).unwrap_err();
        assert!(
            e[0].starts_with("scenes/a.glsl:3: error:") && e[0].contains("bitfieldExtract"),
            "{e:?}"
        );
        assert!(e[0].contains("not available in GLSL 330 core"), "{e:?}");
        // A later built-in glslang doesn't know at all at 330 gets a hint through
        // validate_movie (tested in `later_builtins_get_a_hint`).
        // ... but fine for a film that opted in to 4.30
        assert_eq!(scene(Dialect::desktop(Some(430)), &root, src), Ok(()));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn later_builtins_get_a_hint() {
        let root = dir("hint");
        std::fs::write(
            root.join("scenes/a.glsl"),
            "void mainImage(out vec4 c, in vec2 p) {\n  c = vec4(float(textureQueryLevels(iChannel0)));\n}\n",
        )
        .unwrap();
        let m = Movie::parse(
            "[[shot]]\nid = \"a\"\nscene = \"scenes/a.glsl\"\nduration = 1\n",
            &root,
        )
        .unwrap();
        let e = validate_movie(&m, Dialect::BASELINE);
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(
            e[0].messages[0].starts_with("scenes/a.glsl:2: error:")
                && e[0].messages[0].contains("textureQueryLevels")
                && e[0].messages[0].contains("valid in GLSL 430 core"),
            "{e:?}"
        );
        assert!(validate_movie(&m, Dialect::desktop(Some(430))).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn es_flags_implicit_int_to_float() {
        let root = dir("es");
        assert_eq!(scene(Dialect::WEB, &root, OK), Ok(()));
        // Legal desktop GLSL (implicit conversions exist since 1.20), illegal in ES.
        let src = "void mainImage(out vec4 c, in vec2 p) {\n  float f = 1;\n  c = vec4(f);\n}\n";
        assert_eq!(scene(Dialect::BASELINE, &root, src), Ok(()));
        let e = scene(Dialect::WEB, &root, src).unwrap_err();
        assert!(
            e[0].starts_with("scenes/a.glsl:2: error:") && e[0].contains("cannot convert"),
            "{e:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn deprecated_builtins_dont_crash_glslang() {
        // Without NDEBUG (.cargo/config.toml) a glslang debug assert aborts the
        // process here.
        let root = dir("deprecated");
        let src = "uniform sampler2D iChannel0;\nvoid mainImage(out vec4 c, in vec2 p) {\n  c = texture2D(iChannel0, p);\n}\n";
        assert_eq!(scene(Dialect::BASELINE, &root, src), Ok(()));
        let e = scene(Dialect::WEB, &root, src).unwrap_err();
        assert!(
            e[0].starts_with("scenes/a.glsl:3:") && e[0].contains("texture2D"),
            "{e:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn non_glsl_is_flagged() {
        let root = dir("hlsl");
        let src = "void mainImage(out vec4 c, in vec2 p) {\n  c = vec4(saturate(p.x));\n}\n";
        let e = scene(Dialect::BASELINE, &root, src).unwrap_err();
        assert!(
            e[0].starts_with("scenes/a.glsl:2:") && e[0].contains("saturate"),
            "{e:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    fn all_dialects() -> Vec<Dialect> {
        let mut v: Vec<Dialect> = crate::shader::DESKTOP_VERSIONS
            .iter()
            .map(|v| Dialect::desktop(Some(*v)))
            .collect();
        v.push(Dialect::WEB);
        v
    }

    fn expect_clean(m: &Movie, d: Dialect) {
        let errors = validate_movie(m, d);
        assert!(
            errors.is_empty(),
            "{} at {d}:\n{}",
            m.title,
            errors.iter().map(|e| e.to_string()).collect::<String>()
        );
    }

    /// Fetch a film's libraries into the user cache (git and network), as `shotglass
    /// fetch` would, writing its shotglass.lock if it has none. Returns the lock, or None
    /// when offline outside CI: the strict check of kit films is then left to CI.
    fn fetch_libraries(root: &Path) -> Option<crate::library::Lock> {
        use crate::library;
        let m = Movie::read(root).unwrap();
        let old = library::Lock::load(root).unwrap();
        let missing = library::FetchMode::Missing {
            skip: Default::default(),
        };
        match library::fetch(
            &m.library_specs,
            old.as_ref(),
            &library::Git,
            &library::cache_dir(),
            &missing,
        ) {
            Ok((lock, _)) => {
                if old.is_none() {
                    std::fs::write(root.join(library::LOCK_FILE), lock.to_text()).unwrap();
                }
                Some(lock)
            }
            Err(e) if std::env::var_os("CI").is_none() => {
                eprintln!(
                    "skipping: can't fetch the libraries of {} ({e:#})",
                    root.display()
                );
                None
            }
            Err(e) => panic!("fetching the libraries of {}: {e:#}", root.display()),
        }
    }

    /// The demo uses shotglass-kit, pinned by its committed shotglass.lock.
    #[test]
    fn demo_movie_passes_everywhere() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("movies/big-shadow");
        let lock = crate::library::Lock::load(&root).unwrap();
        assert!(lock.is_some(), "the demo's shotglass.lock is committed");
        let Some(fetched) = fetch_libraries(&root) else {
            return;
        };
        assert_eq!(
            Some(fetched),
            lock,
            "fetching must not move the demo's pins"
        );
        let m = Movie::load(&root).unwrap();
        assert!(m.warnings.is_empty(), "{:?}", m.warnings);
        for d in all_dialects() {
            expect_clean(&m, d);
        }
    }

    /// The golden film for iChannel textures has no libraries.
    #[test]
    fn channels_film_passes_everywhere() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/films/channels");
        let m = Movie::load(&root).unwrap();
        assert!(m.warnings.is_empty(), "{:?}", m.warnings);
        assert!(m.uses_channels() && m.contract_warning().is_none());
        for d in all_dialects() {
            expect_clean(&m, d);
        }
    }

    /// The template's transition comes from shotglass-kit (no built-ins, so no warnings).
    #[test]
    fn template_passes_everywhere() {
        let root = dir("template");
        crate::write_template(&root).unwrap();
        if fetch_libraries(&root).is_some() {
            let m = Movie::load(&root).unwrap();
            assert!(m.warnings.is_empty(), "{:?}", m.warnings);
            for d in all_dialects() {
                expect_clean(&m, d);
            }
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// `<kit/…>` includes resolve through the film's `[libraries]`, and errors inside a
    /// library are reported as `kit/file.glsl:line`.
    #[test]
    fn library_includes_are_checked() {
        let root = dir("library");
        let lib = root.join("vendor/kit");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(
            lib.join("library.toml"),
            "name = \"shotglass-kit/core\"\nversion = \"0.4.0\"\nlicences = [\"MIT\"]\n",
        )
        .unwrap();
        std::fs::write(
            lib.join("level.glsl"),
            "float kit_level() { return 0.5; }\n",
        )
        .unwrap();
        std::fs::write(
            lib.join("bad.glsl"),
            "// kit\nfloat kit_bad() { return 1; }\n",
        )
        .unwrap();
        let film = |include: &str| {
            std::fs::write(
                root.join("scenes/a.glsl"),
                format!("#include <kit/{include}>\n{OK}"),
            )
            .unwrap();
            Movie::load(&root).unwrap()
        };
        crate::project::write_fixture(
            &root,
            "[libraries]\nkit = { path = \"vendor/kit\", version = \"0.4\" }\n\
             [[shot]]\nid = \"a\"\nscene = \"scenes/a.glsl\"\nduration = 1\n",
        )
        .unwrap();
        let m = film("level.glsl");
        for d in all_dialects() {
            expect_clean(&m, d);
        }
        // `return 1;` from a float function: implicit int→float, invalid in GLSL ES.
        let m = film("bad.glsl");
        let errors = validate_movie(&m, Dialect::WEB);
        let text = errors.iter().map(|e| e.to_string()).collect::<String>();
        assert!(text.contains("kit/bad.glsl:2"), "{text}");
        // The film defining a function the library already does: the clash comes first,
        // naming both places, ahead of glslang's "redefinition".
        std::fs::write(
            root.join("scenes/a.glsl"),
            format!(
                "#include <kit/level.glsl>

float kit_level() {{ return 0.25; }}
{OK}"
            ),
        )
        .unwrap();
        let m = Movie::load(&root).unwrap();
        for d in all_dialects() {
            let errors = validate_movie(&m, d);
            assert_eq!(errors.len(), 1, "{d}: {errors:?}");
            let msgs = &errors[0].messages;
            assert!(
                msgs[0].contains(
                    "`kit_level()` is defined by both library \"kit\" (kit/level.glsl:1) and the film (scenes/a.glsl:3)"
                ),
                "{d}: {msgs:#?}"
            );
            assert!(msgs.len() > 1, "glslang's own message follows: {msgs:#?}");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// `type = "fx/wipe"` takes `wipe.glsl` from the library the film calls `fx`.
    #[test]
    fn library_transitions_are_checked() {
        let root = dir("libtransition");
        let lib = root.join("vendor/fx");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(
            lib.join("library.toml"),
            "name = \"me/transitions\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        let wipe = "#ifndef direction\n#define direction vec2(1.0, 0.0)\n#endif\n\
                    vec4 transition(vec2 uv) {\n\
                    \x20   float m = step(progress, dot(uv, normalize(direction)));\n\
                    \x20   return mix(getToColor(uv), getFromColor(uv), m);\n}\n";
        std::fs::write(lib.join("wipe.glsl"), wipe).unwrap();
        std::fs::write(root.join("scenes/a.glsl"), OK).unwrap();
        let film = |kind: &str| {
            crate::project::write_fixture(
                &root,
                &format!(
                    "[libraries]\nfx = {{ path = \"vendor/fx\" }}\n\
                     [[shot]]\nid = \"a\"\nscene = \"scenes/a.glsl\"\nduration = 1\n\
                     [[shot]]\nid = \"b\"\nscene = \"scenes/a.glsl\"\nduration = 1\n\
                     transition = {{ type = \"{kind}\", duration = 0.5, params = {{ direction = [0.0, 1.0] }} }}\n"
                ),
            )
            .unwrap();
            Movie::load(&root).unwrap()
        };
        let m = film("fx/wipe");
        for d in all_dialects() {
            expect_clean(&m, d);
        }
        let errors = validate_movie(&film("fx/swipe"), Dialect::BASELINE);
        assert_eq!(errors.len(), 1, "{errors:?}");
        let text = errors[0].to_string();
        assert!(
            errors[0].what.contains("transition into \"b\" (fx/swipe)")
                && text.contains("transition \"fx/swipe\":1: can't find #include <fx/swipe.glsl>")
                && text.contains("fx/swipe.glsl ("),
            "{text}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_files_are_reported() {
        let root = dir("missing");
        let m = Movie::parse(
            "[[shot]]\nid = \"a\"\nscene = \"scenes/nope.glsl\"\nduration = 1\n",
            &root,
        )
        .unwrap();
        let e = validate_movie(&m, Dialect::BASELINE);
        assert_eq!(e.len(), 1);
        assert!(e[0].what.contains("shot \"a\""), "{:?}", e[0]);
        std::fs::remove_dir_all(&root).ok();
    }
}
