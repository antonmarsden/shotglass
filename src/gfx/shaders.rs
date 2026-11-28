//! Shader source assembly for scenes, layers, buffers, blends, transitions and mattes.

use super::*;

/// The fragment shader for a shot's scene.
pub fn scene_source(dialect: Dialect, shot: &Shot, cues: &str, movie: &Movie) -> Result<Assembled> {
    let mut header = SCENE_HEADER.to_string();
    for (name, a) in &shot.uniforms {
        header += &format!("uniform {} {name};\n", a.glsl_type());
    }
    header += cues;
    Ok(Builder::new(dialect, &header, &movie.root)
        .libraries(&movie.libraries)
        .file(&shot.scene)?
        .footer(SCENE_FOOTER)
        .build())
}

/// The fragment shader for a layer (#69): the scene header plus the layer's
/// opacity, its uniforms (with `SG_HAS_<name>` defines, so a library pass can default
/// the ones a film leaves out), its params as `#define`s, the cues, then the scene
/// (a film file or a library pass) and the layer footer.
pub fn layer_source(
    dialect: Dialect,
    layer: &Layer,
    cues: &str,
    movie: &Movie,
) -> Result<Assembled> {
    let root = &movie.root;
    let mut header = SCENE_HEADER.to_string();
    for (name, a) in &layer.uniforms {
        header += &format!("uniform {} {name};\n#define SG_HAS_{name}\n", a.glsl_type());
    }
    for (name, v) in &layer.params {
        header += &format!("#define {name} {}\n", crate::shader::define_value(v));
    }
    header += cues;
    let b = Builder::new(dialect, &header, root).libraries(&movie.libraries);
    let b = match &layer.scene {
        LayerScene::File(p) => b.file(p)?,
        LayerScene::Pass(kind) => b.pass(root, "layer pass", kind, |_| {
            anyhow::anyhow!(
                "layer \"{}\": type \"{kind}\" needs a library or a .glsl file",
                layer.id
            )
        })?,
        LayerScene::Comp(_) => {
            anyhow::bail!("layer \"{}\" shows a comp: it has no program", layer.id)
        }
    };
    Ok(b.footer(LAYER_FOOTER).build())
}

/// The fragment shader for a buffer's scene or init pass (#134): the scene header,
/// its uniforms (`SG_HAS_<name>`) and params, the cues, the file, and the buffer
/// footer (the raw state).
pub fn buffer_source(
    dialect: Dialect,
    buffer: &crate::movie::Buffer,
    file: &Path,
    cues: &str,
    movie: &Movie,
) -> Result<Assembled> {
    let mut header = SCENE_HEADER.to_string();
    for (name, a) in &buffer.uniforms {
        header += &format!("uniform {} {name};\n#define SG_HAS_{name}\n", a.glsl_type());
    }
    for (name, v) in &buffer.params {
        header += &format!("#define {name} {}\n", crate::shader::define_value(v));
    }
    header += cues;
    Ok(Builder::new(dialect, &header, &movie.root)
        .libraries(&movie.libraries)
        .file(file)?
        .footer(BUFFER_FOOTER)
        .build())
}

/// The fragment shader for a layer's blend pass (#72): the transition header, with
/// `getBase`/`getLayer`/`opacity` naming the picture below, the layer and its opacity,
/// the layer's params as `#define`s, the cues, the pass (a library's or a film file),
/// then the footer that writes `blend(uv)`.
pub fn blend_source(
    dialect: Dialect,
    layer: &Layer,
    kind: &str,
    cues: &str,
    movie: &Movie,
) -> Result<Assembled> {
    let root = &movie.root;
    let mut header = TRANSITION_HEADER.to_string();
    if layer.matte.is_some() {
        header += MATTE_HEADER;
        header += BLEND_MATTE_HEADER;
    } else {
        header += BLEND_HEADER;
    }
    for (name, v) in &layer.params {
        header += &format!("#define {name} {}\n", crate::shader::define_value(v));
    }
    header += cues;
    let b = Builder::new(dialect, &header, root).libraries(&movie.libraries);
    let b = b.pass(root, "blend", kind, |_| {
        anyhow::anyhow!(
            "layer \"{}\": blend \"{kind}\" needs a library or a .glsl file",
            layer.id
        )
    })?;
    Ok(b.footer(&format!("{BLEND_FOOTER}{TRANSITION_FOOTER}"))
        .build())
}

/// Which of `iChannel0..7` an assembled scene mentions, outside the header that
/// declares them (`check` warns when a shot reads a channel it binds nothing to).
pub fn channels_read(src: &Assembled) -> [bool; N_CHANNELS] {
    let text = src.text.replacen(SCENE_HEADER, "", 1);
    let mut out = [false; N_CHANNELS];
    for (n, name) in CHANNEL_UNIFORMS.iter().enumerate() {
        out[n] = crate::shader::mentions_ident(&text, name);
    }
    out
}

/// The fragment shader for a transition (a built-in name or a path ending in .glsl).
pub fn transition_source(
    dialect: Dialect,
    kind: &str,
    params: &[(String, Vec<f32>)],
    cues: &str,
    movie: &Movie,
) -> Result<Assembled> {
    let root = &movie.root;
    let mut header = TRANSITION_HEADER.to_string();
    for (name, v) in params {
        header += &format!("#define {name} {}\n", crate::shader::define_value(v));
    }
    header += cues;
    let b = Builder::new(dialect, &header, root).libraries(&movie.libraries);
    let b = match kind {
        "copy" => b.builtin("copy", COPY)?,
        "over" => b.builtin("over", OVER)?,
        _ => b.pass(root, "transition", kind, |_| {
            anyhow::anyhow!(
                "unknown transition \"{kind}\"; use a library's (\"transitions/iris\") or \
             a file in the film (\"fx/iris.glsl\")"
            )
        })?,
    };
    Ok(b.footer(TRANSITION_FOOTER).build())
}

/// The matted over pass (#74): the over pass with the layer × the matte.
pub fn over_matte_source(dialect: Dialect, cues: &str, movie: &Movie) -> Result<Assembled> {
    let mut header = TRANSITION_HEADER.to_string();
    header += MATTE_HEADER;
    header += cues;
    Ok(Builder::new(dialect, &header, &movie.root)
        .libraries(&movie.libraries)
        .builtin("over-matte", OVER_MATTE)?
        .footer(TRANSITION_FOOTER)
        .build())
}

/// The vertex shader every program uses: one full-screen triangle.
pub fn vertex_source(dialect: Dialect) -> String {
    format!("{}{VERTEX}", dialect.version_line())
}
