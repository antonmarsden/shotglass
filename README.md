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
├── movie.toml          the edit: shots, timings, uniforms, transitions
├── shotglass.toml      the project: engine contract and libraries (see "Projects")
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

Big Shadow is a project of its own: its `shotglass.toml` sits next to `movie.toml`
and names the engine contract and the [libraries](#libraries) it uses:

```toml
shotglass = "0.1"             # optional: the engine contract the film was made for
[libraries]
kit = { source = "https://github.com/antonmarsden/shotglass-kit", subdir = "core" }
```

`movie.toml`:

```toml
title = "Big Shadow"
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
| `iChannel0..7`, `iChannelResolution[8]` | images, text and image sequences from the shot's `channels` (see [Textures](#textures)); an unbound channel reads black and its resolution is 0. Shadertoy code uses 0–3; 4–7 are shotglass's |
| `iChannelTime[8]` | an image sequence's source time of the frame shown, in seconds (see [Footage](#footage)); 0 for other channels |
| `iChannelMeta[8]` | a text channel's metrics in texels: (em, first baseline from the top, line pitch, padding); 0 for images (see [Text](#text)) |
| your `uniforms.*` | declared automatically from `movie.toml` |
| `CUE_<ID>_START`, `CUE_<ID>_SETTLED`, `CUE_<ID>_END`, `MOVIE_DURATION` | `#define`s from the edit (`SETTLED` = the incoming transition has finished) |

`#include "file.glsl"` works relative to the including file, then relative to the movie folder.
`#include <name/file.glsl>` takes `file.glsl` from the library the film calls `name`
(see [Libraries](#libraries)). Each file is included at most once per shader, so
shared helpers can include each other freely.

### Textures

A shot can bind up to eight PNG or JPEG images (or [text blocks](#text)) to
`iChannel0..7`; Shadertoy has four, so pasted code uses 0–3 (needs
`shotglass = "0.1.1"` in `shotglass.toml`, and `"0.1.3"` for channels 4–7):

```toml
channels = ["noise"]            # film-wide default for shots without their own (before any table)

[textures]                      # named once, used by any shot
noise = "<kit/textures/noise-rgba-256.png>"   # from the library the film calls "kit"
brick = "textures/brick.png"                  # from the film folder
grid  = { file = "textures/grid.png", filter = "nearest" }
moon  = { file = "textures/moon.jpg", wrap = "clamp", srgb = true }

[[shot]]
id = "wall"
scene = "scenes/wall.glsl"
duration = 9.0
channels = ["brick", "noise"]                 # iChannel0 = brick, iChannel1 = noise

[[shot]]
id = "sky"
scene = "scenes/sky.glsl"
duration = 6.0
channels = ["", "noise", "textures/stars.png", { file = "textures/logo.png", wrap = "clamp" }]
```

Each entry of `channels` is a name from `[textures]`, a path in the film folder, a
`<library/path>` (pinned by `shotglass.lock` like the library's GLSL), a table
`{ file = "…", … }` with options, or `""` (black). A shot's `channels` replaces the
film-wide default entirely; `channels = []` binds nothing.

#### Channel sources

shotglass tells what an entry is from how it's written, the same way everywhere:

| written | is |
|---|---|
| `""` | nothing: the channel reads black |
| `"brick"` (no `.`, `/` or `<`) | a name from `[textures]` |
| `"textures/brick.png"`, `"<kit/textures/noise.png>"` | an image (`.png`, `.jpg`, `.jpeg`) |
| `"renders/fog_####.png"` (one run of `#` in the file name) | an image sequence, `#` = frame number (see [Footage](#footage)) |
| `"footage/surf.mp4"` (`.mp4`, `.m4v`, `.mov`, `.mkv`, `.webm`, `.avi`) | a video (not yet: footage phase 2) |
| `"@input"`, `"@picture"`, `"@title"` | on a [layer](#layers): another pass's picture |
| `{ file = "…", … }` | an image with options |
| `{ frames = "…", … }` | an image sequence with options |
| `{ texture = "plate", in = 2.0, … }` | a `[textures]` image sequence, re-timed |
| `{ text = "title", … }` | a `[texts]` entry with sampling options (`filter`, `wrap`, `vflip`) |

Names come from `[textures]` and `[texts]`, which share one namespace. Videos are
recognised so a film that uses one gets a clear message (they arrive with footage
phase 2; convert a clip to an image sequence meanwhile), instead of an unrelated error.

| option | values | default |
|---|---|---|
| `filter` | `"mipmap"` (trilinear), `"linear"`, `"nearest"` | `"mipmap"` |
| `wrap` | `"repeat"`, `"clamp"`, `"mirror"` (both axes) | `"repeat"` |
| `vflip` | `true`: upright when sampled with `fragCoord / iResolution.xy` | `true` |
| `srgb` | `true`: `texture()` returns linear values (the GPU decodes sRGB) | `false` |

The defaults are Shadertoy's, so pasted shaders look the same. `iChannelResolution[n]` is
`vec3(width, height, 1.0)` of the image as stored. Leave `srgb` off unless the shader
encodes its own output (ends with something like `pow(col, vec3(1.0 / 2.2))`): shotglass
writes the shader's numbers to the screen as they are, so a linear texel written straight
out looks too dark.

- Images are decoded to 8-bit RGBA by the same code everywhere (16-bit images are rounded,
  with a warning; colour profiles are ignored). The GPU makes the mipmaps.
- At most 4096×4096 per image (`check --web` warns above 2048, the limit on some phones),
  and 256 MB of texture memory per shot.
- Textures load in the background ahead of the playhead, like shader compiles: while
  playing, a shot whose textures aren't uploaded yet keeps the previous frame up, so you
  only ever see frames that `still` and `export` would render.
- Save an image while the player is open and it reloads, without recompiling anything. A
  half-saved or broken image keeps the last working version on screen (the error shows);
  a missing file refuses the edit, like a missing `#include`.
- `check` decodes every texture (also with `--no-gpu`), prints sizes and memory, and warns
  when a scene reads an `iChannel` its shot binds nothing to.

### Footage

An image sequence is a channel whose picture changes with time: one PNG or JPEG per
frame, from Blender or another renderer, a matte, or a shotglass `export --format png`
(needs `shotglass = "0.1.6"`). Write the frame number as one run of `#` in the file name:

```toml
[textures]
fog   = "renders/fog_####.png"                          # fog_0001.png, fog_0002.png, …
matte = { frames = "renders/matte_####.png", fps = 24, first = 1001 }
grain = { frames = "<kit/grain/grain_##.png>", clock = "movie", end = "loop" }

[[shot]]
id = "beach"
scene = "scenes/beach.glsl"
duration = 6.0
channels = ["fog", "matte", "grain"]

[[shot]]
id = "beach-again"                                       # the same frames, later and slower
scene = "scenes/beach.glsl"
duration = 4.0
channels = [{ texture = "fog", in = 4.0, speed = 0.5 }, "matte"]
```

| option | values | default | meaning |
|---|---|---|---|
| `fps` | a number or `"num/den"`, like the film's `fps` | the film's | source frames per second |
| `first` | a whole number | the lowest found | the file number that is the sequence's frame 0 (`1001` for a Blender range) |
| `in` | seconds ≥ 0 | `0` | the source time shown when the channel's clock is 0 |
| `in_frame` | a whole number ≥ 0 | | the same, as a source frame (instead of `in`) |
| `speed` | ≥ 0 | `1` | source seconds per second; `0` holds the `in` frame |
| `clock` | `"shot"`, `"movie"` | `"shot"` | 0 at the shot's (or layer's) start, or at the film's: then one sequence runs on across cuts, like the music |
| `end` | `"hold"`, `"loop"`, `"black"` | `"hold"` | past the last frame: keep it, start again, or read black |

`filter`, `wrap`, `vflip` and `srgb` work as for images, but footage defaults to
`filter = "linear"`: no mipmaps to make for every frame (`"mipmap"` works, at a cost).

- `{ texture = "fog", … }` re-times a `[textures]` sequence; only the timing options
  (`in`, `in_frame`, `speed`, `clock`, `end`) may follow it.
- Film frame n shows the source frame whose interval contains that moment, computed
  exactly: 24 fps frames in a 30 fps film repeat one frame in five, equal rates (29.97
  too) are frame for frame. Footage timing doesn't follow `time_offset`/`time_scale`.
  `iChannelTime[n]` is the source time of the frame shown, and `iChannelResolution[n]`
  the sequence's size (also while it reads black).
- The frames must be contiguous from `first` (a gap is an error naming the missing
  frames) and all the same size. Frames that aren't there yet (a render in progress)
  just make the sequence shorter. `fog_####.png` matches exactly four digits, or more
  when the number needs them (`fog_10000.png`), as Blender writes. Names are
  case-sensitive everywhere.
- Frames load on background threads ahead of the playhead, within the 256 MB texture
  budget (a shot needs room for at least three frames of each sequence). While playing, a
  frame shows only when every sequence's exact frame is uploaded, otherwise the previous
  frame stays up: playback never shows a frame that `still` or `export` wouldn't. The next
  shot's first frames load early, so cuts and a loop film's seam don't stall.
- For speed, use JPEG (quality 90–95) for plates: it decodes several times faster than
  PNG. Use PNG for mattes and anything with alpha (straight, as stored). A 1080p frame is
  8 MB on the GPU. The Performance panel shows the frames on the GPU and how often
  playback waited for one.
- Re-render frames while the player is open and they reload (after 300 ms without
  changes). A half-written frame keeps that frame's last working version on screen; a
  frame missing in the middle refuses the edit, like a missing `#include`.
- `check` lists each sequence (frames, size, rate, length, memory per frame) and decodes
  its first frame; `check --deep` decodes every frame. It warns when a shot runs past
  the end of its sequence with `end = "hold"`, and when an `end = "loop"` sequence on the
  movie clock doesn't fit a loop film's period a whole number of times.
- Video files come with footage phase 2. Meanwhile convert a clip to a sequence:
  `ffmpeg -i clip.mp4 frames/clip_%05d.png`, then `"frames/clip_#####.png"`.

### Text

Text is a texture: name fonts in `[fonts]` and text blocks in `[texts]`, and bind a block
to a channel like an image (needs `shotglass = "0.1.3"`):

```toml
# Google Fonts families, added with `shotglass fonts add` (see "Getting fonts")
[fonts]
body = "<inter/Inter[opsz,wght].ttf>"
bold = { file = "<inter/Inter[opsz,wght].ttf>", variations = { wght = 700 } }
display = { file = "<bungee/Bungee-Regular.ttf>", fallback = ["body"] }
arabic = "<notonaskharabic/NotoNaskhArabic[wght].ttf>"
hand = { file = "fonts/MyHand.otf", licence = "OFL-1.1" }   # a font file in the film folder

[texts]
title = { text = "Big Shadow", font = "display", size = 160 }
tagline = { text = "a tiny mouse, a tealight,\nand a lot of wall", font = "body", size = 72, width = 1400, align = "center" }
hello = { text = "مرحبا بالعالم", font = "arabic", size = 96 }
credits = { file = "credits.txt", font = "body", size = 48, align = "center" }

[[shot]]
id = "titlecard"
scene = "scenes/title.glsl"
duration = 4.0
channels = ["title", "tagline"]          # iChannel0 = title, iChannel1 = tagline
```

shotglass lays the text out and rasterises it once, on the CPU, into a texture whose
**red and alpha** are the antialiased coverage (so `texture(iChannel0, uv).a` is the
text) and whose **green** is a signed distance field: 0.5 on the outline, rising inside.
The distance field stays crisp when a shader scales the text up, and makes outlines,
glows and shadows cheap:

```glsl
float d = texture(iChannel0, uv).g;               // 0.5 at the edge
float w = fwidth(d) * 0.75;
float ink = smoothstep(0.5 - w, 0.5 + w, d);      // crisp at any scale
float outline = smoothstep(0.42 - w, 0.42 + w, d) - ink;
```

What text *looks like* (placement, colour, outlines, fades) is up to the shader, or to a
library such as shotglass-kit's text helpers; the engine only supplies the pixels and the
metrics. `iChannelMeta[n]` gives a text channel's em size, first baseline (from the top),
line pitch and padding, in texels, so blocks can be sized and aligned consistently.
Changing the words or the style makes a new texture and never recompiles a shader.

| `[texts]` option | values | default |
|---|---|---|
| `text` / `file` | the words, or a UTF-8 file of them (one is required); `\n` starts a new line | – |
| `font` | a name from `[fonts]` (required) | – |
| `size` | the em in texture pixels, 4–1024: rasterise as large as it appears on screen | 64 |
| `width` | wrap width in pixels; without it lines break only at `\n` | none |
| `align` | `"left"`, `"center"`, `"right"`, `"start"`, `"end"` (start = left for left-to-right text, right for right-to-left) | `"start"` |
| `line_height` | line pitch in ems | 1.2 |
| `letter_spacing` | extra space after each letter, in ems | 0 |
| `max_lines` | more lines than this is an error (a title that silently wraps is a bug) | none |
| `direction` | `"auto"` (from the first strong letter of each paragraph), `"ltr"`, `"rtl"` | `"auto"` |
| `features` | OpenType features: `["ss01", "-liga", "tnum"]` | none |
| `sdf_spread` | distance-field reach in pixels, also the padding around the text | 8 |
| `filter`, `wrap`, `vflip` | as for images | `"linear"`, `"clamp"`, `true` |

#### Getting fonts

Any [Google Fonts](https://fonts.google.com) family is a library: declare it by name in
the project and name its files in `[fonts]`, like any library file.

```sh
shotglass fonts add "Lobster" "Inter" --film movies/my-film
```

writes both for you, fetches the families and pins them:

```toml
# shotglass.toml
[libraries]
lobster = { google = "Lobster" }
inter = { google = "Inter" }

# movie.toml
[fonts]
lobster = "<lobster/Lobster-Regular.ttf>"
inter = "<inter/Inter[opsz,wght].ttf>"
inter_italic = "<inter/Inter-Italic[opsz,wght].ttf>"
```

- They behave exactly like libraries: any command fetches what's missing (git, only that
  family's folder of google/fonts), `shotglass.lock` pins the commit and a hash of the
  files so every machine gets the same glyphs, `shotglass update inter` moves the pin on
  purpose, `SHOTGLASS_LIB_INTER` points at a local copy, and `freeze`/`bundle` copy in
  exactly the font files a film uses, with their licence. Commit `shotglass.lock`;
  there's no need to commit font files.
- Entries are named after the family, plus the weight and `_italic` for other styles;
  `fonts add` prints a variable font's axes, for `variations`. Rename entries as you like.
- Licences come from where Google Fonts keeps the family: nearly all SIL OFL 1.1, a few
  Apache 2.0 or the Ubuntu Font Licence; all allow using the fonts in a film and passing
  them on unmodified. `shotglass licenses` lists each family with its licence.
- Any other TTF/OTF works too: put it in the film (or a library) and name it in
  `[fonts]`, with its `licence`.

| `[fonts]` option | values |
|---|---|
| `file` | a `.ttf`, `.otf` or `.ttc` in the film or `<library/path>` (pinned by `shotglass.lock`) |
| `index` | the face in a `.ttc` collection (default 0) |
| `variations` | fixed axis values of a variable font: `{ wght = 700, wdth = 90 }` |
| `fallback` | fonts to try, in order, for letters this one doesn't have |
| `licence` | the font's SPDX licence (`"OFL-1.1"`), for a font in the film folder |

- Fonts come only from the film or its libraries, never from the system, and are not
  hinted, so text looks the same on every machine and in `export`.
- Shaping (kerning, ligatures, Arabic joining, marks) is HarfBuzz's, via rustybuzz;
  mixed left-to-right and right-to-left text is ordered per line (Unicode bidi).
- Each letter uses the first font in the fallback chain that has it. A letter no font
  has is an error that names it (`check` and the player refuse the edit), as are a text
  over `max_lines` and a block larger than 4096 pixels on a side.
- Texts lay out on the texture thread like images decode, so playback never waits for
  them; save the text file or a font and the block reloads.
- `check` lists every block's size, lines and fonts, and warns about film fonts without
  a `licence`.

Shadertoy's built-in textures (its noise, "Organic", "Abstract" and so on) aren't licensed
for redistribution, so shotglass doesn't ship them. Use your own images, CC0 ones (e.g.
from Poly Haven or ambientCG), or the textures in shotglass-kit once they're released.
Commit images with the film; `.gitattributes` with `*.png binary` keeps git from touching
them (`shotglass new` writes one). A Git LFS pointer committed by mistake gets its own
error message.

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

### Layers

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

#### Blend modes and reading other layers

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
Comps (sequences inside a layer) and mattes come next (see
`docs/design/layers-compositing.md`).

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
Reads outside that range return silence. For ready-made effects (multi-tap delay,
reverb, smoothed ducking, a look-ahead limiter and soft clip), include shotglass-kit's
`<sound/mix.glsl>`, as the demo's `mix.glsl` does.

Sound is synthesised in chunks of about 0.7 s, just ahead of the playhead (see
[Long films](#long-films)). Each chunk of each track is cached by its source and
timing. Editing one shot's sound re-synthesises only that shot (and the mix), and
picture-only edits don't touch the sound. Until the new chunks are ready, the
previous version keeps playing. Recursive filters (resonant sweeps, feedback reverb) are the main
thing GLSL sound can't do cheaply. Multi-tap delays in the mix get you most of
the way.

### Loop films

`loop = true` makes the film play forever, like a Shadertoy: for screensavers,
installations or visuals behind music. The whole film repeats, and its duration is
the period. A single-shot film is the simplest loop. Needs `shotglass = "0.1.2"` in
`shotglass.toml`.

```toml
loop = true
fps = 30

[[shot]]
id = "forever"
scene = "scenes/forever.glsl"
duration = 8.0                  # the period: a whole number of frames (240 at 30 fps)
```

- **Seamless picture.** The period must be a whole number of frames, so the frame
  after the last is frame 0 (the error names the nearest valid durations). The seam itself is a hard cut from the last shot to the first: the first
  shot can't have a `transition`. For a seam you can't see, end on the image you
  start with. `iMovieTime / MOVIE_DURATION` runs from 0 to 1 once per loop, which
  makes that easy.
- **Time stays within one period.** `iTime`, `iMovieTime`, `iShotTime`, `iFrame` and
  the sound shaders' `time` and `samp` are exactly what the first loop gets, however
  long the film has played. So frame *n* of loop 1000 is frame *n* of loop 1, and
  float precision never runs out (a plain float clock would step in 1/64 s after
  36 hours). There's no loop counter for shaders: every loop is identical.
- **Seamless sound.** Music and shot sounds repeat with the period. The mix reads
  across the seam, so an echo from the end of the loop rings into its start (into the
  first play too: every loop sounds the same). The engine doesn't crossfade the music
  at the seam. Write it so it repeats: make every frequency a whole number of cycles
  per period, or fade it at both ends. `check` warns when the sound jumps at the seam.
  A `sound_lead` on the first shot or a `sound_tail` on the last would cross the seam
  and are refused.
- **In the player,** "repeat movie" starts on for loop films, and playback runs across
  the seam without a gap (picture and sound). Turn it off to stop at the end. The
  timeline shows one period, and the transport counts the loops.
- **Offline,** `still --at` and `strip --at` take times modulo the period (`--at 30`
  on an 8 s loop is 6 s), `wav` writes one period, and `export` writes one period, or
  N with `--loops N`. Because the mix wraps, a single period loops seamlessly when the
  file itself loops (a looping video or GIF; lossy audio codecs add padding, so for a
  sample-exact audio loop use the WAV or FLAC).

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
  panel reports UI update FPS, the film's target FPS, and the rate of successful
  film renders. The purple trace shows the interval between film renders against
  the film's target interval. Skipped frames are counted between successful
  renders during continuous playback. The panel splits stalls into sound and
  compiling, and shows where the worst sound
  stall's time went, down to the GL call. Ticks under its frame graph mark the
  frames that rendered a film frame, queued or collected sound, or compiled;
  hover a frame for its numbers.
- **Image sequences stream ahead of the playhead.** Frames decode on a few worker
  threads and upload in bands, half a second ahead (and the next shot's first frames),
  with their own upload budget, so a still texture never waits behind them nor they
  behind it. A frame whose image isn't uploaded yet is a stall, not a drop: it doesn't
  lower the render scale.
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

## Projects

A project is a folder with a `shotglass.toml`: the engine contract and the
[libraries](#libraries) for every film in it. A film's project is the nearest folder at
or above the film that has one. `shotglass new` makes a film that is a project of its
own (`shotglass.toml` next to `movie.toml`); a film with no `shotglass.toml` anywhere
above it is one too, with no libraries.

For several films that share code, put `shotglass.toml` in the folder above them:

```
my-films/
├── shotglass.toml          shotglass = "0.1", [libraries] for every film
├── shotglass.lock          one lock: every film uses the same pins
├── shotglass.local.toml    uncommitted overrides, e.g. a local kit checkout
├── lib/                    library.toml, …: GLSL shared by the films
├── first-film/movie.toml   films: folders with a movie.toml, no [libraries]
└── second-film/movie.toml
```

```toml
# my-films/shotglass.toml
shotglass = "0.1"
[libraries]
kit   = { source = "https://github.com/antonmarsden/shotglass-kit", subdir = "core" }
films = { path = "lib" }    # relative to the project folder
```

- `check`, `licenses`, `fetch` and `update` take the project folder too: `shotglass
  check my-films --web --no-gpu` checks every film (the root, if it has a `movie.toml`,
  and every folder directly under it that has one) and fails if any fails. `update`
  moves the pins once for all films.
- Commands for one film (`play`, `still`, `strip`, `wav`, `export`, `bundle`, `freeze`)
  take a film folder and refuse a project folder, naming its films.
- A finished film can be [frozen](#freezing-a-finished-film), so later changes to the
  project's libraries never change it.
- `movie.toml` has no `shotglass` or `[libraries]`; loading one that does says to move
  them to `shotglass.toml`.

## Libraries

Shared GLSL comes from versioned libraries that a project declares by name in its
`shotglass.toml` (see [Projects](#projects)). The engine provides only the contract
(uniforms, entry points, the transition interface, cue defines), so upgrading shotglass
doesn't change how a finished film looks or sounds.

```toml
# shotglass.toml
shotglass = "0.1"        # engine contract the films need; other engines refuse with a clear message

[libraries]
kit  = { version = "0.4", source = "https://github.com/antonmarsden/shotglass-kit", subdir = "core" }
sfx  = { version = "0.3", source = "https://github.com/antonmarsden/shotglass-kit", subdir = "sounds" }
toon = { version = "1.2", source = "https://github.com/someone/toon-kit" }
edge = { source = "https://github.com/someone/fx-kit" }   # no version: follow the default branch
mine = { path = "lib/mykit" }            # a library inside the project folder
lobster = { google = "Lobster" }         # a Google Fonts family (see "Getting fonts")
```

```glsl
#include <kit/noise.glsl>
#include <toon/outline.glsl>
```

- **The project chooses the name.** The include prefix is the local name from
  `[libraries]`, so two publishers' libraries can both be called "kit" without clashing.
  Every film of the project uses the same names.
- **`version`** is a semver range (`"0.4"` means `>=0.4.0, <0.5.0`, as in Cargo),
  matched against the source's git tags (`v0.4.2` or `0.4.2`).
- **No `version`** follows a branch instead of releases: the repository's default
  branch, or `branch = "name"`. Fetching pins the branch's newest commit in
  `shotglass.lock`, so the film still doesn't change until `shotglass update`. Handy
  for a library that has no releases yet; prefer a version once it does.
- **`source`** is the https URL of a git repository. **`subdir`** picks a folder of
  it as the library root, so one repository can publish several libraries that a film
  pins separately. Every folder shares the repository's tags.
- **`path`** is a library inside the project folder, relative to it. It may not climb
  above it.
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
project calls them. A peer requirement means the project must list that library itself;
shotglass checks the versions and says what's missing, but never adds anything.

A library with `glsl = 430` only loads in a film that opted in to at least that version
(`glsl = "430"` in `movie.toml`); otherwise loading the film says which library needs it.
Such a film needs a driver with that GLSL and won't play on the web.

### Fetching and shotglass.lock

```sh
shotglass fetch  <film|project>           # download what's missing, write shotglass.lock
shotglass update <film|project> [name..]  # move pins to the newest version in range (or commit)
```

`fetch` (also run automatically by `play`, `still`, `strip`, `check` and `wav` when
something isn't downloaded yet) resolves each library to an exact version and writes
`shotglass.lock` next to the project's `shotglass.toml` (one lock for all its films):
version, source, tag, `subdir`, and a sha256 of the library's files (for a followed
branch, `tag` is the ref and `rev` the commit). **Commit it.** A new library release never
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

Point a library at a working copy without putting machine paths in the project:

```sh
SHOTGLASS_LIB_KIT=../shotglass-kit/core shotglass play movies/my-film
```

or put an uncommitted `shotglass.local.toml` next to `shotglass.toml` (`shotglass new`
adds it to `.gitignore`). One file covers every film of the project:

```toml
[libraries.kit]
path = "C:/src/shotglass-kit/core"
```

The variable is `SHOTGLASS_LIB_` plus the local name in capitals, with `-` as `_`.
It wins over `shotglass.local.toml`. Relative override paths are relative to the project
folder. The version range in `shotglass.toml` still has to match the working copy's
`library.toml`. Live reload watches library folders, overrides included. Overrides never
apply to a [frozen](#freezing-a-finished-film) film.

### Licences

`licences` in `library.toml` takes SPDX identifiers. To see what a film actually uses:

```sh
shotglass licenses <movie>
```

It lists every library the film includes something from (local name, canonical name,
version, licences, and where the files come from: source and pin, a folder in the film,
or an override) and flags non-commercial licences (any `-NC` one, e.g.
`CC-BY-NC-SA-4.0`): a film that includes one can't be used commercially. Libraries in
`[libraries]` that nothing includes are named separately, and so are libraries that
declare no licence.

### Freezing a finished film

```sh
shotglass freeze   <film>     # copy exactly the library files the film includes into it
shotglass unfreeze <film>     # follow the project's libraries again
```

In a project of several films, a finished film would otherwise change whenever the
project's libraries do: a new kit pin, an edit in a shared `lib/`. `freeze` copies
exactly the files the film includes from each library (every `#include`d file and
library texture), its `library.toml` and its licence files (`LICENSE*`, `LICENCE*`,
`COPYING*`, `NOTICE*`, from the library folder and, for a `subdir`, the repository above
it) into `<film>/shotglass-libs/<name>/`, and writes a `shotglass.toml` marked frozen
and a `shotglass.lock` with the sha256 of each copy. The film is then a project of its
own: it loads with no project, network, cache or overrides, and never changes. An edited
frozen file is an error, like an edited cache. Commit the three with the film.

`update` refuses a frozen film. `unfreeze` removes the three, and the film follows its
project again (it refuses if the project can no longer load the film, before removing
anything). A one-film project has nothing to freeze: its `shotglass.lock` already pins
everything. Use `bundle` to host it.

### Bundling for hosting

```sh
shotglass bundle <film> --out site/my-film
```

writes a self-contained copy of the film to a new (or empty) folder outside it: the film
folder (without hidden files, project, lock and override files), plus the same frozen
libraries, `shotglass.toml` and `shotglass.lock` that `freeze` writes, and
`LIBRARY-LICENSES.md`, which lists each library's licences and licence files. The bundle
loads with no project, network, cache or overrides, and never changes. `movie.toml` is
unchanged. A library folder inside the film folder travels with the film as it is.

### Portable paths

The same `movie.toml` has to work on every OS and in a future web player, so paths in
it use forward slashes and stay inside the film folder (paths in `shotglass.toml`,
inside the project folder): no absolute paths, drive
letters, backslashes or `..` that climbs out. URLs only go in `source`. File names are
case-sensitive: `check` (and every command) warns when a name only matches a file by
ignoring case, which works on Windows and macOS but not on Linux or the web.

## Testing GLSL

Golden strips show *that* a film changed. To pin down what a function promises (its
endpoints, range, identities, continuity) write tests in GLSL and run them on the GPU:

```glsl
// tests/unit/animate.test.glsl
#include <kit/animate.glsl>
#include <kit/color.glsl>

void test_back_out_endpoints() {                 // no parameter: runs once
    expect_near(kit_ease_back_out(0.0), 0.0, 1e-6);
    expect_near(kit_ease_back_out(1.0), 1.0, 1e-6);
}

void test_bounce_in_range(float t) {             // property: t sweeps [0, 1]
    float y = kit_ease_bounce_out(t);
    expect(y >= 0.0 && y <= 1.0 + 1e-6, y);
}

// samples: 262144
void test_srgb_round_trip(vec3 c) {              // property: c sweeps the unit cube
    expect_near(kit_srgb_decode(kit_srgb_encode(c)), c, 1e-5);
}
```

```
$ shotglass test tests/unit
tests/unit/animate.test.glsl
  ok    test_back_out_endpoints
  FAIL  test_bounce_in_range (4096 samples)
        tests/unit/animate.test.glsl:11: expect(y >= 0.0 && y <= 1.0 + 1e-6, y)
        at t = 0.3637: got 1.0213
        12 of 4096 samples failed
  ok    test_srgb_round_trip (262144 samples)

error: 2 passed, 1 failed (266241 samples) on llvmpipe (LLVM 20.1.2, 256 bits)
```

- `shotglass test [file|folder] [filter]` runs every `*.test.glsl` file at or under the
  folder (default `.`; hidden folders, `target/` and `shotglass-libs/` are skipped), or
  one file. The filter keeps only tests whose name contains it. It exits non-zero if
  anything fails.
- A test file is assembled like a scene, with the libraries of its
  [project](#projects): the nearest `shotglass.toml` at or above the file. Libraries
  come from the lock, overrides and the cache as for films (missing ones are fetched).
  Keep test files **outside** library folders (e.g. `tests/unit/` next to them): a file
  inside a library ships with it and changes its sha256.
- **Tests** are top-level `void test_<name>()` functions, or property tests taking one
  `float`, `vec2` or `vec3`. A property test's parameter sweeps a regular grid over
  `[0, 1]` per component, both ends included; map it to the range you need
  (`mix(-2.0, 2.0, t)`). Samples: 4096 for a `float`, 256² for a `vec2`, 64³ for a
  `vec3`, or `// samples: N` on the line just above the function (up to 4 194 304; a
  grid rounds up to a whole number of points per axis). Test names must be unique.
- **Each test reports its first failure**: the call as written, with `file:line`, the
  input of the lowest failing sample, the values, and how many samples failed. A test
  where no assertion ran on any sample fails ("ran no assertions"), so an early `return`
  can't hide one. Some samples skipping their assertions is fine.
- **Strict first:** each file must pass the strict GLSL check (as GLSL 330 and as GLSL ES
  3.00, or at the version a library needs) before it runs on the driver. A file that
  doesn't assemble, validate or compile is reported with `file:line`, and the other
  files still run.
- Tests run on the real driver, so they catch driver differences too: run them on your
  GPU as well as in CI (software OpenGL).

### Assertions

The engine provides these to test files only (never to films); they are part of the
engine contract (semver). `T` is `float`, `vec2`, `vec3` or `vec4`, and comparisons are
per component.

| assertion | passes when |
|---|---|
| `expect(bool cond)` | `cond` |
| `expect(bool cond, T value)` | `cond`; shows `value` on failure |
| `expect_eq(T a, T b)` | `a == b` exactly; also `int`, `ivec2`–`ivec4`, `uint`, `uvec2`–`uvec4` and `bool` (integers are reported exactly, whatever their size) |
| `expect_near(T a, T b, float tol)` | `abs(a - b) <= tol`, and both are finite |
| `expect_rel(T a, T b, float rel)` | `abs(a - b) <= rel * max(abs(a), abs(b))`, and both are finite |
| `expect_finite(T x)` | no NaN or infinity |

NaN never passes `expect_near`, `expect_rel` or `expect_finite`. Call assertions in the
test file itself (in a test or in a helper function there): the engine numbers each call
before compiling, which is how a failure names the exact call, even with several on one
line. Names starting with `sgtest_` are reserved in test files.

## Command line

```sh
shotglass play   <movie> [--at T] [--scale auto|0.1..1.0]
shotglass still  <movie> --at 3.5,moon,closeup+2 [--size 1280x720] [--out stills/]
shotglass strip  <movie> [--count 12 | --at ...] [--size 384x216] [--cols 4] [--out strip.png]
                 [--compare ref.png [--bless] [--tolerance 8] [--budget 0.1%] [--max-mean 0.25]]
                 # --compare: fail if the strip differs from ref.png (writes strip.diff.png)
shotglass check  <movie> [--web] [--no-gpu] [--deep] [--size 1280x720] [--frames 8]
                 # validate movie.toml and the GLSL (strictly), compile, synth, time
                 # --deep: also decode every frame of every image sequence
shotglass wav    <movie> [--out sound.wav] [--rate 48000] [--allow-silence]
                 [--compare ref.wav [--bless] [--max-rms 0.0015] [--max-peak 0.01]]
                 # --compare: fail if the sound differs from ref.wav (writes sound.diff.wav)
shotglass export <movie> --out film.webm [--size 1920x1080] [--format webm|av1|ffv1|png|ffmpeg] [--rate 48000]
                 [--loops N]                               # loop = true films: N periods
shotglass new    <dir>
shotglass fetch  <movie>                                   # download libraries, write shotglass.lock
shotglass update <movie> [library..]                       # move library pins to the newest allowed
shotglass licenses <movie>                                 # libraries the film includes, film fonts, and their licences
shotglass fonts add "Family" [..] [--film <dir>]           # Google Fonts families as libraries, plus [fonts] entries
shotglass bundle <movie> --out <dir>                       # self-contained copy with the library files it includes
shotglass freeze <movie>                                   # copy the library files it includes into the film
shotglass unfreeze <movie>                                 # follow the project's libraries again
shotglass test   [file|folder] [filter]                    # GLSL unit and property tests (*.test.glsl) on the GPU
shotglass selftest [filter]                                # GPU + sound checks on this machine
```

`still`, `strip`, `check`, `wav` and `export` render off-screen (`check --no-gpu` doesn't need OpenGL). On a Linux machine with
no display, run them under `xvfb-run`; Mesa's software OpenGL works.

For a film without music or shot sounds, `wav --allow-silence` writes a silent stereo
WAV for the full film duration at the requested sample rate. This path needs no
OpenGL and supports `--compare` and `--bless`.

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
  `duration × rate` samples starting at sample 0 (a [loop film](#loop-films)'s
  period, rounded to whole samples, once per `--loops`). FLAC and WAV keep it bit for
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
- **Golden images** (`tests/golden.rs`): contact sheets of the demo film, the
  `shotglass new` template, `tests/films/channels` (iChannel textures: mipmaps,
  wrap and filter modes, `vflip`) and `tests/films/loop` (a `loop = true` film over
  two periods) are compared with reference PNGs in `tests/golden/`, so
  a change that compiles but looks wrong fails the tests. A small per-pixel
  tolerance absorbs the differences between software OpenGL and real GPUs. To
  update the references on purpose, see [CONTRIBUTING.md](CONTRIBUTING.md#golden-images).
- **Golden soundtracks** (same file): the soundtracks of the demo, the template and
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

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md): build and test commands, golden images,
the PR checklist, licensing, and how to work on shotglass with AI coding agents.

## Licence

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT), at your option.

Shaders copied from shadertoy.com are usually licensed CC BY-NC-SA 3.0 by their
authors. If you include them in a movie, keep their licence notes with them.
