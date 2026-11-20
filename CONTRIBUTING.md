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
(tests/gpu.rs) and the golden-image tests (tests/golden.rs). On a machine without
OpenGL, `SHOTGLASS_SKIP_GPU_TESTS=1` skips the last two; CI still runs them on
software OpenGL.

## Golden images

CI catches "compiles but looks wrong": tests/golden.rs renders a contact sheet
(`shotglass strip`, 8 frames at 320x180) of the demo film and of the `shotglass new`
template and compares each with a reference PNG in `tests/golden/`.

- A strip matches when few pixels differ by more than a small per-pixel tolerance
  and the average difference stays small (`strip --compare`'s defaults). Software
  OpenGL (llvmpipe, as in CI) and real GPUs round a little differently; that's what
  the tolerance is for.
- On a mismatch the test fails and writes the actual strip and a diff image
  (differences in magenta) to `target/tmp/golden/`. CI uploads them as the
  `golden-image-failures` artifact.
- **If the change in the picture is intended**, update the references in the same
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
```

## Adding things

See "Adding things" in [CLAUDE.md](CLAUDE.md): new transitions go in the
[shotglass-kit](https://github.com/antonmarsden/shotglass-kit) repo, every
`movie.toml` field needs validation, a unit test and docs, and renderer behaviour
needs a selftest check that fails when you break the code.
