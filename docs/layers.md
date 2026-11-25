# Layers and blends

Layers over spans of the film, their opacity and scale, blend modes, reading other layers, comps and mattes.

[README](../README.md) · [Movies](movies.md) · [Channels](channels.md) · [Layers and blends](layers.md) · [Sound](sound.md) · [Loop films](loop-films.md) · [The player](player.md) · [Projects and libraries](libraries.md) · [Testing GLSL and quality metrics](testing-and-quality.md) · [Command line and export](cli.md) · [Testing shotglass](testing.md)

## Layers

Layers sit above the shots: a title over a span that crosses cuts and dissolves, a logo
for the whole film, a grade or grain over everything (needs `shotglass = "0.1.4"`).
A layer is a scene shader like a shot's, drawn over everything below it while it's
active, bottom to top in file order:

```toml
[[layer]]
id = "title"
scene = "layers/title.glsl"     # mainImage writes vec4(colour, alpha)
from = "moon"                   # with shot "moon"
to = "closeup.settled"          # until the transition into "closeup" has finished
fade = [0.4, 0.6]               # fade in / out, seconds
scale = 1                       # full resolution: crisp text whatever the render scale
channels = ["title"]            # a [texts] entry, as on shots

[[layer]]
id = "grade"
type = "fx/grade"               # a library pass, named like a transition
opacity = 0.7                   # a 70%-strength grade
channels = ["@input"]           # iChannel0 = the picture below this layer
params = { lift = [0.02, 0.0, 0.04] }
uniforms.uExposure = { keys = [[0, -2.0], [3, 0.0]] }
```

- **Alpha:** `mainImage` writes straight alpha, the Shadertoy way; a layer that writes
  none is opaque. shotglass premultiplies, applies `opacity`, and composites it over
  what's below (source-over). A layer that reads `@input` and writes an opaque result is
  a filter (grade, vignette, letterbox); at `opacity = 0.5` it's a half-strength one.
- **Span:** `from` / `to` are movie seconds or `"<shot>"`, `"<shot>.start"`,
  `".settled"`, `".end"` (an earlier layer has `.start` and `.end`), optionally
  `+ s` / `- s`: `from = "moon + 0.5"`. A bare shot id is its start for `from` and its
  end for `to`. Without them the layer covers the whole film. A loop film's layers stay
  inside one period.
- **Time:** inside a layer, `iTime` (with `time_offset`/`time_scale`), `iShotTime`,
  `iShotDuration` and `iShotProgress` are the layer's own, from 0 at `from`;
  `iMovieTime` is the film's. Keyframes (`uniforms.*`, `opacity`) are in layer-local
  seconds. Cue defines gain `CUE_<LAYER>_START` / `_END`, so sound can follow a title.
- **Scale:** `"auto"` (default) follows the player's render scale like shots; a number
  is a fixed fraction of the output (`1` for text, `0.5` for a cheap grain), the same in
  `still` and `export`.
- **`type` passes** get their `params` as `#define`s and `#define SG_HAS_<name>` for each
  uniform the film sets, so a library pass can give the rest defaults
  (`#ifndef SG_HAS_uExposure` … `const float uExposure = 0.0;`).
- A frame shows only when every active layer is ready (like a shot's programs and
  textures), and a broken layer edit keeps its last working version on screen.
- In the player, each layer has a row under the shots in the timeline, with **M** (mute)
  and **S** (solo) buttons at its left end, to preview without some layers. Solo wins:
  while a layer is soloed only it is drawn, and the mutes (dimmed) come back when you
  turn solo off. While any layer is hidden the transport says "layers muted": `still`
  and `export` always draw them all.

### Blend modes and reading other layers

A layer is composited with source-over unless it names a **blend pass** (needs
`shotglass = "0.1.5"`). Blend passes come from libraries or film files, like
transitions:

```toml
[[layer]]
id = "glow"
scene = "layers/bright.glsl"    # a bright pass of the picture below
scale = 0.5
opacity = 0                     # adds nothing itself...
channels = ["@input"]

[[layer]]
id = "bloom"
scene = "layers/blur.glsl"      # ...but this layer reads it, blurs it
channels = ["@glow"]
blend = "blend/screen"          # and screens it over the picture
```

A blend pass is a small GLSL file that writes `vec4 blend(vec2 uv)`, the new picture
(alpha is ignored), from:

| name | what |
|---|---|
| `getBase(uv)` | the picture below the layer |
| `getLayer(uv)` | the layer, premultiplied, its opacity applied (alpha = coverage × opacity) |
| `opacity` | the layer's opacity (fades included) |
| `ratio` | width / height |

```glsl
// fx/multiply.glsl
vec4 blend(vec2 uv) {
  vec4 b = getBase(uv);
  vec4 l = getLayer(uv);
  vec3 m = b.rgb * l.rgb / max(l.a, 1e-5);   // unpremultiply, multiply
  return vec4(mix(b.rgb, m, l.a), 1.0);      // by coverage × opacity
}
```

A layer's channels can read other passes' pictures:

| channel | is |
|---|---|
| `"@input"` | the picture below this layer (everything under it, composited) |
| `"@picture"` | the picture track, before any layer (for a picture-in-picture, a clean plate) |
| `"@<id>"` | an earlier layer's own picture, at its own size, before it's composited and whatever its opacity: a layer at `opacity = 0` is a hidden helper pass. Black, with resolution 0, while that layer isn't active |

Layer IDs in declarations and channel references use one or more ASCII letters,
digits, underscores, or hyphens. For example, a later layer can read
`id = "bright-pass"` with `channels = ["@bright-pass"]`. The names `@input` and
`@picture` keep their meanings above.

A layer can read only layers below it (earlier in `movie.toml`), so passes never loop.

## Comps

A **comp** is a sequence inside the film: shots, transitions and layers of its own,
rendered to a picture that a shot, a layer, a channel or a matte uses (needs
`shotglass = "0.1.7"`). Declare comps in `movie.toml`; each lives in its own file:

```toml
[comps]
intro = "comps/intro.toml"                                    # full output size
tv = { file = "comps/tv.toml", size = [320, 180], loop = true }

[[shot]]
id = "open"
comp = "intro"            # the whole shot is the comp (instead of scene / type)
duration = 4.0

[[shot]]
id = "room"
scene = "scenes/room.glsl"
duration = 6.0
channels = ["@tv", { source = "@intro", time_offset = 1.0, filter = "linear" }]

[[layer]]
id = "pip"
comp = "tv"               # a layer whose picture is the comp
to = "room"
```

A comp file holds `[[shot]]`, `[[layer]]` and `[channels]`-style entries like a film,
nothing else (no `title`, `fps`, sound, `[comps]`, …: those are the film's). Its paths
are relative to the comp file. Shaders in a comp see:

- **Time:** the comp's own: `iMovieTime` and `iFrame` are comp-local, from 0 at the
  item that uses it (a comp shot or layer starts at its own start; with
  `time_offset`/`time_scale` as usual). A channel `@<comp>` follows the reading shot's
  `iTime` plus the entry's `time_offset`; a matte follows its layer. Past the comp's
  end it holds its last frame (before 0, its first), or wraps with `loop = true`.
- **Cues:** the film's cue defines, plus `COMP_DURATION` and `COMP_CUE_<SHOT>_START` /
  `_END` (and its layers') for the comp's own items.
- **Size:** `size = [w, h]` renders the comp at that size (scale 1, whatever the
  player's render scale); without it a comp shot or channel is the output size and a
  comp layer the layer's size.

Comps are picture only: a comp shot's `sound` is refused, and comps can use other comps
up to 4 deep (never themselves). A comp's picture is opaque. Footage inside a comp
follows the comp's frames, so `still`, `export` and playback show the same source frame.
A frame shows only when everything in every comp it uses is ready, and a broken edit
inside a comp keeps that item's last working version.

## Mattes

A **matte** cuts a layer out by another pass's alpha or brightness (needs
`shotglass = "0.1.7"`):

```toml
[[layer]]
id = "shape"
scene = "layers/shape.glsl"
opacity = 0                                  # a helper: not drawn itself

[[layer]]
id = "sky"
scene = "layers/sky.glsl"
matte = { source = "@shape" }                # alpha matte (default)
# matte = { source = "@letters", channel = "luma", invert = true }   # a comp, by luma, inverted
```

`source` is an earlier layer (`@<id>`) or a comp (`@<comp>`). `channel` is `"alpha"`
(default) or `"luma"` (Rec. 709 luma of the source's colour); `invert = true` keeps the
outside. The matte multiplies the layer's coverage in the composite, with opacity, so
it works with blend passes too, and `@<id>` of a matted layer is the layer before its
matte. While a matte layer isn't active the matte is empty and the matted layer hides
(inverted: shows whole).
