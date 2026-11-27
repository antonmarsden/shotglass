# shotglass — notes for Claude

Rust + OpenGL harness for composing short films from GLSL shaders. User docs are in
README.md (overview, quick start, index) and `docs/` (one page per topic); this file is about working on the code.

## Strategy and cross-repo work

Goals, principles, the placement rule (engine / kit / film), quality metrics and the
agent playbooks live in [shotglass-hq](https://github.com/antonmarsden/shotglass-hq)
(private; cloned beside this repo as `../shotglass-hq`). Read its `STRATEGY.md` and
`agents/` before cross-repo work; this file wins for anything local to this repo.

## Build and test

```sh
cargo build
xvfb-run -a cargo test                 # Linux without a display; plain `cargo test` on Windows/macOS
xvfb-run -a cargo run -- selftest      # just the GPU + sound checks (filter: `selftest strips`)
xvfb-run -a cargo run -- check movies/big-shadow --size 320x180 --frames 1
cargo run -- check movies/big-shadow --web --no-gpu   # strict GLSL (330 + ES 3.00), no GL needed
xvfb-run -a cargo run -- strip movies/big-shadow --count 8 --out /tmp/s.png   # look at the PNG
cargo fmt && cargo clippy --all-targets -- -D warnings
```

- `cargo test` = unit + property tests, then `tests/gpu.rs` (harness = false) runs
  `shotglass selftest` on the main thread (OpenGL windows need it), then
  `tests/golden.rs` compares strips and soundtracks (8 kHz WAVs) of the demo and the
  test films with `tests/golden/*.png` / `*.wav`. `SHOTGLASS_SKIP_GPU_TESTS=1` skips both.
- **Golden images and sound:** if a change alters what the demo or a test film looks
  or sounds like on purpose, re-bless in the same commit: `SHOTGLASS_BLESS=1 xvfb-run -a cargo test --test golden`
  (references are llvmpipe renders, as in CI; Docker recipe in CONTRIBUTING.md), then
  look at the new PNGs. Never bless to make an unexplained diff go away. A failing run
  leaves the actual strip and `<name>.diff.png` (magenta = differs), or the actual WAV
  and `<name>.diff.wav` (with the first differing time in the message), in
  `target/tmp/golden/`; CI uploads them as `golden-image-failures`. On a real GPU
  (not llvmpipe / Apple Software Renderer) the tests compare against an uncommitted
  local baseline in `target/golden-local/<renderer>-<version>/` instead (first run
  writes it; `SHOTGLASS_BLESS=1` refreshes it; `SHOTGLASS_GOLDEN=ci|local` forces a
  mode); it never replaces blessing `tests/golden/` on llvmpipe. Golden films never
  use kit GLSL: the demo keeps its shared GLSL in `movies/big-shadow/lib/` (`base`,
  `sound`, `transitions`: path libraries, no lock, nothing fetched). Keep it that way;
  the template (which follows kit `main`) has no golden.
- CI (`.github/workflows/ci.yml`) pins Rust in `RUST_TOOLCHAIN` and runs clippy with
  `-D warnings`. Upgrade the toolchain on purpose and fix new lints in the same commit.
  Linux and Windows run the GPU tests on Mesa llvmpipe, macOS on the Apple Software
  Renderer (GL 4.1, no 4.2+ entry points: glow's query getters panicked there); the
  macOS job runs only on pushes to `main` and by hand (workflow_dispatch). Skips
  are listed in CONTRIBUTING.md.
- `.github/workflows/canary.yml` (daily + by hand, never on PRs) runs `.github/canary.sh`:
  `shotglass new`'s template on kit `main`: strict check, a frame, a strip and the
  soundtrack (not compared). Red = fix the kit or the template. Nothing on PRs fetches the kit.
- Software OpenGL (llvmpipe) is slow: auto render scale drops to 30% and frame
  timings mean nothing. Judge performance only on a real GPU.
- The owner tests on Windows with a real GPU. Their working copy is
  `C:\software\shotglass` (LF: `.gitattributes` sets `eol=lf`). Push to GitHub and ask them to `git pull`.
  Don't run git commands that write in that folder from the device shell.

## Map

| file | what |
|---|---|
| `main.rs` | CLI (clap): play, still, strip, check (`--deep`: every sequence frame), quality, wav, export, new, fetch, update, licenses, fonts add, bundle, freeze, unfreeze, test, selftest |
| `fonts.rs` | Google Fonts families as libraries (`[libraries] x = { google = "Family" }`): google/fonts folders (`ofl/`, `apache/`, `ufl/`) and `METADATA.pb` → `Library::google` (licence, no `library.toml`); `shotglass fonts add` writes the library and `[fonts]` entries (`toml_edit`) and fetches; tests use a local fixture repo through a `Fetcher` with its own `google_source` |
| `golden.rs` | `strip`/`wav --compare/--bless`: golden-image comparison (per-pixel tolerance, pixel budget, mean limit, diff image); soundtrack comparison (RMS/peak of the difference per 50 ms window, first differing time), WAV reader |
| `export.rs` | offline export: frame n at `frame_time(n)`, raw RGBA piped to ffmpeg (or PNGs), WAV muxed in; fails on a footage frame that didn't decode (`footage_failure`); `Format::builtin` (webm/av1 with the `webm` feature) goes to `webm::export`; `render_yuv` = the pipelined render → YUV → readback loop |
| `yuv.rs`, `glsl/yuv.glsl` | export's YUV pass (#116): final picture → one integer I420 target (BT.709, limited range, chroma sited left; R16UI 10-bit / R8UI 8-bit; Y rows, then rows of Cb + Cr), `YuvReader` = PBO ring + fences polled oldest first (`waits` counts back-pressure), `reference` = the CPU version the selftest compares against |
| `webm/` | built-in WebM (#116): `mux.rs` our WebM/Matroska writer (keyframe clusters, cues, back-patched sizes/duration, Opus CodecDelay + DiscardPadding; `read` for tests), `av1.rs` OBU/sequence header → `av1C`, TD stripping; with the `webm` feature: `encode.rs` (SVT-AV1 through `svt_shim.c`, settings by name via `svt_av1_enc_parse_parameter`; `OpusPackets`), `export.rs` (sound → Opus first, then frames → encoder thread → muxer); `ENCODER_LICENCES` for `shotglass licenses` |
| `build.rs` | `webm` feature: SVT-AV1 from `vendor/SVT-AV1` (submodule) / `SVT_AV1_SRC` / `SVT_AV1_DIR` with CMake (`-O2 -DNDEBUG` passed explicitly: cmake-rs drops CMake's release flags), C-only with a warning without nasm; the shim with cc |
| `footage.rs` | image sequences (#70), CPU side: `Pattern` (`fog_####.png`: prefix, digits, suffix; wider numbers as Blender writes), `scan` once at `Movie::load` (`first`, contiguous `count`, frame 0's size; gap / padding / case errors), `check_frames` for `check --deep`, `MIN_FRAMES` (3 per sequence in the shot budget) |
| `movie.rs` | `movie.toml` (+ its project) → `Movie`; timeline (`at(t)`), rational fps (`frame_time`/`frame_at`), cue defines, sound gains/windows, `[[layer]]` → `Movie::layers` (spans, `opacity_at`, `layers_at`), `channels`/`[textures]`/`[fonts]`/`[texts]` → `Shot::channels` (`Channel { source: Image(FileRef) \| Text(TextSource) \| Frames(Sequence) \| …, timing }`, `files()` = every single file it reads, `written()` adds a sequence's pattern, `sequence_files()` its scanned frames), footage (#70): `frames`/`texture = "…"` re-timing, `Timing` (`in`/`in_frame`, `speed`, `clock`, `end`), `scan_footage`, `FrameMap::source_frame` (the pure film frame → source frame map: exact rationals, `source_snap`, wraps loop films) and `Movie::frame_map`/`source_frame(ItemRef, c, n)`, `loop` (`loop_frames`, `loop_period`, `wrap`, `sound_samples`); comps (#74): `[comps]` → `Movie::comps` (`Comp`: a nested `Movie` from its own file via `build_sequence`/`parse_comp_file`, `size`, `loop`, `local(t)` holds or wraps), `Shot::comp`, `LayerScene::Comp`, `Source::Comp { time_offset }`, `Layer::matte` (`Matte`/`MatteSource`), `check_comp_graph` (cycles, depth ≤ `MAX_COMP_DEPTH`), `comp_cue_defines`; buffers (#134): `[[buffer]]` → `Movie::buffers` (`Buffer`: span, `start_frame`/`end_frame`, `resets` as frames, `BufferSize` fixed or scale, `BufferFormat`, `history`, `is_pure`), `Source::Buffer { index, back }` (`@prev`, `@<id>`, `frame = -k`), refused in comps, memory limit at parse |
| `texture.rs` | iChannel textures, CPU side: PNG/JPEG → RGBA8 (flipped for `vflip`), `load_text` (layout + raster → RGBA8 + `meta`), size limits, GPU memory (`frame_bytes`: footage without mipmaps; `shot_memory` counts a sequence's minimum window), error messages (HTML page, Git LFS pointer, half-saved file) |
| `text.rs` | text (#68): fonts (read-fonts `model::Font` via skrifa: parsing, variations, metrics, outlines; no system fonts, no hinting), shaping (HarfRust, #125), bidi per paragraph, greedy UAX #14 breaking, layout → `Layout` (glyphs, size, `meta` = iChannelMeta); raster: 4×4 coverage (R, A) + exact EDT signed distance on a 2×2 grid (G); fixture fonts in `tests/films/text/fonts` |
| `msdf.rs` | glyph atlases (#71): one MTSDF tile per distinct glyph (the `fdsm` crate, skrifa outlines via `Pen`), shelf-packed with 1-texel gutters into a `TooDee` atlas; reference `tests/golden/msdf-atlas.png` (one-level tolerance: determinism is per stack) |
| `glyphrun.rs` | glyph runs (#71): the table texture (format 1 in the module doc: header, line, glyph and subtitle entry records; `KIND_RUN`/`KIND_SET`), `build` (a layout), `build_set` (glyph sets, by code point), `build_subtitles` (cues → one block + entries); `decode` for tests |
| `subtitles.rs` | SRT/WebVTT → cues in film ms (tags, overrides, settings, NOTE/STYLE/REGION dropped; overlaps refused) |
| `anim.rs` | constants/keyframes, eases; `glsl_fn` turns keyframes into GLSL for sound shaders |
| `shader.rs` | source assembly, `#include` (film-relative, `<lib/file>`, include-once), `#line 1 N` per file, driver-log → file:line; `Dialect` (fixed `330 core` / `300 es` baseline, `glsl` opt-in) |
| `project.rs` | projects (#59): `shotglass.toml` (engine contract + `[libraries]`), nearest one at or above a film, a project's films, film-or-project command targets, `split_fixture`/`write_fixture` for test films written as one text |
| `library.rs` | `[libraries]` specs, `library.toml`, overrides (`shotglass.local.toml`, `SHOTGLASS_LIB_*`, relative to the project), path rules, `shotglass.lock` (frozen `path` entries win over overrides), sha256, `Fetcher` (git; `download_sparse` for Google Fonts families, one folder per cache entry `<rev>-<slug>`), peer/contract checks |
| `bundle.rs` | `licenses` (libraries a film includes, SPDX licences, non-commercial flag; film-folder fonts with their `licence`), `freeze`/`unfreeze` and `bundle` (one writer: exactly the included library files in `shotglass-libs/`, a `shotglass.toml` marked frozen, lock `path` entries; `bundle` adds the film folder and `LIBRARY-LICENSES.md`) |
| `glsl_test.rs` | `shotglass test` (#63): `*.test.glsl` discovery, `test_*` scan (`Param`, `Grid`, `// samples:`), `number_assertions` (id inserted into each `expect*(` call), the test prelude (engine contract), dispatcher (pixel → test sample, RGBA32UI MRT results), banded draws, decode → first failure per test |
| `quality/` | `shotglass quality` (#96). `config.rs`: `quality.toml` → `Config` (metrics by `use`, params → `#define`s, `range`, `threshold`, limits by `Stat`: mean/min/max/over/pNN, sorted), `tighten` (the ratchet: toml_edit, only stricter, 3 significant digits). `mod.rs`: `METRIC_HEADER` (`getFrame`/`getReference`/`getPrev`/`getNext`; `value.a` = measure, `.rgb` = heatmap), a variant is rendered only if a metric calls its function (`variants_read`), `@reference` = N× render box-filtered on the CPU, `@prev`/`@next` = `neighbour` (clamped, wrapped in loop films), `run`. `gpu.rs`: RGBA32F targets, reduction (4×4 blocks, two MRT targets: (sum, max, finite, non-finite), (over, min); non-finite by exponent bits), histogram (one point per pixel, additive blend into 256 bins). `loudness.rs`: ITU-R BS.1770-4 `Meter` in Rust (K-weighting at 48 kHz, gated integrated, momentary/short-term max, sample and 4× true peak), fed by `Soundtrack::render_all` for `[sound]` (`SoundStat`, `SoundReport`). `[[sound.metric]]` (`Kind::Sound`): `SOUND_HEADER` (`getSound`, `mainSoundMetric`), samples in 2^20 blocks (`SOUND_W`×`SOUND_ROWS` map, `SOUND_HALO` context), the same reductions with a sample `count` (`sg_count`: a partial last block), `max_at` by reading back only a block that beats the max. `testing.rs`: metric tests (`*.test.toml`, run by `shotglass test`): cases bind PNG/GLSL pictures or WAV/GLSL samples, judged by the limits, `expect = "fail"`; `notMeasured()` (-Inf) is left out of every reduction (`unmeasured`). `stats.rs`: `Reduced` per frame, `Totals` (percentiles from the histogram, clamped to min/max), `judge`, `Report` (JSON schema 2 + heatmap sheets); `sources` feeds the strict check |
| `validate.rs` | strict, driver-independent check (glslang) of every assembled shader; `validate_movie(m, dialect)` is the API `check`/`check --web` and the web player use |
| `gfx.rs` | `Renderer`: lazy prioritised compiles, program cache (disk + LRU), textures (decode thread → banded uploads in `pump` → `glGenerateMipmap`; sampler objects; LRU under a 256 MB budget), off-screen targets, transitions/freeze, layers (picture → `picture`; each active layer drawn into `layer` at its scale, or its own exact-size `held[j]` when a later layer reads it, then the over pass or its blend pass (in `ShotKeys::trans`) into a `work` picture; final copy; pass keys `INPUT_TEX`/`PICTURE_TEX`/`LAYER_TEX - j`; layers follow shots in `wanted`; `set_hidden_layers` for the player's mute/solo), strips, GPU timer, audio blocks (submit → PBO + fence → poll/wait); footage (#70): a `FootageEntry` per footage channel and version (`ChanKey::footage`, `tex` = `FOOTAGE_UNRESOLVED` until `resolve(keys, n)` puts in the frame's texture or `FOOTAGE_BLACK`), frames are ordinary store textures keyed by file path + size + mtime (`frame_tex`, one stat per version and frame), `footage_order` (window `n − 2 .. n + ½ s` and heads of items starting within 2 s, by `source_frame`), decode pool (`Decoder`, `min(4, cores − 2)` workers), separate footage upload budget and in-flight cap, eviction every pump (passed frames first, their GL textures to `spare` for reuse), `blocking_footage_loads`, `footage_misses`, `footage_failure`; comps (#74): sequences (0 = film, 1 + c = comp c) flattened in `wanted` (`seq_base`), per-sequence scratch targets `SeqTargets` (`cur`), per frame `plan_seq`/`plan_chans`/`plan_inst` → comp instances (memoised by `InstKey`) drawn into `Which::Inst` targets before the film; comp shots/layers use the copy program as their scene; mattes in the composite (`over_matte`, `BLEND_MATTE_HEADER`, unit 2, `clear` when the source is inactive) |
| `gfx/fold.rs` | buffers (#134): the canonical fold. `Fold` (cursor `frame`, a ring of `history + 1` targets per buffer with per-slot frames, checkpoints), `fold_key` (parts: decl, sizes, picked program keys per buffer, film keys if a buffer reads `@picture`/a comp, each with its first reading frame; `first_changed` → per-frame invalidation, #137), `make_plan` (cursor / nearest checkpoint ≤ n / cold start; pure buffers start N back), `advance_to` (sets the playhead) → `advance_buffers` (steps `STEP_BUDGET` per call when non-blocking or sliced, else all; `blocking_catchups`; progressive scrub: `preview` → `approx` frame read by `buf_pass`, or `lookback`), `fill_checkpoints` (background fill, non-blocking), `slow_seek_hint` (smoothed wall time per step), checkpoints by blit every round(fps) frames (`checkpoint_interval`: 1 for `cache = "dense"`) under `buffer_cache_budget` (setting, env, 1/8 VRAM, 512 MB), eviction by `evict_score` (distance / 2^tz(frame)), skipping a checkpoint that would be evicted first; buffer pass keys `BUF_TEX - (b·65 + back)` |
| `sound.rs` | `Soundtrack`: music / per-shot / mix tracks synthesised in cached chunks; `chunk` blocks, `stream` never does |
| `audio.rs` | cpal output, lock-free `fill` callback, chunk store (wraps for loop films, `wrap_span`), master `Clock` (f64, `set_loop`), WAV writer |
| `app.rs` | egui player: transport, timeline, panels, live reload, auto scale, sound streaming |
| `headless.rs` | hidden-window GL context for CLI commands |
| `selftest.rs` | GPU/sound checks on fixture films written to a temp dir |
| `tests/golden.rs`, `tests/golden/` | golden tests: 8-frame 320x180 strips and 8 kHz soundtrack of the demo (self-contained: its shared GLSL is in `lib/`), a strip of `tests/films/channels` (textures), a strip of `tests/films/text` (text blocks, distance field, RTL, iChannel6/7), a strip of `tests/films/layers` (spans, fades, fixed scale, `@input`), a strip of `tests/films/blends` (blend passes, `@picture`, `@<layer>`), a strip of `tests/films/footage` (image sequences: 24 in 30, re-timing, `clock = "movie"`, `end = "black"`, `iChannelTime`; tiny CC0 frames), a strip of `tests/films/glyphs` (a staggered glyph run, SRT subtitles on a layer, a glyph-set clock; film-local table reader), a two-period strip and the soundtrack of `tests/films/loop` (`loop = true`), a strip of `tests/films/comps` (comp shot, sized looping comp as a channel and a matted layer, luma matte from a comp), a strip of `tests/films/feedback` (a decaying trail, a reaction-diffusion buffer with init and resets, history reads), and the demo's `quality` heatmaps (`tests/golden/quality/big-shadow/<metric>.png`, via `quality --compare`), vs reference PNGs / WAVs |
| `tests/cli.rs` | the binary from the command line, no GPU: `check --strict` fails on any warning check prints (subtitle styling, a font with no licence, counted together), "ok" only after the verdict |
| `glsl/copy.glsl`, `glsl/over.glsl` | the engine's internal copy pass and layer composite (premultiplied over, × opacity); not transitions films can name |
| `movies/big-shadow/` | demo film (its shared GLSL in `lib/`, path libraries); `templates/new/` is what `shotglass new` writes (follows kit `main`, checked by the canary) |

## Invariants (each was a real bug; most have a test)

- **Playback never blocks on a compile or a texture.** While playing, `render(.., blocking=false)` returns
  None and the previous frame stays up. Paused/scrubbing may block. A shot is usable
  (`keys_ready`) only when its programs *and* every bound texture are `Ready`, so playback
  never shows a half-uploaded texture. Textures decode on a worker thread and upload in
  bands (`upload_budget` per `pump`); only `usable(.., blocking=true)` loads one on the
  spot, counted in `blocking_texture_loads` (selftest: 0 while playing).
- **Render once per film frame** (`movie.fps`) while playing. The auto scale's budget is a film frame,
  and it is driven by GPU time only. Frames slowed by housekeeping (compiles, sound
  synthesis) are "stalls", not drops, and must not lower the scale.
- **No per-frame GPU→CPU syncs on the render path.** `glGetError` stalls NVIDIA's threaded
  driver for up to a refresh; `context_lost()` asks only on evidence while playing (a frame gap
  > 0.5 s or a GPU frame > 500 ms) and once a second when paused (selftest checks this). Same for `read_pixels`,
  `finish` and query results that aren't ready. Sound is no exception: while playing it goes
  through `Soundtrack::stream` (`submit_audio` → PBO + fence, `poll_audio` with
  `glClientWaitSync(.., 0)`), never `wait_audio`. Selftest counts `blocking_audio_reads`.
  Paused/scrubbing may block for the chunk under the playhead; headless commands block.
  Every glGet / query / fence poll is a driver round trip on threaded drivers: ask limits
  once at startup, poll fences oldest-first and stop at the first unpassed one. A round trip
  can wait for the previous present (a whole refresh), so `stream_sound` runs *after* the
  frame's render is queued, never before it. Readback and mix-input buffers are persistently
  mapped where GL 4.4 allows (no map/upload calls); fences are polled in batches
  (`COLLECT_AFTER`). Selftest streams with and without persistent buffers.
- **The audio callback (`fill`) never blocks or allocates.** Only atomics, `ArcSwap` and `try_lock`.
- **`Clock::time()` never decreases while playing** (except a loop film's wrap to 0 at the end of
  a period: the unwrapped f64 time underneath never decreases). Use the sound card position only
  when it agrees with the system timer (within 0.25 s); when it does, the timer estimate moves to
  it, so the timer follows the card's drift over hours of looping.
- **Loop films (`loop = true`): shaders only ever see time within one period.** The period is
  the duration, a whole number of frames (`loop_frames`); frame n of loop k is frame n of loop 0
  (`FrameInfo::at` wraps in f64, `FrameInfo::frame` by frame number; never wrap an f32 time far
  out). The player's clock wraps at `Movie::sound_samples(rate)` samples (one period, rounded),
  so picture and sound can't drift apart; the sound card position wraps at `wrap_span` (whole
  periods, ≥ 2^28 samples) so it never overflows. Sound is one period long and the mix reads it
  circularly (`circular_segments`: before 0 is the end, past the end is the start), so an echo
  rings over the seam, into the first play too. No transition across the seam; a lead on the
  first shot or a tail on the last is refused. No loop-count uniform. Selftest "loop films"
  pins all of this against a long unlooped render.
- **A broken edit keeps the last working version on screen** (per shot id fallback, which
  remembers programs and textures). Missing files (assembly errors, a missing texture)
  refuse the whole new version; a broken or half-saved image falls back per texture.
  Failed sound edits retain the previous soundtrack only when its sample rate, sample
  count and loop mode match. Otherwise the player clears its audio chunks and plays
  the new picture timing in silence until the sound compiles.
- **Sound is identical whatever the chunk size,** and banded vs single draws give the same
  output, and streamed (async) vs blocking synthesis agree bit for bit (the selftest only reports, not fails, a mismatch on
  `INEXACT_SOUND_RENDERERS`: the Apple Software Renderer is ~1e-5 off). The mix may read at most `mix_lookback` back and 50 ms ahead.
- **Deterministic frames:** a frame depends only on (movie, n, size, scale), where a buffer's state is
  the canonical fold from its last reset (below), so playback,
  `still` and `export` agree. Frame n is at `movie.frame_time(n)`; `FrameInfo::at` derives
  iFrame with `frame_at`, never `t * fps` (rounding drops frames at 30000/1001).
  The scope is **one GPU/CPU stack** (same machine, OS, driver, build): run to run
  there, output is bit-identical. Across stacks it need not be (drivers, GPUs and
  platform maths libraries differ); goldens use tolerances and per-renderer
  baselines for that. Don't pay for cross-platform bit-exactness (custom maths,
  vendored forks) unless something specific needs it; prefer reference images with
  a tolerance over cross-platform hashes.
- **Slow frames are drawn in viewport strips, never scissored ones** (the Apple Software
  Renderer leaves parts of scissored strips undrawn: stale rows from the previous frame).
  Selftest "slow frames drawn in strips" pins the count with `set_tiles(Some(n))`.
- **Don't trust built-in `sin`/`cos` where an error is magnified** (the Apple Software
  Renderer's are ~5e-4 off): the demo's `rot()` uses `sincosPrecise` (common.glsl), or
  its zoomed close-up camera lands on different floor/wall points than on llvmpipe.
- **Texture uploads set and restore GL unpack state** (`UNPACK_ALIGNMENT` 1 for the band, then 4;
  row length/skips 0) and unbind `PIXEL_UNPACK_BUFFER` first: the sound path binds it, and a
  client-memory upload with it bound reads a buffer offset. The scene pass unbinds its
  sampler objects after drawing (they'd override the composite pass and egui on units 0–7).
- **CPU readbacks use `gfx::read_pixels_into`** to check destination bounds, unbind
  `PIXEL_PACK_BUFFER`, and restore GL pack defaults (`PACK_ROW_LENGTH`/`SKIP_*` to 0,
  alignment 4, byte swapping off on desktop GL). `glReadPixels` never sees the slice length.
  Headless renderers call `set_readback(true)` (composite drawn upside down, no CPU flip);
  the player doesn't.
- **Shaders compile as `#version 330 core` (or `300 es`) whatever the driver offers**, unless the
  film sets `glsl = "410"|"430"` (refused on drivers that can't). Selftest checks `__VERSION__`.
  Everything in this repo (demo, template, transitions, selftest fixtures) must pass
  `check --web`: the strict glslang check at 330 and as GLSL ES 3.00. The dev GPU (NVIDIA)
  accepts invalid GLSL that other drivers reject, so passing locally isn't proof.
- **The engine provides a contract, not looks.** Uniforms, entry points, the transition interface
  and cue defines are the engine's API (semver). Reusable GLSL belongs in versioned libraries
  (the shotglass-kit repo, #3–#5), not embedded in the binary. The built-in transitions
  were removed (the kit has them). Don't add built-in GLSL. The exceptions are contract,
  not looks: the `shotglass test` prelude (`glsl_test::prelude`: the `expect*`
  assertions, only test shaders get it), and, from layers (#69), the layer footer
  (premultiply), the over pass (premultiplied source-over × opacity), the blend header
  (`getBase`/`getLayer`/`opacity`), matte multiply and `SG_HAS_*` defines, which define
  what a layer at opacity o *is*. Every other blend mode and effect is kit GLSL.
- **Channel entries are classified in one place**, `movie::ChannelSource::of`, by spelling
  (`""`, `@output`, `[textures]` name, image / `####` sequence / video path); kinds not built
  yet (video) are refused with what brings them. The count is `movie::N_CHANNELS`
  (8: `iChannel0..7`, `iChannelResolution`/`iChannelTime`/`iChannelMeta[8]` in the scene
  header). Engine-internal units in the scene pass go above the channels (layers: 8–9).
  Never bake the count into a file format or data layout.
- **Layers: a frame shows only when every active layer is ready** (`usable` for each, plus the
  over program), never the picture without a layer `still` would draw (selftest "layers:
  playback waits…"). Films without layers take exactly the old path (no extra passes, same
  cues, same goldens). `@input`/`@picture` bind full-size pictures, `@<layer>` a held target of
  exactly that layer's size; a pass that isn't drawn this frame (inactive, hidden) reads black.
  Opacity is applied by the composite, not the footer, so `@<layer>` sees the layer itself
  (an `opacity = 0` helper pass works). Hidden layers are a preview-only thing: export never
  sets them (selftest "layers: hidden…").
- **Comps and mattes (#74): a frame shows only when every comp it uses is ready**, items
  included, recursively (selftest "comps: playback waits…"). A comp's time is its user's: a comp
  shot or layer its iTime, a channel the reader's iTime + `time_offset`, a matte its layer's;
  outside the comp it holds (or wraps with `loop = true`), and the comp frame is
  `frame_at` of that, so footage in comps is exact too. Comp output is opaque. A matte is
  applied in the composite with the opacity, so `@<id>` sees the layer unmatted. Films
  without comps or mattes take exactly the old path (same passes, same goldens).
- **Footage: exact frame or stall** (#70). Which source frame a channel shows is
  `FrameMap::source_frame(n)`, a pure function of the film frame (`film_frame(movie, t)` =
  `frame_at(wrap(t))`, as `iFrame`; a frozen transition's outgoing shot uses the
  transition's first frame), so playback, `still` and `export` show the same frame. A
  shot is usable only when *that* frame is `Ready` on every footage channel (or the
  fallback version has it), never a neighbour. Frames decode on the pool and upload in
  bands in `pump` with their own budget; only `usable(.., blocking=true)` loads one on the
  spot, counted in `blocking_footage_loads` (selftest: 0 while playing). Footage entries
  are per version (`generation` in the key), so a re-rendered frame reloads and a
  half-written one falls back to the previous version's frame; a missing frame in the
  middle (a gap) refuses the version at `Movie::load`. Don't use `floor_snap` for source
  frames: its 1e-4 relative tolerance rounds frame 2000.8 up (`source_snap` is tight).
- **Built-in WebM export never syncs per frame** (#116). Frames go render → YUV pass →
  PBO + fence, collected oldest first (`export::render_yuv`); the loop waits on the GPU
  only when the ring is full and at the final drain (selftest "export YUV pass" pins
  it). Frame n's block timestamp is n·den/num seconds, rounded to the nearest
  millisecond. The decoded sound has exactly `wav`'s sample count (Opus pre-skip +
  DiscardPadding), and the YUV values follow `yuv::reference` (selftest "export writes
  every frame and sample").
- **Buffers (#134): the fold is canonical, never skipped.** A buffer's state at frame n is its state at
  the last reset r ≤ n stepped once per frame r..n; stalls step every missed frame, seeks restore the
  nearest checkpoint ≤ n (or cold-start; pure history buffers N frames back), and the result is
  bit-identical to a straight walk (selftest "buffers: …" checks). Buffers step after the picture,
  before layers; `@picture` in a buffer is rendered at the buffer's size, scale 1, never the render
  scale. While playing, catch-up is sliced (the previous frame stays up, the clock waits); no
  checkpoint ever reads back to the CPU. Films without buffers take exactly the old path.
  Buffer steps never draw in strips (the strip count follows the GPU timer; the Apple
  Software Renderer rounds strips differently, so state would depend on timing). Phase 2
  (#137): approximate frames (progressive scrub, `preview_lookback`) exist only while
  the player sets `set_scrub_preview`; lookback states are never checkpointed or stepped
  on (cursor dropped). The background fill (`fill_checkpoints`) never blocks and never
  moves the playhead eviction keeps near.
- **Text blocks are deterministic and laid out off the render thread.** Bytes depend only on
  the font files, the `[texts]` entry and the engine (no system fonts, no hinting, plain
  IEEE maths, exact EDT; `text::tests::raster_is_pinned` pins a sha256: bump it on purpose,
  like a golden). They rasterise on the decode worker like images decode; `Movie::load`
  lays every text out once, so missing glyphs, `max_lines` and oversize blocks refuse the
  edit. A text edit never recompiles (selftest "text textures").

## Adding things

- **New transition:** it goes in the kit (shotglass-kit `transitions/`), not the engine.
  Films use it as `type = "<library>/<name>"`. A bare name (`type = "iris"`) is an
  error that names the kit replacement. No compatibility code for old films.
- **Libraries and the engine contract live in the project** (`shotglass.toml`), never in
  `movie.toml`; `shotglass.lock` and overrides sit next to `shotglass.toml`. A frozen
  film is a project of its own. Test films written as one text go through
  `project::write_fixture` / `split_fixture`.
- **`movie.toml` field:** add it to the serde struct (`deny_unknown_fields`) and the resolved
  struct, validate it in `Movie::parse`, add a unit test, and update `docs/movies.md` (or the topic page under `docs/`) and
  `templates/new/movie.toml`.
- **A new demo/template look, or a film-level feature worth guarding visually:** add a
  case to `tests/golden.rs` and bless its reference.
- **`shotglass test` behaviour:** the selftest "shotglass test" check pins the whole report
  of a fixture file; extend the fixture and its expected text. Metric tests (`*.test.toml`) have
  their own check ("metric tests") pinned the same way.
- **Renderer behaviour:** add a selftest check. Mutation-check it: break the code and make sure
  the check fails.
- Working in parallel with other sessions: see "Working with AI agents" in CONTRIBUTING.md
  (one issue per branch; `gfx.rs`, `sound.rs`, `audio.rs` take one change at a time).
- Keep user-facing docs in README.md (overview + index of `docs/`) and the topic pages in
  `docs/` (`docs/design/` holds design notes); a new page gets a line in the README index. Open work is in GitHub issues: #35 roadmap to 1.0,
  #66 text/layers/footage epic (next:
  #73 video; #127 WOFF/WOFF2; later items in #75), #2 web player, #51 live mode (#53–#56), #60 real-time mode, #137–#138 feedback buffers phases 2–3.

## Conventions

- 2D data (images, grids) uses the owner's `toodee` crate: `gfx::Frame` is
  `TooDee<[u8; 4]>` (RGBA, top row first); use views and `CopyOps`/`TranslateOps`
  rather than hand-rolled row loops. Don't force it where data isn't really 2D.

## Commits

End commit messages with the attribution lines (Co-Authored-By, session) your agent session gives.
Run fmt, clippy and the full test suite before pushing.
