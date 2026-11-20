# shotglass — notes for Claude

Rust + OpenGL harness for composing short films from GLSL shaders. User docs are in
README.md; this file is about working on the code.

## Build and test

```sh
cargo build
xvfb-run -a cargo test                 # Linux without a display; plain `cargo test` on Windows/macOS
xvfb-run -a cargo run -- selftest      # just the GPU + sound checks (filter: `selftest strips`)
xvfb-run -a cargo run -- check movies/big-shadow --size 320x180 --frames 1
xvfb-run -a cargo run -- strip movies/big-shadow --count 8 --out /tmp/s.png   # look at the PNG
cargo fmt && cargo clippy --all-targets -- -D warnings
```

- `cargo test` = unit + property tests, then `tests/gpu.rs` (harness = false) runs
  `shotglass selftest` on the main thread (OpenGL windows need it).
  `SHOTGLASS_SKIP_GPU_TESTS=1` skips it.
- CI (`.github/workflows/ci.yml`) pins Rust in `RUST_TOOLCHAIN` and runs clippy with
  `-D warnings`. Upgrade the toolchain on purpose and fix new lints in the same commit.
- Software OpenGL (llvmpipe) is slow: auto render scale drops to 30% and frame
  timings mean nothing. Judge performance only on a real GPU.
- The owner tests on Windows with a real GPU. Their working copy is
  `C:\software\shotglass` (CRLF checkout). Push to GitHub and ask them to `git pull`.
  Don't run git commands that write in that folder from the device shell.

## Map

| file | what |
|---|---|
| `main.rs` | CLI (clap): play, still, strip, check, wav, new, selftest |
| `movie.rs` | `movie.toml` → `Movie`; timeline (`at(t)`), cue defines, sound gains/windows |
| `anim.rs` | constants/keyframes, eases; `glsl_fn` turns keyframes into GLSL for sound shaders |
| `shader.rs` | source assembly, `#include`, `#line 1 N` per file, driver-log → file:line |
| `gfx.rs` | `Renderer`: lazy prioritised compiles, program cache (disk + LRU), off-screen targets, transitions/freeze, strips, GPU timer, audio rendering |
| `sound.rs` | `Soundtrack`: music / per-shot / mix tracks synthesised in cached chunks |
| `audio.rs` | cpal output, lock-free `fill` callback, chunk store, master `Clock`, WAV writer |
| `app.rs` | egui player: transport, timeline, panels, live reload, auto scale, sound streaming |
| `headless.rs` | hidden-window GL context for CLI commands |
| `selftest.rs` | GPU/sound checks on fixture films written to a temp dir |
| `glsl/transitions/` | built-in transitions (params are `#define`s with `#ifndef` defaults) |
| `movies/big-shadow/` | demo film; `templates/new/` is what `shotglass new` writes |

## Invariants (each was a real bug; most have a test)

- **Playback never blocks on a compile.** While playing, `render(.., blocking=false)` returns
  None and the previous frame stays up. Paused/scrubbing may block.
- **Render once per film frame** (`movie.fps`) while playing. The auto scale's budget is a film frame,
  and it is driven by GPU time only. Frames slowed by housekeeping (compiles, sound
  synthesis) are "stalls", not drops, and must not lower the scale.
- **No per-frame GPU→CPU syncs on the render path.** `glGetError` stalls NVIDIA's threaded
  driver; `context_lost()` is rate-limited (selftest checks this). Same for `read_pixels`,
  `finish` and query results that aren't ready.
- **The audio callback (`fill`) never blocks or allocates.** Only atomics, `ArcSwap` and `try_lock`.
- **`Clock::time()` never decreases while playing.** Use the sound card position only when it
  agrees with the system timer (within 0.25 s).
- **A broken edit keeps the last working version on screen** (per shot id fallback). Missing
  files (assembly errors) refuse the whole new version.
- **Sound is identical whatever the chunk size,** and banded vs single draws give the same
  output. The mix may read at most `mix_lookback` back and 50 ms ahead.
- **Deterministic frames:** a frame depends only on (movie, t, size, scale), so playback,
  `still` and a future export agree.
- Shaders should stay portable: GLSL 3.30 features only, and GLSL-ES-compatible so a future
  web player can run them (#2, #6). The dev GPU (NVIDIA) accepts some invalid GLSL that
  other drivers reject, so passing locally isn't proof.
- **The engine provides a contract, not looks.** Uniforms, entry points, the transition interface
  and cue defines are the engine's API (semver). Reusable GLSL belongs in versioned libraries
  (the shotglass-kit repo, #3–#5), not embedded in the binary; the built-in transitions are
  slated to move there. Don't add new built-in GLSL.

## Adding things

- **New transition:** it belongs in the kit (#4) once libraries exist. Until then, a built-in
  one goes in `src/glsl/transitions/<name>.glsl`, registered in `BUILTIN_TRANSITIONS`
  (gfx.rs), added to `transition_endpoints` in selftest.rs, and listed in the README table.
- **`movie.toml` field:** add it to the serde struct (`deny_unknown_fields`) and the resolved
  struct, validate it in `Movie::parse`, add a unit test, and update the README and
  `templates/new/movie.toml`.
- **Renderer behaviour:** add a selftest check. Mutation-check it: break the code and make sure
  the check fails.
- Keep user-facing docs in README.md. Open work is in GitHub issues: #1 export, #2 web player,
  #3 libraries by name and version, #4 kit core/transitions, #5 the shotglass-kit repo,
  #6 portable GLSL (fixed `330 core` baseline + strict validation in `check`).

## Conventions

- 2D data (images, grids) uses the owner's `toodee` crate: `gfx::Frame` is
  `TooDee<[u8; 4]>` (RGBA, top row first); use views and `CopyOps`/`TranslateOps`
  rather than hand-rolled row loops. Don't force it where data isn't really 2D.

## Commits

End commit messages with the Co-Authored-By / Claude-Session lines given in the session.
Run fmt, clippy and the full test suite before pushing.
