# shotglass

Compose short films, cartoons and full-length episodes from GLSL shaders. Shots, transitions and
GPU-synthesised sound sit on one scrubbable timeline, with live reload and
headless stills. Rust + OpenGL, no browser.

![A contact sheet of a shotglass film](docs/big-shadow-strip.png)

- **Shots are Shadertoy-style `.glsl` files.** Write `mainImage`, use `iTime` and
  `iResolution`, and declare nothing else. Most Shadertoy code pastes straight in.
- **Transitions combine two shots.** Fades, irises, dissolves and wipes come from
  [shotglass-kit](https://github.com/antonmarsden/shotglass-kit), or you can write your
  own using the [gl-transitions](https://gl-transitions.com)
  `getFromColor` / `getToColor` / `progress` convention.
- **The sound is GLSL too.** Music runs across the cuts, each shot can have its own
  sound that fades with the transitions, and an optional mix pass adds echo, reverb
  or ducking. All of it is synthesised on the GPU.
- **The sound card is the clock.** A slow frame is skipped instead of drifting out
  of sync, and adaptive resolution keeps frames on time.
- **Live reload.** Save a shader or `movie.toml` and the player reloads in place
  at the same moment. A broken shader shows its errors (with correct file:line)
  while the last working version keeps playing. If a sound shader fails and the
  film length or loop mode changes, playback is silent until the sound compiles.
- **Long films too.** Shaders compile just ahead of the playhead and sound is
  synthesised in small chunks, so a long, many-shot episode opens straight away
  and memory doesn't grow with its length (see [Long films](docs/player.md#long-films)).

## Quick start

You need [Rust](https://rustup.rs) and a GPU with OpenGL 3.3 or newer.

```sh
git clone https://github.com/antonmarsden/shotglass
cd shotglass
cargo run --release -- new movies/my-film
cargo run --release -- play movies/my-film
```

`shotglass new` writes a small two-shot film with music, a shot sound and a
transition, commented as a starting point. It uses
[shotglass-kit](https://github.com/antonmarsden/shotglass-kit) for its transition,
downloaded with `git` on the first run (see [Libraries](docs/libraries.md#libraries)).

A finished film is in `movies/big-shadow` (`cargo run --release -- play movies/big-shadow`).
It's also what the golden tests render, so it keeps its shared GLSL in its own `lib/`.

Or install it with `cargo install --path .`, then run `shotglass play movies/my-film`.

To install from crates.io instead, set `CXXFLAGS` as shown:

```sh
CXXFLAGS=-DNDEBUG cargo install shotglass              # Linux / macOS
$env:CXXFLAGS="-DNDEBUG"; cargo install shotglass      # Windows PowerShell
```

The strict GLSL checker (glslang) must be built with `NDEBUG`, or one of its debug
assertions aborts `shotglass check` on valid shaders. Inside this repository,
`.cargo/config.toml` sets it for you, but Cargo ignores that file when installing
from crates.io.

The built-in WebM export (`export --format webm`, the `webm` cargo feature) builds
SVT-AV1 and libopus from source, so building shotglass needs
[CMake](https://cmake.org), and [nasm](https://nasm.us) 2.14 or newer for SVT-AV1's
fast paths (without nasm it still builds, warns, and encodes several times slower).
In a clone, fetch SVT-AV1 first: `git submodule update --init`. To link an installed
SVT-AV1 instead, set `SVT_AV1_DIR` to its prefix; `--no-default-features --features
strict-glsl` leaves the encoders out (WebM then goes through ffmpeg).

## Main concepts

- **Movie:** a folder with a `movie.toml` (the edit: shots, timings, uniforms,
  transitions) and its shaders. See [Movies](docs/movies.md).
- **Shot:** a Shadertoy-style `mainImage` shader over a span of the timeline, with its
  own clock (`iShotTime`) and optional `channels` (images, footage, text).
- **Transition:** a GLSL pass that combines the outgoing and incoming shots.
- **Layer:** a pass drawn over a span of the film and composited over the picture.
- **Sound:** music, per-shot sound and a mix pass, synthesised on the GPU in GLSL.
- **Project and libraries:** `shotglass.toml` holds the engine contract and the shared
  GLSL libraries (such as shotglass-kit) that shaders `#include`, pinned by `shotglass.lock`.
- **Checks:** `shotglass check` validates strictly, `shotglass test` runs GLSL tests,
  `shotglass quality` measures frames and sound against limits.

## Documentation

The detailed user documentation lives in [`docs/`](docs/):

- [Movies: `movie.toml`, shaders and transitions](docs/movies.md): How a film is laid out on disk, every `movie.toml` field, scene shaders and their uniforms, transitions, and writing portable GLSL.
- [Channels: textures, footage and text](docs/channels.md): What a shot can bind to `iChannel0..7`: images, image sequences (footage) and laid-out text, plus getting fonts.
- [Layers and blends](docs/layers.md): Layers over spans of the film, their opacity and scale, blend modes and reading other layers.
- [Sound](docs/sound.md): Music, per-shot sound and the mix pass, all synthesised on the GPU in GLSL.
- [Loop films](docs/loop-films.md): Films that play forever (`loop = true`), Shadertoy-style, with a seamless picture and sound.
- [The player](docs/player.md): The interactive player, avoiding frame drops, and how long films stay fast.
- [Projects and libraries](docs/libraries.md): Projects (`shotglass.toml`), shared GLSL libraries, `shotglass.lock`, local overrides, licences, freezing and bundling.
- [Testing GLSL and quality metrics](docs/testing-and-quality.md): `shotglass test` (GLSL unit/property tests, metric tests) and `shotglass quality` (picture and sound metrics with limits).
- [Command line and export](docs/cli.md): Every `shotglass` command and its options, and exporting a film to video (WebM built in, other formats with ffmpeg).
- [Testing shotglass](docs/testing.md): How shotglass itself is tested (unit, property, selftest, golden images and sounds) and its robustness measures.

Design notes for features (past and planned) are in [`docs/design/`](docs/design/).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md): build and test commands, golden images,
the PR checklist, licensing, and how to work on shotglass with AI coding agents.

## Licence

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT), at your option.

Shaders copied from shadertoy.com are usually licensed CC BY-NC-SA 3.0 by their
authors. If you include them in a movie, keep their licence notes with them.
