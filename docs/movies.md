# Movies: `movie.toml`, shaders and transitions

How a film is laid out on disk, every `movie.toml` field, scene shaders and their uniforms, transitions, and writing portable GLSL.

[README](../README.md) · [Movies](movies.md) · [Channels](channels.md) · [Layers and blends](layers.md) · [Sound](sound.md) · [Loop films](loop-films.md) · [The player](player.md) · [Projects and libraries](libraries.md) · [Testing GLSL and quality metrics](testing-and-quality.md) · [Command line and export](cli.md) · [Testing shotglass](testing.md)

## A movie

A movie is a folder. This is what `shotglass new movies/my-film` writes:

```
movies/my-film/
├── movie.toml          the edit: shots, timings, uniforms, transitions
├── shotglass.toml      the project: engine contract and libraries (see "Projects")
├── scenes/
│   ├── first.glsl      void mainImage(out vec4 fragColor, in vec2 fragCoord)
│   └── second.glsl
├── music.glsl          vec2 mainSound(int samp, float time), movie time
└── sfx/                one sound per shot, shot time
    └── chime.glsl
```

Add folders as the film grows: shared helpers (`common.glsl`, `#include`d), a mix
pass (`mix.glsl`), `textures/`, `fonts/`. A movie with its own `shotglass.toml` is a
project of its own; that file names the engine contract and the
[libraries](libraries.md#libraries) it uses:

```toml
shotglass = "0.1"             # optional: the engine contract the film was made for
[libraries]
transitions = { source = "https://github.com/antonmarsden/shotglass-kit", subdir = "transitions" }
```

A fuller `movie.toml`:

```toml
title = "My short"
aspect = [16, 9]
fps = 30                      # or 29.97, or "30000/1001"; frame stepping, iFrame and export
# glsl = "430"                # opt in to a later GLSL; see "Portable shaders" (default 330)
# loop = true                 # play forever, Shadertoy-style; see "Loop films"

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

A shot can be a comp instead of a scene (`comp = "intro"`, with the comp declared in
`[comps]`), and layers can be comps or carry a `matte`: see
[Layers and blends](layers.md#comps).

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
| `iChannel0..7`, `iChannelResolution[8]` | images, text and image sequences from the shot's `channels` (see [Textures](channels.md#textures)); an unbound channel reads black and its resolution is 0. Shadertoy code uses 0–3; 4–7 are shotglass's |
| `iChannelTime[8]` | an image sequence's source time of the frame shown, in seconds (see [Footage](channels.md#footage)); 0 for other channels |
| `iChannelMeta[8]` | a text channel's metrics in texels: (em, first baseline from the top, line pitch, padding); 0 for images (see [Text](channels.md#text)) |
| your `uniforms.*` | declared automatically from `movie.toml` |
| `CUE_<ID>_START`, `CUE_<ID>_SETTLED`, `CUE_<ID>_END`, `MOVIE_DURATION` | `#define`s from the edit (`SETTLED` = the incoming transition has finished) |

`#include "file.glsl"` works relative to the including file, then relative to the movie folder.
`#include <name/file.glsl>` takes `file.glsl` from the library the film calls `name`
(see [Libraries](libraries.md#libraries)). Each file is included at most once per shader, so
shared helpers can include each other freely.

### Transitions

`type` names a transition in a library, `<library>/<name>`: `transitions/iris` is
`iris.glsl` in the library the film calls `transitions`. shotglass-kit's are, in
`shotglass.toml`:

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
wouldn't run in a browser, including identifiers containing two consecutive
underscores (reserved in WebGL 2). `check --web` turns those warnings into errors.
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
