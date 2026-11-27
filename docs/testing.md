# Testing shotglass

How shotglass itself is tested (unit, property, selftest, golden images and sounds) and its robustness measures.

[README](../README.md) · [Movies](movies.md) · [Channels](channels.md) · [Layers and blends](layers.md) · [Feedback buffers](buffers.md) · [Sound](sound.md) · [Loop films](loop-films.md) · [The player](player.md) · [Projects and libraries](libraries.md) · [Testing GLSL and quality metrics](testing-and-quality.md) · [Command line and export](cli.md) · [Testing shotglass](testing.md)

## Testing

```sh
cargo test                  # unit + property tests, then `shotglass selftest` on your GPU
shotglass selftest          # just the GPU and sound checks (handy after a driver update)
shotglass selftest strips   # only checks whose name contains "strips"
```

- **Unit and property tests** cover timeline maths, keyframes, error-log parsing,
  the render-scale logic and the master clock (against a simulated sound card that
  can lag, stall or run away). Random timelines and malformed `movie.toml` files
  are generated too, to check the parser never panics. The strict GLSL checker is
  tested on non-portable snippets, and the demo film must pass it at every `glsl`
  version and as GLSL ES (the `shotglass new` template, which follows shotglass-kit's
  `main`, is checked daily by the kit canary instead).
- **`selftest`** writes small fixture films to a temporary folder (each must also
  pass the strict GLSL check, at the baseline and as GLSL ES) and checks on the
  actual GPU and driver that:
  - transitions start exactly on the old shot and end on the new one;
  - live and frozen crossfades blend correctly;
  - reduced render scale has no edge bleed, and strip-drawn frames match single draws;
  - errors point at the right file:line, including inside `#include`s and as
    `name/file.glsl:line` inside libraries, and each file is included once;
  - shaders get `#version 330 core` whatever the driver offers (or the film's
    `glsl` opt-in, refused on drivers that can't provide it);
  - a broken edit keeps the last working version on screen;
  - the program cache reloads identically and survives corruption;
  - evicted shaders come back correctly;
  - chunked sound matches a single-pass render sample for sample, with no clicks
    at cuts, fades or chunk edges;
  - streamed sound (as during playback) never waits for the GPU and matches the
    blocking render bit for bit.
  - reading a frame into part of a bigger image lands exactly in place, leaves the
    rest alone and restores OpenGL's pixel-pack state;
  - texture channels bind the right image to the right unit, upright as on Shadertoy
    (or flipped with `vflip = false`), with `iChannelResolution`; filter, wrap,
    mipmaps and sRGB follow `movie.toml`, and a file used with several samplers is
    uploaded once;
  - playback never loads a texture on the spot: uploads are spread over frames and
    the shot appears only once its textures are complete; a broken image keeps the
    last working one, a missing one refuses the edit, an image edit compiles nothing;
    evicted textures come back identically, frozen transitions use the outgoing
    shot's textures, and uploads ignore and restore stray pixel-unpack state;
  - `export` writes every frame and sample: a PNG sequence always, and FFV1 + FLAC
    when ffmpeg is installed (decoded and compared with `still` and `wav`);
  - in a loop film the frame at t + k·period is the frame at t (by frame number, even
    a million loops in, and by time), and one looped period of sound is the steady
    state of a long unlooped render: no click at the seam, the echo carries over, the
    same whatever the chunk size, streamed or not.
  - `shotglass test` reports exactly the expected failures (first failure per test,
    inputs, NaN, grid endpoints, banded draws, "ran no assertions") on a fixture
    project, and a broken test file doesn't stop the others.
- **Golden images** (`tests/golden.rs`): contact sheets of the demo film,
  `tests/films/channels` (iChannel textures: mipmaps,
  wrap and filter modes, `vflip`) and `tests/films/loop` (a `loop = true` film over
  two periods) are compared with reference PNGs in `tests/golden/`, so
  a change that compiles but looks wrong fails the tests. A small per-pixel
  tolerance absorbs the differences between software OpenGL and real GPUs. To
  update the references on purpose, see [CONTRIBUTING.md](../CONTRIBUTING.md#golden-images).
- **Golden soundtracks** (same file): the soundtracks of the demo and
  the loop film are compared with
  reference WAVs, window by window (RMS and peak of the difference), and a failure
  names the first time that differs.
- On Linux without a display, use `xvfb-run -a cargo test` (that's what CI does).
  Set `SHOTGLASS_SKIP_GPU_TESTS=1` to skip the selftest and the golden tests.
- **Docker:** `docker build -f docker/test.Dockerfile -t shotglass-test . && docker run --init --rm shotglass-test`
  runs the same suite on software OpenGL, reproducibly. It tests shotglass, not your
  driver. GPU access from a Linux container on Docker Desktop goes through WSL2's
  Direct3D 12 bridge (`--device /dev/dxg`, `-v /usr/lib/wsl:/usr/lib/wsl`,
  `LD_LIBRARY_PATH=/usr/lib/wsl/lib`). That's a translation layer, so for real-driver
  results run the tests natively.

### Robustness

- **Driver watchdog.** If a frame takes more than ~100 ms on the GPU, scenes are
  drawn in horizontal strips, flushed one at a time, and sound is always
  synthesised in small bands. No single GPU job runs long enough to trip the
  watchdog (Windows resets the GPU after ~2 s). If the driver resets anyway, for
  example from a shader with an infinite loop, the player stops and says so
  instead of crashing. (While playing it only asks the driver after a frame that
  took over half a second, since asking costs a frame on some drivers.)
- **Bad input** in `movie.toml` is reported, never a crash (fuzzed by the
  property tests).
