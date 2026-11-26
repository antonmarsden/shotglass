# Channels: textures, footage and text

What a shot can bind to `iChannel0..7`: images, image sequences (footage) and laid-out text, plus getting fonts.

[README](../README.md) · [Movies](movies.md) · [Channels](channels.md) · [Layers and blends](layers.md) · [Sound](sound.md) · [Loop films](loop-films.md) · [The player](player.md) · [Projects and libraries](libraries.md) · [Testing GLSL and quality metrics](testing-and-quality.md) · [Command line and export](cli.md) · [Testing shotglass](testing.md)

## Textures

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

### Channel sources

shotglass tells what an entry is from how it's written, the same way everywhere:

| written | is |
|---|---|
| `""` | nothing: the channel reads black |
| `"brick"` (no `.`, `/` or `<`) | a name from `[textures]` |
| `"textures/brick.png"`, `"<kit/textures/noise.png>"` | an image (`.png`, `.jpg`, `.jpeg`) |
| `"renders/fog_####.png"` (one run of `#` in the file name) | an image sequence, `#` = frame number (see [Footage](#footage)) |
| `"footage/surf.mp4"` (`.mp4`, `.m4v`, `.mov`, `.mkv`, `.webm`, `.avi`) | a video (not yet: footage phase 2) |
| `"@input"`, `"@picture"`, `"@title"` | on a [layer](layers.md#layers): another pass's picture |
| `"@intro"` (a `[comps]` name) | a [comp](layers.md#comps)'s picture, at the reader's `iTime` |
| `{ file = "…", … }` | an image with options |
| `{ frames = "…", … }` | an image sequence with options |
| `{ texture = "plate", in = 2.0, … }` | a `[textures]` image sequence, re-timed |
| `{ text = "title", … }` | a `[texts]` entry with sampling options (`filter`, `wrap`, `vflip`) |
| `{ source = "@intro", time_offset = 1.0, … }` | a comp, shifted in time, with `filter` (no `"mipmap"`) and `wrap` |

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

## Footage

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

## Text

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
title = { text = "My Short", font = "display", size = 160 }
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

### Getting fonts

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
- Shaping (kerning, ligatures, Arabic joining, marks) is HarfBuzz's, via HarfRust;
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
