//! Export the shaders assembled by the film checker or the GPU test harness.

use crate::shader::Dialect;
use crate::validate::{Source, Stage, fullscreen_vertex_source};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
struct Manifest {
    schema: u32,
    dialect: String,
    programs: Vec<Program>,
}

#[derive(Serialize)]
struct Program {
    label: String,
    vertex: &'static str,
    fragment: String,
    /// Index = GLSL source string number in fragment diagnostics.
    sources: Vec<String>,
}

pub fn run(target: &Path, web: bool, out: &Path) -> Result<()> {
    let (dialect, sources) = if target.is_file()
        && target
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".test.glsl"))
    {
        let files = crate::test_files(vec![target.to_path_buf()])?;
        let file = &files[0];
        let dialect = if web {
            Dialect::WEB
        } else {
            Dialect::desktop(file.desktop_opt_in(Default::default()))
        };
        let fragment = crate::glsl_test::prepare(file, dialect)
            .map(|prepared| prepared.asm)
            .map_err(|messages| anyhow::anyhow!("{}", messages.join("\n")));
        (
            dialect,
            vec![
                fullscreen_vertex_source(dialect),
                Source {
                    what: file.display.clone(),
                    stage: Stage::Fragment,
                    src: fragment,
                },
            ],
        )
    } else {
        if target.is_file() && target.extension().is_some_and(|ext| ext == "glsl") {
            bail!(
                "{}: use a film or a *.test.glsl file to supply a shader entry point",
                target.display()
            );
        }
        let movie = crate::load_movie(target)?;
        let dialect = if web {
            Dialect::WEB
        } else {
            Dialect::desktop(movie.glsl)
        };
        (dialect, crate::strict_sources(&movie, dialect)?)
    };
    write(out, dialect, sources)
}

fn write(out: &Path, dialect: Dialect, sources: Vec<Source>) -> Result<()> {
    const VERTEX: &str = "fullscreen.vert";
    let mut vertex = None;
    let mut fragments = Vec::new();
    let mut programs = Vec::new();

    // Resolve every source before creating any output.
    for Source { what, stage, src } in sources {
        let src = src.with_context(|| format!("assembling {what}"))?;
        match stage {
            Stage::Vertex => {
                if vertex.replace(src.text).is_some() {
                    bail!("expected one shared vertex shader");
                }
            }
            Stage::Fragment => {
                let fragment = format!("shader-{:03}.frag", programs.len() + 1);
                fragments.push((fragment.clone(), src.text));
                programs.push(Program {
                    label: what,
                    vertex: VERTEX,
                    fragment,
                    sources: src.files,
                });
            }
        }
    }
    let vertex = vertex.context("missing shared vertex shader")?;
    let count = programs.len();
    let manifest = serde_json::to_string_pretty(&Manifest {
        schema: 1,
        dialect: dialect.to_string(),
        programs,
    })?;

    std::fs::create_dir_all(out)?;
    if std::fs::read_dir(out)?.next().is_some() {
        bail!("{} already exists and isn't empty", out.display());
    }

    std::fs::write(out.join(VERTEX), vertex)?;
    for (name, text) in fragments {
        std::fs::write(out.join(name), text)?;
    }
    std::fs::write(out.join("manifest.json"), format!("{manifest}\n"))?;
    println!("wrote {count} shader program(s) to {}", out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_web_test_with_includes_and_harness() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("noise.test.glsl");
        let out = dir.path().join("out");
        std::fs::write(
            dir.path().join("helper.glsl"),
            "float helper() { return 1.0; }",
        )?;
        std::fs::write(
            &target,
            "#include \"helper.glsl\"\nvoid test_helper() { expect_eq(helper(), 1.0); }",
        )?;
        run(&target, true, &out)?;

        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(out.join("manifest.json"))?)?;
        assert_eq!(manifest["schema"], 1);
        let programs = manifest["programs"].as_array().unwrap();
        assert_eq!(programs.len(), 1);
        let program = &programs[0];
        let vertex = std::fs::read_to_string(out.join(program["vertex"].as_str().unwrap()))?;
        let fragment = std::fs::read_to_string(out.join(program["fragment"].as_str().unwrap()))?;
        assert!(vertex.starts_with("#version 300 es\n"));
        assert!(fragment.starts_with("#version 300 es\n"));
        assert!(fragment.contains("float helper()") && fragment.contains("void main()"));
        assert!(
            program["sources"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file == "helper.glsl")
        );
        assert!(run(&target, true, &out).is_err());
        Ok(())
    }
}
