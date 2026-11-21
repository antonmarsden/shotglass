# shotglass — notes for Claude

Rust + OpenGL harness for composing short films from GLSL shaders. User docs are in
README.md; this file is about working on the code.

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
  template with `tests/golden/*.png` / `*.wav`. `SHOTGLASS_SKIP_GPU_TESTS=1` skips both.
- **Golden images and sound:** if a change alters what the demo or the template looks
  or sounds like on purpose, re-bless in the same commit: `SHOTGLASS_BLESS=1 xvfb-run -a cargo test --test golden`
  (references are llvmpipe renders, as in CI; Docker recipe in CONTRIBUTING.md), then
  look at the new PNGs. Never bless to make an unexplained diff go away. A failing run
  leaves the actual strip and `<name>.diff.png` (magenta = differs), or the actual WAV
  and `<name>.diff.wav` (with the first differing time in the message), in
  `target/tmp/golden/`; CI uploads them as `golden-image-failures`. The library pins
  come from `movies/big-shadow/shotglass.lock` (the template test reuses its
  `transitions` pin), so moving that pin can need a re-bless too.
- CI (`.github/workflows/ci.yml`) pins Rust in `RUST_TOOLCHAIN` and runs clippy with
  `-D warnings`. Upgrade the toolchain on purpose and fix new lints in the same commit.
  Linux and Windows run the GPU tests on Mesa llvmpipe, macOS on the Apple Software
  Renderer (GL 4.1, no 4.2+ entry points: glow's query getters panicked there); the
  macOS job runs only on pushes to `main` and by hand (workflow_dispatch). Skips
  are listed in CONTRIBUTING.md.
- `.github/workflows/canary.yml` (daily + by hand, never on PRs) runs `.github/canary.sh`:
  the demo and template with their pins moved to kit `main`, strict check + golden
  comparisons. Red = bump the pin and re-bless, or fix the kit.
- Software OpenGL (llvmpipe) is slow: auto render scale drops to 30% and frame
  timings mean nothing. Judge performance only on a real GPU.
- The owner tests on Windows with a real GPU. Their working copy is
  `C:\software\shotglass` (CRLF checkout). Push to GitHub and ask them to `git pull`.
  Don't run git commands that write in that folder from the device shell.

## Map

| file | what |
|---|---|
| `main.rs` | CLI (clap): play, still, strip, check, wav, export, new, selftest |
| `golden.rs` | `strip`/`wav --compare/--bless`: golden-image comparison (per-pixel tolerance, pixel budget, mean limit, diff image); soundtrack comparison (RMS/peak of the difference per 50 ms window, first differing time), WAV reader |
| `export.rs` | offline export: frame n at `frame_time(n)`, raw RGBA piped to ffmpeg (or PNGs), WAV muxed in |
| `movie.rs` | `movie.toml` → `Movie`; timeline (`at(t)`), rational fps (`frame_time`/`frame_at`), cue defines, sound gains/windows |
| `anim.rs` | constants/keyframes, eases; `glsl_fn` turns keyframes into GLSL for sound shaders |
| `shader.rs` | source assembly, `#include` (film-relative, `<lib/file>`, include-once), `#line 1 N` per file, driver-log → file:line; `Dialect` (fixed `330 core` / `300 es` baseline, `glsl` opt-in) |
| `library.rs` | `[libraries]` specs, `library.toml`, overrides, path rules, `shotglass.lock`, sha256, `Fetcher` (git), peer/contract checks |
| `validate.rs` | strict, driver-independent check (glslang) of every assembled shader; `validate_movie(m, dialect)` is the API `check`/`check --web` and the web player use |
| `gfx.rs` | `Renderer`: lazy prioritised compiles, program cache (disk + LRU), off-screen targets, transitions/freeze, strips, GPU timer, audio blocks (submit → PBO + fence → poll/wait) |
| `sound.rs` | `Soundtrack`: music / per-shot / mix tracks synthesised in cached chunks; `chunk` blocks, `stream` never does |
| `audio.rs` | cpal output, lock-free `fill` callback, chunk store, master `Clock`, WAV writer |
| `app.rs` | egui player: transport, timeline, panels, live reload, auto scale, sound streaming |
| `headless.rs` | hidden-window GL context for CLI commands |
| `selftest.rs` | GPU/sound checks on fixture films written to a temp dir |
| `tests/golden.rs`, `tests/golden/` | golden tests: 8-frame 320x180 strips and 8 kHz soundtracks of the demo and the template vs reference PNGs / WAVs |
| `glsl/copy.glsl` | the engine's internal copy pass (not a transition films can name; theirs come from libraries or files) |
| `movies/big-shadow/` | demo film; `templates/new/` is what `shotglass new` writes |

## Invariants (each was a real bug; most have a test)

- **Playback never blocks on a compile.** While playing, `render(.., blocking=false)` returns
  None and the previous frame stays up. Paused/scrubbing may block.
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
- **`Clock::time()` never decreases while playing.** Use the sound card position only when it
  agrees with the system timer (within 0.25 s).
- **A broken edit keeps the last working version on screen** (per shot id fallback). Missing
  files (assembly errors) refuse the whole new version.
- **Sound is identical whatever the chunk size,** and banded vs single draws give the same
  output, and streamed (async) vs blocking synthesis agree bit for bit (the selftest only reports, not fails, a mismatch on
  `INEXACT_SOUND_RENDERERS`: the Apple Software Renderer is ~1e-5 off). The mix may read at most `mix_lookback` back and 50 ms ahead.
- **Deterministic frames:** a frame depends only on (movie, t, size, scale), so playback,
  `still` and `export` agree. Frame n is at `movie.frame_time(n)`; `FrameInfo::at` derives
  iFrame with `frame_at`, never `t * fps` (rounding drops frames at 30000/1001).
- **Slow frames are drawn in viewport strips, never scissored ones** (the Apple Software
  Renderer leaves parts of scissored strips undrawn: stale rows from the previous frame).
  Selftest "slow frames drawn in strips" pins the count with `set_tiles(Some(n))`.
- **Don't trust built-in `sin`/`cos` where an error is magnified** (the Apple Software
  Renderer's are ~5e-4 off): the demo's `rot()` uses `sincosPrecise` (common.glsl), or
  its zoomed close-up camera lands on different floor/wall points than on llvmpipe.
- **Readbacks restore GL pack state** (`PACK_ROW_LENGTH`/`SKIP_*` to 0, alignment 4) and
  check bounds first: `glReadPixels` never sees the slice length. Headless renderers call
  `set_readback(true)` (composite drawn upside down, no CPU flip); the player doesn't.
- **Shaders compile as `#version 330 core` (or `300 es`) whatever the driver offers**, unless the
  film sets `glsl = "410"|"430"` (refused on drivers that can't). Selftest checks `__VERSION__`.
  Everything in this repo (demo, template, transitions, selftest fixtures) must pass
  `check --web`: the strict glslang check at 330 and as GLSL ES 3.00. The dev GPU (NVIDIA)
  accepts invalid GLSL that other drivers reject, so passing locally isn't proof.
- **The engine provides a contract, not looks.** Uniforms, entry points, the transition interface
  and cue defines are the engine's API (semver). Reusable GLSL belongs in versioned libraries
  (the shotglass-kit repo, #3–#5), not embedded in the binary. The built-in transitions
  were removed (the kit has them). Don't add built-in GLSL.

## Adding things

- **New transition:** it goes in the kit (shotglass-kit `transitions/`), not the engine.
  Films use it as `type = "<library>/<name>"`. A bare name (`type = "iris"`) is an
  error that names the kit replacement. No compatibility code for old films.
- **`movie.toml` field:** add it to the serde struct (`deny_unknown_fields`) and the resolved
  struct, validate it in `Movie::parse`, add a unit test, and update the README and
  `templates/new/movie.toml`.
- **A new demo/template look, or a film-level feature worth guarding visually:** add a
  case to `tests/golden.rs` and bless its reference.
- **Renderer behaviour:** add a selftest check. Mutation-check it: break the code and make sure
  the check fails.
- Working in parallel with other sessions: see "Working with AI agents" in CONTRIBUTING.md
  (one issue per branch; `gfx.rs`, `sound.rs`, `audio.rs` take one change at a time).
- Keep user-facing docs in README.md. Open work is in GitHub issues: #1 export (follow-ups: PBO readback), #2 web player,
  #3 libraries by name and version, #4 kit core/transitions, #5 the shotglass-kit repo,
  #6 portable GLSL (done: fixed baseline, `glsl` opt-in, strict `check` / `check --web`).

## Conventions

- 2D data (images, grids) uses the owner's `toodee` crate: `gfx::Frame` is
  `TooDee<[u8; 4]>` (RGBA, top row first); use views and `CopyOps`/`TranslateOps`
  rather than hand-rolled row loops. Don't force it where data isn't really 2D.

## Commits

End commit messages with the Co-Authored-By / Claude-Session lines given in the session.
Run fmt, clippy and the full test suite before pushing.
