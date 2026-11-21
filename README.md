# shotglass

Compose short films, cartoons and full-length episodes from GLSL shaders. Shots, transitions and
GPU-synthesised sound sit on one scrubbable timeline, with live reload and
headless stills. Rust + OpenGL, no browser.

![Big Shadow contact sheet](docs/big-shadow-strip.png)

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
  while the last working version keeps playing.
- **Long films too.** Shaders compile just ahead of the playhead and sound is
  synthesised in small chunks, so a long, many-shot episode opens straight away
  and memory doesn't grow with its length (see [Long films](#long-films)).

## Quick start

You need [Rust](https://rustup.rs) and a GPU with OpenGL 3.3 or newer.

```sh
git clone https://github.com/antonmarsden/shotglass
cd shotglass
cargo run --release -- play movies/big-shadow
```

The demo uses [shotglass-kit](https://github.com/antonmarsden/shotglass-kit), so the
first run downloads it with `git` (see [Libraries](#libraries)).

To start your own film:

```sh
cargo run --release -- new movies/my-film
cargo run --release -- play movies/my-film
```

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

## A movie

A movie is a folder:

```
movies/big-shadow/
├── movie.toml          the edit: shots, timings, uniforms, transitions, libraries
├── shotglass.lock      the exact library versions (written by `shotglass fetch`)
├── common.glsl         shared helpers (#include "../common.glsl"), built on the kit
├── scenes/
│   ├── shadow.glsl
│   ├── moon.glsl
│   └── black.glsl
├── music.glsl          vec2 mainSound(int samp, float time), movie time
├── sfx/                one sound per shot, shot time
│   ├── candle.glsl
│   ├── plucks.glsl
│   └── thump.glsl
└── mix.glsl            vec2 mainMix(int samp, float time)
```

`movie.toml`:

```toml
shotglass = "0.1"             # optional: the engine contract the film was made for
title = "Big Shadow"
aspect = [16, 9]
fps = 30                      # or 29.97, or "30000/1001"; frame stepping, iFrame and export
# glsl = "430"                # opt in to a later GLSL; see "Portable shaders" (default 330)

[sound]
music = "music.glsl"          # optional
mix = "mix.glsl"              # optional; without it, music and shot sounds are summed
mix_lookback = 1.0            # how far back (s) the mix may read music()/sfx(); default 1
volume = 0.9

[[shot]]
id = "wall"
scene = "scenes/shadow.glsl"
duration = 9.0
uniforms.uRadius = 0.003                                       # float
uniforms.uOrbit  = [0.0, 0.0]                                  # vec2 (up to vec4)
uniforms.uDist   = { keys = [[0, 0.95], [6, 0.22]], ease = "smoother" }
uniforms.uLight  = { keys = [[0, 1], [6.4, 1], [8.2, 0, "in"]] }   # per-key ease
sound = "sfx/candle.glsl"     # this shot's sound

[[shot]]
id = "moon"
scene = "scenes/moon.glsl"
duration = 8.0
# The transition *into* this shot, here shotglass-kit's fade. No transition = a hard cut.
transition = { type = "transitions/fade", duration = 1.4, params = { hold = 0.25 } }

[[shot]]
id = "closeup"
scene = "scenes/shadow.glsl"     # shots can reuse a scene with different uniforms
duration = 8.0
time_offset = 20.0               # iTime starts here (default 0); time_scale also available
transition = { type = "transitions/iris", duration = 1.5, freeze = true, ease = "inout", params = { center = [0.72, 0.70] } }
sound = "sfx/plucks.glsl"
sound_lead = 0.6                 # start the sound before the picture (J-cut); sound_tail lets it ring on

[[marker]]
t = 6.4
label = "flame gutters"
```

Shots overlap by the length of the transition between them, so a 9 s shot
followed by a 1.4 s transition hands over at 7.6 s. `shotglass check` reports
any transition that is longer than its shots allow.

Keyframe times are in shot-local seconds. The eases are `linear`, `smooth`
(default), `smoother`, `in`, `out`, `inout` and `step`.

### Scene shaders

Scene shaders are written like Shadertoy shaders:

```glsl
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord / iResolution.xy;
    fragColor = vec4(uv, 0.5 + 0.5 * sin(iTime), 1.0);
}
```

These uniforms are available:

| uniform | meaning |
|---|---|
| `iResolution`, `iTime`, `iTimeDelta`, `iFrame`, `iFrameRate`, `iDate`, `iMouse` | as on Shadertoy. `iTime` is the shot's own clock (plus `time_offset`) |
| `iShotTime`, `iShotDuration`, `iShotProgress` | shot-local seconds, the shot's length, and 0→1 over the shot |
| `iMovieTime` | seconds since the start of the movie |
| `iChannel0..3` | declared so pasted code compiles; they are bound to black for now |
| your `uniforms.*` | declared automatically from `movie.toml` |
| `CUE_<ID>_START`, `CUE_<ID>_SETTLED`, `CUE_<ID>_END`, `MOVIE_DURATION` | `#define`s from the edit (`SETTLED` = the incoming transition has finished) |

`#include "file.glsl"` works relative to the including file, then relative to the movie folder.
`#include <name/file.glsl>` takes `file.glsl` from the library the film calls `name`
(see [Libraries](#libraries)). Each file is included at most once per shader, so
shared helpers can include each other freely.

### Transitions

`type` names a transition in a library, `<library>/<name>`: `transitions/iris` is
`iris.glsl` in the library the film calls `transitions`. shotglass-kit's are:

```toml
[libraries]
transitions = { source = "https://github.com/antonmarsden/shotglass-kit", subdir = "transitions" }
```

| type | params (defaults) |
|---|---|
| `transitions/crossfade` | – |
| `transitions/fade` | `color = [0,0,0]`, `hold = 0` (fraction of the transition spent fully on the colour) |
| `transitions/iris` | `center = [0.5,0.5]`, `softness = 0.004`, `closing = 0` (1 = shrink onto the old shot) |
| `transitions/dissolve` | `scale = 9`, `softness = 0.08` |
| `transitions/wipe` | `direction = [1,0]`, `softness = 0.01` |

shotglass itself has no built-in transitions: the engine provides the transition
interface, and the looks come from libraries or your own files.

A custom transition is a `.glsl` file in the film, referenced by path, e.g. `type = "fx/heart.glsl"`:

```glsl
#ifndef softness
#define softness 0.02          // each param arrives as a #define
#endif
vec4 transition(vec2 uv) {     // uv is 0..1; ratio = width / height
    vec2 p = (uv - 0.5) * vec2(ratio, 1.0);
    p.y -= 0.25 * abs(p.x);                                          // a rough heart
    float r = progress * 1.4;
    float inside = smoothstep(r + softness, r - softness, length(p * vec2(1.0, 1.4)));
    return mix(getFromColor(uv), getToColor(uv), inside);
}
```

To port a shader from gl-transitions, turn its `uniform float x; // = 0.3` lines
into the `#ifndef` / `#define` form shown above.

`freeze = true` renders the outgoing shot once, at the frame where the
transition starts, and holds it. Only the incoming shot renders live, so the
transition costs about the same as a normal frame. This works well for wipes and
irises. Leave it off for slow dissolves, where a still frame looks dead.

### Sound

All sound is GLSL, synthesised on the GPU when the movie loads. It comes in three layers:

```
music.glsl   movie time, runs across cuts ───────────────► music() ─┐
shot sounds  shot time + shot uniforms, faded per transition ► sfx() ─┼─► mix.glsl ─► speakers
                                                                     (or music + sfx)
```

**Music** (`[sound] music`) is `vec2 mainSound(int samp, float time)` with `time` in
movie seconds. Use the cue `#define`s to place it against the edit:

```glsl
vec2 mainSound(int samp, float time) {
    float k = smoothstep(CUE_MOON_START, CUE_MOON_SETTLED, time);   // follow the fade
    float s = 0.2 * sin(6.2831 * mix(110.0, 146.8, k) * time);
    return vec2(s);
}
```

**Shot sounds** (`sound = "sfx/candle.glsl"` on a shot) use the same signature, but
`time` is shot-local (like `iTime`) and the shot's uniforms are available, evaluated
at that time. The candle in the demo multiplies by `uLight`, so its crackle dies
exactly when the flame does. Shot sounds move with their shot when you retime the
edit. During a transition the outgoing and incoming sounds crossfade, using the
transition's ease and equal-power gains. `sound_lead` starts a sound before its
shot (a J-cut; `time` is negative during the lead), and `sound_tail` lets it ring
on after the shot. In both cases the sound skips the fade on that side.
`sound_gain` scales it.

**The mix** (`[sound] mix`) is optional: `vec2 mainMix(int samp, float time)` runs
after everything else and can read any sample of the two buses. That makes
delays, echoes and multi-tap reverbs cheap lookups, which a single `mainSound`
can't do:

```glsl
vec2 mainMix(int samp, float time) {
    int ms = int(iSampleRate * 0.001);
    vec2 echo = music(samp - 310 * ms) * 0.35;                       // 310 ms echo
    float duck = 1.0 - 0.7 * smoothstep(0.12, 0.35, sfxLevel(samp)); // music dips under loud effects
    return (music(samp) + echo) * duck + sfx(samp);
}
```

The mix can read up to `mix_lookback` seconds back (default 1) and 50 ms ahead.
Reads outside that range return silence.

Sound is synthesised in chunks of about 0.7 s, just ahead of the playhead (see
[Long films](#long-films)). Each chunk of each track is cached by its source and
timing. Editing one shot's sound re-synthesises only that shot (and the mix), and
picture-only edits don't touch the sound. Until the new chunks are ready, the
previous version keeps playing. Recursive filters (resonant sweeps, feedback reverb) are the main
thing GLSL sound can't do cheaply. Multi-tap delays in the mix get you most of
the way.

### Portable shaders

A film that compiles on your machine should compile on every machine with
OpenGL 3.3, and in a browser. Two things get in the way: drivers offer different
GLSL versions, and some drivers (NVIDIA especially) accept code the spec forbids.
shotglass handles both.

**One language everywhere.** Every shader is compiled as `#version 330 core`
(OpenGL 3.3), on every desktop driver, even one that offers 4.6. On OpenGL ES it's
`#version 300 es`, which the web player will use too. The harness writes the
`#version` line; a `#version` in your file is ignored. So `bitfieldExtract` (GLSL 4.00)
or `textureQueryLevels` (4.30) fail on your machine too, not just on someone
else's Mac.

**Opting in to more.** If a film really needs a later version, say so in `movie.toml`:

```toml
glsl = "430"     # or "410" (the most macOS offers); "330" is the default
```

This narrows where the film runs. macOS stops at 4.10, Windows on ARM at 3.30, and
browsers only run GLSL ES 3.00. On a driver that can't provide the version, the
film is refused with a message saying so, and `check` warns about it.

**Strict checking.** `shotglass check` runs every shader (scenes, transitions,
sound, mix, with your `#include`s) through [glslang](https://github.com/KhronosGroup/glslang),
the Khronos reference compiler, built into shotglass. It doesn't depend on your
driver, so it catches code your driver accepts but others reject. Errors point at
your file and line:

```
shot "moon" (scenes/moon.glsl):
  scenes/moon.glsl:12: error: 'textureQueryLevels' : no matching overloaded function found (valid in GLSL 430 core: this film compiles as GLSL 330 core, the portable baseline)
```

`check` also tries every shader as GLSL ES 3.00 and warns about anything that
wouldn't run in a browser. `check --web` turns those warnings into errors.
`check --no-gpu` runs only these checks, with no OpenGL needed (handy in CI).

Common things that work on one driver and fail on another:

| construct | problem | portable version |
|---|---|---|
| `float x = 1;`, `v * 2`, `pow(x, 2)`, `i < 3.0` | implicit int→float: legal desktop GLSL, an error in GLSL ES | `1.0`, `v * 2.0`, `pow(x, 2.0)`, `float(i) < 3.0` |
| `bitfieldExtract`, `fma`, `textureQueryLevels`, `textureGather` | from GLSL 4.x | write it out, or opt in with `glsl` |
| `saturate`, `lerp`, `frac`, `float3`, `mul` | HLSL, accepted by some drivers | `clamp(x, 0.0, 1.0)`, `mix`, `fract`, `vec3`, `*` |
| `texture2D`, `gl_FragColor` | deprecated; gone in GLSL ES | `texture`, `fragColor` (`mainImage`'s out parameter) |
| `x % y` on floats | `%` is for integers | `mod(x, y)` |
| `layout(binding = 0)` | needs GLSL 4.20 | leave bindings to the harness |
| a `sampler3D` or `isampler2D` | GLSL ES needs a precision qualifier | `uniform highp sampler3D t;` |

## The player

`shotglass play <movie> [--at 12.5 | --at moon | --at moon+2] [--scale auto | 1.0]`

| key | action |
|---|---|
| Space | play / pause |
| ← → | step one frame |
| Shift + ← → | ±1 second |
| `[` `]` | previous / next shot |
| `,` `.` | jump to just before the previous / next transition |
| L | loop the shot or transition under the playhead (or double-click the timeline) |
| 1 2 3 | speed ¼×, ½×, 1× |
| Q | cycle render scale: auto, 100%, 50% |
| M | mute |
| R | reload now |
| H / Tab | hide the side panel |

To scrub, click or drag on the timeline. Hatched blocks are transitions;
"(frozen)" marks transitions that use `freeze`.

### Avoiding frame drops

- **Audio is the master clock.** The picture always shows the frame for the
  sample being heard, so a late frame never causes drift.
- **One render per film frame.** Playback renders each frame of the film once, at
  the `fps` in `movie.toml`, and holds it until the next, so a 30 fps film needs
  half the GPU work of redrawing at 60 Hz. You see exactly the frames `still` renders.
- **Adaptive resolution, only when needed.** Auto mode times each frame on the GPU
  (`GL_TIME_ELAPSED`). It lowers the render scale only when the average GPU time
  is above 90% of a film frame, and climbs back as soon as there's room.
  Slow frames caused by compiling or sound synthesis don't count. When you
  pause, it renders at full quality. To fix the scale, use
  `play --scale 1.0` (or press Q).
- **Shaders compile ahead of the playhead**, on driver threads where the GPU
  supports `KHR_parallel_shader_compile`, and get one tiny warm-up draw each. While
  playing, the player never waits for a compile: if a shot isn't ready, the previous
  frame stays up. When paused or scrubbing, it compiles on the spot.
- **Sound never makes a frame wait for the GPU.** While playing, each frame queues
  a block or two of sound synthesis on the GPU and collects the blocks that have
  finished (a pixel buffer readback behind a fence), so the player never waits for
  the GPU to finish the scene before the sound. Sound work runs after the frame's
  picture has been queued, so even a slow driver call can't delay the picture. When paused or scrubbing, the chunk
  under the playhead is made on the spot. Right after a seek during playback the
  sound can start a frame or two late. Where the driver supports it (GL 4.4), the
  sound buffers stay mapped, so reading results and writing mix inputs need no
  driver call at all (`SHOTGLASS_NO_PERSISTENT=1` turns this off). The Performance
  panel splits stalls into sound and compiling, and shows where the worst sound
  stall's time went, down to the GL call. Ticks under its frame graph mark the
  frames that rendered a film frame, queued or collected sound, or compiled;
  hover a frame for its numbers.
- **Frozen transitions** render only one scene.
- `shotglass check` times every shot and transition, so you can see which ones
  are over budget.

### Long films

Nothing is loaded all at once, so a 20-minute episode costs about the same as a
20-second short. In a test, a 9-minute, 80-shot film opened at once, compiled its
remaining shaders in the background in a few seconds, and synthesised its whole
soundtrack (`shotglass wav`) in 4 s:

- **Pictures.** Shaders compile nearest the playhead first, everything within
  10 s ahead, and the rest in the background so errors still show up early. At
  most 64 compiled programs are kept; the least recently used are dropped.
  Compiled programs are also saved to disk (`%LOCALAPPDATA%\shotglass\programs` on
  Windows, `~/.cache/shotglass/programs` elsewhere). Reopening a film, or coming
  back to an evicted shot, skips the compile. Set `SHOTGLASS_NO_CACHE=1` to turn
  this off, or `SHOTGLASS_CACHE_DIR` to move it.
- **Sound.** Chunks are synthesised up to 6 s ahead of the playhead, a few
  blocks per frame without waiting for the GPU, and chunks more than a few seconds away are
  forgotten. Memory stays at a few MB whatever the length. `wav` and `check`
  stream chunk by chunk too.
- The Sound panel shows how much sound is buffered ahead and any underruns.
  The overlay shows shaders still compiling.

## Libraries

Shared GLSL comes from versioned libraries that a film declares by name. The engine
provides only the contract (uniforms, entry points, the transition interface, cue
defines), so upgrading shotglass doesn't change how a finished film looks or sounds.

```toml
shotglass = "0.1"        # engine contract this film needs; other engines refuse with a clear message

[libraries]
kit  = { version = "0.4", source = "https://github.com/antonmarsden/shotglass-kit", subdir = "core" }
sfx  = { version = "0.3", source = "https://github.com/antonmarsden/shotglass-kit", subdir = "sounds" }
toon = { version = "1.2", source = "https://github.com/someone/toon-kit" }
edge = { source = "https://github.com/someone/fx-kit" }   # no version: follow the default branch
mine = { path = "lib/mykit" }            # a library inside the film's own folder
```

```glsl
#include <kit/noise.glsl>
#include <toon/outline.glsl>
```

- **The film chooses the name.** The include prefix is the local name from
  `[libraries]`, so two publishers' libraries can both be called "kit" without clashing.
- **`version`** is a semver range (`"0.4"` means `>=0.4.0, <0.5.0`, as in Cargo),
  matched against the source's git tags (`v0.4.2` or `0.4.2`).
- **No `version`** follows a branch instead of releases: the repository's default
  branch, or `branch = "name"`. Fetching pins the branch's newest commit in
  `shotglass.lock`, so the film still doesn't change until `shotglass update`. Handy
  for a library that has no releases yet; prefer a version once it does.
- **`source`** is the https URL of a git repository. **`subdir`** picks a folder of
  it as the library root, so one repository can publish several libraries that a film
  pins separately. Every folder shares the repository's tags.
- **`path`** is a library inside the film folder. It may not climb above it.
- Errors inside a library file show `kit/noise.glsl:12`. A missing include lists
  every place that was searched.
- **Clashes are named.** If two libraries (or a library and the film) define the same
  function or struct, a failed compile starts with "`hash21(vec2)` is defined by both
  library "toon" (toon/outline.glsl:12) and library "kit" (kit/noise.glsl:3)" rather
  than only the driver's "redefinition". Overloads with different parameters are fine.
  Library functions should start with the library's own prefix (`kit_`) to avoid this.

### library.toml

Each library root has one:

```toml
name = "shotglass-kit/core"        # canonical name
version = "0.4.2"
requires_shotglass = "0.1"         # engine contract it needs (optional)
licences = ["MIT"]                 # SPDX identifiers
glsl = 430                         # optional: needs more than the GLSL 3.30 baseline (410 or 430)

[requires]                         # peer libraries: checked, not installed
"shotglass-kit/sounds" = ">=0.3"
```

Libraries are pure GLSL and data: no scripts, build steps or plugins. Inside a
library, includes of *other* libraries use canonical names
(`#include <shotglass-kit/sounds/bell.glsl>`), which shotglass maps to whatever the
film calls them. A peer requirement means the film must list that library itself;
shotglass checks the versions and says what's missing, but never adds anything.

A library with `glsl = 430` only loads in a film that opted in to at least that version
(`glsl = "430"` in `movie.toml`); otherwise loading the film says which library needs it.
Such a film needs a driver with that GLSL and won't play on the web.

### Fetching and shotglass.lock

```sh
shotglass fetch  <movie>           # download what's missing, write shotglass.lock
shotglass update <movie> [name..]  # move pins to the newest version in range (or commit)
```

`fetch` (also run automatically by `play`, `still`, `strip`, `check` and `wav` when
something isn't downloaded yet) resolves each library to an exact version and writes
`shotglass.lock` next to `movie.toml`: version, source, tag, `subdir`, and a sha256
of the library's files (for a followed branch, `tag` is the ref and `rev` the commit). **Commit it with the film.** A new library release never
changes a finished film; only `update` moves a pin. If the files don't match the lock
(a moved tag, an edited cache), that's an error. Loading a film (including live
reload) only reads the lock and the cache and never touches the network.

Downloads use `git` and are cached per user, shared between films and between
libraries from the same repository and version, and work offline afterwards:
`%LOCALAPPDATA%\shotglass\libs` on Windows, `~/Library/Caches/shotglass/libs` on macOS,
`~/.cache/shotglass/libs` elsewhere (`SHOTGLASS_CACHE` overrides it).

The hash is sha256 over the library root's files, sorted by their `/`-separated path
relative to the root and skipping hidden files and folders (`.git`); CRLF becomes LF in
UTF-8 files so Windows checkouts hash the same. For each file it hashes the path, a 0
byte, the content length as 8 little-endian bytes, then the content.

### Working on a library locally

Point a library at a working copy without putting machine paths in the film:

```sh
SHOTGLASS_LIB_KIT=../shotglass-kit/core shotglass play movies/my-film
```

or put an uncommitted `movie.local.toml` next to `movie.toml` (`shotglass new` adds
it to `.gitignore`):

```toml
[libraries.kit]
path = "C:/src/shotglass-kit/core"
```

The variable is `SHOTGLASS_LIB_` plus the local name in capitals, with `-` as `_`.
It wins over `movie.local.toml`. Relative override paths are relative to the film
folder. The version range in `movie.toml` still has to match the working copy's
`library.toml`. Live reload watches library folders, overrides included.

### Portable paths

The same `movie.toml` has to work on every OS and in a future web player, so paths in
it use forward slashes and stay inside the film folder: no absolute paths, drive
letters, backslashes or `..` that climbs out. URLs only go in `source`. File names are
case-sensitive: `check` (and every command) warns when a name only matches a file by
ignoring case, which works on Windows and macOS but not on Linux or the web.

## Command line

```sh
shotglass play   <movie> [--at T] [--scale auto|0.1..1.0]
shotglass still  <movie> --at 3.5,moon,closeup+2 [--size 1280x720] [--out stills/]
shotglass strip  <movie> [--count 12 | --at ...] [--size 384x216] [--cols 4] [--out strip.png]
                 [--compare ref.png [--bless] [--tolerance 8] [--budget 0.1%] [--max-mean 0.25]]
                 # --compare: fail if the strip differs from ref.png (writes strip.diff.png)
shotglass check  <movie> [--web] [--no-gpu] [--size 1280x720] [--frames 8]
                 # validate movie.toml and the GLSL (strictly), compile, synth, time
shotglass wav    <movie> [--out sound.wav] [--rate 48000]
                 [--compare ref.wav [--bless] [--max-rms 0.0015] [--max-peak 0.01]]
                 # --compare: fail if the sound differs from ref.wav (writes sound.diff.wav)
shotglass export <movie> --out film.webm [--size 1920x1080] [--format webm|av1|ffv1|png|ffmpeg] [--rate 48000]
shotglass new    <dir>
shotglass fetch  <movie>                                   # download libraries, write shotglass.lock
shotglass update <movie> [library..]                       # move library pins to the newest allowed
shotglass selftest [filter]                                # GPU + sound checks on this machine
```

`still`, `strip`, `check`, `wav` and `export` render off-screen (`check --no-gpu` doesn't need OpenGL). On a Linux machine with
no display, run them under `xvfb-run`; Mesa's software OpenGL works.

## Export

`shotglass export` renders the film offline to a file. Playback is untouched: this
is a separate path that never drops a frame or lowers the resolution.

```sh
shotglass export movies/big-shadow --out big-shadow.webm                  # VP9 + Opus
shotglass export movies/big-shadow --out big-shadow.mkv --size 3840x2160  # lossless FFV1 + FLAC
shotglass export movies/big-shadow --out frames/                          # PNG sequence + sound.wav
```

- **Frame-accurate.** Frame *n* is rendered at exactly *n* / `fps` (`fps` can be a
  ratio, e.g. `"30000/1001"`, and is written to the file as one), at full size,
  waiting for every shader to compile. It's the same picture `shotglass still`
  renders at that time.
- **Sample-accurate.** The soundtrack is the same 16-bit PCM `shotglass wav` writes,
  `duration × rate` samples starting at sample 0. FLAC and WAV keep it bit for
  bit. Opus is lossy and has an encoder delay, which WebM records so players skip
  it and stay in sync.
- **Colour.** Video formats are converted with the BT.709 matrix and tagged BT.709,
  limited range, so players don't shift the colours. FFV1 stores the RGB pixels
  as rendered (full range, tagged as RGB). VP9 and AV1 store colour at half
  resolution, so the size must be even.
- **Progress and Ctrl-C.** Progress goes to the terminal. Ctrl-C stops the export
  and removes the half-written file (a second Ctrl-C quits at once).

| `--format` | default for | video | audio | use it for |
|---|---|---|---|---|
| `webm` | `.webm` | VP9, CRF 20 | Opus 192 kb/s | sharing, the web |
| `av1` | | AV1 (SVT-AV1, else libaom) | Opus 192 kb/s | smaller files, slower encode |
| `ffv1` | `.mkv` | FFV1, lossless RGB | FLAC | archival, re-encoding later |
| `png` | a folder (no extension) | `frame_000000.png` … | `sound.wav` | editors and compositing; needs no ffmpeg |
| `ffmpeg` | | your `--ffmpeg-args` | | anything else your ffmpeg can do |

All the defaults are royalty-free codecs. To use another one, pass your own ffmpeg
output arguments; the container comes from the `--out` extension, and you choose
the colour handling yourself:

```sh
shotglass export movies/big-shadow --out film.mov --format ffmpeg \
  --ffmpeg-args "-c:v prores_ks -profile:v 3 -c:a pcm_s16le"
```

### Installing ffmpeg

Every format except `png` pipes raw frames to an [ffmpeg](https://ffmpeg.org)
process, so ffmpeg must be on your `PATH` (or set `SHOTGLASS_FFMPEG` to its full
path). shotglass doesn't link or ship any codecs.

- **Windows:** `winget install Gyan.FFmpeg` (then open a new terminal), or download a
  build from <https://ffmpeg.org/download.html> and add its `bin` folder to `PATH`.
- **macOS:** `brew install ffmpeg`
- **Linux:** `sudo apt install ffmpeg` (Debian/Ubuntu), `sudo dnf install ffmpeg`
  (Fedora, with RPM Fusion), or your distribution's package.

Check with `ffmpeg -encoders`: `webm` needs `libvpx-vp9` and `libopus`, `av1` needs
`libsvtav1` or `libaom-av1`, and `ffv1` needs `ffv1` and `flac` (built in).

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
  tested on non-portable snippets, and the demo film and the `shotglass new` template
  must pass it at every `glsl` version and as GLSL ES.
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
  - `export` writes every frame and sample: a PNG sequence always, and FFV1 + FLAC
    when ffmpeg is installed (decoded and compared with `still` and `wav`).
- **Golden images** (`tests/golden.rs`): contact sheets of the demo film and the
  `shotglass new` template are compared with reference PNGs in `tests/golden/`, so
  a change that compiles but looks wrong fails the tests. A small per-pixel
  tolerance absorbs the differences between software OpenGL and real GPUs. To
  update the references on purpose, see [CONTRIBUTING.md](CONTRIBUTING.md#golden-images).
- **Golden soundtracks** (same file): the soundtracks of both films are compared with
  reference WAVs, window by window (RMS and peak of the difference), and a failure
  names the first time that differs.
- On Linux without a display, use `xvfb-run -a cargo test` (that's what CI does).
  Set `SHOTGLASS_SKIP_GPU_TESTS=1` to skip the selftest and the golden tests.
- **Docker:** `docker build -f docker/test.Dockerfile -t shotglass-test . && docker run --rm shotglass-test`
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

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md): build and test commands, golden images,
the PR checklist, licensing, and how to work on shotglass with AI coding agents.

## Licence

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT), at your option.

Shaders copied from shadertoy.com are usually licensed CC BY-NC-SA 3.0 by their
authors. If you include them in a movie, keep their licence notes with them.
