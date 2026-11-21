# Contributing to shotglass

Thanks for helping. Notes for working on the code are in [CLAUDE.md](CLAUDE.md)
(it is written for agents, but it's the most complete map of the code and its
invariants); user docs are in [README.md](README.md).

## Before you push

```sh
cargo fmt && cargo clippy --all-targets -- -D warnings
cargo test                      # Linux without a display: xvfb-run -a cargo test
cargo run -- check movies/big-shadow --web --no-gpu
```

`cargo test` runs the unit and property tests, `shotglass selftest` on your GPU
(tests/gpu.rs) and the golden-image and soundtrack tests (tests/golden.rs). On a machine without
OpenGL, `SHOTGLASS_SKIP_GPU_TESTS=1` skips the last two; CI still runs them (see
below).

Faster loops while you work:

```sh
cargo run -- selftest                 # just the GPU + sound checks; `selftest strips` filters by name
cargo run -- check movies/big-shadow --size 320x180 --frames 1   # compile and render one frame
```

### Look at the picture

Tests pass on pictures they can't judge. When a change could alter what a film looks
like, render a contact sheet and open it:

```sh
cargo run -- strip movies/big-shadow --count 8 --out target/tmp/strip.png
cargo run -- still movies/big-shadow --at 12.5,moon --out target/tmp/stills    # frames at times or shot ids
cargo run -- wav   movies/big-shadow --out target/tmp/sound.wav                 # the sound
```

A strip is `--count` frames spread evenly over the film, in rows of `--cols` (4),
left to right, top to bottom, each at `--size` (the golden tests use 8 at 320x180);
`--at` picks the times yourself. Compare it with the same command run on `main`, or
with `tests/golden/*.png`. For timing and frame rate, use a real GPU: software OpenGL
drops the render scale to 30% and its timings mean nothing.

## Pull requests

The [PR template](.github/pull_request_template.md) has a checklist. In short:

- one issue per PR; say which (`Closes #N`);
- a test for the change: a unit test, a selftest check for renderer behaviour (break
  the code and make sure it fails), or a golden case for a new look;
- every shader in the repo passes `check --web`;
- name each [invariant in CLAUDE.md](CLAUDE.md#invariants-each-was-a-real-bug-most-have-a-test)
  your change touches, and how you kept it;
- golden references re-blessed in the same commit only when the change is meant to
  look or sound different, never to make an unexplained diff go away.

The owner tests every change to the renderer, sound or player on Windows with a real
GPU before merging: CI has no GPU, and the dev GPU (NVIDIA) behaves differently from
the software renderers CI uses.

## Licensing

Contributions are accepted under the project's licence: by opening a pull request you
agree that your contribution is licensed under both Apache-2.0 and MIT, like the rest
of shotglass (inbound = outbound). No sign-off line (DCO) is needed.

Only contribute code you wrote or that you know is compatible. Shadertoy's default
licence, CC BY-NC-SA, is **non-commercial** and can't come into the engine, the
template or the demo. A permissively licensed snippet (MIT, Apache-2.0, public
domain, CC0) is fine if you name its source and licence in the file.

## Working with AI agents

Much of shotglass is written by coding agents (Claude Code) working from issues and
tested by the owner on a real GPU. If you work that way too:

- **One issue per session.** Start each session from an issue with a clear "done
  when", and keep unrelated fixes for their own issue. [CLAUDE.md](CLAUDE.md) is the
  agent's map; update it in the same PR when the code it describes changes.
- **One branch (or worktree) per issue**, named `claude/<topic>`, so sessions never
  share a working copy. `git worktree add ../shotglass-<topic> -b claude/<topic>`
  gives each its own folder.
- **Lanes.** Sessions can run in parallel when they touch different lanes:

  | safe in parallel | one change at a time |
  |---|---|
  | `app.rs` (player UI), `export.rs`, `library.rs`, `main.rs` subcommands, docs, kit libraries (another repo) | `gfx.rs`, `sound.rs`, `audio.rs` |

  The right-hand files hold the invariants about blocking, GPU syncs, the audio
  callback and bit-exact sound; two concurrent changes there interact in ways neither
  session's tests see. Finish, review and merge one before starting the next.
  `movie.rs`, `shader.rs` and `selftest.rs` are shared by many changes: expect
  conflicts and rebase rather than merge by hand.
- **Fresh-context review before the real-GPU test.** When the session says it's done,
  review the diff in a new session (`/code-review`, or a plain "review this branch
  against CLAUDE.md's invariants") that hasn't seen the reasoning behind it. Fix what
  it finds, then ask the owner to pull the branch and test on the real GPU.
- **Agents push, the owner pulls.** An agent in a remote or cloud session pushes its
  branch and asks the owner to `git pull`; it doesn't run git commands that write in
  the owner's working copy from a device shell.

## Issues

Use the templates: [bug](.github/ISSUE_TEMPLATE/bug.md) (with OS, GPU, driver and the
output of `shotglass selftest`), [feature](.github/ISSUE_TEMPLATE/feature.md), or
[library](.github/ISSUE_TEMPLATE/library.md) for the library system. New shader code
for the kit (functions, transitions) goes to the
[shotglass-kit issues](https://github.com/antonmarsden/shotglass-kit/issues).

## CI

CI (`.github/workflows/ci.yml`) runs fmt, clippy, `cargo test` (unit tests, GPU
selftest, golden images and soundtracks) and the strict `check --web --no-gpu` on
Linux, Windows and macOS, all with the same pinned `RUST_TOOLCHAIN`. macOS is the
slowest, so it runs only on pushes to `main` (after a merge) and when the workflow is
started by hand ("Run workflow" on the Actions tab, any branch): do that for a branch
that changes the renderer or the demo's shaders. Only CI fails a golden test that
can't fetch the films' libraries; an offline `cargo test` skips it with a message.
None of the machines has a real GPU:

| | OpenGL | skipped |
|---|---|---|
| Linux | Mesa llvmpipe under Xvfb | nothing (also renders and exports the demo) |
| Windows | Mesa llvmpipe ([mesa-dist-win](https://github.com/pal1000/mesa-dist-win)) | the selftest's video export (no ffmpeg; PNG + WAV export still checked) |
| macOS | Apple Software Renderer (OpenGL 4.1; the runners have no GPU) | none of the golden tests (they compare against the same llvmpipe references; this renderer's built-in `sin`/`cos` are ~5e-4 off, so the demo computes its camera rotation itself). The selftest's "sound matches exactly" comparisons (chunked, banded, streamed, exported) are reported, not failed: this renderer's sound differs by about 1e-5 with the size of the block drawn (`INEXACT_SOUND_RENDERERS` in `selftest.rs`); everything else in those checks still runs. Also the video export, as on Windows. |

### Kit canary

The tests above use shotglass-kit at the commit pinned in
`movies/big-shadow/shotglass.lock` (the template test reuses its `transitions` pin), so
a kit change can't turn CI red on its own. The flip side is that nothing notices drift
until someone moves the pin. The kit canary (`.github/workflows/canary.yml`, script
`.github/canary.sh`) does: daily and by hand ("Run workflow" on the Actions tab), never
on pushes or PRs, so it can't block a merge. On Linux (Mesa llvmpipe under Xvfb) it
copies the demo and `shotglass new`'s template to a scratch folder, moves their pins to
the kit's current `main` (`shotglass update` / `shotglass fetch`), and runs
`check --web --no-gpu` and the same golden strip and soundtrack comparisons as
`tests/golden.rs`. On failure it uploads the actual and diff images / WAVs and the
moved locks as `kit-canary-failures`. Locally: `xvfb-run -a .github/canary.sh`.

A red canary means the kit's `main` no longer gives the demo or the template the
blessed look and sound (or no longer passes the strict check). Either the change is
intended: bump the pin (`shotglass update movies/big-shadow`) and re-bless in the same
commit (see below); or it isn't: fix the kit (or the engine, if it broke the kit's
contract).

## Golden images

CI catches "compiles but looks wrong" (or sounds wrong): tests/golden.rs renders a
contact sheet (`shotglass strip`, 8 frames at 320x180) and the soundtrack
(`shotglass wav`, at 8 kHz) of the demo film and of the `shotglass new` template, and
compares them with reference PNGs and WAVs in `tests/golden/`.

- A strip matches when few pixels differ by more than a small per-pixel tolerance
  and the average difference stays small (`strip --compare`'s defaults). Software
  OpenGL (llvmpipe, as in CI) and real GPUs round a little differently; that's what
  the tolerance is for.
- A soundtrack matches when, in every 50 ms window, the RMS and the peak of the
  difference from the reference stay small (`wav --compare`'s defaults). A failure
  names the first time that differs, and the worst.
- On a mismatch the test fails and writes the actual strip and a diff image
  (differences in magenta), or the actual WAV and the difference as `<name>.diff.wav`,
  to `target/tmp/golden/`. CI uploads them as the `golden-image-failures` artifact.
- **If the change in the picture or the sound is intended**, update the references in the same
  commit as the change, and look at them before committing:

  ```sh
  SHOTGLASS_BLESS=1 xvfb-run -a cargo test --test golden      # Linux, software GL (preferred)
  ```

  The references are rendered with Mesa's llvmpipe, like CI, so bless on Linux
  (or in Docker: see below) when you can. On Windows/macOS,
  `$env:SHOTGLASS_BLESS=1; cargo test --test golden` (PowerShell) works too, as long
  as the result still passes on llvmpipe in CI.
- Docker, from any OS:

  ```sh
  docker build -f docker/test.Dockerfile -t shotglass-test .
  docker run --rm -v "$PWD/tests/golden:/src/tests/golden" -e SHOTGLASS_BLESS=1 \
      shotglass-test xvfb-run -a cargo test --release --test golden
  ```

You can compare any film against a reference of your own the same way:

```sh
shotglass strip movies/my-film --size 320x180 --count 8 --compare ref.png --bless   # once
shotglass strip movies/my-film --size 320x180 --count 8 --compare ref.png           # later
shotglass wav   movies/my-film --compare ref.wav --bless                            # sound
```

## Adding things

See "Adding things" in [CLAUDE.md](CLAUDE.md): new transitions go in the
[shotglass-kit](https://github.com/antonmarsden/shotglass-kit) repo, every
`movie.toml` field needs validation, a unit test and docs, and renderer behaviour
needs a selftest check that fails when you break the code.
