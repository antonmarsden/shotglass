//! The soundtrack: a movie-level music track, one sound per shot, and an optional
//! mix pass — all GLSL, all synthesised on the GPU.
//!
//! ```text
//! music.glsl  (movie time) ───────────────────────────► music bus ─┐
//! shot sounds (shot time, shot uniforms) × fades ─────► sfx bus  ──┼─► mix.glsl ─► out
//!                                                                  └─► (or music + sfx)
//! ```
//!
//! Every track is cached by its full source text and sample range, so editing one
//! shot's sound only re-synthesises that shot.

use crate::anim::GLSL_EASE;
use crate::gfx::{Renderer, ShaderError, TRACK_WIDTH};
use crate::movie::Movie;
use crate::shader::{Assembled, Builder};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

const COMMON: &str = "\
uniform float iSampleRate;
uniform int iBlockOffset;
uniform int iBlockWidth;
out vec4 shotglass_out;
";

const SAMPLE_INDEX: &str =
    "int s = iBlockOffset + int(gl_FragCoord.y) * iBlockWidth + int(gl_FragCoord.x);";

fn mix_helpers() -> String {
    format!(
        "\
uniform sampler2D sg_music;
uniform sampler2D sg_sfx;
uniform int sg_len;
vec2 sg_fetch(sampler2D t, int i) {{
  if (i < 0 || i >= sg_len) return vec2(0.0);
  return texelFetch(t, ivec2(i % {w}, i / {w}), 0).rg;
}}
// The music track and the sum of all shot sounds (already faded for transitions).
vec2 music(int samp) {{ return sg_fetch(sg_music, samp); }}
vec2 sfx(int samp) {{ return sg_fetch(sg_sfx, samp); }}
// Peak level of the sfx bus around samp (30 ms back, 10 ms ahead): for ducking.
float sfxLevel(int samp) {{
  int ms = int(iSampleRate * 0.001);
  float m = 0.0;
  for (int k = -30; k <= 10; k += 2) {{
    vec2 v = sfx(samp + k * ms);
    m = max(m, max(abs(v.x), abs(v.y)));
  }}
  return m;
}}
",
        w = TRACK_WIDTH
    )
}

pub struct Soundtrack {
    cache: HashMap<u64, Arc<Vec<f32>>>,
}

pub struct Built {
    /// Interleaved stereo for the whole movie.
    pub samples: Vec<f32>,
    /// Changes whenever the audible result could have changed.
    pub key: u64,
    /// Tracks that had to be synthesised (the rest came from the cache).
    pub rendered: Vec<String>,
}

impl Default for Soundtrack {
    fn default() -> Self {
        Self::new()
    }
}

impl Soundtrack {
    pub fn new() -> Soundtrack {
        Soundtrack {
            cache: HashMap::new(),
        }
    }

    pub fn build(
        &mut self,
        r: &mut Renderer,
        m: &Movie,
        sr: u32,
    ) -> Result<Built, Vec<ShaderError>> {
        let total = (m.duration * sr as f32).ceil() as usize;
        let cues = m.cue_defines();
        let mut errors = vec![];
        let mut rendered = vec![];
        let mut used = vec![];
        let mut key_parts: Vec<u64> = vec![sr as u64, total as u64];

        // ---- music
        let music = match &m.music {
            None => None,
            Some(path) => {
                let what = format!("music ({})", rel(path, &m.root));
                let src = Builder::new(r.dialect(), &format!("{COMMON}{GLSL_EASE}{cues}"), &m.root)
                    .file(path)
                    .map(|b| b.footer(&format!("void main() {{\n  {SAMPLE_INDEX}\n  shotglass_out = vec4(mainSound(s, float(s) / iSampleRate), 0.0, 1.0);\n}}")).build());
                self.track(
                    r,
                    src,
                    &what,
                    0,
                    total,
                    sr,
                    (0.0, m.duration),
                    &mut used,
                    &mut rendered,
                    &mut errors,
                    &mut key_parts,
                )
            }
        };

        // ---- per-shot sounds, faded to follow the picture, summed into the sfx bus
        let mut sfx = vec![0f32; total * 2];
        for (i, shot) in m.shots.iter().enumerate() {
            let (Some(snd), Some((from, to))) = (&shot.sound, m.sound_window(i)) else {
                continue;
            };
            let first = (from * sr as f32).floor() as usize;
            let last = ((to * sr as f32).ceil() as usize).min(total);
            if last <= first {
                continue;
            }
            let mut header = format!(
                "{COMMON}{GLSL_EASE}uniform float iShotStart;\nuniform float iShotDuration;\n{cues}"
            );
            let mut assign = String::new();
            for (name, a) in &shot.uniforms {
                header += &format!("{} {name};\n", a.glsl_type());
                header += &a.glsl_fn(&format!("sg_u_{name}"));
                assign += &format!("  {name} = sg_u_{name}(t);\n");
            }
            let what = format!("sound for \"{}\" ({})", shot.id, rel(&snd.path, &m.root));
            let src = Builder::new(r.dialect(), &header, &m.root).file(&snd.path).map(|b| {
                b.footer(&format!(
                    "void main() {{\n  {SAMPLE_INDEX}\n  float t = float(s) / iSampleRate - iShotStart;\n{assign}  shotglass_out = vec4(mainSound(s, t), 0.0, 1.0);\n}}"
                ))
                .build()
            });
            key_parts.push(hash(&format!(
                "{} {} {} {}",
                snd.gain, snd.lead, snd.tail, shot.index
            )));
            if let Some(data) = self.track(
                r,
                src,
                &what,
                first,
                last - first,
                sr,
                (shot.start, shot.duration),
                &mut used,
                &mut rendered,
                &mut errors,
                &mut key_parts,
            ) {
                for (k, frame) in data.chunks_exact(2).enumerate() {
                    let idx = first + k;
                    let g = m.sound_gain(i, idx as f32 / sr as f32);
                    if g != 0.0 {
                        sfx[idx * 2] += frame[0] * g;
                        sfx[idx * 2 + 1] += frame[1] * g;
                    }
                }
            }
        }
        // Transition timing changes the fades even when no shader changed.
        key_parts.push(hash(&cues));
        for s in &m.shots {
            if let Some(t) = &s.transition_in {
                key_parts.push(hash(&format!("{} {:?}", t.duration, t.ease)));
            }
        }

        // ---- mix
        let mut out = match &m.mix {
            Some(path) => {
                let what = format!("mix ({})", rel(path, &m.root));
                let src = Builder::new(r.dialect(), &format!("{COMMON}{GLSL_EASE}{cues}{}", mix_helpers()), &m.root)
                    .file(path)
                    .map(|b| b.footer(&format!("void main() {{\n  {SAMPLE_INDEX}\n  shotglass_out = vec4(mainMix(s, float(s) / iSampleRate), 0.0, 1.0);\n}}")).build());
                match src {
                    Err(e) => {
                        errors.push(ShaderError {
                            what,
                            messages: vec![format!("{e:#}")],
                        });
                        vec![]
                    }
                    Ok(src) => {
                        let zeros;
                        let music_ref: &[f32] = match &music {
                            Some(mu) => mu,
                            None => {
                                zeros = vec![0f32; total * 2];
                                &zeros
                            }
                        };
                        let input_key = hash(&format!("{:?}", key_parts));
                        let k = hash(&format!("mix {} {input_key}", src.text));
                        used.push(k);
                        match self.cache.get(&k) {
                            Some(d) => d.as_ref().clone(),
                            None => match errors.is_empty().then(|| {
                                r.render_audio(
                                    &src,
                                    &what,
                                    0,
                                    total,
                                    sr,
                                    (0.0, m.duration),
                                    Some((music_ref, &sfx)),
                                )
                            }) {
                                Some(Ok(d)) => {
                                    rendered.push("mix".to_string());
                                    self.cache.insert(k, Arc::new(d.clone()));
                                    d
                                }
                                Some(Err(e)) => {
                                    errors.push(e);
                                    vec![]
                                }
                                None => vec![],
                            },
                        }
                    }
                }
            }
            None => {
                let mut o = sfx;
                if let Some(mu) = &music {
                    for (a, b) in o.iter_mut().zip(mu.iter()) {
                        *a += b;
                    }
                }
                o
            }
        };
        if !errors.is_empty() {
            return Err(errors);
        }
        if let Some(p) = &m.mix {
            key_parts.push(hash(&p.to_string_lossy()));
        }
        for x in &mut out {
            *x = (*x * m.volume).clamp(-1.0, 1.0);
        }
        key_parts.push(m.volume.to_bits() as u64);
        // Forget tracks this version no longer uses.
        self.cache.retain(|k, _| used.contains(k));
        Ok(Built {
            samples: out,
            key: hash(&format!("{key_parts:?}")),
            rendered,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn track(
        &mut self,
        r: &mut Renderer,
        src: anyhow::Result<Assembled>,
        what: &str,
        first: usize,
        count: usize,
        sr: u32,
        shot: (f32, f32),
        used: &mut Vec<u64>,
        rendered: &mut Vec<String>,
        errors: &mut Vec<ShaderError>,
        key_parts: &mut Vec<u64>,
    ) -> Option<Arc<Vec<f32>>> {
        let src = match src {
            Ok(s) => s,
            Err(e) => {
                errors.push(ShaderError {
                    what: what.into(),
                    messages: vec![format!("{e:#}")],
                });
                return None;
            }
        };
        let k = hash(&format!("{} {first} {count} {sr} {:?}", src.text, shot));
        used.push(k);
        key_parts.push(k);
        if let Some(d) = self.cache.get(&k) {
            return Some(d.clone());
        }
        match r.render_audio(&src, what, first, count, sr, shot, None) {
            Ok(d) => {
                let d = Arc::new(d);
                self.cache.insert(k, d.clone());
                rendered.push(what.split(" (").next().unwrap_or(what).to_string());
                Some(d)
            }
            Err(e) => {
                errors.push(e);
                None
            }
        }
    }
}

fn rel(p: &Path, root: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

fn hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}
