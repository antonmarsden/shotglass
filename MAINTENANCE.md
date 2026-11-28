# Maintenance

Track deferred cleanup, dependency upgrades, and upstream changes worth revisiting. For each item, record what is blocking it and when to check again. Link a GitHub issue when there is actionable work to track.

## Unify the GUI and film-text font stacks

Checked 2026-11-28. Shotglass film text uses `harfrust` 0.16.1 and `skrifa` 0.50.0, both on `read-fonts` 0.47.0. The GUI pulls in `epaint` 0.37.0 via `eframe`, which resolves to `harfrust` 0.12.0 and `skrifa` 0.44.0 on `read-fonts` 0.41.0. Both are required by `epaint`. [Published epaint dependencies](https://docs.rs/crate/epaint/0.37.0)

Revisit when an `eframe`/`epaint` release supports HarfRust 0.16.x and Skrifa 0.50.x on `read-fonts` 0.47.x, or a newer shared set. At the time of the check, egui's development branch still used HarfRust 0.15 and Skrifa 0.49. Its manifest identifies Fontique, Glifo via Vello CPU, and usvg as dependencies that need compatible upgrades. [Upstream egui manifest](https://github.com/emilk/egui/blob/main/Cargo.toml)

After updating, check the locked dependency tree for a single version of each crate. Run the pinned text raster test, the text and glyph goldens, and a GUI playback smoke test.

## Rewrite the test films so each says what it tests

Added 2026-11-28. The films under `tests/films/` grew one feature at a time, and it is not always clear from a film or its golden strip what it checks. Develop new test films where each one states its purpose (a comment at the top of `movie.toml` and a short note in the picture or README beside it) and shows the behaviour plainly, so a golden diff points at the feature that broke.

Blocked on nothing; do it in small steps, one film per change, re-blessing its golden on llvmpipe and looking at the new strip each time.

## Replace the Big Shadow demo with something more epic

Added 2026-11-28. `movies/big-shadow/` is the demo and a golden film (strip, soundtrack and `quality` heatmaps). Replace it with a more ambitious film that shows off what shotglass can do now (layers, text, buffers, comps).

The new demo must stay self-contained like the old one (shared GLSL in its own `lib/`, no kit GLSL), pass `check --web`, and come with re-blessed goldens and updated README/docs references (`docs/big-shadow-strip.png`).
