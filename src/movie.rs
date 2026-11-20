//! `movie.toml` parsing and the timeline: which shot (or transition) is on screen at time t.

use crate::anim::{Anim, Ease, vector};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- file format

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MovieFile {
    title: Option<String>,
    /// Width:height, e.g. [16, 9]. Default 16:9.
    aspect: Option<[f32; 2]>,
    /// Frame rate used for frame stepping and iFrame. Playback itself runs at the display rate.
    fps: Option<f32>,
    sound: Option<SoundFile>,
    #[serde(rename = "shot", default)]
    shots: Vec<ShotFile>,
    #[serde(rename = "marker", default)]
    markers: Vec<MarkerFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SoundFile {
    /// GLSL file with `vec2 mainSound(int samp, float time)`; time is movie time.
    shader: String,
    volume: Option<f32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShotFile {
    id: String,
    scene: String,
    duration: f32,
    /// iTime at the start of the shot (default 0). Lets two shots use different moments of one scene.
    time_offset: Option<f32>,
    /// iTime speed (default 1).
    time_scale: Option<f32>,
    #[serde(default)]
    uniforms: BTreeMap<String, toml::Value>,
    /// Transition *into* this shot from the previous one. No transition = hard cut.
    transition: Option<TransitionFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitionFile {
    #[serde(rename = "type")]
    kind: String,
    duration: f32,
    #[serde(default)]
    freeze: bool,
    ease: Option<String>,
    #[serde(default)]
    params: BTreeMap<String, toml::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkerFile {
    t: f32,
    label: String,
}

// ---------------------------------------------------------------- resolved movie

#[derive(Clone, Debug)]
pub struct Transition {
    /// Built-in name (crossfade, fade, iris, dissolve, wipe) or a path to a .glsl file.
    pub kind: String,
    pub duration: f32,
    /// Render the outgoing shot once (its frame at the transition start) instead of live.
    pub freeze: bool,
    pub ease: Ease,
    /// Become `#define name value` in the transition shader.
    pub params: Vec<(String, Vec<f32>)>,
}

#[derive(Clone, Debug)]
pub struct Shot {
    pub id: String,
    pub index: usize,
    pub start: f32,
    pub duration: f32,
    pub scene: PathBuf,
    pub time_offset: f32,
    pub time_scale: f32,
    pub uniforms: Vec<(String, Anim)>,
    pub transition_in: Option<Transition>,
}

impl Shot {
    pub fn end(&self) -> f32 {
        self.start + self.duration
    }
    /// Time at which the incoming transition has finished.
    pub fn settled(&self) -> f32 {
        self.start + self.transition_in.as_ref().map_or(0.0, |t| t.duration)
    }
    /// iTime for the scene shader at shot-local time `local`.
    pub fn scene_time(&self, local: f32) -> f32 {
        self.time_offset + local * self.time_scale
    }
}

#[derive(Clone, Debug)]
pub struct Marker {
    pub t: f32,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct Movie {
    pub root: PathBuf,
    pub title: String,
    pub aspect: f32,
    pub fps: f32,
    pub sound: Option<PathBuf>,
    pub volume: f32,
    pub shots: Vec<Shot>,
    pub markers: Vec<Marker>,
    pub duration: f32,
}

/// What is on screen at a moment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum At {
    Shot {
        shot: usize,
        local: f32,
    },
    Transition {
        from: usize,
        to: usize,
        from_local: f32,
        to_local: f32,
        /// 0..1, linear.
        progress: f32,
    },
}

impl Movie {
    /// Load `movie.toml` from a movie folder (or the path of the toml itself).
    pub fn load(path: &Path) -> Result<Movie> {
        let file = if path.is_dir() {
            path.join("movie.toml")
        } else {
            path.to_path_buf()
        };
        let root = file.parent().map(Path::to_path_buf).unwrap_or_default();
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        Movie::parse(&text, &root).with_context(|| format!("in {}", file.display()))
    }

    pub fn parse(text: &str, root: &Path) -> Result<Movie> {
        let f: MovieFile = toml::from_str(text)?;
        if f.shots.is_empty() {
            bail!("a movie needs at least one [[shot]]");
        }
        let mut shots: Vec<Shot> = Vec::new();
        let mut ids = std::collections::HashSet::new();
        for (index, s) in f.shots.into_iter().enumerate() {
            let ctx = || format!("shot \"{}\"", s.id);
            if !ids.insert(s.id.clone()) {
                bail!("duplicate shot id \"{}\"", s.id);
            }
            if s.id.is_empty()
                || !s
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                bail!("shot id \"{}\" may only use letters, digits, _ and -", s.id);
            }
            if s.duration.is_nan() || s.duration <= 0.0 {
                bail!("{}: duration must be > 0", ctx());
            }
            let mut uniforms = Vec::new();
            for (name, v) in &s.uniforms {
                if !is_ident(name) {
                    bail!("{}: \"{name}\" is not a valid GLSL name", ctx());
                }
                let a = Anim::from_toml(v).with_context(|| format!("{}: uniform {name}", ctx()))?;
                uniforms.push((name.clone(), a));
            }
            let transition_in = match s.transition {
                None => None,
                Some(t) => {
                    if index == 0 {
                        bail!("{}: the first shot can't have a transition in", ctx());
                    }
                    if t.duration.is_nan() || t.duration <= 0.0 {
                        bail!("{}: transition duration must be > 0", ctx());
                    }
                    let mut params = Vec::new();
                    for (name, v) in &t.params {
                        if !is_ident(name) {
                            bail!(
                                "{}: transition param \"{name}\" is not a valid GLSL name",
                                ctx()
                            );
                        }
                        params.push((
                            name.clone(),
                            vector(v).with_context(|| format!("{}: param {name}", ctx()))?,
                        ));
                    }
                    Some(Transition {
                        kind: t.kind,
                        duration: t.duration,
                        freeze: t.freeze,
                        ease: match t.ease {
                            Some(e) => Ease::parse(&e)?,
                            None => Ease::Smooth,
                        },
                        params,
                    })
                }
            };
            // Shots overlap by the incoming transition's length.
            let start = match shots.last() {
                None => 0.0,
                Some(p) => {
                    let d = transition_in.as_ref().map_or(0.0, |t| t.duration);
                    let prev_in = p.transition_in.as_ref().map_or(0.0, |t| t.duration);
                    if d > s.duration {
                        bail!(
                            "{}: transition ({d}s) is longer than the shot ({}s)",
                            ctx(),
                            s.duration
                        );
                    }
                    if prev_in + d > p.duration + 1e-4 {
                        bail!(
                            "{}: transition ({d}s) overlaps the previous transition; shot \"{}\" is {}s \
                             but its two transitions need {}s",
                            ctx(),
                            p.id,
                            p.duration,
                            prev_in + d
                        );
                    }
                    p.end() - d
                }
            };
            shots.push(Shot {
                id: s.id,
                index,
                start,
                duration: s.duration,
                scene: root.join(&s.scene),
                time_offset: s.time_offset.unwrap_or(0.0),
                time_scale: s.time_scale.unwrap_or(1.0),
                uniforms,
                transition_in,
            });
        }
        let duration = shots.last().unwrap().end();
        let aspect = match f.aspect {
            Some([w, h]) if w > 0.0 && h > 0.0 => w / h,
            Some(_) => bail!("aspect must be two positive numbers, e.g. [16, 9]"),
            None => 16.0 / 9.0,
        };
        Ok(Movie {
            root: root.to_path_buf(),
            title: f.title.unwrap_or_else(|| "Untitled".into()),
            aspect,
            fps: f.fps.unwrap_or(30.0).max(1.0),
            volume: f.sound.as_ref().and_then(|s| s.volume).unwrap_or(1.0),
            sound: f.sound.map(|s| root.join(s.shader)),
            shots,
            markers: f
                .markers
                .into_iter()
                .map(|m| Marker {
                    t: m.t,
                    label: m.label,
                })
                .collect(),
            duration,
        })
    }

    pub fn at(&self, t: f32) -> At {
        let t = t.clamp(0.0, self.duration);
        let i = self.shots.iter().rposition(|s| t >= s.start).unwrap_or(0);
        let s = &self.shots[i];
        if let Some(tr) = &s.transition_in
            && t < s.start + tr.duration
            && i > 0
        {
            let p = &self.shots[i - 1];
            return At::Transition {
                from: i - 1,
                to: i,
                from_local: t - p.start,
                to_local: t - s.start,
                progress: ((t - s.start) / tr.duration).clamp(0.0, 1.0),
            };
        }
        At::Shot {
            shot: i,
            local: t - s.start,
        }
    }

    pub fn shot_by_id(&self, id: &str) -> Option<&Shot> {
        self.shots.iter().find(|s| s.id == id)
    }

    /// `#define`s shared by every shader so pictures and sound agree on timing.
    pub fn cue_defines(&self) -> String {
        let mut s = format!("#define MOVIE_DURATION {}\n", glsl_float(self.duration));
        for shot in &self.shots {
            let id = shot.id.to_ascii_uppercase().replace('-', "_");
            s += &format!("#define CUE_{id}_START {}\n", glsl_float(shot.start));
            s += &format!("#define CUE_{id}_SETTLED {}\n", glsl_float(shot.settled()));
            s += &format!("#define CUE_{id}_END {}\n", glsl_float(shot.end()));
        }
        s
    }

    /// Regions worth looping: each shot, and each transition with a little padding.
    pub fn region_at(&self, t: f32) -> (f32, f32) {
        match self.at(t) {
            At::Transition { to, .. } => {
                let s = &self.shots[to];
                (
                    (s.start - 0.75).max(0.0),
                    (s.settled() + 0.75).min(self.duration),
                )
            }
            At::Shot { shot, .. } => {
                let s = &self.shots[shot];
                (s.start, s.end())
            }
        }
    }
}

pub fn glsl_float(x: f32) -> String {
    let s = format!("{x}");
    if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("NaN") {
        s
    } else {
        format!("{s}.0")
    }
}

fn is_ident(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        && !s.starts_with("gl_")
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: &str = r#"
        title = "t"
        [[shot]]
        id = "a"
        scene = "a.glsl"
        duration = 5
        [[shot]]
        id = "b"
        scene = "b.glsl"
        duration = 4
        transition = { type = "crossfade", duration = 1 }
        [[shot]]
        id = "c"
        scene = "c.glsl"
        duration = 3
    "#;

    #[test]
    fn layout_overlaps_transitions() {
        let m = Movie::parse(M, Path::new(".")).unwrap();
        let starts: Vec<f32> = m.shots.iter().map(|s| s.start).collect();
        assert_eq!(starts, vec![0.0, 4.0, 8.0]);
        assert_eq!(m.duration, 11.0);
    }

    #[test]
    fn at_resolves_shots_and_transitions() {
        let m = Movie::parse(M, Path::new(".")).unwrap();
        assert_eq!(
            m.at(1.0),
            At::Shot {
                shot: 0,
                local: 1.0
            }
        );
        match m.at(4.5) {
            At::Transition {
                from,
                to,
                from_local,
                to_local,
                progress,
            } => {
                assert_eq!((from, to), (0, 1));
                assert_eq!(from_local, 4.5);
                assert_eq!(to_local, 0.5);
                assert_eq!(progress, 0.5);
            }
            other => panic!("expected transition, got {other:?}"),
        }
        assert_eq!(
            m.at(5.0),
            At::Shot {
                shot: 1,
                local: 1.0
            }
        );
        // hard cut
        assert_eq!(
            m.at(8.0),
            At::Shot {
                shot: 2,
                local: 0.0
            }
        );
        // clamped
        assert_eq!(
            m.at(99.0),
            At::Shot {
                shot: 2,
                local: 3.0
            }
        );
        assert_eq!(
            m.at(-1.0),
            At::Shot {
                shot: 0,
                local: 0.0
            }
        );
    }

    #[test]
    fn cues_are_defines() {
        let m = Movie::parse(M, Path::new(".")).unwrap();
        let d = m.cue_defines();
        assert!(d.contains("#define CUE_B_START 4.0"));
        assert!(d.contains("#define CUE_B_SETTLED 5.0"));
        assert!(d.contains("#define MOVIE_DURATION 11.0"));
    }

    #[test]
    fn rejects_overlapping_transitions() {
        let bad = r#"
            [[shot]]
            id = "a"
            scene = "a.glsl"
            duration = 2
            [[shot]]
            id = "b"
            scene = "b.glsl"
            duration = 2
            transition = { type = "fade", duration = 1.5 }
            [[shot]]
            id = "c"
            scene = "c.glsl"
            duration = 2
            transition = { type = "fade", duration = 1.0 }
        "#;
        let e = Movie::parse(bad, Path::new(".")).unwrap_err().to_string();
        assert!(e.contains("overlaps"), "{e}");
    }

    #[test]
    fn rejects_unknown_fields() {
        let bad = "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=1\nlenght=2\n";
        assert!(Movie::parse(bad, Path::new(".")).is_err());
    }
}
