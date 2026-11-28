use super::*;

/// Parse a test film written as one text: `shotglass = …` and `[libraries]` go to
/// the project file (see `project::split_fixture`), the rest to movie.toml.
fn fixture(text: &str, root: &Path) -> Result<Movie> {
    let (p, m) = crate::project::split_fixture(text);
    let project = crate::project::Project::parse(&p, root)?;
    Movie::parse_in(&m, root, &project)
}

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
        transition = { type = "fx/crossfade.glsl", duration = 1 }
        [[shot]]
        id = "c"
        scene = "c.glsl"
        duration = 3
    "#;

fn with_fps(fps: &str) -> Result<Movie> {
    fixture(&format!("fps = {fps}\n{M}"), Path::new("."))
}

#[test]
fn fps_is_rational() {
    let rate = |s: &str| with_fps(s).map(|m| (m.fps_num, m.fps_den)).unwrap();
    assert_eq!(rate("30"), (30, 1));
    assert_eq!(rate("24.0"), (24, 1));
    assert_eq!(rate("\"30000/1001\""), (30000, 1001));
    assert_eq!(rate("29.97"), (30000, 1001));
    assert_eq!(rate("23.976"), (24000, 1001));
    assert_eq!(rate("59.94"), (60000, 1001));
    assert_eq!(rate("12.5"), (25, 2));
    assert_eq!(rate("\"50/2\""), (25, 1));
    assert_eq!(rate("\" 60 \""), (60, 1));
    assert_eq!(fixture(M, Path::new(".")).unwrap().fps_den, 1);
    for bad in [
        "0", "-30", "0.5", "\"30/0\"", "\"x\"", "\"1/2\"", "29.9701", "5000",
    ] {
        assert!(with_fps(bad).is_err(), "fps = {bad} should be rejected");
    }
}

#[test]
fn frame_time_and_frame_at_agree() {
    for fps in [
        "30",
        "\"30000/1001\"",
        "\"24000/1001\"",
        "60",
        "12.5",
        "\"7/3\"",
    ] {
        let m = with_fps(fps).unwrap();
        for n in (0..200_000u64).step_by(7).chain([1, 2, 3, 299, 300, 301]) {
            assert_eq!(
                m.frame_at(m.frame_time(n)) as u64,
                n,
                "fps {fps}, frame {n}"
            );
        }
        // Halfway between frames is still the earlier frame.
        let mid = (m.frame_time(10) + m.frame_time(11)) / 2.0;
        assert_eq!(m.frame_at(mid), 10);
        let mid = (m.frame_time(10_000) + m.frame_time(10_001)) / 2.0;
        assert_eq!(m.frame_at(mid), 10_000);
        let boundary = m.frame_time_precise(10_001);
        assert_eq!(m.frame_at_precise(boundary - 1e-9), 10_000);
        assert_eq!(m.frame_at_precise(boundary), 10_001);
        let n = m.frame_count();
        assert!(m.frame_time(n - 1) < m.duration && m.frame_time(n) >= m.duration);
    }
    assert_eq!(with_fps("30").unwrap().frame_count(), 330); // 11 s
}

#[test]
fn layout_overlaps_transitions() {
    let m = fixture(M, Path::new(".")).unwrap();
    let starts: Vec<f32> = m.shots.iter().map(|s| s.start).collect();
    assert_eq!(starts, vec![0.0, 4.0, 8.0]);
    assert_eq!(m.duration, 11.0);
}

#[test]
fn at_resolves_shots_and_transitions() {
    let m = fixture(M, Path::new(".")).unwrap();
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
    let m = fixture(M, Path::new(".")).unwrap();
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
            transition = { type = "fx/fade.glsl", duration = 1.5 }
            [[shot]]
            id = "c"
            scene = "c.glsl"
            duration = 2
            transition = { type = "fx/fade.glsl", duration = 1.0 }
        "#;
    let e = fixture(bad, Path::new(".")).unwrap_err().to_string();
    assert!(e.contains("overlaps"), "{e}");
}

#[test]
fn shot_sound_follows_transitions() {
    let m = fixture(
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
            transition = { type = "fx/crossfade.glsl", duration = 1, ease = "linear" }
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
        let m = fixture(
                &format!(
                    "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=3\nsound=\"a.glsl\"\n\
                     [[shot]]\nid=\"b\"\nscene=\"b.glsl\"\nduration=3\nsound=\"b.glsl\"\n\
                     transition = {{ type = \"fx/crossfade.glsl\", duration = 1.5, ease = \"{ease}\" }}\n"
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
                let mut s = String::from("channels = [\"noise\"]\n[textures]\nnoise = { file = \"t/noise.png\", srgb = true }\n");
                let mut prev_in = 0.0f32;
                let mut prev_dur = 0.0f32;
                for (i, (dur, tr, freeze)) in shots.into_iter().enumerate() {
                    s += &format!("[[shot]]\nid=\"s{i}\"\nscene=\"x.glsl\"\nduration={dur}\n");
                    match i % 3 {
                        1 => s += "channels = [\"noise\", \"\", \"t/a.png\"]\n",
                        2 => s += "channels = [{ file = \"t/b.jpg\", filter = \"nearest\", wrap = \"clamp\" }]\n",
                        _ => {}
                    }
                    let mut this_in = 0.0;
                    if let (Some(frac), true) = (tr, i > 0) {
                        // keep it legal: fits both shots and doesn't overlap the previous one
                        let room = (prev_dur - prev_in).min(dur);
                        let d = frac * room * 0.95;
                        if d > 0.01 {
                            s += &format!("transition = {{ type = \"fx/crossfade.glsl\", duration = {d}, freeze = {freeze} }}\n");
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
        /// Every spelling of a channel entry either binds or fails with a message that
        /// names the entry; the classifier and its error paths never panic.
        #[test]
        fn channel_spellings_never_panic(entry in "[a-zA-Z0-9_#@<>/.%-]{0,24}") {
            let toml = format!(
                "[[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 1\nchannels = [\"{entry}\"]\n"
            );
            let _ = ChannelSource::of(&entry);
            if let Err(e) = Movie::parse(&toml, Path::new("/film")) {
                let e = format!("{e:#}");
                prop_assert!(e.contains("channels[0]"), "{}: {}", entry, e);
            }
        }
    }

    proptest! {
        /// For speed ≥ 0, a footage channel's source frame never goes back within a
        /// shot (before `loop` wraps), whatever the rates, in and origin, and it never
        /// panics; `black` only ever turns black, and stays so.
        #[test]
        fn source_frame_never_goes_back(
            film in prop::sample::select(vec![(24u32, 1u32), (25, 1), (30, 1), (30000, 1001), (60, 1), (24000, 1001)]),
            src in prop::sample::select(vec![(12u32, 1u32), (24, 1), (25, 1), (30000, 1001), (60, 1), (7, 3)]),
            start in prop::option::of(0.0f64..20.0),
            start_frame in 0u64..500,
            speed in prop::sample::select(vec![0.0f64, 0.25, 0.5, 1.0, 1.5, 2.0, 3.7]),
            origin in 0.0f32..100.0,
            count in 1u64..400,
            black in any::<bool>(),
            movie_clock in any::<bool>(),
            from in 0u64..5000,
        ) {
            let m = FrameMap {
                film,
                loop_frames: None,
                origin,
                timing: Timing {
                    start: start.map_or(In::Frame(start_frame), In::Seconds),
                    speed,
                    clock: if movie_clock { FootageClock::Movie } else { FootageClock::Shot },
                    end: if black { End::Black } else { End::Hold },
                },
                src,
                count,
            };
            let mut prev: Option<u64> = Some(0);
            for n in from..from + 300 {
                let k = m.source_frame(n);
                match (prev, k) {
                    (Some(p), Some(k)) => prop_assert!(k >= p && k < count, "{} then {} at {}", p, k, n),
                    (None, Some(k)) => prop_assert!(false, "black, then frame {} at {}", k, n),
                    (_, None) => prop_assert!(black),
                }
                prev = k;
            }
        }
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

const SHOT: &str = "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n";

#[test]
fn engine_contract_is_checked_first() {
    let v = crate::library::contract();
    let ok = format!("shotglass = \"{}.{}\"\n{SHOT}", v.major, v.minor);
    assert_eq!(
        fixture(&ok, Path::new(".")).unwrap().shotglass,
        Some(format!("{}.{}", v.major, v.minor))
    );
    // A film from the future may use fields we don't know; the engine message wins.
    let future = format!("shotglass = \"99.1\"\nnew_thing = 3\n{SHOT}");
    let e = format!("{:#}", fixture(&future, Path::new(".")).unwrap_err());
    assert!(
        e.contains("needs shotglass 99.1") && e.contains("newer"),
        "{e}"
    );
    assert!(fixture(&format!("shotglass = 0.4\n{SHOT}"), Path::new(".")).is_err());
}

#[test]
fn transition_types() {
    assert_eq!(
        transition_kind("fx/iris.glsl"),
        TransitionKind::File("fx/iris.glsl")
    );
    assert_eq!(
        transition_kind("transitions/iris"),
        TransitionKind::Library("transitions", "iris.glsl".into())
    );
    assert_eq!(
        transition_kind("kit/fx/wipe"),
        TransitionKind::Library("kit", "fx/wipe.glsl".into())
    );
    assert_eq!(transition_kind("iris"), TransitionKind::Name("iris"));

    let kit = "transitions = { source = \"https://github.com/antonmarsden/shotglass-kit\", \
                   subdir = \"transitions\" }\n";
    let film = |libs: &str, kinds: &[&str]| {
        let mut t = format!("[libraries]\n{libs}{SHOT}");
        for (i, k) in kinds.iter().enumerate() {
            t += &format!(
                "[[shot]]\nid = \"s{i}\"\nscene = \"a.glsl\"\nduration = 1\n\
                     transition = {{ type = \"{k}\", duration = 0.5 }}\n"
            );
        }
        fixture(&t, Path::new("."))
    };
    // Library transitions: no warning, and nothing in the film to watch for them.
    let m = film(kit, &["transitions/iris", "transitions/fade"]).unwrap();
    assert!(m.warnings.is_empty(), "{:?}", m.warnings);
    assert_eq!(m.referenced_files(), vec![PathBuf::from("./a.glsl")]);
    // The library must be declared, and the file stays inside it.
    let e = format!("{:#}", film(kit, &["kit/iris"]).unwrap_err());
    assert!(
        e.contains("names library \"kit\", but [libraries] has \"transitions\""),
        "{e}"
    );
    let e = format!("{:#}", film("", &["kit/iris"]).unwrap_err());
    assert!(e.contains("[libraries] has none"), "{e}");
    let e = format!("{:#}", film(kit, &["transitions/../x"]).unwrap_err());
    assert!(e.contains("climbs"), "{e}");
    // Bare names (the removed built-ins, or the engine's internal "copy") are
    // refused, and the error names the kit replacement.
    let e = format!("{:#}", film("", &["iris"]).unwrap_err());
    assert!(
        e.contains("no built-in transitions")
            && e.contains("subdir = \"transitions\"")
            && e.contains("type = \"transitions/iris\""),
        "{e}"
    );
    assert!(film(kit, &["copy"]).is_err());
    // With the kit's transitions already declared, it names the film's own local name.
    let fx = kit.replacen("transitions =", "fx =", 1);
    let e = format!("{:#}", film(&fx, &["fade"]).unwrap_err());
    assert!(e.contains("type = \"fx/fade\""), "{e}");
}

#[test]
fn libraries_are_declared_by_local_name() {
    let m = fixture(
            &format!(
                "[libraries]\n\
                 kit = {{ version = \"0.4\", source = \"https://github.com/antonmarsden/shotglass-kit\", subdir = \"core\" }}\n\
                 mine = {{ path = \"lib/mykit\" }}\n{SHOT}"
            ),
            Path::new("."),
        )
        .unwrap();
    assert_eq!(m.library_specs.len(), 2);
    assert_eq!(m.library_specs[0].name, "kit");
    assert_eq!(m.library_specs[0].subdir.as_deref(), Some("core"));
    assert_eq!(m.library_specs[1].path.as_deref(), Some("lib/mykit"));
    assert!(m.libraries.is_empty(), "parse doesn't touch the disk");
    let bad = format!("[libraries]\nkit = {{ path = \"../kit\" }}\n{SHOT}");
    assert!(format!("{:#}", fixture(&bad, Path::new(".")).unwrap_err()).contains("climbs"));
    let bad = format!("[libraries]\nkit = {{ path = \"kit\", url = \"x\" }}\n{SHOT}");
    assert!(fixture(&bad, Path::new(".")).is_err());
}

#[test]
fn paths_must_be_web_friendly() {
    // Each line goes into shot "b".
    for (line, why) in [
        ("scene = \"/home/me/a.glsl\"", "absolute"),
        ("scene = \"C:/films/a.glsl\"", "drive letter"),
        ("scene = \"scenes\\\\a.glsl\"", "backslash"),
        ("scene = \"../other/a.glsl\"", "climbs"),
        ("scene = \"x.glsl\"\nsound = \"sfx/../../a.glsl\"", "climbs"),
        (
            "scene = \"x.glsl\"\ntransition = { type = \"/t/wipe.glsl\", duration = 0.5 }",
            "absolute",
        ),
    ] {
        let text = format!("{SHOT}[[shot]]\nid = \"b\"\nduration = 1\n{line}\n");
        let e = format!("{:#}", fixture(&text, Path::new(".")).unwrap_err());
        assert!(e.contains(why) && e.contains("shot \"b\""), "{line}: {e}");
    }
    let text = format!("[sound]\nmusic = \"D:\\\\m.glsl\"\n{SHOT}");
    let e = format!("{:#}", fixture(&text, Path::new(".")).unwrap_err());
    assert!(
        e.contains("[sound] music") && e.contains("backslash"),
        "{e}"
    );
    // Paths that climb and come back down inside the film are fine.
    let text = format!("{SHOT}[[shot]]\nid = \"b\"\nduration = 1\nscene = \"scenes/../b.glsl\"\n");
    assert!(fixture(&text, Path::new(".")).is_ok());
}

#[test]
fn load_resolves_libraries_and_warns_about_case() {
    let dir = std::env::temp_dir().join(format!("shotglass-movie-load-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("lib/mykit")).unwrap();
    std::fs::create_dir_all(dir.join("Scenes")).unwrap();
    std::fs::write(dir.join("Scenes/a.glsl"), "").unwrap();
    std::fs::write(
        dir.join("lib/mykit/library.toml"),
        "name = \"me/mykit\"\nversion = \"0.2.0\"\nlicences = [\"MIT\"]\n",
    )
    .unwrap();
    crate::project::write_fixture(
        &dir,
        "[libraries]\nmine = { path = \"lib/mykit\", version = \"0.2\" }\n\
             [[shot]]\nid = \"a\"\nscene = \"scenes/a.glsl\"\nduration = 1\n",
    )
    .unwrap();
    let m = Movie::load(&dir).unwrap();
    assert_eq!(m.libraries[0].canonical, "me/mykit");
    assert_eq!(m.libraries[0].root, dir.join("lib/mykit"));
    assert!(
        m.warnings.iter().any(|w| w.contains("\"Scenes/a.glsl\"")),
        "{:?}",
        m.warnings
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_library_needing_newer_glsl_needs_the_film_to_opt_in() {
    let dir = std::env::temp_dir().join(format!("shotglass-movie-libglsl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("lib/compute")).unwrap();
    std::fs::write(
        dir.join("lib/compute/library.toml"),
        "name = \"me/compute\"
version = \"1.0.0\"
glsl = 430
",
    )
    .unwrap();
    let toml = |glsl: &str| {
        crate::project::write_fixture(
            &dir,
            &format!(
                "{glsl}[libraries]
fx = {{ path = \"lib/compute\" }}
                     [[shot]]
id = \"a\"
scene = \"a.glsl\"
duration = 1
"
            ),
        )
        .unwrap();
    };
    for film in [
        "",
        "glsl = \"410\"
",
    ] {
        toml(film);
        let e = format!("{:#}", Movie::load(&dir).unwrap_err());
        assert!(
            e.contains("fx (me/compute 1.0.0) needs GLSL 430") && e.contains("glsl = \"430\""),
            "{e}"
        );
    }
    toml(
        "glsl = \"430\"
",
    );
    assert_eq!(Movie::load(&dir).unwrap().libraries[0].glsl, Some(430));
    // Only versions shotglass offers.
    std::fs::write(
        dir.join("lib/compute/library.toml"),
        "name = \"me/compute\"
version = \"1.0.0\"
glsl = 450
",
    )
    .unwrap();
    let e = format!("{:#}", Movie::load(&dir).unwrap_err());
    assert!(e.contains("glsl = 450"), "{e}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn glsl_opt_in() {
    let shot = "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=1\n";
    assert_eq!(fixture(shot, Path::new(".")).unwrap().glsl, None);
    for v in ["330", "410", "430"] {
        let m = fixture(&format!("glsl = \"{v}\"\n{shot}"), Path::new(".")).unwrap();
        assert_eq!(m.glsl, Some(v.parse().unwrap()));
    }
    for bad in ["\"450\"", "\"300 es\"", "\"\"", "430"] {
        let e = fixture(&format!("glsl = {bad}\n{shot}"), Path::new("."));
        assert!(e.is_err(), "glsl = {bad} should be refused");
    }
    let e = fixture(&format!("glsl = \"460\"\n{shot}"), Path::new("."))
        .unwrap_err()
        .to_string();
    assert!(e.contains("\"410\" or \"430\""), "{e}");
}

fn channels_film(top: &str, textures: &str, shots: &[&str]) -> Result<Movie> {
    let mut t = format!("{top}[libraries]\nkit = {{ path = \"lib/kit\" }}\n[textures]\n{textures}");
    let shots = if shots.is_empty() { &[""][..] } else { shots };
    for (i, extra) in shots.iter().enumerate() {
        t += &format!("[[shot]]\nid = \"s{i}\"\nscene = \"a.glsl\"\nduration = 1\n{extra}\n");
    }
    fixture(&t, Path::new("/film"))
}

fn err(r: Result<Movie>) -> String {
    format!("{:#}", r.unwrap_err())
}

#[test]
fn channels_short_and_long_forms() {
    let m = channels_film(
            "",
            "noise = \"<kit/textures/noise.png>\"\n\
             grid = { file = \"textures/grid.png\", filter = \"nearest\", wrap = \"mirror\" }\n\
             moon = { file = \"textures/moon.JPG\", wrap = \"clamp\", srgb = true, vflip = false }\n",
            &["channels = [\"grid\", \"noise\", \"\", \"moon\"]",
              "channels = [\"\", \"textures/stars.png\", { file = \"<kit/logo.jpeg>\", wrap = \"clamp\" }]"],
        )
        .unwrap();
    let c = &m.shots[0].channels;
    let grid = c[0].as_ref().unwrap();
    assert_eq!(
        grid.image().unwrap().file,
        PathBuf::from("/film/textures/grid.png")
    );
    assert_eq!(
        (grid.filter, grid.wrap, grid.vflip, grid.srgb),
        (Filter::Nearest, Wrap::Mirror, true, false)
    );
    let noise = c[1].as_ref().unwrap();
    assert_eq!(noise.image().unwrap().library.as_deref(), Some("kit"));
    assert_eq!(noise.label(), "kit/textures/noise.png");
    assert_eq!(noise.image().unwrap().rel, "textures/noise.png");
    assert_eq!(
        (noise.filter, noise.wrap),
        (Filter::Mipmap, Wrap::Repeat),
        "Shadertoy's defaults"
    );
    assert!(c[2].is_none());
    let moon = c[3].as_ref().unwrap();
    assert_eq!(
        (moon.wrap, moon.srgb, moon.vflip),
        (Wrap::Clamp, true, false)
    );
    let c = &m.shots[1].channels;
    assert!(c[0].is_none() && c[3].is_none());
    assert_eq!(c[1].as_ref().unwrap().label(), "textures/stars.png");
    assert_eq!(c[2].as_ref().unwrap().wrap, Wrap::Clamp);
    assert!(m.uses_channels());
    assert!(m.warnings.is_empty(), "{:?}", m.warnings);
    // Film textures are referenced files (case check, web preload); library ones aren't.
    let files = m.referenced_files();
    assert!(files.contains(&PathBuf::from("/film/textures/grid.png")));
    assert!(files.contains(&PathBuf::from("/film/textures/stars.png")));
    assert!(!files.iter().any(|f| f.to_string_lossy().contains("noise")));
    // A plain kit/… path (no brackets) is in the film folder, as for #include "…".
    let m = channels_film("", "", &["channels = [\"kit/textures/noise.png\"]"]).unwrap();
    let c = m.shots[0].channels[0].as_ref().unwrap();
    assert_eq!(
        (
            c.image().unwrap().library.as_ref(),
            c.image().unwrap().file.clone()
        ),
        (None, PathBuf::from("/film/kit/textures/noise.png"))
    );
}

#[test]
fn film_default_channels_and_replacement() {
    let m = channels_film(
        "channels = [\"noise\"]\n",
        "noise = \"t/noise.png\"\nbrick = \"t/brick.png\"\n",
        &["", "channels = [\"brick\"]", "channels = []"],
    )
    .unwrap();
    let label = |i: usize, n: usize| m.shots[i].channels[n].as_ref().map(|c| c.label());
    assert_eq!(
        label(0, 0).as_deref(),
        Some("t/noise.png"),
        "default applies"
    );
    assert_eq!(
        label(1, 0).as_deref(),
        Some("t/brick.png"),
        "replaced, not merged"
    );
    assert_eq!(label(1, 1), None);
    assert!(
        m.shots[2].channels.iter().all(Option::is_none),
        "channels = [] binds nothing"
    );
    // Without any channels, nothing is bound and the film is unchanged.
    let m = fixture(M, Path::new(".")).unwrap();
    assert!(!m.uses_channels() && m.contract_warning().is_none());
}

#[test]
fn channels_are_validated() {
    let e = err(channels_film(
        "",
        "",
        &["channels = [\"\", \"\", \"\", \"\", \"\", \"\", \"\", \"\", \"\"]"],
    ));
    assert!(
        e.contains("shot \"s0\": channels has 9 entries; there are 8 (iChannel0..7)"),
        "{e}"
    );
    let e = err(channels_film(
        "",
        "brick = \"b.png\"\nnoise = \"n.png\"\ngrid = \"g.png\"\n",
        &["channels = [\"brick\", \"noize\"]"],
    ));
    assert!(
            e.contains("shot \"s0\": channels[1]: no [textures] or [texts] entry called \"noize\" (there are \"brick\", \"grid\", \"noise\")"),
            "{e}"
        );
    let e = err(channels_film("", "", &["channels = [\"noise\"]"]));
    assert!(e.contains("there are none"), "{e}");
    let e = err(channels_film("", "noise = \"<kat/t/noise.png>\"\n", &[]));
    assert!(
            e.contains("[textures] noise: \"<kat/t/noise.png>\" names library \"kat\", but [libraries] has \"kit\""),
            "{e}"
        );
    let e = err(channels_film(
        "",
        "",
        &["channels = [\"textures/sky.webp\"]"],
    ));
    assert!(
        e.contains("channels[0]: textures/sky.webp: shotglass reads PNG and JPEG"),
        "{e}"
    );
    let e = err(channels_film(
        "",
        "grid = { file = \"g.png\", filter = \"point\" }\n",
        &[],
    ));
    assert!(
        e.contains(
            "[textures] grid: filter \"point\" isn't one of \"mipmap\", \"linear\", \"nearest\""
        ),
        "{e}"
    );
    let e = err(channels_film(
        "",
        "grid = { file = \"g.png\", wrap = \"tile\" }\n",
        &[],
    ));
    assert!(
        e.contains("wrap \"tile\" isn't one of \"repeat\", \"clamp\", \"mirror\""),
        "{e}"
    );
    let e = err(channels_film(
        "",
        "grid = { file = \"g.png\", vflip = \"yes\" }\n",
        &[],
    ));
    assert!(e.contains("vflip must be true or false"), "{e}");
    // Unknown fields in a texture table (deny_unknown_fields, by hand).
    let e = err(channels_film(
        "",
        "grid = { file = \"g.png\", filtr = \"nearest\" }\n",
        &[],
    ));
    assert!(e.contains("unknown option `filtr`"), "{e}");
    let e = err(channels_film(
        "",
        "",
        &["channels = [{ filter = \"nearest\" }]"],
    ));
    assert!(e.contains("needs file"), "{e}");
    // Path rules, as for every other path.
    for (bad, why) in [
        ("../x.png", "climbs"),
        ("/x.png", "absolute"),
        ("t\\\\x.png", "backslash"),
        ("<kit/../../x.png>", "climbs"),
    ] {
        let e = err(channels_film("", "", &[&format!("channels = [\"{bad}\"]")]));
        assert!(e.contains(why) && e.contains("channels[0]"), "{bad}: {e}");
    }
    // Names are identifiers; [textures] values are files, not names.
    let e = err(channels_film("", "\"my-tex\" = \"a.png\"\n", &[]));
    assert!(e.contains("[textures] my-tex"), "{e}");
    let e = err(channels_film("", "a = \"noise\"\n", &[]));
    assert!(e.contains("isn't a file"), "{e}");
    let e = err(channels_film("", "", &["channels = [3]"]));
    assert!(e.contains("expected a texture name"), "{e}");
    // Shot-level unknown fields are still refused.
    assert!(channels_film("", "", &["channel = [\"a.png\"]"]).is_err());
}

#[test]
fn channel_sources_by_spelling() {
    use ChannelSource::*;
    for (s, kind) in [
        ("", Empty),
        ("@input", Output("input")),
        ("@title", Output("title")),
        ("brick", Name("brick")),
        ("my_tex2", Name("my_tex2")),
        ("textures/brick.png", Image("textures/brick.png")),
        ("brick.JPG", Image("brick.JPG")),
        (
            "<kit/textures/noise.png>",
            Image("<kit/textures/noise.png>"),
        ),
        ("textures/sky.webp", Image("textures/sky.webp")), // refused later, as an image
        ("renders/fog_####.png", Sequence("renders/fog_####.png")),
        ("<kit/grain_##.png>", Sequence("<kit/grain_##.png>")),
        ("footage/surf.mp4", Video("footage/surf.mp4")),
        ("plate.MOV", Video("plate.MOV")),
        (
            "<kit/loops/caustics.webm>",
            Video("<kit/loops/caustics.webm>"),
        ),
        ("clips.mp4/still.png", Image("clips.mp4/still.png")), // the file name decides
    ] {
        assert_eq!(ChannelSource::of(s), kind, "{s}");
    }
}

#[test]
fn channel_kinds_that_arrive_later() {
    let shot = |entry: &str| err(channels_film("", "", &[&format!("channels = [{entry}]")]));
    for (entry, says) in [
        (
            r#""@picture""#,
            r#"channels[0]: "@picture" is another pass's picture; only a [[layer]]'s channels can read one"#,
        ),
        (
            r#""footage/surf.mp4""#,
            r#"channels[0]: "footage/surf.mp4": shotglass reads video as AV1 in WebM; convert it once: shotglass conform footage/surf.mp4"#,
        ),
        (
            r#"{ video = "a.mov", in = 2.0 }"#,
            "shotglass conform a.mov",
        ),
        (r#"{ file = "a.mp4" }"#, "shotglass conform a.mp4"),
    ] {
        let e = shot(entry);
        assert!(e.contains(says), "{entry}: {e}");
    }
    // Malformed spellings get their own messages, not "arrives later".
    for (entry, says) in [
        (
            r#""@""#,
            "after @ comes input, picture, or the id of a layer or comp",
        ),
        (r#""@my layer""#, "after @ comes"),
        (
            r#""r_##/f_##.png""#,
            "# marks frame numbers and may only appear in the file name",
        ),
        (r#""r/f_##_##.png""#, "an image sequence has one run of #"),
        (
            r#""renders/fog_%04d.png""#,
            r#"write the frame number as # ("renders/fog_####.png")"#,
        ),
        (
            r#"{ file = "a.png", video = "b.mp4" }"#,
            "a channel table takes one of file, frames, video, texture or text (this one has file and video)",
        ),
    ] {
        let e = shot(entry);
        assert!(e.contains(says), "{entry}: {e}");
    }
    // [textures] values are files: @ sources only go in a shot's channels.
    let e = err(channels_film("", "below = \"@input\"\n", &[]));
    assert!(
        e.contains("[textures] below: \"@input\" isn't a file"),
        "{e}"
    );
    // A video that isn't WebM names the command that converts it.
    let e = shot(r#""a.mp4""#);
    assert!(e.contains("shotglass conform a.mp4"), "{e}");
}

#[test]
fn video_channels_parse() {
    let m = channels_film(
        "",
        "plate = \"clips/plate.webm\"\n\
         surf = { video = \"<kit/v/surf.WEBM>\", fps = 24, matrix = \"bt2020\", range = \"pc\", \
         clock = \"movie\", end = \"loop\" }\n",
        &[
            "channels = [\"plate\", \"surf\", { texture = \"plate\", in = 2.5, speed = 0.5 }, \
           { video = \"c/d.webm\", in_frame = 3 }]",
        ],
    )
    .unwrap();
    let c = |n: usize| m.shots[0].channels[n].as_ref().unwrap();
    let plate = c(0).sequence().unwrap();
    assert_eq!(plate.pattern.file, PathBuf::from("/film/clips/plate.webm"));
    assert_eq!(plate.video, Some(VideoOptions::default()));
    assert_eq!(
        (c(0).filter, c(0).vflip, c(0).srgb, c(0).timing),
        (Filter::Linear, true, false, Timing::default())
    );
    let surf = c(1).sequence().unwrap();
    assert_eq!(surf.fps, Some((24, 1)));
    assert_eq!(
        surf.video,
        Some(VideoOptions {
            matrix: Some(crate::video::Matrix::Bt2020),
            full_range: Some(true),
        })
    );
    assert_eq!(
        (c(1).timing.clock, c(1).timing.end),
        (FootageClock::Movie, End::Loop)
    );
    assert_eq!(c(2).source, c(0).source);
    assert_eq!(c(2).timing.speed, 0.5);
    assert_eq!(c(3).timing.start, In::Frame(3));
    // A video is one file, checked by name like an image.
    assert_eq!(c(0).files().len(), 1);
    let w = m.contract_warning().unwrap();
    assert!(w.contains("video channels"), "{w}");
    assert!(w.contains("0.1.12"), "{w}");
}

#[test]
fn video_channels_are_validated() {
    let shot = |entry: &str| {
        err(channels_film(
            "",
            "img = \"t/i.png\"\n",
            &[&format!("channels = [\"img\", {entry}]")],
        ))
    };
    for (entry, says) in [
        (
            r#""a.mov""#,
            "shotglass reads video as AV1 in WebM; convert it once: shotglass conform a.mov",
        ),
        (r#"{ video = "a.mp4" }"#, "shotglass conform a.mp4"),
        (
            r#"{ video = "a.png" }"#,
            "video = \"a.png\" must be a video file (.webm)",
        ),
        (
            r#"{ file = "a.webm" }"#,
            "\"a.webm\" is a video; write video = \"a.webm\"",
        ),
        (r#"{ frames = "a.webm" }"#, "is a video; write video"),
        (
            r#"{ video = "a.webm", first = 3 }"#,
            "first is for image sequences; a video starts at frame 0",
        ),
        (
            r#"{ video = "a.webm", matrix = "bt470" }"#,
            r#"matrix "bt470" isn't one of "bt601", "bt709", "bt2020""#,
        ),
        (
            r#"{ video = "a.webm", range = "full" }"#,
            r#"range "full" isn't one of "tv", "pc""#,
        ),
        (
            r#"{ file = "a.png", matrix = "bt709" }"#,
            "matrix is for videos",
        ),
        (
            r#"{ frames = "a/b_##.png", range = "tv" }"#,
            "range is for videos",
        ),
        (
            r#"{ video = "a.webm", bogus = 1 }"#,
            "unknown option `bogus` (a video has video, fps, matrix, range",
        ),
    ] {
        let e = shot(entry);
        assert!(e.contains(says), "{entry}: {e}");
    }
}

#[test]
fn sequence_channels_parse() {
    let m = channels_film(
            "",
            "fog = \"renders/fog_####.png\"\n\
             matte = { frames = \"<kit/m/matte_####.png>\", fps = \"24000/1001\", first = 1001, \
             clock = \"movie\", end = \"loop\", filter = \"nearest\", srgb = true }\n",
            &["channels = [\"fog\", \"matte\", { texture = \"fog\", in = 2.5, speed = 0.5 }, \
               { texture = \"matte\", in_frame = 7, end = \"black\" }, { frames = \"a/b_##.jpg\", in = 1 }, \
               { file = \"x/y_#.png\", fps = 12 }]"],
        )
        .unwrap();
    let c = |n: usize| m.shots[0].channels[n].as_ref().unwrap();
    let fog = c(0).sequence().unwrap();
    assert_eq!(
        fog.pattern.file,
        PathBuf::from("/film/renders/fog_####.png")
    );
    assert_eq!(
        (fog.fps, fog.first, fog.scan.is_some()),
        (None, None, false)
    );
    assert_eq!(
        (c(0).filter, c(0).wrap, c(0).vflip, c(0).srgb, c(0).timing),
        (Filter::Linear, Wrap::Repeat, true, false, Timing::default()),
        "footage samples linearly by default (no mipmaps per frame)"
    );
    let matte = c(1).sequence().unwrap();
    assert_eq!(matte.pattern.library.as_deref(), Some("kit"));
    assert_eq!(c(1).label(), "kit/m/matte_####.png");
    assert_eq!((matte.fps, matte.first), (Some((24000, 1001)), Some(1001)));
    assert_eq!((c(1).filter, c(1).srgb), (Filter::Nearest, true));
    assert_eq!(
        (c(1).timing.clock, c(1).timing.end),
        (FootageClock::Movie, End::Loop)
    );
    // Re-timed: the entry's source and options, the channel's own timing on top.
    assert_eq!(c(2).source, c(0).source);
    assert_eq!(
        c(2).timing,
        Timing {
            start: In::Seconds(2.5),
            speed: 0.5,
            ..Timing::default()
        }
    );
    assert_eq!(c(3).source, c(1).source);
    assert_eq!(
        c(3).timing,
        Timing {
            start: In::Frame(7),
            end: End::Black,
            ..c(1).timing
        }
    );
    assert_eq!(c(4).timing.start, In::Seconds(1.0));
    assert_eq!(c(4).label(), "a/b_##.jpg");
    // The spelling decides, in a table too.
    assert_eq!(c(5).sequence().unwrap().fps, Some((12, 1)));
    // Used through texture = "…" counts as used; the library is referenced.
    assert!(m.warnings.is_empty(), "{:?}", m.warnings);
    assert!(m.library_refs.contains("kit"));
    // A sequence's frames aren't files to check by name (the scan finds them).
    assert!(c(0).files().is_empty() && c(0).written().len() == 1);
    assert!(
        !m.referenced_files()
            .iter()
            .any(|f| f.to_string_lossy().contains('#'))
    );
    let w = m.contract_warning().unwrap();
    assert!(
        w.contains("image sequences, which need shotglass 0.1.6"),
        "{w}"
    );
    let m = channels_film("", "fog = \"r/f_##.png\"\n", &["channels = [\"fog\"]"]).unwrap();
    assert!(m.textures().len() == 1 && m.uses_channels());
}

#[test]
fn sequence_channels_are_validated() {
    let shot = |entry: &str| {
        err(channels_film(
            "",
            "fog = \"r/fog_####.png\"\nimg = \"t/i.png\"\n",
            &[&format!("channels = [\"fog\", \"img\", {entry}]")],
        ))
    };
    for (entry, says) in [
        (
            r#"{ file = "a.png", in = 1 }"#,
            r#"channels[2]: "in" is for footage (image sequences); a.png isn't footage"#,
        ),
        (
            r#"{ file = "a.png", fps = 24 }"#,
            "fps is for image sequences",
        ),
        (
            r#"{ frames = "a/b_##.png", in = 1, in_frame = 2 }"#,
            "give in (seconds) or in_frame (a source frame), not both",
        ),
        (
            r#"{ frames = "a/b_##.png", speed = -1 }"#,
            "speed must be a number ≥ 0",
        ),
        (
            r#"{ frames = "a/b_##.png", in = "x" }"#,
            "in must be a number ≥ 0",
        ),
        (
            r#"{ frames = "a/b_##.png", in_frame = 1.5 }"#,
            "in_frame must be a whole number ≥ 0",
        ),
        (
            r#"{ frames = "a/b_##.png", clock = "wall" }"#,
            r#"clock "wall" isn't one of "shot", "movie""#,
        ),
        (
            r#"{ frames = "a/b_##.png", end = "bounce" }"#,
            r#"end "bounce" isn't one of "hold", "loop", "black""#,
        ),
        (r#"{ frames = "a/b_##.png", fps = 0 }"#, "fps must be"),
        (
            r#"{ frames = "a/b_##.png", fps = "24/x" }"#,
            "fps must look like",
        ),
        (
            r#"{ frames = "a/b_##.png", first = -1 }"#,
            "first must be a whole number ≥ 0",
        ),
        (
            r#"{ frames = "a/b.png" }"#,
            "needs one run of # in its file name",
        ),
        (
            r#"{ frames = "a/b_%03d.png" }"#,
            r#"write the frame number as # ("a/b_###.png")"#,
        ),
        (
            r#"{ frames = "a/b_##.gif" }"#,
            "an image sequence's frames are PNG or JPEG images",
        ),
        (
            r#"{ frames = "a/b_##.png", bogus = 1 }"#,
            "unknown option `bogus` (an image sequence has frames, fps, first, in",
        ),
        (
            r#"{ frames = "fog" }"#,
            "frames = \"fog\" must be a path to a .png or .jpg sequence",
        ),
        (
            r#"{ texture = "img" }"#,
            "texture = \"img\" re-times footage, and t/i.png isn't footage (bind it as \"img\")",
        ),
        (
            r#"{ texture = "nope" }"#,
            r#"texture = "nope": no [textures] entry called "nope" (there are "fog", "img")"#,
        ),
        (
            r#"{ texture = "fog", filter = "nearest" }"#,
            "only timing options (in, in_frame, speed, clock, end) may follow texture",
        ),
        (
            r#"{ texture = "fog", fps = 24 }"#,
            "`fps` goes on the [textures] entry",
        ),
        (
            r#"{ texture = 3 }"#,
            "texture must be the name of a [textures] entry",
        ),
    ] {
        let e = shot(entry);
        assert!(e.contains(says), "{entry}: {e}");
    }
    let e = err(channels_film(
        "",
        "fog = \"r/f_##.png\"\nx = { texture = \"fog\" }\n",
        &[],
    ));
    assert!(
        e.contains("[textures] x: texture = \"fog\" re-times a [textures] entry"),
        "{e}"
    );
}

fn rate_map(film: (u32, u32), src: (u32, u32)) -> FrameMap {
    FrameMap {
        film,
        loop_frames: None,
        origin: 0.0,
        timing: Timing {
            clock: FootageClock::Movie,
            ..Timing::default()
        },
        src,
        count: u64::MAX,
    }
}

#[test]
fn source_frame_is_exact_for_rate_pairs() {
    // Equal rates are the identity for a million frames, NTSC included.
    for r in [(30, 1), (30000, 1001), (24000, 1001), (25, 1)] {
        let m = rate_map(r, r);
        for n in 0..1_000_000u64 {
            assert_eq!(m.source_frame(n), Some(n), "{r:?} frame {n}");
        }
    }
    // Different rates: "the source frame whose interval contains this instant",
    // which is exact integer arithmetic: floor(n · src · film_den / (film · src_den)).
    for (film, src) in [
        ((30, 1), (24, 1)),
        ((24, 1), (25, 1)),
        ((24, 1), (60, 1)),
        ((30000, 1001), (24000, 1001)),
        ((60, 1), (24000, 1001)),
        ((25, 1), (30000, 1001)),
    ] {
        let m = rate_map(film, src);
        for n in 0..200_000u64 {
            let want =
                (n as u128 * src.0 as u128 * film.1 as u128) / (film.0 as u128 * src.1 as u128);
            assert_eq!(
                m.source_frame(n),
                Some(want as u64),
                "{src:?} in {film:?}, frame {n}"
            );
        }
    }
    // clock = "shot" with an item starting late in a film (an f32 start, rounded):
    // still the identity from its first frame.
    for r in [(30, 1), (30000, 1001)] {
        for n0 in [1u64, 33, 1799, 107_892, 1_000_003] {
            let start = (n0 as f64 * r.1 as f64 / r.0 as f64) as f32;
            let m = FrameMap {
                origin: start,
                timing: Timing::default(),
                ..rate_map(r, r)
            };
            for j in 0..2000 {
                assert_eq!(m.source_frame(n0 + j), Some(j), "{r:?} from {n0}, +{j}");
            }
        }
    }
    let m = rate_map((30, 1), (24, 1));
    let v: Vec<u64> = (0..12).map(|n| m.source_frame(n).unwrap()).collect();
    assert_eq!(
        v,
        [0, 0, 1, 2, 3, 4, 4, 5, 6, 7, 8, 8],
        "one repeat every five"
    );
}

#[test]
fn source_frame_timing_options() {
    let base = FrameMap {
        film: (30, 1),
        loop_frames: None,
        origin: 0.0,
        timing: Timing::default(),
        src: (24, 1),
        count: 48,
    };
    let with = |t: Timing| FrameMap { timing: t, ..base };
    let at = |m: FrameMap, n: u64| m.source_frame(n);
    // in (seconds) and in_frame agree: 0.5 s at 24 fps is frame 12.
    let a = with(Timing {
        start: In::Seconds(0.5),
        ..Timing::default()
    });
    let b = with(Timing {
        start: In::Frame(12),
        ..Timing::default()
    });
    for n in 0..40 {
        assert_eq!(at(a, n), at(b, n));
    }
    assert_eq!(at(a, 0), Some(12));
    // speed: half (each source frame on ~2.5 film frames), double, 0 = freeze at in.
    let half = with(Timing {
        speed: 0.5,
        ..Timing::default()
    });
    assert_eq!(
        (0..10).map(|n| at(half, n).unwrap()).collect::<Vec<_>>(),
        [0, 0, 0, 1, 1, 2, 2, 2, 3, 3]
    );
    let double = with(Timing {
        speed: 2.0,
        ..Timing::default()
    });
    assert_eq!(at(double, 5), Some(8));
    let freeze = with(Timing {
        start: In::Frame(7),
        speed: 0.0,
        ..Timing::default()
    });
    assert!((0..100).all(|n| at(freeze, n) == Some(7)));
    // end: frame 47 is the last (film frame 59 is source 47.2); past it…
    assert_eq!(at(base, 59), Some(47));
    assert_eq!(at(base, 60), Some(47), "hold");
    assert_eq!(at(base, 1000), Some(47), "hold");
    let looped = with(Timing {
        end: End::Loop,
        ..Timing::default()
    });
    assert_eq!(at(looped, 60), Some(0));
    assert_eq!(at(looped, 61), Some(0));
    assert_eq!(at(looped, 62), Some(1));
    let black = with(Timing {
        end: End::Black,
        ..Timing::default()
    });
    assert_eq!((at(black, 59), at(black, 60)), (Some(47), None));
    // clock = "shot": 0 at the item's start, even an f32 start that isn't exact
    // (1.1 s is film frame 33); clock = "movie" ignores it. Before the start (and
    // rounding below 0) clamps to frame 0.
    let shot = FrameMap {
        origin: 1.1,
        ..base
    };
    assert_eq!(
        (at(shot, 33), at(shot, 34), at(shot, 35)),
        (Some(0), Some(0), Some(1))
    );
    assert_eq!(at(shot, 0), Some(0));
    let movie = FrameMap {
        origin: 1.1,
        timing: Timing {
            clock: FootageClock::Movie,
            ..Timing::default()
        },
        ..base
    };
    assert_eq!(at(movie, 33), Some(26));
    // The outgoing shot of a transition keeps moving past its own end.
    let long = FrameMap {
        count: 1000,
        ..base
    };
    assert_eq!(at(long, 100), Some(80));
    // Loop films: frame n of loop k is frame n of loop 0.
    let lp = FrameMap {
        loop_frames: Some(45),
        timing: Timing {
            end: End::Loop,
            ..Timing::default()
        },
        ..base
    };
    for n in 0..45 {
        for k in 1..4 {
            assert_eq!(at(lp, n + 45 * k), at(lp, n));
        }
    }
    assert_eq!(
        (
            base.source_time(12),
            FrameMap {
                src: (30000, 1001),
                ..base
            }
            .source_time(30)
        ),
        (0.5, 1.001)
    );
}

#[test]
fn load_scans_sequences_and_maps_frames() {
    let dir = std::env::temp_dir().join(format!("shotglass-movie-seq-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("r")).unwrap();
    std::fs::write(dir.join("a.glsl"), "").unwrap();
    for k in 1..=6 {
        image::RgbaImage::new(4, 2)
            .save(dir.join(format!("r/f_{k:02}.png")))
            .unwrap();
    }
    let film = |channels: &str| {
        crate::project::write_fixture(
                &dir,
                &format!(
                    "fps = 30\n[[shot]]\nid = \"lead\"\nscene = \"a.glsl\"\nduration = 0.5\n\
                     [[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 1\nchannels = {channels}\n"
                ),
            )
            .unwrap();
        Movie::load(&dir)
    };
    let m = film("[\"r/f_##.png\", { frames = \"r/f_##.png\", first = 3, fps = 15 }]").unwrap();
    let seq = m.shots[1].channels[0].as_ref().unwrap().sequence().unwrap();
    let scan = seq.scan.as_ref().unwrap();
    assert_eq!((scan.first, scan.count, scan.size), (1, 6, Some((4, 2))));
    assert_eq!(scan.path(0), dir.join("r/f_01.png"));
    let s2 = m.shots[1].channels[1].as_ref().unwrap().sequence().unwrap();
    assert_eq!(s2.scan.as_ref().unwrap().count, 4);
    // The second shot starts at film frame 15: its clock ("shot") starts there.
    let item = ItemRef::Shot(1);
    assert_eq!(m.source_frame(item, 0, 15), Some(0));
    assert_eq!(m.source_frame(item, 0, 20), Some(5));
    assert_eq!(m.source_frame(item, 0, 25), Some(5), "hold");
    assert_eq!(
        m.source_frame(item, 1, 17),
        Some(1),
        "15 fps: one frame per two"
    );
    assert_eq!(m.source_frame(item, 2, 17), None, "unbound");
    assert_eq!(m.source_frame(ItemRef::Shot(0), 0, 0), None, "no channels");
    // Edit mistakes refuse the version.
    let e = format!(
        "{:#}",
        film("[{ frames = \"r/f_##.png\", in = 5 }]").unwrap_err()
    );
    assert!(
        e.contains(
            "shot \"s\": channels[0]: in = 5 is past the end of r/f_##.png (0.20 s, 6 frames)"
        ),
        "{e}"
    );
    assert!(film("[{ frames = \"r/f_##.png\", in = 5, end = \"loop\" }]").is_ok());
    let e = format!("{:#}", film("[\"r/g_##.png\"]").unwrap_err());
    assert!(
        e.contains("channels[0]: r/g_##.png: matches no files"),
        "{e}"
    );
    std::fs::remove_file(dir.join("r/f_03.png")).unwrap();
    let e = format!("{:#}", film("[\"r/f_##.png\"]").unwrap_err());
    assert!(
        e.contains("r/f_##.png: frame 3 is missing (between 1 and 6)"),
        "{e}"
    );
    // With first after the gap, the frames from there on are a sequence.
    assert!(film("[{ frames = \"r/f_##.png\", first = 4 }]").is_ok());
    std::fs::remove_dir_all(&dir).ok();
}

/// A PNG that is only a signature and a header: enough for `texture::dimensions`.
fn png_header(w: u32, h: u32) -> Vec<u8> {
    fn crc(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }
    let mut chunk = b"IHDR".to_vec();
    chunk.extend(w.to_be_bytes());
    chunk.extend(h.to_be_bytes());
    chunk.extend([8, 6, 0, 0, 0]);
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend(13u32.to_be_bytes());
    v.extend(&chunk);
    v.extend(crc(&chunk).to_be_bytes());
    // An empty IDAT: the decoder reads the header up to the first one.
    v.extend(0u32.to_be_bytes());
    v.extend(b"IDAT");
    v.extend(crc(b"IDAT").to_be_bytes());
    v
}

#[test]
fn a_shot_whose_footage_cant_fit_is_refused() {
    let dir = std::env::temp_dir().join(format!("shotglass-movie-seqmem-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("r")).unwrap();
    std::fs::write(dir.join("a.glsl"), "").unwrap();
    // Two 4096² sequences: three frames each (the minimum window) are 384 MB.
    for name in ["a_1.png", "b_1.png"] {
        std::fs::write(dir.join("r").join(name), png_header(4096, 4096)).unwrap();
    }
    let film = |channels: &str| {
        crate::project::write_fixture(
            &dir,
            &format!(
                "[[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 1\nchannels = {channels}\n"
            ),
        )
        .unwrap();
        Movie::load(&dir)
    };
    assert!(film("[\"r/a_#.png\"]").is_ok(), "one fits (192 MB)");
    let e = format!("{:#}", film("[\"r/a_#.png\", \"r/b_#.png\"]").unwrap_err());
    assert!(
        e.contains("shot \"s\": its textures need 384 MB, more than the 256 MB budget"),
        "{e}"
    );
    assert!(
        e.contains("r/a_#.png 192 MB (3 frames of 4096×4096)"),
        "{e}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

fn layers_film(layers: &str) -> Result<Movie> {
    fixture(
        &format!(
            "[libraries]\nfx = {{ path = \"lib/fx\" }}\n\
                 [[shot]]\nid = \"moon\"\nscene = \"a.glsl\"\nduration = 3\n\
                 [[shot]]\nid = \"closeup\"\nscene = \"b.glsl\"\nduration = 3\n\
                 transition = {{ type = \"fx/mix.glsl\", duration = 1 }}\n{layers}"
        ),
        Path::new("/film"),
    )
}

#[test]
fn layers_parse_and_resolve_spans() {
    // moon: 0–3, closeup: 2–5 (settled at 3).
    let m = layers_film(
            "[[layer]]\nid = \"title\"\nscene = \"layers/title.glsl\"\nfrom = \"moon + 0.5\"\nto = \"closeup.settled\"\nfade = [0.4, 0.6]\nscale = 1\nuniforms.uSize = 0.08\nchannels = [\"\", \"@input\"]\n\
             [[layer]]\nid = \"grain\"\ntype = \"fx/grain\"\nscale = 0.5\nopacity = 0.6\nparams = { amount = 0.3 }\n\
             [[layer]]\nid = \"bars\"\ntype = \"fx/bars.glsl\"\nfrom = \"title.end - 0.5\"\nto = 4.5\nopacity = { keys = [[0, 0.0], [1, 1.0]] }\n",
        )
        .unwrap();
    let [title, grain, bars] = &m.layers[..] else {
        panic!("three layers")
    };
    assert_eq!((title.start, title.end), (0.5, 3.0));
    assert_eq!(title.fade, (0.4, 0.6));
    assert_eq!(title.scale, Some(1.0));
    assert_eq!(
        title.scene,
        LayerScene::File(PathBuf::from("/film/layers/title.glsl"))
    );
    assert_eq!(title.channels[1].as_ref().unwrap().source, Source::Input);
    // No span: the whole film.
    assert_eq!((grain.start, grain.end), (0.0, 5.0));
    assert_eq!(grain.scene, LayerScene::Pass("fx/grain".into()));
    assert_eq!(grain.params, vec![("amount".to_string(), vec![0.3])]);
    assert_eq!(bars.start, 2.5);
    // Opacity: fades (smooth) times the keyframes / constant.
    let near = |a: f32, b: f32| (a - b).abs() < 1e-5;
    assert!(near(title.opacity_at(0.0), 0.0));
    assert!(near(title.opacity_at(0.2), 0.5));
    assert!(near(title.opacity_at(1.0), 1.0));
    assert!(near(title.opacity_at(2.5 - 0.3), 0.5));
    assert!(near(grain.opacity_at(3.0), 0.6));
    assert!(near(bars.opacity_at(0.5), 0.5));
    // Active layers: half-open spans, bottom to top, with layer-local times.
    assert_eq!(m.layers_at(0.25), vec![(1, 0.25)]);
    assert_eq!(m.layers_at(2.75), vec![(0, 2.25), (1, 2.75), (2, 0.25)]);
    assert_eq!(m.layers_at(3.0), vec![(1, 3.0), (2, 0.5)]);
    // Cues for layers; the contract asks for 0.1.4.
    let cues = m.cue_defines();
    assert!(cues.contains("#define CUE_TITLE_START 0.5\n"), "{cues}");
    assert!(cues.contains("#define CUE_TITLE_END 3.0\n"), "{cues}");
    let w = m.contract_warning().unwrap();
    assert!(w.contains("layers, which need shotglass 0.1.4"), "{w}");
    // A film without layers has exactly the cues it had.
    let plain = layers_film("").unwrap();
    assert!(plain.layers.is_empty() && !plain.cue_defines().contains("TITLE"));
}

#[test]
fn blends_and_layer_references() {
    let m = layers_film(
            "[[layer]]\nid = \"bright-pass\"\nscene = \"l.glsl\"\nopacity = 0\n\
             [[layer]]\nid = \"screen\"\nscene = \"l.glsl\"\nblend = \"fx/screen\"\nchannels = [\"@bright-pass\", \"@picture\", \"@input\"]\n\
             [[layer]]\nid = \"local\"\nscene = \"l.glsl\"\nblend = \"fx/my-blend.glsl\"\n\
             [[layer]]\nid = \"plain\"\nscene = \"l.glsl\"\nblend = \"over\"\n",
        )
        .unwrap();
    let screen = &m.layers[1];
    assert_eq!(screen.blend.as_deref(), Some("fx/screen"));
    let src: Vec<&Source> = screen
        .channels
        .iter()
        .flatten()
        .map(|c| &c.source)
        .collect();
    assert_eq!(
        src,
        [
            &Source::Layer {
                index: 0,
                id: "bright-pass".into()
            },
            &Source::Picture,
            &Source::Input
        ]
    );
    assert_eq!(screen.channels[0].as_ref().unwrap().label(), "@bright-pass");
    assert_eq!(m.layers[2].blend.as_deref(), Some("fx/my-blend.glsl"));
    // "over" is the engine's source-over: no blend pass.
    assert_eq!(m.layers[3].blend, None);
    // Pass outputs aren't textures.
    assert!(m.textures().is_empty());
    let w = m.contract_warning().unwrap();
    assert!(
        w.contains("blend modes, @picture or @<layer>, which need shotglass 0.1.5"),
        "{w}"
    );
    // A later layer can't be read (the stack stays a DAG).
    let e = err(layers_film(
        "[[layer]]\nid = \"a\"\nscene = \"l.glsl\"\nchannels = [\"@b\"]\n\
             [[layer]]\nid = \"b\"\nscene = \"l.glsl\"\n",
    ));
    assert!(e.contains("\"@b\" is a later layer"), "{e}");
}

/// A film in a temp folder with comp files: `files` are (path, text).
fn comps_film(movie: &str, files: &[(&str, &str)]) -> (tempfile::TempDir, Result<Movie>) {
    let dir = tempfile::tempdir().unwrap();
    for (p, text) in files {
        let f = dir.path().join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, text).unwrap();
    }
    let m = fixture(movie, dir.path());
    (dir, m)
}

const COMP_AB: &str = "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n\
         [[shot]]\nid = \"b\"\nscene = \"b.glsl\"\nduration = 1\n\
         transition = { type = \"x.glsl\", duration = 0.5 }\n\
         [[layer]]\nid = \"t\"\nscene = \"t.glsl\"\nfrom = \"b\"\n";

#[test]
fn comps_parse() {
    let (_d, m) = comps_film(
        "[comps]\nintro = \"comps/intro.toml\"\ntv = { file = \"comps/tv.toml\", size = [64, 36], loop = true }\n\
             [[shot]]\nid = \"open\"\ncomp = \"intro\"\nduration = 2\ntime_offset = 0.5\n\
             [[shot]]\nid = \"room\"\nscene = \"r.glsl\"\nduration = 2\n\
             channels = [\"@tv\", { source = \"@tv\", time_offset = 2, filter = \"nearest\" }]\n\
             [[layer]]\nid = \"lt\"\ncomp = \"intro\"\nfrom = 1\nopacity = 0.5\n\
             [[layer]]\nid = \"title\"\nscene = \"t.glsl\"\nmatte = { source = \"@lt\", channel = \"luma\", invert = true }\n\
             [[layer]]\nid = \"m2\"\nscene = \"t.glsl\"\nmatte = { source = \"@tv\" }\n",
        &[
            ("comps/intro.toml", COMP_AB),
            (
                "comps/tv.toml",
                "[[shot]]\nid = \"s\"\nscene = \"s.glsl\"\nduration = 0.5\n",
            ),
        ],
    );
    let m = m.unwrap();
    assert_eq!(m.comps.len(), 2);
    let (intro, tv) = (&m.comps[0], &m.comps[1]);
    assert_eq!(
        (intro.name.as_str(), intro.size, intro.looping),
        ("intro", None, false)
    );
    assert_eq!((tv.size, tv.looping), (Some((64, 36)), true));
    assert_eq!(intro.movie.duration, 1.5);
    assert_eq!(intro.movie.layers[0].start, 0.5);
    assert_eq!(tv.movie.loop_frames, Some(15));
    assert_eq!(m.shots[0].comp, Some(0));
    assert_eq!(m.shots[0].scene, m.root.join("comps/intro.toml"));
    let ch = m.shots[1].channels[1].as_ref().unwrap();
    assert_eq!(
        ch.source,
        Source::Comp {
            index: 1,
            name: "tv".into(),
            time_offset: 2.0
        }
    );
    assert_eq!(ch.filter, Filter::Nearest);
    assert_eq!(m.layers[0].scene, LayerScene::Comp(0));
    assert_eq!(
        m.layers[1].matte,
        Some(Matte {
            source: MatteSource::Layer(0),
            luma: true,
            invert: true
        })
    );
    assert_eq!(m.layers[2].matte.unwrap().source, MatteSource::Comp(1));
    // Comp cues are comp-local; the film's cues don't change.
    let c = intro.movie.comp_cue_defines();
    assert!(c.contains("#define COMP_DURATION 1.5\n"), "{c}");
    assert!(c.contains("#define COMP_CUE_B_START 0.5\n"), "{c}");
    assert!(c.contains("#define COMP_CUE_T_END 1.5\n"), "{c}");
    assert!(!m.cue_defines().contains("COMP_"));
    // The comp files are watched.
    let files = m.referenced_files();
    assert!(files.contains(&m.root.join("comps/tv.toml")));
    assert!(files.contains(&m.root.join("s.glsl")));
    // Comps and mattes need 0.1.7.
    let w = m.contract_warning().unwrap();
    assert!(
        w.contains("comps or mattes, which need shotglass 0.1.7"),
        "{w}"
    );
    // Glyph runs need 0.1.8.
    let runs = texts_film(
        "body = \"fonts/Body.ttf\"\n",
        "run = { text = \"a\", font = \"body\", glyphs = true }\n",
        &["channels = [\"run\"]"],
    )
    .unwrap();
    let w = runs.contract_warning().unwrap();
    assert!(
        w.contains("glyph runs, glyph sets or subtitles, which need shotglass 0.1.8"),
        "{w}"
    );
}

#[test]
fn comp_time_holds_or_wraps() {
    let (_d, m) = comps_film(
        "[comps]\nhold = \"h.toml\"\nwrap = { file = \"h.toml\", loop = true }\n\
             [[shot]]\nid = \"s\"\ncomp = \"hold\"\nduration = 1\n",
        &[(
            "h.toml",
            "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n",
        )],
    );
    let m = m.unwrap();
    let (hold, wrap) = (&m.comps[0], &m.comps[1]);
    let last = m.frame_time(29);
    assert_eq!(hold.local(-3.0), 0.0);
    assert_eq!(hold.local(0.25), 0.25);
    assert_eq!(hold.local(5.0), last);
    assert!((wrap.local(1.25) - 0.25).abs() < 1e-6);
    assert!((wrap.local(-0.25) - 0.75).abs() < 1e-6);
}

#[test]
fn comps_are_validated() {
    let one = "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n";
    for (movie, files, says) in [
        (
            "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"y\"\nduration = 1\n",
            vec![("x.toml", one)],
            "comp \"y\": no [comps] entry called that (there are \"x\")",
        ),
        (
            "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nscene = \"a.glsl\"\nduration = 1\n",
            vec![("x.toml", one)],
            "give scene or comp, not both",
        ),
        (
            "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\nchannels = [\"\"]\n",
            vec![("x.toml", one)],
            "a comp shot has no shader of its own",
        ),
        (
            "[comps]\nx = \"missing.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\n",
            vec![],
            "[comps] x: reading missing.toml",
        ),
        (
            "[comps]\nx = \"x.png\"\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\n",
            vec![],
            "a comp file is a .toml",
        ),
        (
            "[comps]\nx = { file = \"x.toml\", size = [0, 4] }\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\n",
            vec![("x.toml", one)],
            "size must be [width, height]",
        ),
        (
            "[comps]\nx = { file = \"x.toml\", speed = 2 }\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\n",
            vec![("x.toml", one)],
            "unknown option `speed` (a comp has file, size and loop)",
        ),
        (
            "[comps]\ninput = \"x.toml\"\n[[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 1\n",
            vec![("x.toml", one)],
            "isn't input or picture",
        ),
        (
            "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\n",
            vec![(
                "x.toml",
                "fps = 24\n[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n",
            )],
            "`fps` belongs to the film",
        ),
        (
            "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\n",
            vec![(
                "x.toml",
                "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\nsound = \"s.glsl\"\n",
            )],
            "comps are picture only",
        ),
        (
            "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\n",
            vec![("x.toml", "")],
            "a comp needs at least one [[shot]]",
        ),
        (
            "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\n",
            vec![(
                "x.toml",
                "[[shot]]\nid = \"a\"\ncomp = \"x\"\nduration = 1\n",
            )],
            "comp \"x\" can't show itself",
        ),
        (
            "[comps]\nx = \"x.toml\"\ny = \"y.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"x\"\nduration = 1\n",
            vec![
                (
                    "x.toml",
                    "[[shot]]\nid = \"a\"\ncomp = \"y\"\nduration = 1\n",
                ),
                (
                    "y.toml",
                    "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\nchannels = [\"@x\"]\n",
                ),
            ],
            "comps show each other in a cycle: x → y → x",
        ),
        (
            "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 1\n[[layer]]\nid = \"x\"\nscene = \"l.glsl\"\n",
            vec![("x.toml", one)],
            "layers and comps share @ names",
        ),
        (
            "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 1\nchannels = [{ source = \"@x\", filter = \"mipmap\" }]\n",
            vec![("x.toml", one)],
            "filter \"mipmap\" isn't available on @ sources",
        ),
        (
            "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 1\n[[layer]]\nid = \"l\"\nscene = \"l.glsl\"\nchannels = [{ source = \"@input\", time_offset = 1 }]\n",
            vec![("x.toml", one)],
            "time_offset is for comps",
        ),
        (
            "[comps]\nx = \"x.toml\"\n[[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 1\n[[layer]]\nid = \"l\"\ncomp = \"x\"\nuniforms.u = 1\n",
            vec![("x.toml", one)],
            "a comp layer has no shader of its own",
        ),
    ] {
        let (_d, m) = comps_film(movie, &files);
        let e = err(m);
        assert!(e.contains(says), "{movie}: {e}");
    }
    // Five levels deep is refused; four are fine.
    let chain = |n: usize| {
        let mut files = vec![];
        let mut comps = String::from("[comps]\n");
        for i in 0..n {
            comps += &format!("c{i} = \"c{i}.toml\"\n");
            let body = if i + 1 < n {
                format!(
                    "[[shot]]\nid = \"a\"\ncomp = \"c{}\"\nduration = 1\n",
                    i + 1
                )
            } else {
                "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n".to_string()
            };
            files.push((format!("c{i}.toml"), body));
        }
        let movie = format!("{comps}[[shot]]\nid = \"s\"\ncomp = \"c0\"\nduration = 1\n");
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (_d, m) = comps_film(&movie, &files);
        m
    };
    assert!(chain(4).is_ok());
    let e = err(chain(5));
    assert!(e.contains("at most 4 levels"), "{e}");
}

#[test]
fn layers_are_validated() {
    for (layer, says) in [
        (
            "id = \"moon\"\nscene = \"l.glsl\"",
            "the id is taken; shots and layers share ids",
        ),
        ("id = \"a b\"\nscene = \"l.glsl\"", "may only use letters"),
        ("id = \"x\"", "a layer needs scene = \"…\""),
        (
            "id = \"x\"\nscene = \"l.glsl\"\ntype = \"fx/grain\"",
            "give one of scene, type and comp",
        ),
        (
            "id = \"x\"\ntype = \"grain\"",
            "needs a library (\"fx/grain\")",
        ),
        (
            "id = \"x\"\ntype = \"kit/grain\"",
            "names library \"kit\", but [libraries] has \"fx\"",
        ),
        (
            "id = \"x\"\nscene = \"l.glsl\"\nfrom = \"mooon\"",
            "no shot or earlier layer called \"mooon\" (shots: moon, closeup)",
        ),
        (
            "id = \"x\"\nscene = \"l.glsl\"\nfrom = \"moon.middle\"",
            "a shot has .start, .settled and .end",
        ),
        (
            "id = \"x\"\nscene = \"l.glsl\"\nfrom = \"moon + soon\"",
            "the offset after + must be seconds",
        ),
        (
            "id = \"x\"\nscene = \"l.glsl\"\nfrom = 3\nto = 2",
            "must lie within the film (0–5.0 s), from before to",
        ),
        (
            "id = \"x\"\nscene = \"l.glsl\"\nto = 9",
            "must lie within the film",
        ),
        (
            "id = \"x\"\nscene = \"l.glsl\"\nfrom = 1\nto = 2\nfade = [0.8, 0.8]",
            "must be ≥ 0 and fit the span",
        ),
        (
            "id = \"x\"\nscene = \"l.glsl\"\nscale = 2",
            "scale must be \"auto\" or a fraction",
        ),
        (
            "id = \"x\"\nscene = \"l.glsl\"\nscale = \"half\"",
            "scale must be \"auto\" or a fraction",
        ),
        (
            "id = \"x\"\nscene = \"l.glsl\"\nopacity = [1, 2]",
            "opacity is one number",
        ),
        (
            "id = \"x\"\nscene = \"l.glsl\"\nblend = \"screen\"",
            "blend \"screen\" needs a library (\"blend/screen\") or a film file",
        ),
        (
            "id = \"x\"\nscene = \"l.glsl\"\ncomp = \"intro\"",
            "give one of scene, type and comp",
        ),
        (
            "id = \"x\"\ncomp = \"intro\"",
            "comp \"intro\": no [comps] entry called that (there are none)",
        ),
        (
            "id = \"x\"\nscene = \"l.glsl\"\nmatte = { source = \"@t\" }",
            "source \"@t\" is neither an earlier layer nor a comp",
        ),
        (
            "id = \"x\"\nscene = \"l.glsl\"\nmatte = { source = \"t\" }",
            "must be \"@<earlier layer>\" or \"@<comp>\"",
        ),
        (
            "id = \"x\"\nscene = \"l.glsl\"\ntransition_in = { type = \"fx/iris\" }",
            "layer transitions arrive in a later phase",
        ),
        (
            "id = \"x\"\nscene = \"l.glsl\"\nchannels = [\"@nobody\"]",
            "\"@nobody\" is not a layer below this one",
        ),
        (
            "id = \"x\"\nscene = \"l.glsl\"\nchannels = [\"@x\"]",
            "\"@x\" is this layer; a layer can't read itself",
        ),
        (
            "id = \"x\"\nscene = \"l.glsl\"\nblend = \"kit/screen\"",
            "blend \"kit/screen\" names library \"kit\", but [libraries] has \"fx\"",
        ),
        (
            "id = \"x\"\nscene = \"l.glsl\"\nopactiy = 1",
            "unknown field `opactiy`",
        ),
    ] {
        let e = err(layers_film(&format!("[[layer]]\n{layer}\n")));
        assert!(e.contains(says), "{layer}: {e}");
    }
    // Layers can't be spanned by a later layer's id; a shot can't read @input.
    let e = err(layers_film(
        "[[layer]]\nid = \"a\"\nscene = \"l.glsl\"\nto = \"b.end\"\n[[layer]]\nid = \"b\"\nscene = \"l.glsl\"\n",
    ));
    assert!(e.contains("no shot or earlier layer called \"b\""), "{e}");
    let e = err(fixture(
        "[[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 1\nchannels = [\"@input\"]\n",
        Path::new("/film"),
    ));
    assert!(
            e.contains("\"@input\" is another pass's picture; only a [[layer]]'s channels can read one (a shot has nothing below it)"),
            "{e}"
        );
    // A loop film's layers stay inside one period.
    let e = err(fixture(
        "loop = true\n[[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = 2\n[[layer]]\nid = \"x\"\nscene = \"l.glsl\"\nfrom = 1\nto = 3\n",
        Path::new("/film"),
    ));
    assert!(
        e.contains("a loop film's layers stay inside one period"),
        "{e}"
    );
}

fn texts_film(fonts: &str, texts: &str, shots: &[&str]) -> Result<Movie> {
    let mut t = format!(
        "[libraries]\nkit = {{ path = \"lib/kit\" }}\n[textures]\nbrick = \"t/brick.png\"\n[fonts]\n{fonts}[texts]\n{texts}"
    );
    let shots = if shots.is_empty() { &[""][..] } else { shots };
    for (i, extra) in shots.iter().enumerate() {
        t += &format!("[[shot]]\nid = \"s{i}\"\nscene = \"a.glsl\"\nduration = 1\n{extra}\n");
    }
    fixture(&t, Path::new("/film"))
}

#[test]
fn fonts_and_texts_parse() {
    let m = texts_film(
            "body = \"fonts/Body.ttf\"\nbold = { file = \"<kit/fonts/Var.ttf>\", variations = { wght = 700 }, fallback = [\"body\", \"hebrew\"], licence = \"OFL-1.1\" }\nhebrew = { file = \"fonts/Hebrew.otf\", index = 1 }\n",
            "title = { text = \"Big Shadow\", font = \"bold\", size = 160, align = \"center\", width = 1400, line_height = 1.25, letter_spacing = 0.05, max_lines = 2, direction = \"auto\", features = [\"ss01\", \"-liga\"], sdf_spread = 12 }\ncredits = { file = \"credits.txt\", font = \"body\" }\n",
            &["channels = [\"title\", \"brick\", { text = \"credits\", wrap = \"repeat\" }, \"\", \"\", \"\", \"\", \"title\"]"],
        )
        .unwrap();
    let c = &m.shots[0].channels;
    let title = c[0].as_ref().unwrap();
    let t = title.text().unwrap();
    assert_eq!(t.name, "title");
    assert_eq!(t.body, TextBody::Inline("Big Shadow".into()));
    // The chain: the font, then its fallbacks depth first.
    let chain: Vec<&str> = t.fonts.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(chain, ["bold", "body", "hebrew"]);
    assert_eq!(t.fonts[0].variations, vec![("wght".to_string(), 700.0)]);
    assert_eq!(t.fonts[0].file.library.as_deref(), Some("kit"));
    assert_eq!(t.fonts[0].licence.as_deref(), Some("OFL-1.1"));
    assert_eq!(t.fonts[2].index, 1);
    assert_eq!(t.fonts[1].file.file, PathBuf::from("/film/fonts/Body.ttf"));
    let s = &t.style;
    assert_eq!(
        (s.size, s.width, s.line_height, s.letter_spacing, s.spread),
        (160.0, Some(1400.0), 1.25, 0.05, 12.0)
    );
    assert_eq!(
        (s.align, s.max_lines, s.direction),
        (
            crate::text::Align::Center,
            Some(2),
            crate::text::Direction::Auto
        )
    );
    assert_eq!(s.features, ["ss01", "-liga"]);
    // Text defaults suit text: linear, clamp, upright.
    assert_eq!(
        (title.filter, title.wrap, title.vflip, title.srgb),
        (Filter::Linear, Wrap::Clamp, true, false)
    );
    assert_eq!(title.label(), "text \"title\"");
    // Images and texts mix; a table can re-sample a text; channel 7 is usable.
    assert!(c[1].as_ref().unwrap().image().is_some());
    let credits = c[2].as_ref().unwrap();
    assert_eq!(credits.wrap, Wrap::Repeat);
    assert!(matches!(&credits.text().unwrap().body, TextBody::File(f) if f.label == "credits.txt"));
    assert_eq!(c[7].as_ref(), Some(title));
    // Files a text reads: its file and every font.
    let files: Vec<&str> = credits.files().iter().map(|f| f.label.as_str()).collect();
    assert_eq!(files, ["credits.txt", "fonts/Body.ttf"]);
    // Text and channels 4..7 need 0.1.3.
    let w = m.contract_warning().unwrap();
    assert!(
        w.contains("text ([fonts] and [texts]), which needs shotglass 0.1.3"),
        "{w}"
    );
    assert!(
        w.contains("iChannel4..7, which need shotglass 0.1.3"),
        "{w}"
    );
    assert!(w.contains("shotglass = \"0.1.3\""), "{w}");
}

#[test]
fn fonts_and_texts_are_validated() {
    let body = "body = \"fonts/Body.ttf\"\n";
    let text_err = |texts: &str| err(texts_film(body, texts, &[]));
    for (texts, says) in [
        (
            "t = { font = \"body\" }\n",
            "a text needs text = \"…\", file = \"…\" (UTF-8) or subtitles",
        ),
        (
            "t = { text = \"a\", file = \"a.txt\", font = \"body\" }\n",
            "give one of text, file or subtitles (this one has text and file)",
        ),
        ("t = { text = \"a\" }\n", "a text needs font = \"…\""),
        (
            "t = { text = \"a\", font = \"bodyy\" }\n",
            "font \"bodyy\" isn't in [fonts] (there are \"body\")",
        ),
        (
            "t = { text = \"a\", font = \"body\", size = 2 }\n",
            "size must be 4–1024",
        ),
        (
            "t = { text = \"a\", font = \"body\", width = 0 }\n",
            "width must be 1–4096",
        ),
        (
            "t = { text = \"a\", font = \"body\", align = \"justify\" }\n",
            "align \"justify\" isn't one of \"left\", \"center\", \"right\", \"start\", \"end\"",
        ),
        (
            "t = { text = \"a\", font = \"body\", direction = \"ttb\" }\n",
            "direction \"ttb\" isn't one of",
        ),
        (
            "t = { text = \"a\", font = \"body\", max_lines = 0 }\n",
            "max_lines must be a whole number ≥ 1",
        ),
        (
            "t = { text = \"a\", font = \"body\", features = [\"not a feature!\"] }\n",
            "isn't an OpenType feature",
        ),
        (
            "t = { text = \"a\", font = \"body\", srgb = true }\n",
            "srgb is for images",
        ),
        (
            "t = { text = \"a\", font = \"body\", colour = \"red\" }\n",
            "unknown option `colour`",
        ),
        (
            "t = { text = \"a\", font = \"body\", glyphs = \"0123\" }\n",
            "add set = true",
        ),
        (
            "t = { font = \"body\", set = true }\n",
            "set = true needs the characters as glyphs = \"0123456789:.\"",
        ),
        (
            "t = { text = \"a\", font = \"body\", glyphs = \"01\", set = true }\n",
            "text doesn't go with set = true",
        ),
        (
            "t = { font = \"body\", glyphs = \"\", set = true }\n",
            "has no characters",
        ),
        (
            "t = { font = \"body\", glyphs = 3 }\n",
            "glyphs must be true, false or",
        ),
        (
            "t = { subtitles = \"subs.srt\", font = \"body\" }\n",
            "subtitles are drawn as a glyph run; add glyphs = true",
        ),
        (
            "t = { subtitles = \"subs.txt\", font = \"body\", glyphs = true }\n",
            "must be a .srt or .vtt file",
        ),
        (
            "t = { text = \"a\", subtitles = \"s.srt\", font = \"body\", glyphs = true }\n",
            "give one of text, file or subtitles (this one has text and subtitles)",
        ),
        (
            "t = { text = \"a\", font = \"body\", atlas_size = 64 }\n",
            "atlas_size is for glyph runs",
        ),
        (
            "t = { text = \"a\", font = \"body\", glyphs = true, atlas_size = 4 }\n",
            "atlas_size must be 8–256",
        ),
        (
            "t = { text = \"a\", font = \"body\", glyphs = true, filter = \"mipmap\" }\n",
            "a glyph atlas isn't mipmapped",
        ),
        (
            "t = { text = \"a\", font = \"body\", glyphs = true, vflip = false }\n",
            "vflip isn't for glyph runs",
        ),
        (
            "t = { font = \"body\", glyphs = \"01\", set = true, width = 100 }\n",
            "width and max_lines are for laid-out text, not a glyph set",
        ),
        ("t = \"Hello\"\n", "a text is a table"),
        (
            "brick = { text = \"a\", font = \"body\" }\n",
            "[texts] brick: [textures] has a \"brick\" too",
        ),
        (
            "my-text = { text = \"a\", font = \"body\" }\n",
            "a text name may only use letters",
        ),
    ] {
        let e = text_err(texts);
        assert!(e.contains(says), "{texts}: {e}");
    }
    for (fonts, says) in [
        ("f = \"fonts/a.woff2\"\n", "WOFF/WOFF2 fonts aren't read"),
        (
            "f = \"fonts/a.png\"\n",
            "shotglass reads TrueType and OpenType fonts",
        ),
        (
            "f = { file = \"a.ttf\", fallback = [\"nope\"] }\n",
            "fallback \"nope\" isn't in [fonts]",
        ),
        (
            "f = { file = \"a.ttf\", variations = { weight = 7 } }\n",
            "\"weight\" isn't an axis tag",
        ),
        (
            "f = { file = \"a.ttf\", index = -1 }\n",
            "index must be a whole number ≥ 0",
        ),
        (
            "f = { file = \"a.ttf\", size = 3 }\n",
            "unknown option `size`",
        ),
        ("f = { index = 1 }\n", "a font table needs file"),
        ("f = \"../a.ttf\"\n", "climbs"),
    ] {
        let e = err(texts_film(fonts, "", &[]));
        assert!(e.contains(says), "{fonts}: {e}");
    }
    // A fallback cycle is fine: each font once.
    let m = texts_film(
            "a = { file = \"a.ttf\", fallback = [\"b\"] }\nb = { file = \"b.ttf\", fallback = [\"a\"] }\n",
            "t = { text = \"x\", font = \"a\" }\n",
            &["channels = [\"t\"]"],
        )
        .unwrap();
    let t = m.shots[0].channels[0].as_ref().unwrap().text().unwrap();
    let names: Vec<&str> = t.fonts.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["a", "b"]);
    // Channel tables: text names a [texts] entry; srgb stays refused.
    let one = "t = { text = \"a\", font = \"body\" }\n";
    let e = err(texts_film(
        body,
        one,
        &["channels = [{ text = \"brick\" }]"],
    ));
    assert!(
        e.contains("text = \"brick\": no [texts] entry called \"brick\""),
        "{e}"
    );
    let e = err(texts_film(
        body,
        one,
        &["channels = [{ text = \"t\", srgb = true }]"],
    ));
    assert!(e.contains("srgb is for images"), "{e}");
    // Unused texts are pointed out like unused textures.
    let m = texts_film(body, one, &["channels = [\"brick\"]"]).unwrap();
    assert_eq!(
        m.warnings,
        vec!["[texts] t isn't used by any shot's channels".to_string()]
    );
}

#[test]
fn unused_textures_warn_and_contract_is_checked() {
    let m = channels_film(
        "",
        "noise = \"n.png\"\nspare = \"s.png\"\n",
        &["channels = [\"noise\"]"],
    )
    .unwrap();
    assert_eq!(
        m.warnings,
        vec!["[textures] spare isn't used by any shot's channels".to_string()]
    );
    let w = m.contract_warning().unwrap();
    assert!(w.contains("shotglass = \"0.1.1\""), "{w}");
    let m = channels_film("shotglass = \"0.1\"\n", "", &["channels = [\"n.png\"]"]).unwrap();
    assert!(m.contract_warning().unwrap().contains("it says \"0.1\""));
    let m = channels_film("shotglass = \"0.1.1\"\n", "", &["channels = [\"n.png\"]"]).unwrap();
    assert_eq!(m.contract_warning(), None);
}

#[test]
fn load_checks_texture_files_and_resolves_library_ones() {
    let dir = std::env::temp_dir().join(format!("shotglass-movie-tex-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("lib/kit/textures")).unwrap();
    std::fs::create_dir_all(dir.join("textures")).unwrap();
    std::fs::write(dir.join("a.glsl"), "").unwrap();
    std::fs::write(
        dir.join("lib/kit/library.toml"),
        "name = \"me/kit\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let img = |p: &str, w: u32, h: u32| {
        image::RgbaImage::new(w, h).save(dir.join(p)).unwrap();
    };
    img("lib/kit/textures/noise.png", 4, 4);
    img("textures/stars.png", 2, 2);
    let film = |channels: &str| {
        crate::project::write_fixture(
                &dir,
                &format!("[libraries]\nkit = {{ path = \"lib/kit\" }}\n[[shot]]\nid = \"sky\"\nscene = \"a.glsl\"\nduration = 1\nchannels = {channels}\n"),
            )
            .unwrap();
        Movie::load(&dir)
    };
    let m = film("[\"<kit/textures/noise.png>\", \"textures/stars.png\"]").unwrap();
    assert_eq!(
        m.shots[0].channels[0]
            .as_ref()
            .unwrap()
            .image()
            .unwrap()
            .file,
        dir.join("lib/kit/textures/noise.png")
    );
    assert_eq!(m.textures().len(), 2);
    let e = format!(
        "{:#}",
        film("[\"\", \"\", \"textures/strs.png\"]").unwrap_err()
    );
    assert!(
            e.contains("shot \"sky\": channels[2]: \"textures/strs.png\" not found (did you mean \"textures/stars.png\"?)"),
            "{e}"
        );
    let e = format!("{:#}", film("[\"<kit/textures/nois.png>\"]").unwrap_err());
    assert!(e.contains("did you mean \"kit/textures/noise.png\""), "{e}");
    // A case-only match is the usual case warning.
    let m = film("[\"textures/Stars.png\"]");
    if let Ok(m) = m {
        assert!(
            m.warnings.iter().any(|w| w.contains("Stars.png")),
            "{:?}",
            m.warnings
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn glyph_runs_bind_their_atlas_and_table() {
    let m = texts_film(
        "body = \"fonts/Body.ttf\"\n",
        "credits = { file = \"credits.txt\", font = \"body\", glyphs = true, atlas_size = 64 }\n\
             digits = { glyphs = \"0123456789:\", set = true, font = \"body\" }\n\
             subs = { subtitles = \"subs.vtt\", font = \"body\", width = 900, glyphs = true }\n\
             title = { text = \"t\", font = \"body\" }\n",
        &[
            "channels = [\"credits\", { text = \"credits\", part = \"glyphs\" }, \"digits\", \
               { text = \"digits\", part = \"glyphs\" }, { text = \"subs\", part = \"atlas\" }, \
               { text = \"subs\", part = \"glyphs\" }, \"title\"]",
        ],
    )
    .unwrap();
    let c = &m.shots[0].channels;
    let t = |i: usize| c[i].as_ref().unwrap().text().unwrap();
    assert_eq!(
        (t(0).glyphs, t(0).part, t(0).atlas_size),
        (GlyphMode::Run, TextPart::Main, 64.0)
    );
    assert_eq!((t(1).glyphs, t(1).part), (GlyphMode::Run, TextPart::Table));
    assert_eq!((t(2).glyphs, t(3).part), (GlyphMode::Set, TextPart::Table));
    assert_eq!(t(2).body, TextBody::Inline("0123456789:".into()));
    assert_eq!(t(2).atlas_size, ATLAS_SIZE);
    assert!(matches!(&t(4).body, TextBody::Subtitles(f) if f.label == "subs.vtt"));
    assert_eq!((t(6).glyphs, t(6).part), (GlyphMode::Block, TextPart::Main));
    // Atlases filter linearly; tables are nearest + clamp; glyph runs never flip;
    // their layout isn't limited to one texture's height.
    let ch = |i: usize| c[i].as_ref().unwrap();
    assert_eq!((ch(0).filter, ch(0).vflip), (Filter::Linear, false));
    assert_eq!(
        (ch(1).filter, ch(1).wrap, ch(1).vflip),
        (Filter::Nearest, Wrap::Clamp, false)
    );
    assert!(t(0).style.tall && !t(6).style.tall);
    assert!(ch(6).vflip);
    // The subtitle file is one of the channel's files (watched, bundled).
    assert!(ch(4).files().iter().any(|f| f.label == "subs.vtt"));

    let bind_err = |entry: &str| {
        err(texts_film(
            "body = \"fonts/Body.ttf\"\n",
            "run = { text = \"a\", font = \"body\", glyphs = true }\nblock = { text = \"a\", font = \"body\" }\n",
            &[&format!("channels = [{entry}]")],
        ))
    };
    for (entry, says) in [
        (
            r#"{ text = "block", part = "glyphs" }"#,
            "part = \"glyphs\" is for glyph runs; text \"block\" is a block",
        ),
        (
            r#"{ text = "run", part = "table" }"#,
            "part must be \"atlas\" or \"glyphs\"",
        ),
        (
            r#"{ text = "run", part = "glyphs", filter = "linear" }"#,
            "filter doesn't apply",
        ),
        (
            r#"{ file = "a.png", part = "glyphs" }"#,
            "it goes with text = \"…\"",
        ),
    ] {
        let e = bind_err(entry);
        assert!(e.contains(says), "{entry}: {e}");
    }
}

#[test]
fn glyph_runs_load_lay_out_and_check_subtitle_times() {
    use crate::glyphrun::{HEADER_TEXELS, KIND_RUN, KIND_SET, TABLE_WIDTH, decode};
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    std::fs::create_dir_all(dir.join("fonts")).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/films/text/fonts/NotoSans-subset.ttf"),
        dir.join("fonts/sans.ttf"),
    )
    .unwrap();
    std::fs::write(dir.join("a.glsl"), "").unwrap();
    std::fs::write(
            dir.join("subs.srt"),
            "1\n00:00:00,500 --> 00:00:01,000\nBig Shadow\n\n2\n00:00:01,000 --> 00:00:01,900\nthe wall\n",
        )
        .unwrap();
    let film = |duration: f32| {
        crate::project::write_fixture(
                dir,
                &format!(
                    "[fonts]\nsans = \"fonts/sans.ttf\"\n[texts]\n\
                     run = {{ text = \"Big Shadow\\nover the wall\", font = \"sans\", size = 40, glyphs = true }}\n\
                     digits = {{ glyphs = \"0123456789\", set = true, font = \"sans\" }}\n\
                     subs = {{ subtitles = \"subs.srt\", font = \"sans\", glyphs = true }}\n\
                     [[shot]]\nid = \"s\"\nscene = \"a.glsl\"\nduration = {duration}\n\
                     channels = [\"run\", {{ text = \"run\", part = \"glyphs\" }}, \"digits\", \
                     {{ text = \"digits\", part = \"glyphs\" }}, {{ text = \"subs\", part = \"glyphs\" }}]\n"
                ),
            )
            .unwrap();
        Movie::load(dir)
    };
    let m = film(2.0).unwrap();
    let text = |i: usize| {
        m.shots[0].channels[i]
            .as_ref()
            .unwrap()
            .text()
            .unwrap()
            .clone()
    };
    // The decode worker's output: the atlas, and a table a shader can read.
    let atlas = crate::texture::load_text(&text(0), true).unwrap();
    assert_eq!(atlas.meta, [ATLAS_SIZE, ATLAS_SPREAD, 40.0, 0.0]);
    assert!(atlas.width > 0 && atlas.rgba.len() == atlas.width * atlas.height * 4);
    let table = crate::texture::load_text(&text(1), true).unwrap();
    assert_eq!(table.width, TABLE_WIDTH);
    assert_eq!(
        table.meta,
        [TABLE_WIDTH as f32, HEADER_TEXELS as f32, 40.0, ATLAS_SPREAD]
    );
    let texels: Vec<[u8; 4]> = table
        .rgba
        .chunks(4)
        .map(|c| c.try_into().unwrap())
        .collect();
    let d = decode(&texels).unwrap();
    assert_eq!((d.kind, d.lines.len(), d.glyphs.len()), (KIND_RUN, 2, 23));
    let set: Vec<[u8; 4]> = crate::texture::load_text(&text(3), true)
        .unwrap()
        .rgba
        .chunks(4)
        .map(|c| c.try_into().unwrap())
        .collect();
    assert_eq!(decode(&set).unwrap().kind, KIND_SET);
    let subs: Vec<[u8; 4]> = crate::texture::load_text(&text(4), true)
        .unwrap()
        .rgba
        .chunks(4)
        .map(|c| c.try_into().unwrap())
        .collect();
    assert_eq!(
        decode(&subs).unwrap().entries,
        [(500, 1000, 0, 1), (1000, 1900, 1, 1)]
    );
    // The atlas and table of one text share one build; editing a file it
    // reads makes a new one.
    let a = text(0).glyph_run_shared().unwrap();
    let b = text(1).glyph_run_shared().unwrap();
    assert!(std::sync::Arc::ptr_eq(&a, &b));
    let before = text(4).glyph_run_shared().unwrap();
    std::fs::write(
        dir.join("subs.srt"),
        "1\n00:00:00,500 --> 00:00:01,000\nBig Shadows\n\n\
             2\n00:00:01,000 --> 00:00:01,900\nthe walls\n",
    )
    .unwrap();
    let after = text(4).glyph_run_shared().unwrap();
    assert!(!std::sync::Arc::ptr_eq(&before, &after));
    assert_eq!(decode(after.table.data()).unwrap().entries.len(), 2);
    // Bound only through a channel table, the texts still count as used.
    assert!(m.warnings.is_empty(), "{:?}", m.warnings);
    // Memory accounting sees every part.
    let (bytes, parts) = crate::texture::shot_memory(&m.shots[0].channels);
    assert!(bytes > 0 && parts.len() == 5, "{parts:?}");
    // A cue past the end of the film refuses the version.
    let e = format!("{:#}", film(1.5).unwrap_err());
    assert!(
        e.contains("the cue at 0:00:01.000 ends at 0:00:01.900, after the film's 0:00:01.500"),
        "{e}"
    );
}

#[test]
fn rejects_unknown_fields() {
    let bad = "[[shot]]\nid=\"a\"\nscene=\"a.glsl\"\nduration=1\nlenght=2\n";
    assert!(fixture(bad, Path::new(".")).is_err());
}

fn loop_film(extra: &str, shots: &str) -> Result<Movie> {
    fixture(&format!("loop = true\n{extra}\n{shots}"), Path::new("."))
}

#[test]
fn loop_field_sets_a_whole_frame_period() {
    let m = loop_film("", M).unwrap();
    // 5 + 4 + 3 - 1 (transition) = 11 s = 330 frames at 30 fps
    assert_eq!(m.loop_frames, Some(330));
    assert_eq!(m.frame_count(), 330);
    assert_eq!(m.loop_period(), Some(11.0));
    assert_eq!(m.sound_samples(48000), 528000);
    assert_eq!(m.sound_samples(44100), 485100);
    let m = fixture(M, Path::new(".")).unwrap();
    assert_eq!(m.loop_frames, None);
    assert_eq!(m.loop_period(), None);
    let m = fixture(&format!("loop = false\n{M}"), Path::new(".")).unwrap();
    assert_eq!(m.loop_frames, None);
    // NTSC: 300 frames at 30000/1001 = 10.01 s; the period is exact, the samples round.
    let one = "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 10.01\n";
    let m = loop_film("fps = \"30000/1001\"", one).unwrap();
    assert_eq!(m.loop_frames, Some(300));
    assert!((m.loop_period().unwrap() - 10.01).abs() < 1e-12);
    assert_eq!(m.sound_samples(48000), 480480);
}

/// `loop = "warm"` (#138): a loop film (or comp) whose seam-spanning buffers carry
/// their state over the seam.
#[test]
fn loop_warm() {
    let one = "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 2\n";
    let buf = "[[buffer]]\nid = \"t\"\nscene = \"b.glsl\"\nchannels = [\"@prev\"]\n";
    let m = fixture(
        &format!("shotglass = \"0.1.10\"\nloop = \"warm\"\n{one}{buf}"),
        Path::new("."),
    )
    .unwrap();
    assert_eq!((m.loop_frames, m.warm), (Some(60), true));
    assert_eq!(warm_buffers(&m).count(), 1);
    assert!(m.warnings.is_empty(), "{:?}", m.warnings);
    assert_eq!(m.contract_warning(), None);
    let m = fixture(
        &format!("shotglass = \"0.1.9\"\nloop = \"warm\"\n{one}{buf}"),
        Path::new("."),
    )
    .unwrap();
    assert!(m.contract_warning().unwrap().contains("loop = \"warm\""));
    // A plain loop isn't warm.
    let m = loop_film("", one).unwrap();
    assert!(!m.warm);
    // No buffer spans the seam: a warning, not an error.
    let late = "[[buffer]]\nid = \"t\"\nscene = \"b.glsl\"\nfrom = 1\n";
    let m = fixture(&format!("loop = \"warm\"\n{one}{late}"), Path::new(".")).unwrap();
    assert!(
        m.warnings.iter().any(|w| w.contains("has no effect")),
        "{:?}",
        m.warnings
    );
    assert_eq!(warm_buffers(&m).count(), 0);
    // Anything else is refused.
    for bad in ["\"hot\"", "1", "[true]"] {
        let e = err(fixture(&format!("loop = {bad}\n{one}"), Path::new(".")));
        assert!(
            e.contains("loop must be true, false or \"warm\""),
            "{bad}: {e}"
        );
    }
    // Comps too.
    let (_d, m) = comps_film(
        "[comps]\nc = { file = \"c.toml\", loop = \"warm\" }\n\
             [[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n",
        &[("c.toml", &format!("{one}{buf}"))],
    );
    let m = m.unwrap();
    assert!(m.comps[0].looping && m.comps[0].movie.warm && !m.warm);
    let (_d, m) = comps_film(
        "[comps]\nc = { file = \"c.toml\", loop = \"hot\" }\n\
             [[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n",
        &[("c.toml", one)],
    );
    assert!(err(m).contains("loop must be true, false or \"warm\""));
}

#[test]
fn loop_period_must_be_whole_frames() {
    let one = |d: &str| format!("[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = {d}\n");
    let e = loop_film("", &one("2.01")).unwrap_err().to_string();
    assert!(e.contains("whole number of frames"), "{e}");
    assert!(e.contains("60.300 frames at 30 fps"), "{e}");
    assert!(
        e.contains("2.0000s (60 frames) or 2.0333s (61 frames)"),
        "{e}"
    );
    let e = loop_film("fps = 24", &one("0.01")).unwrap_err().to_string();
    assert!(e.contains("(1 frames)"), "{e}");
    // Without loop, any duration is fine.
    assert!(fixture(&one("2.01"), Path::new(".")).is_ok());
    // A typo'd value is a type error, not silently ignored.
    assert!(fixture(&format!("loop = 3\n{}", one("2")), Path::new(".")).is_err());
}

#[test]
fn loop_refuses_sound_across_the_seam() {
    let shots = |a: &str, b: &str| {
        format!(
            "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 2\nsound = \"a.glsl\"\n{a}\n\
                 [[shot]]\nid = \"b\"\nscene = \"b.glsl\"\nduration = 2\nsound = \"b.glsl\"\n{b}\n"
        )
    };
    let e = loop_film("", &shots("sound_lead = 0.5", ""))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("shot \"a\": sound_lead") && e.contains("seam"),
        "{e}"
    );
    let e = loop_film("", &shots("", "sound_tail = 0.5"))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("shot \"b\": sound_tail") && e.contains("seam"),
        "{e}"
    );
    // Inside the film (between the shots) they're fine, and so is either without loop.
    assert!(loop_film("", &shots("sound_tail = 0.5", "sound_lead = 0.5")).is_ok());
    assert!(
        fixture(
            &shots("sound_lead = 0.5", "sound_tail = 0.5"),
            Path::new(".")
        )
        .is_ok()
    );
}

#[test]
fn loop_time_wraps_to_the_period() {
    let m = loop_film("", M).unwrap();
    assert_eq!(m.wrap(0.0), 0.0);
    assert_eq!(m.wrap(3.25), 3.25);
    assert_eq!(m.wrap(11.0), 0.0);
    assert_eq!(m.wrap(14.25), 3.25);
    assert_eq!(m.wrap(-1.0), 10.0);
    // Far out, an f32 time is coarse, but it wraps into the period, never onto it.
    let w = m.wrap(1.0e7);
    assert!((0.0..11.0).contains(&w), "{w}");
    assert_eq!(m.wrap_frame(329), 329);
    assert_eq!(m.wrap_frame(330), 0);
    assert_eq!(m.wrap_frame(330 * 1000 + 7), 7);
    // f64: hours in, precise to well under a sample.
    let p = 11.0;
    let t = 1000.0 * p + 3.25;
    assert!((wrap_time(t, p) - 3.25).abs() < 1e-9);
    assert!((0.0..p).contains(&wrap_time(p * 3.0 - 1e-15, p)));
    // Frames of loop k land on the frames of loop 0 (a few loops in: an f32 time
    // 1000 loops in is too coarse for that, which is why the player and export
    // wrap in f64 or by frame number).
    for k in [1u64, 2, 3] {
        for n in [0u64, 1, 165, 329] {
            let t = m.frame_time(k * 330 + n);
            assert_eq!(m.frame_at(m.wrap(t)), n as i64, "loop {k} frame {n}");
        }
    }
    // Not a loop film: unchanged.
    let m = fixture(M, Path::new(".")).unwrap();
    assert_eq!(m.wrap(14.25), 14.25);
    assert_eq!(m.wrap_frame(400), 400);
}

#[test]
fn loop_needs_contract_0_1_2() {
    let m = loop_film("", M).unwrap();
    let w = m.contract_warning().unwrap();
    assert!(
        w.contains("loop") && w.contains("shotglass = \"0.1.2\""),
        "{w}"
    );
    let m = loop_film("shotglass = \"0.1.1\"", M).unwrap();
    assert!(m.contract_warning().unwrap().contains("it says \"0.1.1\""));
    let m = loop_film("shotglass = \"0.1.2\"", M).unwrap();
    assert_eq!(m.contract_warning(), None);
}

#[test]
fn buffers_parse_spans_resets_and_channels() {
    // moon: 0–3 s, closeup: 2–5 s (frames 60–150 at 30 fps).
    let m = layers_film(
            "[[buffer]]\nid = \"trail\"\nscene = \"buf/trail.glsl\"\ninit = \"buf/seed.glsl\"\n\
             from = \"moon\"\nto = \"closeup.end\"\nsize = [96, 54]\nreset = [\"closeup\", 4.5]\n\
             channels = [\"@prev\", \"@picture\"]\nuniforms = { decay = 0.9 }\n\
             [[buffer]]\nid = \"hist\"\nscene = \"buf/copy.glsl\"\nscale = 0.5\nformat = \"rgba8\"\n\
             history = 8\nreset = \"cuts\"\n\
             channels = [\"@trail\", { source = \"@trail\", frame = \"prev\" }, \"@picture\"]\n\
             [[buffer]]\nid = \"sim\"\nscene = \"buf/rd.glsl\"\nformat = \"rgba32f\"\nfrom = 1\n\
             channels = [\"@prev\", { source = \"@sim\", frame = -1 }]\n\
             [[layer]]\nid = \"show\"\nscene = \"l.glsl\"\n\
             channels = [\"@trail\", { source = \"@hist\", frame = -8 }, \"@input\"]\n",
        )
        .unwrap();
    assert_eq!(m.buffers.len(), 3);
    let t = &m.buffers[0];
    assert_eq!((t.start_frame, t.end_frame), (0, 150));
    assert_eq!(t.resets, vec![0, 60, 135]);
    assert_eq!(t.reset_at(59), Some(0));
    assert_eq!(t.reset_at(60), Some(60));
    assert_eq!(t.reset_at(149), Some(135));
    assert_eq!(t.size, BufferSize::Fixed(96, 54));
    assert_eq!(t.format, BufferFormat::Rgba16f);
    assert_eq!(t.history, 1);
    assert!(t.init.is_some() && !t.is_pure());
    assert_eq!(
        t.channels[0].as_ref().unwrap().source,
        Source::Buffer {
            index: 0,
            id: "trail".into(),
            back: 1
        }
    );
    assert_eq!(t.channels[1].as_ref().unwrap().source, Source::Picture);
    let h = &m.buffers[1];
    assert_eq!(h.resets, vec![0, 60], "cuts: every shot start in the span");
    assert_eq!((h.history, h.format), (8, BufferFormat::Rgba8));
    assert_eq!(h.size.at((1920, 1080)), (960, 540));
    assert!(!h.is_pure(), "it reads another buffer");
    let sim = &m.buffers[2];
    assert_eq!((sim.start_frame, sim.resets.clone()), (30, vec![30]));
    assert!(!sim.active(29) && sim.active(30) && sim.active(149) && !sim.active(150));
    // rgba32f is read nearest (float filtering isn't portable).
    assert_eq!(sim.channels[0].as_ref().unwrap().filter, Filter::Nearest);
    assert_eq!(t.channels[0].as_ref().unwrap().filter, Filter::Linear);
    let l = &m.layers[0];
    assert_eq!(
        l.channels[1].as_ref().unwrap().source,
        Source::Buffer {
            index: 1,
            id: "hist".into(),
            back: 8
        }
    );
    // A pure history buffer reads no buffer state.
    let m = layers_film(
        "[[buffer]]\nid = \"h\"\nscene = \"c.glsl\"\nhistory = 4\nchannels = [\"@picture\"]\n",
    )
    .unwrap();
    assert!(m.buffers[0].is_pure());
    // Loop films reset every buffer at frame 0 (its span start).
    let m = loop_film(
        "shotglass = \"0.1.9\"",
        "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 2\n\
             [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\nchannels = [\"@prev\"]\n",
    )
    .unwrap();
    assert_eq!(m.buffers[0].resets, vec![0]);
    assert_eq!(m.buffers[0].end_frame, 60);
}

#[test]
fn buffers_are_validated() {
    let bad = |b: &str| err(layers_film(b));
    let one = |extra: &str| {
        bad(&format!(
            "[[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\n{extra}\n"
        ))
    };
    assert!(one("scale = \"auto\"").contains("can't be \"auto\""));
    assert!(one("scale = 1.5").contains("(0, 1]"));
    assert!(one("size = [10, 10]\nscale = 0.5").contains("not both"));
    assert!(one("size = [0, 10]").contains("1–8192"));
    assert!(one("format = \"rgb8\"").contains("isn't one of"));
    assert!(one("history = 0").contains("1 to 64"));
    assert!(one("history = 65").contains("1 to 64"));
    assert!(one("cache = \"sparse\"").contains("use \"dense\""));
    assert!(one("preview_lookback = 0").contains("more than 0"));
    assert!(one("preview_lookback = -1").contains("more than 0"));
    assert!(one("reset = [\"nope\"]").contains("no shot or earlier layer"));
    assert!(one("from = 3\nreset = [1]").contains("outside the buffer's span"));
    assert!(one("reset = 3").contains("\"cuts\" or a list"));
    assert!(one("blend = \"x\"").contains("unknown field"));
    assert!(one("channels = [\"@input\"]").contains("buffers step before layers"));
    assert!(one("channels = [\"@b\"]").contains("is this buffer; read its previous state"));
    assert!(one("channels = [{ source = \"@b\", frame = -2 }]").contains("history = 2"));
    assert!(one("channels = [{ source = \"@b\", frame = 1 }]").contains("negative"));
    assert!(
        one("channels = [{ source = \"@picture\", frame = -1 }]").contains("isn't a [[buffer]]")
    );
    // Forward-only: a later buffer's current state can't be read, its previous can.
    let two = |chans: &str| {
        layers_film(&format!(
            "[[buffer]]\nid = \"a\"\nscene = \"a.glsl\"\nchannels = {chans}\n\
                 [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\n"
        ))
    };
    assert!(err(two("[\"@b\"]")).contains("later buffer"));
    assert!(two("[{ source = \"@b\", frame = \"prev\" }]").is_ok());
    // Layers aren't readable from buffers; buffers aren't readable from shots.
    let e = bad("[[layer]]\nid = \"t\"\nscene = \"t.glsl\"\n\
             [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\nchannels = [\"@t\"]\n");
    assert!(e.contains("buffers step before layers, unless"), "{e}");
    let e = err(fixture(
        "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\nchannels = [\"@b\"]\n\
             [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\n",
        Path::new("/film"),
    ));
    assert!(e.contains("draw before buffers step"), "{e}");
    let e = err(fixture(
        "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\nchannels = [\"@prev\"]\n",
        Path::new("/film"),
    ));
    assert!(e.contains("only a [[buffer]]'s channels"), "{e}");
    let e = bad("[[layer]]\nid = \"t\"\nscene = \"t.glsl\"\nchannels = [\"@prev\"]\n");
    assert!(e.contains("only a [[buffer]]'s channels"), "{e}");
    // Ids are shared with shots, layers and comps.
    let e = bad("[[buffer]]\nid = \"moon\"\nscene = \"b.glsl\"\n");
    assert!(e.contains("the id is taken"), "{e}");
    let e = bad("[[buffer]]\nid = \"prev\"\nscene = \"b.glsl\"\n");
    assert!(e.contains("isn't prev"), "{e}");
    // Required memory is checked at load.
    let e =
        bad("[[buffer]]\nid = \"big\"\nscene = \"b.glsl\"\nformat = \"rgba32f\"\nhistory = 64\n");
    assert!(
        e.contains("MB of GPU memory at once") && e.contains("\"big\""),
        "{e}"
    );
}

/// Buffers in comps (#138): they parse against the comp's shots and layers, read
/// comp-local sources, and a comp with buffers (directly or through a comp it shows)
/// is shown in one place only.
#[test]
fn comp_buffers() {
    const C: &str = "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 2\n\
             [[layer]]\nid = \"show\"\nscene = \"t.glsl\"\nchannels = [\"@b\"]\n\
             [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\nchannels = [\"@prev\", \"@picture\"]\n\
             reset = [1]\nscale = 0.5\n";
    let (_d, m) = comps_film(
        "shotglass = \"0.1.10\"\n[comps]\nc = { file = \"c.toml\", size = [64, 36] }\n\
             [[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n",
        &[("c.toml", C)],
    );
    let m = m.unwrap();
    assert!(m.buffers.is_empty());
    let b = &m.comps[0].movie.buffers;
    assert_eq!(b.len(), 1);
    assert_eq!((b[0].start_frame, b[0].end_frame), (0, 60));
    assert_eq!(b[0].resets, vec![0, 30]);
    let l = &m.comps[0].movie.layers[0];
    assert!(matches!(
        l.channels[0].as_ref().unwrap().source,
        Source::Buffer {
            index: 0,
            back: 0,
            ..
        }
    ));
    assert_eq!(m.contract_warning(), None);
    // An older contract is told about 0.1.10.
    let (_d, m) = comps_film(
        "shotglass = \"0.1.9\"\n[comps]\nc = \"c.toml\"\n\
             [[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n",
        &[("c.toml", C)],
    );
    assert!(m.unwrap().contract_warning().unwrap().contains("0.1.10"));
    // Shown twice: refused, naming both uses.
    let (_d, m) = comps_film(
        "[comps]\nc = \"c.toml\"\n\
             [[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n\
             [[layer]]\nid = \"l\"\nscene = \"t.glsl\"\nchannels = [\"@c\"]\n",
        &[("c.toml", C)],
    );
    let e = err(m);
    assert!(
        e.contains("shown in one place only")
            && e.contains("shot \"s\"")
            && e.contains("layer \"l\"'s channels"),
        "{e}"
    );
    // Through a comp that shows it: the outer comp is shown twice.
    let (_d, m) = comps_film(
        "[comps]\nc = \"c.toml\"\nouter = \"o.toml\"\n\
             [[shot]]\nid = \"s\"\ncomp = \"outer\"\nduration = 1\n\
             [[shot]]\nid = \"s2\"\ncomp = \"outer\"\nduration = 1\n",
        &[
            ("c.toml", C),
            (
                "o.toml",
                "[[shot]]\nid = \"x\"\ncomp = \"c\"\nduration = 1\n",
            ),
        ],
    );
    let e = err(m);
    assert!(
        e.contains("comp \"outer\"") && e.contains("in a comp it shows"),
        "{e}"
    );
    // Read once from a film buffer: fine (the comp's channel use counts).
    let (_d, m) = comps_film(
        "[comps]\nc = \"c.toml\"\n[[shot]]\nid = \"s\"\nscene = \"s.glsl\"\nduration = 1\n\
             [[buffer]]\nid = \"fb\"\nscene = \"b.glsl\"\nchannels = [\"@c\"]\n",
        &[("c.toml", C)],
    );
    assert!(m.is_ok(), "{:?}", m.err());
    // A comp without buffers may still be shown many times.
    let (_d, m) = comps_film(
        "[comps]\nc = \"c.toml\"\n\
             [[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n\
             [[shot]]\nid = \"s2\"\ncomp = \"c\"\nduration = 1\n",
        &[(
            "c.toml",
            "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n",
        )],
    );
    assert!(m.is_ok());
    // Comp buffers follow the buffer rules: @input is refused, ids are the comp's.
    let (_d, m) = comps_film(
        "[comps]\nc = \"c.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n",
        &[(
            "c.toml",
            "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n\
                 [[buffer]]\nid = \"a\"\nscene = \"b.glsl\"\n",
        )],
    );
    assert!(err(m).contains("the id is taken"));
    // Memory: the film's and every comp's worst frames add up.
    let big = "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n\
             [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\nformat = \"rgba32f\"\nhistory = 17\n";
    let (_d, m) = comps_film(
        "[comps]\nc = \"c.toml\"\nd = \"d.toml\"\n\
             [[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n\
             [[shot]]\nid = \"t\"\ncomp = \"d\"\nduration = 1\n",
        &[("c.toml", big), ("d.toml", big)],
    );
    let e = err(m);
    assert!(
        e.contains("the film and its comps") && e.contains("comp \"d\""),
        "{e}"
    );
    // A sized comp's `scale` buffers are counted at its size, not at 1080p: 64×36
    // rgba32f × 64 states is under 3 MB (at 1920×1080 it would be 2 GB).
    let (_d, m) = comps_film(
        "[comps]\nc = { file = \"c.toml\", size = [64, 36] }\n\
             [[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n",
        &[(
            "c.toml",
            "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n\
                 [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\nformat = \"rgba32f\"\nhistory = 63\n",
        )],
    );
    assert!(m.is_ok(), "{:?}", m.err());
}

/// `after = "<layer>"` (#138): the buffer steps right after that layer, reads
/// `@input` and layers up to it; later layers read its state; step order is (rank,
/// file order).
#[test]
fn buffers_after_a_layer() {
    const L: &str = "[[layer]]\nid = \"t\"\nscene = \"t.glsl\"\nchannels = [\"@pre\"]\n\
             [[layer]]\nid = \"u\"\nscene = \"u.glsl\"\n\
             [[layer]]\nid = \"v\"\nscene = \"v.glsl\"\n";
    let film = |bufs: &str| {
        layers_film(&format!(
            "{L}[[buffer]]\nid = \"pre\"\nscene = \"p.glsl\"\nchannels = [\"@prev\"]\n{bufs}"
        ))
    };
    let m = film(
        "[[buffer]]\nid = \"smear\"\nscene = \"s.glsl\"\nafter = \"u\"\n\
             channels = [\"@prev\", \"@input\", \"@t\", \"@u\", \"@pre\", \"@picture\"]\n\
             [[buffer]]\nid = \"echo\"\nscene = \"e.glsl\"\nafter = \"t\"\nhistory = 4\n\
             channels = [\"@input\"]\n",
    )
    .unwrap();
    let s = &m.buffers[1];
    assert_eq!((s.after, s.rank()), (Some(1), 2));
    assert!(s.reads_stack());
    let src = |i: usize| s.channels[i].as_ref().unwrap().source.clone();
    assert_eq!(src(1), Source::Input);
    assert_eq!(
        src(2),
        Source::Layer {
            index: 0,
            id: "t".into()
        }
    );
    assert_eq!(
        src(3),
        Source::Layer {
            index: 1,
            id: "u".into()
        }
    );
    assert_eq!(src(5), Source::Picture);
    // Layer t (below the anchor) reads buffer state: smear's state depends on it.
    assert!(s.stack_reads_state && !s.is_pure());
    // echo reads only the composite up to t, which reads @pre: not pure either.
    assert!(m.buffers[2].stack_reads_state && !m.buffers[2].is_pure());
    assert_eq!(m.buffers[0].after, None);
    assert!(!m.buffers[0].reads_stack());
    // A history buffer of a composite whose layers read no buffer is pure.
    let m = layers_film(
        "[[layer]]\nid = \"t\"\nscene = \"t.glsl\"\n\
             [[buffer]]\nid = \"h\"\nscene = \"h.glsl\"\nafter = \"t\"\nhistory = 3\n\
             channels = [\"@input\"]\n\
             [[layer]]\nid = \"show\"\nscene = \"s.glsl\"\nchannels = [\"@h\"]\n",
    )
    .unwrap();
    assert!(m.buffers[0].is_pure());
    assert!(matches!(
        m.layers[1].channels[0].as_ref().unwrap().source,
        Source::Buffer {
            index: 0,
            back: 0,
            ..
        }
    ));
    // A later buffer stepping at an earlier place may be read (it steps first)...
    let m = film(
        "[[buffer]]\nid = \"late\"\nscene = \"l.glsl\"\nafter = \"v\"\nchannels = [\"@mid\"]\n\
             [[buffer]]\nid = \"mid\"\nscene = \"m.glsl\"\nafter = \"t\"\n",
    );
    assert!(m.is_ok(), "{:?}", m.err());
    // ...but not one stepping later in the frame, even if it's earlier in the file.
    let e = err(film(
        "[[buffer]]\nid = \"mid\"\nscene = \"m.glsl\"\nafter = \"t\"\n\
             [[buffer]]\nid = \"late\"\nscene = \"l.glsl\"\nafter = \"v\"\n\
             [[buffer]]\nid = \"x\"\nscene = \"x.glsl\"\nafter = \"u\"\nchannels = [\"@late\"]\n",
    ));
    assert!(
        e.contains("steps later in the frame (after layer \"v\")"),
        "{e}"
    );
    // A buffer without after can't read an after buffer's current state.
    let e = err(film(
        "[[buffer]]\nid = \"a\"\nscene = \"a.glsl\"\nafter = \"t\"\n\
             [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\nchannels = [\"@a\"]\n",
    ));
    assert!(e.contains("steps later in the frame"), "{e}");
    // Same place: file order.
    let e = err(film(
        "[[buffer]]\nid = \"a\"\nscene = \"a.glsl\"\nafter = \"u\"\nchannels = [\"@b\"]\n\
             [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\nafter = \"u\"\n",
    ));
    assert!(e.contains("later buffer"), "{e}");
    // Layers read an after buffer's state only above its anchor; below or at it, the
    // previous state.
    let lay = |l: &str| {
        layers_film(&format!(
            "[[layer]]\nid = \"t\"\nscene = \"t.glsl\"\n{l}\n\
                 [[layer]]\nid = \"u\"\nscene = \"u.glsl\"\n\
                 [[buffer]]\nid = \"a\"\nscene = \"a.glsl\"\nafter = \"t\"\n"
        ))
    };
    let e = err(lay("channels = [\"@a\"]"));
    assert!(
        e.contains("steps after layer \"t\", which isn't below this layer"),
        "{e}"
    );
    assert!(lay("channels = [{ source = \"@a\", frame = \"prev\" }]").is_ok());
    // Reading too high, or the stack without after.
    let bad = |b: &str| err(film(b));
    let e = bad("[[buffer]]\nid = \"a\"\nscene = \"a.glsl\"\nafter = \"t\"\nchannels = [\"@u\"]\n");
    assert!(
        e.contains("is a layer above this buffer's after = \"t\""),
        "{e}"
    );
    let e = bad("[[buffer]]\nid = \"a\"\nscene = \"a.glsl\"\nchannels = [\"@input\"]\n");
    assert!(e.contains("or give the buffer after = \"<layer>\""), "{e}");
    let e = bad("[[buffer]]\nid = \"a\"\nscene = \"a.glsl\"\nafter = \"nope\"\n");
    assert!(
        e.contains("after = \"nope\" names no layer; the layers are \"t\", \"u\", \"v\""),
        "{e}"
    );
    let e = bad("[[buffer]]\nid = \"a\"\nscene = \"a.glsl\"\nafter = \"moon\"\n");
    assert!(e.contains("names no layer"), "{e}");
    // Contract 0.1.11.
    let c = |v: &str| {
        fixture(
                &format!(
                    "shotglass = \"{v}\"\n[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n\
                     [[layer]]\nid = \"t\"\nscene = \"t.glsl\"\n\
                     [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\nafter = \"t\"\nchannels = [\"@input\"]\n"
                ),
                Path::new("/film"),
            )
            .unwrap()
    };
    assert!(c("0.1.10").contract_warning().unwrap().contains("0.1.11"));
    assert_eq!(c("0.1.11").contract_warning(), None);
    // In a comp: after names a comp layer.
    let (_d, m) = comps_film(
        "shotglass = \"0.1.11\"\n[comps]\nc = \"c.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n",
        &[(
            "c.toml",
            "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n\
                 [[layer]]\nid = \"t\"\nscene = \"t.glsl\"\n\
                 [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\nafter = \"t\"\nchannels = [\"@input\", \"@t\"]\n",
        )],
    );
    let m = m.unwrap();
    assert_eq!(m.comps[0].movie.buffers[0].after, Some(0));
    assert_eq!(m.contract_warning(), None);
    let (_d, m) = comps_film(
        "[comps]\nc = \"c.toml\"\n[[shot]]\nid = \"s\"\ncomp = \"c\"\nduration = 1\n\
             [[layer]]\nid = \"film-layer\"\nscene = \"t.glsl\"\n",
        &[(
            "c.toml",
            "[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n\
                 [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\nafter = \"film-layer\"\n",
        )],
    );
    assert!(err(m).contains("names no layer"));
}

#[test]
fn buffers_need_contract_0_1_9() {
    let film = |v: &str| {
        fixture(
            &format!(
                "shotglass = \"{v}\"\n[[shot]]\nid = \"a\"\nscene = \"a.glsl\"\nduration = 1\n\
                     [[buffer]]\nid = \"b\"\nscene = \"b.glsl\"\n"
            ),
            Path::new("/film"),
        )
        .unwrap()
    };
    assert!(film("0.1.8").contract_warning().unwrap().contains("0.1.9"));
    assert_eq!(film("0.1.9").contract_warning(), None);
}
