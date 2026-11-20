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
    /// Opt in to a later desktop GLSL version: "330" (the default), "410" or "430".
    glsl: Option<String>,
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
    #[serde(alias = "shader")]
    music: Option<String>,
    /// GLSL file with `vec2 mainMix(int samp, float time)`, reading music() and sfx().
    mix: Option<String>,
    /// How far back (seconds) the mix may read music()/sfx() (default 1).
    mix_lookback: Option<f32>,
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
    /// GLSL `mainSound` for this shot; `time` is shot-local, like iTime.
    sound: Option<String>,
    /// Start the sound this many seconds before the shot (a J-cut).
    sound_lead: Option<f32>,
    /// Let the sound ring on this many seconds after the shot (an L-cut).
    sound_tail: Option<f32>,
    sound_gain: Option<f32>,
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
pub struct ShotSound {
    pub path: PathBuf,
    pub lead: f32,
    pub tail: f32,
    pub gain: f32,
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
    pub sound: Option<ShotSound>,
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
    /// Desktop GLSL version the film opted in to (`glsl` in movie.toml); None = the
    /// portable baseline (330 core, or 300 es on OpenGL ES).
    pub glsl: Option<u32>,
    /// Movie-level music/ambience (`mainSound`, movie time).
    pub music: Option<PathBuf>,
    /// Final mix pass (`mainMix`).
    pub mix: Option<PathBuf>,
    pub mix_lookback: f32,
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
            let lead = s.sound_lead.unwrap_or(0.0);
            let tail = s.sound_tail.unwrap_or(0.0);
            if lead < 0.0 || tail < 0.0 || lead.is_nan() || tail.is_nan() {
                bail!("{}: sound_lead and sound_tail must be >= 0", ctx());
            }
            if s.sound.is_none()
                && (s.sound_lead.is_some() || s.sound_tail.is_some() || s.sound_gain.is_some())
            {
                bail!(
                    "{}: sound_lead/sound_tail/sound_gain need sound = \"file.glsl\"",
                    ctx()
                );
            }
            let sound = s.sound.map(|p| ShotSound {
                path: root.join(p),
                lead,
                tail,
                gain: s.sound_gain.unwrap_or(1.0),
            });
            shots.push(Shot {
                sound,
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
        let glsl = match f.glsl.as_deref().map(str::trim) {
            None => None,
            Some(v) => match v.parse::<u32>() {
                Ok(n) if crate::shader::DESKTOP_VERSIONS.contains(&n) => Some(n),
                _ => bail!(
                    "glsl = \"{v}\" isn't supported; use \"330\" (the portable default), \"410\" or \"430\""
                ),
            },
        };
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
            glsl,
            volume: f.sound.as_ref().and_then(|s| s.volume).unwrap_or(1.0),
            music: f
                .sound
                .as_ref()
                .and_then(|s| s.music.as_ref())
                .map(|p| root.join(p)),
            mix: f
                .sound
                .as_ref()
                .and_then(|s| s.mix.as_ref())
                .map(|p| root.join(p)),
            mix_lookback: f.sound.as_ref().and_then(|s| s.mix_lookback).unwrap_or(1.0),
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

    pub fn has_sound(&self) -> bool {
        self.music.is_some() || self.shots.iter().any(|s| s.sound.is_some())
    }

    /// Where a shot's sound plays: (from, to) in movie seconds.
    pub fn sound_window(&self, i: usize) -> Option<(f32, f32)> {
        let s = &self.shots[i];
        let snd = s.sound.as_ref()?;
        Some((
            (s.start - snd.lead).max(0.0),
            (s.end() + snd.tail).min(self.duration),
        ))
    }

    /// Gain of shot `i`'s sound at movie time `t`: equal-power fades that follow the
    /// picture transitions, except where a lead or tail lets the sound carry over.
    pub fn sound_gain(&self, i: usize, t: f32) -> f32 {
        let Some((from, to)) = self.sound_window(i) else {
            return 0.0;
        };
        if t < from || t > to {
            return 0.0;
        }
        let s = &self.shots[i];
        let snd = s.sound.as_ref().unwrap();
        let mut g = snd.gain;
        if let (Some(tr), true) = (&s.transition_in, snd.lead == 0.0)
            && t < s.start + tr.duration
        {
            let p = tr.ease.apply((t - s.start) / tr.duration);
            g *= (p * std::f32::consts::FRAC_PI_2).sin();
        }
        if let (Some(next), true) = (self.shots.get(i + 1), snd.tail == 0.0)
            && let Some(tr) = &next.transition_in
            && t > next.start
        {
            let p = tr.ease.apply((t - next.start) / tr.duration);
            g *= (p * std::f32::consts::FRAC_PI_2).cos();
        }
        // 5 ms de-click ramps at the window edges
        let ramp = 0.005;
        g * ((t - from) / ramp).clamp(0.0, 1.0) * ((to - t) / ramp).clamp(0.0, 1.0)
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
    fn shot_sound_follows_transitions() {
        let m = Movie::parse(
            r#"
            [[shot]]
            id = "a"
            scene = "a.glsl"
            duration = 5
            sound = "a.glsl"
            [[shot]]
            id = "b"
            scene = "b.glsl"
            duration = 4
            sound = "b.glsl"
            sound_lead = 0.5
            transition = { type = "crossfade", duration = 1, ease = "linear" }
            "#,
            Path::new("."),
        )
        .unwrap();
        // a fades out across the transition (4..5) with equal power
        assert!((m.sound_gain(0, 2.0) - 1.0).abs() < 1e-6);
        assert!((m.sound_gain(0, 4.5) - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-5);
        assert!(m.sound_gain(0, 4.999) < 0.01);
        // b has a lead: it starts 0.5 s early at full level and skips the fade-in
        assert_eq!(m.sound_window(1), Some((3.5, 8.0)));
        assert_eq!(m.sound_gain(1, 3.4), 0.0);
        assert!((m.sound_gain(1, 4.2) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn transition_fades_keep_constant_power() {
        for ease in ["linear", "smooth", "inout"] {
            let m = Movie::parse(
                &format!(
                    "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=3\nsound=\"a.glsl\"\n\
                     [[shot]]\nid=\"b\"\nscene=\"b.glsl\"\nduration=3\nsound=\"b.glsl\"\n\
                     transition = {{ type = \"crossfade\", duration = 1.5, ease = \"{ease}\" }}\n"
                ),
                Path::new("."),
            )
            .unwrap();
            // b starts at 1.5; the crossfade runs 1.5..3.0 (away from the 5 ms edge ramps)
            for i in 1..100 {
                let t = 1.5 + 1.5 * i as f32 / 100.0;
                let (ga, gb) = (m.sound_gain(0, t), m.sound_gain(1, t));
                let power = ga * ga + gb * gb;
                assert!((power - 1.0).abs() < 1e-4, "{ease} at {t}: {power}");
            }
        }
    }

    mod props {
        use super::super::*;
        use proptest::prelude::*;

        /// A random but valid timeline: durations, optional transitions that fit.
        fn timeline() -> impl Strategy<Value = String> {
            prop::collection::vec((0.2f32..6.0, prop::option::of(0.05f32..1.0), any::<bool>()), 1..12).prop_map(|shots| {
                let mut s = String::new();
                let mut prev_in = 0.0f32;
                let mut prev_dur = 0.0f32;
                for (i, (dur, tr, freeze)) in shots.into_iter().enumerate() {
                    s += &format!("[[shot]]\nid=\"s{i}\"\nscene=\"x.glsl\"\nduration={dur}\n");
                    let mut this_in = 0.0;
                    if let (Some(frac), true) = (tr, i > 0) {
                        // keep it legal: fits both shots and doesn't overlap the previous one
                        let room = (prev_dur - prev_in).min(dur);
                        let d = frac * room * 0.95;
                        if d > 0.01 {
                            s += &format!("transition = {{ type = \"crossfade\", duration = {d}, freeze = {freeze} }}\n");
                            this_in = d;
                        }
                    }
                    prev_in = this_in;
                    prev_dur = dur;
                }
                s
            })
        }

        proptest! {
            #[test]
            fn at_is_consistent_everywhere(toml in timeline(), samples in prop::collection::vec(0.0f32..1.0, 1..60)) {
                let m = Movie::parse(&toml, Path::new(".")).unwrap();
                prop_assert!(m.duration > 0.0);
                for u in samples {
                    let t = u * m.duration;
                    match m.at(t) {
                        At::Shot { shot, local } => {
                            prop_assert!(shot < m.shots.len());
                            prop_assert!(local >= -1e-4 && local <= m.shots[shot].duration + 1e-4);
                            prop_assert!((m.shots[shot].start + local - t).abs() < 1e-3);
                        }
                        At::Transition { from, to, from_local, to_local, progress } => {
                            prop_assert_eq!(from + 1, to);
                            prop_assert!(m.shots[to].transition_in.is_some());
                            prop_assert!((0.0..=1.0).contains(&progress));
                            prop_assert!(from_local <= m.shots[from].duration + 1e-3);
                            prop_assert!(to_local >= -1e-4);
                        }
                    }
                }
            }

            #[test]
            fn shots_tile_the_movie(toml in timeline()) {
                let m = Movie::parse(&toml, Path::new(".")).unwrap();
                prop_assert_eq!(m.shots[0].start, 0.0);
                for w in m.shots.windows(2) {
                    // the next shot starts when the previous ends, minus its transition
                    let d = w[1].transition_in.as_ref().map_or(0.0, |t| t.duration);
                    prop_assert!((w[1].start - (w[0].end() - d)).abs() < 1e-4);
                    prop_assert!(w[1].start >= w[0].start);
                }
                prop_assert!((m.duration - m.shots.last().unwrap().end()).abs() < 1e-4);
            }

            #[test]
            fn parser_never_panics(text in "\\PC{0,300}") {
                let _ = Movie::parse(&text, Path::new("."));
            }

            #[test]
            fn parser_never_panics_on_near_valid_input(toml in timeline(), cut in 0usize..400, junk in "[\\[\\]{}=\",.0-9a-z_ \n-]{0,12}") {
                let mut t = toml.clone();
                let at = cut.min(t.len());
                if t.is_char_boundary(at) {
                    t.insert_str(at, &junk);
                }
                let _ = Movie::parse(&t, Path::new("."));
            }
        }
    }

    #[test]
    fn glsl_opt_in() {
        let shot = "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=1\n";
        assert_eq!(Movie::parse(shot, Path::new(".")).unwrap().glsl, None);
        for v in ["330", "410", "430"] {
            let m = Movie::parse(&format!("glsl = \"{v}\"\n{shot}"), Path::new(".")).unwrap();
            assert_eq!(m.glsl, Some(v.parse().unwrap()));
        }
        for bad in ["\"450\"", "\"300 es\"", "\"\"", "430"] {
            let e = Movie::parse(&format!("glsl = {bad}\n{shot}"), Path::new("."));
            assert!(e.is_err(), "glsl = {bad} should be refused");
        }
        let e = Movie::parse(&format!("glsl = \"460\"\n{shot}"), Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(e.contains("\"410\" or \"430\""), "{e}");
    }

    #[test]
    fn rejects_unknown_fields() {
        let bad = "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=1\nlenght=2\n";
        assert!(Movie::parse(bad, Path::new(".")).is_err());
    }
}
