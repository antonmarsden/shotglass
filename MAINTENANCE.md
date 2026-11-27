# Maintenance

Track deferred cleanup, dependency upgrades, and upstream changes worth revisiting. For each item, record what is blocking it and when to check again. Link a GitHub issue when there is actionable work to track.

## Unify the GUI and film-text font stacks

Checked 2026-11-28. Shotglass film text uses `harfrust` 0.16.1 and `skrifa` 0.50.0, both on `read-fonts` 0.47.0. The GUI pulls in `epaint` 0.37.0 via `eframe`, which resolves to `harfrust` 0.12.0 and `skrifa` 0.44.0 on `read-fonts` 0.41.0. Both are required by `epaint`. [Published epaint dependencies](https://docs.rs/crate/epaint/0.37.0)

Revisit when an `eframe`/`epaint` release supports HarfRust 0.16.x and Skrifa 0.50.x on `read-fonts` 0.47.x, or a newer shared set. At the time of the check, egui's development branch still used HarfRust 0.15 and Skrifa 0.49. Its manifest identifies Fontique, Glifo via Vello CPU, and usvg as dependencies that need compatible upgrades. [Upstream egui manifest](https://github.com/emilk/egui/blob/main/Cargo.toml)

After updating, check the locked dependency tree for a single version of each crate. Run the pinned text raster test, the text and glyph goldens, and a GUI playback smoke test.
