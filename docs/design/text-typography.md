# Design: text and typography

Status: **proposed** (design only; nothing here is implemented). Builds on
[iChannel textures](ichannel.md) (#22), whose texture store, decode worker, banded uploads
and `keys_ready` gating it reuses.

## Summary

Text becomes a new kind of texture source. A film names fonts in `[fonts]` and text blocks
in `[texts]`; a shot binds a text block to an `iChannel` exactly as it binds an image
(`channels = ["title"]`). The engine shapes and lays out the text on the CPU (rustybuzz,
unicode-bidi, unicode-linebreak, our own small line breaker), rasterises it on the decode
worker into an RGBA8 image whose channels carry both antialiased **coverage** and a
**signed distance field**, and uploads it through the existing banded, budgeted,
non-blocking texture path. Block metrics (em size, first baseline, line height, padding)
reach the shader through one new uniform array, `iChannelMeta[4]`, so editing the words
never recompiles a shader.

What text *looks like* (placement helpers, outlines, glows, shadows, fades, wipes, stagger)
is GLSL in a new shotglass-kit library, `text`. The engine adds no GLSL: it provides
pixels, metrics and a documented layout, which is contract, not looks.

The MVP (phase 1) covers title cards, lower thirds, short credits and block-level kinetic
type (keyframed position, scale, opacity, SDF effects). Phase 2 adds **glyph runs**: an
MSDF glyph atlas plus a glyph table, for per-glyph kinetic type, long credits rolls and
subtitles timed to the timeline. Phase 3 adds a film-level **overlay** pass so subtitles and
lower thirds sit above transitions without every scene including them.

Fonts are never taken from the system: only from the film folder or a library, so a
frame stays a function of (movie, t, size, scale) and library fonts are pinned by
`shotglass.lock`'s sha256 like library GLSL. The engine ships no fonts; the kit can ship
OFL-licensed ones in a `fonts` library, and `shotglass licenses`/`bundle` report and carry
them.

## Use cases

| use case | what it needs | phase |
|---|---|---|
| Title card ("Big Shadow", centred, fades in over the moon) | one block, crisp when scaled, keyframed opacity/scale | 1 |
| Lower third (name + role, slides in, holds, slides out) | two blocks with different fonts and sizes, aligned on a baseline; a plate drawn in GLSL | 1 |
| Outlined / glowing / shadowed type, type over busy footage | SDF so outline width and glow radius are shader parameters | 1 |
| Short credits (one screen, or a few pages cut between) | multi-line blocks, centring, line height, wrapping at a width | 1 |
| Long credits roll (hundreds of lines) | more text than one 4096² texture holds at a readable size | 2 |
| Kinetic type: letters drop in one by one, words pop on a beat (cues) | per-glyph index, word index, line index, glyph rect | 2 |
| Subtitles timed to the timeline (SRT/WebVTT) | many short blocks, each with start/end times, selected by `iMovieTime` | 2 |
| Subtitles above transitions, on every shot | a pass composited after the cut, independent of shots | 3 |
| Counters, timecode, dynamic strings | a glyph set (e.g. `0123456789:`) addressed by code from GLSL | 2 (glyph sets) |
| Colour emoji, vertical CJK, justification, hyphenation | — | later / not planned |

Timing comes from what films already have: keyframed `uniforms.*` (eases included), cue
defines (`CUE_<ID>_START/SETTLED/END`), `iShotTime`/`iShotProgress`/`iMovieTime`, and
markers. No new timing machinery is needed for phases 1 and 2.

## Representation

### Options

1. **Engine composites text over the frame** (Remotion/DOM style): the engine draws text
   after the scene. *Rejected*: it is looks in the engine (fonts placed and styled by
   Rust), shaders can't distort, mask, refract or light the text, and it can't sit
   *inside* a scene (text on a wall, under fog).
2. **CPU-rasterised block textures** (coverage only): simplest, reuses the image path
   unchanged. Blurry or blocky when a shader scales the block up (kinetic zooms, close-ups);
   outline and glow have to be faked with blurs.
3. **Block textures with coverage + SDF** (*recommended for phase 1*): the same block
   texture, but the green channel holds a signed distance. Crisp edges at several times
   magnification, outline/glow/shadow/bevel as cheap shader maths, and still one texture,
   one channel, the existing pipeline. Per-glyph animation isn't possible (the block is one
   picture), and SDF rounds sharp corners when magnified a lot.
4. **(M)SDF glyph atlas + glyph table** (*recommended for phase 2*): each distinct glyph
   once in an atlas (MSDF: sharp corners at any scale), and a table of glyph instances
   (position, atlas rect, cluster/word/line index). Unlimited text length, per-glyph
   transforms, glyph sets for dynamic strings. The shader has to *find* the glyph under a
   pixel (a fragment-only scene can't draw instanced quads), so it costs a search per pixel
   and needs more GLSL (kit), and two channels.
5. **GPU vector outlines** (Slug-style: quadratic curves in a texture, coverage solved per
   pixel): perfect at any scale, but heavy GLSL, heavy per-pixel cost on llvmpipe and
   phones, and IP status to check. *Not planned*; the glyph-table design leaves room for
   an `outlines` data texture later if a kit author wants it.
6. **Re-rasterise at output resolution every frame/scale change**: crisp, but rasterising
   on the render thread blocks playback, auto scale would thrash it, and it ties textures to
   the render size. *Rejected.*

Phases 1 and 2 are complementary, not competing: a block texture is the simple, cheap
answer for most titles; glyph runs are for type that moves letter by letter or doesn't fit.

### Block texture format (phase 1)

One RGBA8 image per text block, uploaded like any image (so `filter`, `wrap`, `vflip`
work as they do for images; `srgb` is refused for text: the channels are data):

| channel | contents |
|---|---|
| R | coverage, 0–1, antialiased (box filter of a 4×4 supersampled outline fill) |
| G | signed distance: 0.5 at the outline, +0.5 / `spread` per pixel inside, clamped |
| B | 0 (reserved) |
| A | coverage (same as R), so pasted code doing `texture(iChannel0, uv).a` just works |

- `spread` (default 8 px at the block's em size; option `sdf_spread`) is also the
  **padding** on every side of the content box, so linear filtering and the distance field
  never touch the texture edge. Defaults for text: `filter = "linear"`, `wrap = "clamp"`
  (an image's defaults are Shadertoy's `mipmap`/`repeat`, which suit text badly).
- The distance is computed by an exact Euclidean distance transform (Felzenszwalb–
  Huttenlocher, integer arithmetic on a supersampled coverage mask), not by per-curve
  float maths, so it is trivially identical on every platform including wasm.
- White text, no colour: colour, gradients and texture fills are the shader's job (the
  kit's helpers take a colour). This keeps one format and keeps looks out of the engine.

### Metrics: `iChannelMeta`

A shader needs a block's em size, first baseline and line height to size blocks
consistently and align them on baselines (lower thirds). Options considered:

- `#define TEXT_<NAME>_*` (like cue defines): changes program source, so editing a block's
  size or wording recompiles shaders, and since defines are film-wide, every shader.
- A metrics row inside the texture: filtered sampling bleeds it into the picture.
- Overloading `iChannelResolution[n].z`: one number isn't enough and z is Shadertoy's.

Decision: a new header uniform, always declared:

```glsl
uniform vec4 iChannelMeta[4];   // text channel: (em px, first baseline px from top, line height px, padding px); 0 otherwise
```

All values are in texel units of the bound texture (top-left origin, before `vflip`
mapping; the kit hides the flip). It is set per draw, like `iChannelResolution`, so
editing text changes a texture and a uniform, never a program. Adding it to
`SCENE_HEADER` changes every assembled source once, so the disk program cache is
rebuilt once after the upgrade; frames of existing films are unchanged (a golden run
proves it). Phase 2 extends the meaning for glyph-run channels (below), still within
`vec4`.

### Glyph runs (phase 2)

`glyphs = true` on a text turns it into two channel sources: `"<name>"` (the atlas) and
`"<name>.glyphs"` (the table).

- **Atlas**: MTSDF, RGBA8: RGB = multi-channel distance (corners stay sharp), A = true
  distance (for soft effects). Each distinct (font, glyph id) once, rasterised at an atlas
  em size (`atlas_size`, default 48 px) with 4 px spread, shelf-packed, power-of-two-free.
  Generated with a pure-Rust MSDF generator (candidate: `fdsm`; else a small port of
  msdfgen's edge colouring); determinism is pinned by a golden hash test.
- **Table**: RGBA8 texels, `filter = "nearest"`, read with `texelFetch`. 16-bit values
  packed as hi/lo bytes so RGBA8 stays the only texture format (exact, filter-free, valid
  in GLSL ES 3.00 and WebGL 2 without float-texture extensions). Rows:
  - row 0: header (glyph count, line count, entry count, layout em size, ascent, descent);
  - a **line table**: per visual line, its baseline y, x extent, first glyph, glyph count;
  - a **glyph table**: per glyph, 4 texels: pen position (x, y), atlas rect (x, y, w, h),
    bearing, and indices (glyph-in-block, cluster, word, line) for staggering;
  - for subtitles, an **entry table**: per entry, start/end in milliseconds (24-bit) and
    its first line / line count.

  Glyphs within a line are stored in visual order (after bidi reordering), sorted by x, so
  the kit's lookup is: line from y (lines have uniform pitch: a division, then ±1), glyph
  from x (binary search, then check neighbours for overlap from kerning/italics). That's
  O(log n) per pixel with no acceleration grid.
- `iChannelMeta[n]` for the table channel holds (table width, header row, layout em size,
  atlas spread) so the kit can address it without magic numbers.
- Limits: 65 535 glyphs per run (16-bit indices); atlas and table each ≤ 4096² and
  counted against the shot's 256 MB like images.
- **Glyph sets** (`glyphs = "0123456789:."` with `set = true`) put those glyphs in the
  atlas and a code-point → glyph table, so GLSL can draw strings it computes (a counter,
  timecode). The kit provides `drawNumber(...)`. No shaping across set glyphs (digits and
  punctuation only make sense here; documented).

Known limit: per-glyph transforms large enough to move a glyph off its line break the
inverse lookup. The kit handles small motion (drop-in by an em, scale ≤ 2×) by searching
a neighbourhood; wilder effects use one block texture per word, or the overlay pass.

## Fonts, shaping, layout

### Loading

- TTF, OTF (CFF and CFF2), TTC (with `index`), variable fonts (`variations = { wght = 700 }`).
  WOFF/WOFF2 are refused with a message ("convert to TTF/OTF"); they add a decompressor
  for no gain in an offline tool.
- Parsed with `ttf-parser` (the parser rustybuzz uses). Loaded on the decode worker,
  cached by file sha256 for the life of the renderer, so ten blocks in one font parse it
  once.
- Sources follow textures exactly: film-relative paths and `<lib/path>`, the latter pinned
  by `shotglass.lock` (library hashing already covers binary files). **No system fonts**,
  no fontconfig, no `fontdb` system scan: a film renders identically on any machine and
  in the browser.
- No hinting (outlines as designed). Film text is large and often animated; hinting is
  platform-flavoured and would break determinism.

### Shaping and Unicode scope

- **rustybuzz** (pure-Rust HarfBuzz port: kerning, ligatures, mark positioning, Arabic
  joining, Indic reordering) shapes each run. OpenType features per text:
  `features = ["ss01", "-liga", "tnum"]`.
- **unicode-bidi** (UAX #9) for mixed-direction paragraphs; `direction = "auto" | "ltr" | "rtl"`
  sets the paragraph base direction. RTL is in scope from phase 1: it's cheap with these
  crates and hard to retrofit.
- **unicode-script** splits runs by script; **unicode-linebreak** (UAX #14) gives break
  opportunities; **unicode-segmentation** for grapheme clusters (stagger counts graphemes,
  not code points).
- **Fallback** is explicit: a font's `fallback = ["noto-arabic", "noto-cjk"]` list, tried
  per cluster. A cluster no font covers renders `.notdef` and `check` reports it as an
  error with the code point and the text's name (a tofu box in a finished film is a bug).
- Not in scope: vertical text, colour fonts (COLR/CBDT/SVG; an SDF can't hold colour; a
  colour emoji could be an ordinary image channel), hyphenation, Knuth–Plass justification,
  text on a path (that's a shader effect over a glyph run).

`cosmic-text` was considered: it bundles shaping, bidi, fallback and layout, but is built
around system font databases and swash rasterising, pulls in more than we need, and its
layout decisions would become our contract and change with its releases. Our layout is a
few hundred lines on top of rustybuzz and is ours to keep stable. Revisit if layout grows
(rich text, inline spans).

### Layout

Per text: `size` (em in texture pixels), `width` (wrap width in px; none = no wrapping,
lines only at `\n`), `align = "left" | "center" | "right" | "start" | "end"`,
`line_height` (multiple of the font's ascent+descent+gap, default 1.2), `letter_spacing`
(em units), `max_lines` (error if exceeded: a title that silently wraps to three lines is
a bug). Greedy line breaking at UAX #14 opportunities; a word wider than `width` breaks at
grapheme boundaries with a warning. Trailing spaces don't count toward alignment.

Sizes are texture pixels, not screen pixels: a block is rasterised once, independent of
output size and auto render scale (determinism, no re-raster while playing). The shader
decides on-screen size; the kit's `textPlace` maps an em to a fraction of frame height, so
`size` is a quality knob ("rasterise big enough for the largest it gets on screen") and
`check` warns when a block is magnified more than ~4× at 1080p in its shot if the kit
helper's scale is a constant (best effort; otherwise documented guidance).

Phase 1 inline styling: none. One font, size and direction per block; a lower third is
two blocks. Phase 2 may add spans (`text = [{ t = "Anna ", font = "bold" }, { t = "director" }]`)
in glyph runs, where mixing fonts is natural.

## movie.toml

Following CLAUDE.md's "movie.toml field" rules: serde structs (`deny_unknown_fields`),
resolved structs, validation in `Movie::parse`, unit tests, README and
`templates/new/movie.toml` updated, and an engine contract patch bump (next 0.1.x after
the current one) because the file gains fields and the header gains a uniform.

```toml
shotglass = "0.1.3"            # in shotglass.toml: the contract that has [fonts]/[texts]

[fonts]
body    = "<fonts/inter/Inter-Regular.ttf>"            # from the library the project calls "fonts"
bold    = { file = "<fonts/inter/Inter-Variable.ttf>", variations = { wght = 700 } }
display = { file = "fonts/Bungee-Regular.ttf", fallback = ["body"] }
arabic  = "fonts/NotoNaskhArabic-Regular.ttf"

[texts]
title   = { text = "Big Shadow", font = "display", size = 160 }
tagline = { text = "a tiny mouse, a tealight,\nand a lot of wall", font = "body", size = 72, width = 1400, align = "center", line_height = 1.25 }
name    = { text = "Anna Ng", font = "bold", size = 64 }
role    = { text = "Director", font = "body", size = 40 }
hello   = { text = "مرحبا بالعالم", font = "arabic", size = 96, direction = "auto" }
credits = { file = "credits.txt", font = "body", size = 48, align = "center", glyphs = true }  # phase 2
subs    = { subtitles = "subs.vtt", font = "body", size = 56, width = 1600, glyphs = true }  # phase 2
digits  = { glyphs = "0123456789:.", set = true, font = "body", size = 64 }                 # phase 2

[[shot]]
id = "titlecard"
scene = "scenes/title.glsl"
duration = 4.0
channels = ["title", "tagline"]               # iChannel0 = title block, iChannel1 = tagline
uniforms.uIn = { keys = [[0, 0.0], [1.2, 1.0]], ease = "smooth" }

[[shot]]
id = "interview"
scene = "scenes/interview.glsl"
duration = 8.0
channels = ["footage", "name", "role"]        # images and texts mix freely
```

- `[fonts]` values: a path / `<lib/path>`, or `{ file, index, variations, fallback }`.
- `[texts]` values: exactly one of `text`, `file` (UTF-8 text file, read like a texture
  file, live-reloaded) or `subtitles` (`.srt`/`.vtt`, times in film seconds); `glyphs`
  without `set` alongside one of them, or `glyphs = "…"` with `set = true` alone.
  Layout options as above; sampling options `filter`, `wrap`, `vflip` as for images
  (`srgb` refused).
- **Names share one namespace** with `[textures]` (a `channels` entry is looked up in
  both; a duplicate name is an error). `"credits.glyphs"` addresses a glyph run's table.
  Inline text in `channels` isn't allowed: text always has a name, so `check` can talk
  about it.
- Validation (`Movie::parse`, no GPU): fonts exist (with `did_you_mean`, the LFS-pointer
  and HTML-page messages `texture.rs` already has), referenced fonts are defined, texts
  are used by some shot (warning, like unused `[textures]`), `size` 4–1024, `width`
  ≤ 4096, sizes of the resulting block ≤ `MAX_SIZE` (layout runs at parse time on the
  CPU: it is fast, and the block size feeds `shot_memory`), `subtitles` entries don't
  overlap and lie within the film, `max_lines` holds.
- A film using `[fonts]`/`[texts]` with an older `shotglass` contract gets the same
  "needs shotglass 0.1.3" message `channels` gets.

## Shader side: the kit's `text` library

The engine documents the format (README "Text" section and this doc); everything that
reads it lives in shotglass-kit as a library, e.g. subdir `text/`,
`library.toml` `name = "shotglass-kit/text"`, `requires_shotglass = "0.1.3"`,
`licences = ["MIT"]`, and passing `check --web` (it is GLSL ES 3.00-clean: no implicit
int→float, `highp` where needed, no 4.x built-ins). Sketch of its API:

```glsl
#include <text/block.glsl>

// Where the block lands: centre at `pos` (0–1 of the frame), em = `em` of frame height.
TextBox b = textPlace(iChannelResolution[0], iChannelMeta[0], fragCoord, iResolution.xy, vec2(0.5, 0.55), 0.12);
float cov  = textCoverage(iChannel0, b);           // R, antialiased with fwidth on the SDF when magnified
float edge = textEdge(iChannel0, b, 0.0);          // SDF-based, width-aware, any scale
float line = textOutline(iChannel0, b, 0.06);      // outline 0.06 em wide
float glow = textGlow(iChannel0, b, 0.3);
vec3 col = mix(bg, vec3(1.0), edge * uIn);
```

`text/runs.glsl` (phase 2) finds the glyph under a pixel and returns `Glyph { id, cluster,
word, line, local uv, rect }`; `text/kinetic.glsl` has stagger helpers
(`stagger(i, n, t, spread)`, per-glyph eased offsets, typewriter reveal by grapheme);
`text/subtitles.glsl` picks the active entry by `iMovieTime` and draws it with a plate;
`text/number.glsl` draws a value with a glyph set. Fonts ship in a separate kit library
(`fonts/`) so a film that brings its own fonts doesn't download them.

Because metrics come in `iChannelMeta` and positions are texel-based, the kit never needs
the font file; the engine-side layout is the single source of truth.

## Engine vs library

| engine (Rust, contract) | kit (GLSL, looks) |
|---|---|
| font loading, shaping, bidi, line breaking, layout | placement in the frame, sizing in frame units |
| rasterising coverage, SDF, MSDF atlas; glyph/line/entry tables | coverage/SDF sampling, AA, outlines, glows, shadows, bevels |
| upload via the texture store; memory accounting; gating | fades, slides, typewriter, per-glyph stagger, wobble |
| `[fonts]`/`[texts]` syntax and validation; SRT/WebVTT parsing | subtitle styling and placement, credits roll speed and easing |
| `iChannelMeta`, the documented texture/table layouts (semver) | lower-third plates, title templates |
| `check` diagnostics (missing glyphs, sizes, licences) | fonts themselves (a `fonts` library, OFL) |
| overlay pass (phase 3): when it draws, what it binds | what the overlay looks like |

The one judgement call is the R/A coverage channel: it is "looks-free" (it's the outline
fill, no styling) and it lets a film use text with zero library GLSL, the way an image
channel needs none.

## Renderer integration (`gfx.rs`, `texture.rs`)

No new state machine. A texture entry's source becomes an enum:

```rust
enum TexSource { File(PathBuf), Text(Arc<TextJob>) }   // TextJob: resolved fonts (paths + sha), layout input, part (block | atlas | table)
```

- **Key**: a text entry's key hashes the font file hashes, the layout input and the part,
  so an edit to one block re-rasterises that block only, and two shots binding the same
  text share one texture (as images do).
- **Decode worker**: `Decoder` jobs carry a `TexSource`; for `Text` the worker lays out
  and rasterises instead of decoding, producing the same `DecodedImage` (RGBA8, already
  flipped for `vflip`). From there `Decoded → Uploading → Ready` is unchanged: banded uploads
  under `upload_budget` per `pump`, prioritised by distance to the playhead (`LOOKAHEAD`),
  LRU under the 256 MB budget. So **playback never blocks**: a shot whose text isn't
  `Ready` fails `keys_ready` and the previous frame stays up, exactly like an unuploaded
  image; only `usable(.., blocking=true)` (paused, `still`, `export`) rasterises on the
  spot, counted in `blocking_texture_loads`.
- **Mipmaps**: only when `filter = "mipmap"` (not the text default), via
  `glGenerateMipmap` as for images. Atlases are never mipmapped (MSDF doesn't mip well);
  tables are always `nearest`.
- **Cost**: layout is cheap; rasterising a 2048×512 block with 4×4 supersampling + EDT is
  tens of milliseconds on one core, which is why it belongs on the worker, not the render
  thread. The worker may parallelise later; output must not depend on scheduling.
- **`iChannelMeta`**: set alongside `iChannelResolution` per draw from the entry's metrics
  (computed with the layout, stored next to the `DecodedImage`).
- **Unchanged GL invariants**: uploads go through the same code that sets and restores
  unpack state and unbinds `PIXEL_UNPACK_BUFFER`; sampler objects as for images.
- **File watcher**: add `ttf|otf|ttc|txt|srt|vtt` to the reload extensions.

Since `gfx.rs` takes one change at a time (CONTRIBUTING.md), the renderer part of each
phase is one PR, separate from the parsing/layout PR.

## Invariants

- **Playback never blocks**: covered above; selftest asserts `blocking_texture_loads == 0`
  and no `None` frames beyond the usual while a text shot plays in from cold.
- **Deterministic frames**: text bytes depend only on the font files (pinned by sha256 when
  from a library), the `[texts]` entry and the engine version. No system fonts, no
  hinting, no time, locale or thread-count dependence; integer EDT; f32 maths in layout is
  plain IEEE (Rust doesn't contract to FMA), and a unit test pins a sha256 of rasterised
  output, run on Linux, Windows and macOS CI. Same frame in `play`, `still`, `export`.
- **`check --web`**: the engine adds no GLSL; `iChannelMeta` is a plain `vec4` array
  (fine in ES 3.00); tables are RGBA8 + `texelFetch` (no float textures). The kit `text`
  library and the test film's GLSL must pass `check --web`. `check --web` warns above
  2048 px per side as for images.
- **Live reload keeps the last working version**: a missing font or text file refuses the
  edit (like a missing image); a broken font file, a `.notdef` glyph, or a block over the
  size limit fails that texture only, and the per-shot-id fallback keeps the previous
  texture on screen with the error shown. Changing text never recompiles a program.
- **Memory**: text textures count in `texture::shot_memory` (so `check` prints them) and
  the 256 MB LRU budget; parse-time layout gives exact sizes before any GPU work.
- **Loop films**: nothing special (text is static data); subtitles in a loop film must lie
  within one period (validated).

## Licensing

- The engine bundles **no fonts** (egui's UI fonts aren't film fonts).
- `[fonts]` from libraries are reported by `shotglass licenses` via the library's
  `licences` (SPDX: `OFL-1.1`, `Apache-2.0`, …). A film-folder font has no manifest, so
  phase 1 adds an optional `licence = "OFL-1.1"` on a `[fonts]` entry and `licenses` lists
  film fonts too, flagging ones without a licence.
- `bundle` and `freeze` already copy exactly the library files a film includes; they
  learn that `[fonts]` (and `[texts] file/subtitles`) reference files, like `[textures]`.
  `LIBRARY-LICENSES.md` names fonts.
- Kit `fonts` library: OFL-1.1 families only (embedding and redistribution allowed; not
  sold by themselves; Reserved Font Names apply to *modified* fonts, so we ship files
  unmodified and subset nothing on disk). Candidates: Inter, Atkinson Hyperlegible, a
  display face, Noto subsets for Arabic/Hebrew/Devanagari/CJK (large: CJK needs its own
  library so nobody fetches 20 MB by accident).
- Exported films contain rasterised text, not font files; OFL places no restriction on
  that.

## Testing

- **Unit tests** (`text.rs`, `movie.rs`):
  - parsing: every field, short and long font forms, name clashes with `[textures]`,
    exactly-one-source rule, `srgb` refused, `glyphs`/`set` rules, contract message,
    `did_you_mean`; property test round-tripping generated `[texts]`, as the channels
    tests do.
  - layout: greedy breaking at UAX #14 points, `width` and `max_lines`, each alignment,
    `line_height`, trailing spaces, an over-wide word, bidi visual order for mixed
    Hebrew/English, Arabic joining (shaped glyph ids differ from isolated forms), fallback
    per cluster, `.notdef` reported with its code point.
  - raster: EDT against a brute-force distance on small masks; coverage/SDF agree on
    which side of 0.5 a texel is; a sha256 of a small block pinned (catches accidental
    nondeterminism and crate upgrades that change output: then bump on purpose, like a
    golden).
  - phase 2: table packing/unpacking round trip, lines sorted by x in visual order,
    SRT/WebVTT parsing (times, overlaps, styling tags stripped), glyph-set lookup.
  - fixture fonts: a small OFL font committed under `tests/fonts/` (with its licence),
    subsetted Latin + Hebrew + Arabic ranges to keep the repo small.
- **Selftest** ("text textures"; mutation-check each by breaking the code):
  - a text shot played from cold: no blocking loads, frames held until ready, then identical
    to `still` of the same frame;
  - live edit of the text: no program compile happens (count compiles), the new block
    appears; a corrupt font keeps the old block and reports the error;
  - `iChannelMeta` values reach the shader (a fixture scene writes them out as colours);
  - unbound / image channels read `iChannelMeta == vec4(0)`.
- **Golden**: `tests/films/text`, 8-frame 320×180 strip: a coverage-only title, an SDF
  title magnified 4× with an outline, a wrapped centred paragraph, an RTL line, and (phase
  2) a staggered glyph run and a subtitle. Its GLSL is local to the test film (`fx/`), not
  the kit, so engine goldens don't move with the kit; the canary covers the kit library
  against kit `main`. Also re-run the existing goldens unchanged after the `iChannelMeta`
  header change: proof existing films are untouched.
- **`check`**: lays out every text with `--no-gpu`, prints block sizes and memory, reports
  missing glyphs, fonts without licences, and magnification warnings.

## Phased plan

**Phase 1 — block textures (MVP)**
1. `text.rs`: font loading (ttf-parser), shaping (rustybuzz), bidi, line breaking, layout,
   coverage + SDF raster; unit tests; fixture font. No renderer changes.
2. `movie.rs`: `[fonts]`, `[texts]` (`text`/`file`), channel lookup across both tables,
   validation, `shot_memory`; `check` output; `licenses`/`bundle`/`freeze` follow font
   files; README "Text" section; template comment; contract bump.
3. `gfx.rs` (one PR): `TexSource::Text` on the worker, keys, `iChannelMeta` in the header
   and per draw, watcher extensions; selftest checks; golden `tests/films/text`.
4. kit: `text/block.glsl` (place, coverage, edge, outline, glow, shadow) and a `fonts`
   library with two or three OFL families; a title card in the template or demo (re-bless
   if the demo changes on purpose).

**Phase 2 — glyph runs**
5. MSDF atlas generation, glyph/line tables, glyph sets, SRT/WebVTT `subtitles`; unit tests.
6. `"name.glyphs"` channel entries; `iChannelMeta` for tables; selftest + golden cases.
7. kit: `text/runs.glsl`, `text/kinetic.glsl`, `text/subtitles.glsl`, `text/number.glsl`;
   credits-roll example.

**Phase 3 — overlay pass**
8. `[[overlay]]` in movie.toml: a film-level scene (own `channels`, uniforms, time window)
   drawn after the cut/transition, before the composite, with the frame as an input
   sampler. Contract change (minor in 0.x terms if it changes the frame pipeline). Lets
   subtitles, lower thirds and credits span shots and transitions. Its own design doc;
   it's useful beyond text (vignettes, grain, watermarks).

**Later**: spans (rich text) in glyph runs, justification, colour emoji as images,
`outlines` data for vector rendering, text in transitions.

## Open questions

1. **Always declare `iChannelMeta`, or only in shots that bind text?** Always is simpler and
   gives a clear contract; it costs one program-cache rebuild on upgrade. Recommended:
   always.
2. **Default SDF spread and supersampling**: 8 px / 4×4 is a guess; tune on the golden film
   for outline widths up to ~0.15 em at 4× magnification.
3. **Sizes in texture pixels vs frame units**: texture pixels keep rasterisation
   independent of output size (determinism); is a `quality`-style option friendlier than
   `size`? Leaning: keep `size`, the kit maps it.
4. **Two channels per glyph run** uses half of a shot's four. Alternatives: pack atlas and
   table in one texture (filtering bleed, mip issues), or raise the channel count (header
   and Shadertoy-compat change). Decide when phase 2 starts, with a real kinetic example.
5. **MSDF generator**: `fdsm` maturity and determinism vs porting msdfgen's core. Spike
   before phase 2.
6. **Subtitles as text data vs overlay**: phase 2 subtitles still need every shot's scene to
   include the kit helper. Is that acceptable until phase 3, or should phase 3 come first
   for subtitle-heavy users?
7. **Web player font size**: CJK fonts are large; should `bundle` warn above a size, or
   should the engine subset to used glyphs at bundle time (a modified font under OFL's
   RFN rule, so only for fonts without reserved names)?

## Decisions (proposed)

- Text is a texture source, bound with `channels`; names share `[textures]`' namespace.
- Phase 1: one RGBA8 block texture per text: R/A coverage, G signed distance, B reserved;
  text defaults `linear`/`clamp`.
- Metrics via a new `uniform vec4 iChannelMeta[4]`, set per draw; text edits never
  recompile.
- Layout on rustybuzz + unicode-bidi + unicode-linebreak, our own greedy layout; RTL and
  complex scripts in scope; no hinting; no system fonts; explicit fallback chains.
- Rasterise on the decode worker; reuse banded uploads, priorities, LRU budget and
  `keys_ready` unchanged.
- No GLSL in the engine; the kit's `text` library owns sampling and effects, its `fonts`
  library owns OFL fonts.
- Phase 2 glyph runs: MTSDF atlas + RGBA8-packed tables read with `texelFetch`; phase 3
  overlay pass in its own design.
- Contract: patch bump for `[fonts]`/`[texts]` and `iChannelMeta`.
