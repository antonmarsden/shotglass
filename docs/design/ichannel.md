# Design: iChannel textures

Issue: [#22](https://github.com/antonmarsden/shotglass/issues/22). Status: **implemented**
(decisions at the end; PR 1 and PR 2 of the plan landed together, see "Implementation notes"). The heavy-assets design (#32) builds on the decisions here.

Channels now also carry text blocks ([text-typography.md](text-typography.md)), `@`
layer outputs ([layers-compositing.md](layers-compositing.md)) and image-sequence footage
([video-inputs.md](video-inputs.md)); every entry is classified by
`movie::ChannelSource::of`, and the scene header declares `iChannelMeta[8]`.

## Summary

A shot can bind up to eight images to `iChannel0..7` (`movie::N_CHANNELS`), as on Shadertoy. Images are PNG or
JPEG files from the film folder or from a library, so library images are pinned by the
sha256 that `shotglass.lock` already records. They are decoded on the CPU by the same Rust
code on every platform, including the web, and stored as RGBA8, flipped on the decode
thread. Only level 0 is uploaded; the GPU makes the mipmaps (`glGenerateMipmap`), which
is the fast path, at the price of mip levels that differ slightly between drivers. Uploads happen ahead of the playhead, a few
megabytes per frame, prioritised the same way as compiles. While playing, a shot whose
textures aren't uploaded yet is handled like a shot that hasn't compiled: the previous
frame stays up. So every frame you see is the frame `still` and `export` render. The
scene header declares `iChannel0..7`, `iChannelResolution[8]`, `iChannelTime[8]` and
`iChannelMeta[8]`.
The engine contract gets a patch bump (0.1.0 → 0.1.1) because `movie.toml` gains a field.

Not in this design: cubemaps, 3D textures, buffers (multipass), video, keyboard and sound
inputs, textures in transitions and sound shaders, and KTX2. The syntax leaves room for
each of them (see [Later](#later)).

## What exists today

- `SCENE_HEADER` (`gfx.rs`) already declares `uniform sampler2D iChannel0..3`,
  `uniform vec3 iChannelResolution[4]` and `uniform float iChannelTime[4]`. `draw_scene`
  binds a 1×1 black texture (`Renderer::dummy`) to units 0–3 and never sets
  `iChannelResolution` (so it reads 0). The README documents this: "declared so pasted
  code compiles; they are bound to black for now".
- Programs are keyed by a hash of their assembled text. Compiles run nearest the playhead
  first (`pump`, `priority`, `LOOKAHEAD` = 10 s), and `usable(i, blocking)` decides whether a
  shot can be drawn: the current version, else the last working version for that shot id
  (`fallback`), else nothing (non-blocking) or compile now (blocking).
- Libraries are hashed file by file (`library::hash_dir`) and pinned in `shotglass.lock`.
  The hash already covers binary files: the CRLF→LF step only applies to files that are
  valid UTF-8, and PNG (first byte 0x89) and JPEG (0xFF) never are.
- The `image` crate is a dependency with only the `png` feature.
- The player's file watcher reloads on `glsl|toml|lock|frag|inc|h` changes.

## movie.toml

### Syntax

```toml
shotglass = "0.1.1"
channels = ["noise"]           # film-wide default for every shot (top-level keys go before tables)

[textures]                     # named textures, defined once, used by any shot
noise = "<kit/textures/noise-rgba-256.png>"      # from a library the film calls "kit" (illustrative: the kit ships no textures yet)
brick = "textures/brick.png"                    # from the film folder
grid  = { file = "textures/grid.png", filter = "nearest" }
moon  = { file = "textures/moon.jpg", wrap = "clamp", srgb = true }

[[shot]]
id = "wall"
scene = "scenes/wall.glsl"
duration = 9.0
channels = ["brick", "noise"]                    # iChannel0 = brick, iChannel1 = noise

[[shot]]
id = "sky"
scene = "scenes/sky.glsl"
duration = 6.0
channels = ["", "noise", "textures/stars.png"]   # "" leaves iChannel0 black

[[shot]]
id = "title"
scene = "scenes/title.glsl"
duration = 4.0
channels = [{ file = "textures/logo.png", wrap = "clamp" }]   # inline, with options
# no `channels`: this shot gets the film-wide default; `channels = []` binds nothing
```

**`channels`** is an array of at most eight entries; entry *n* is `iChannel`*n*. Each entry is:

| entry | meaning |
|---|---|
| `"brick"` | a name from `[textures]` (a GLSL-style identifier: letters, digits, `_`) |
| `"textures/stars.png"` | a file in the film folder, with default options (anything containing `.` or `/` is a path) |
| `"<kit/textures/noise.png>"` | a file in a library, same angle brackets as `#include <kit/…>` |
| `{ file = "…", filter = …, … }` | a file (either kind) with options |
| `""` | nothing: the channel stays black and its `iChannelResolution` is 0 |

Names and paths can't be confused: identifiers have no `.` or `/`, and a path needs an
image extension. A plain `kit/textures/noise.png` (no brackets) is a path in the film
folder, exactly as for `#include "…"` versus `#include <…>`.

**Shared vs per shot.** The top-level `channels` applies to every shot that doesn't set its
own. A shot's `channels` *replaces* the default entirely (no merging by index), so what a
shot reads is visible in one place. `[textures]` is for naming a texture once when several
shots use it, or when it has options.

**Options** (all optional; the defaults are Shadertoy's, so pasted shaders look right):

| option | values | default | notes |
|---|---|---|---|
| `filter` | `"mipmap"`, `"linear"`, `"nearest"` | `"mipmap"` | `mipmap` = trilinear (`LINEAR_MIPMAP_LINEAR` / `LINEAR`) |
| `wrap` | `"repeat"`, `"clamp"`, `"mirror"` | `"repeat"` | both axes; `clamp` = `CLAMP_TO_EDGE` |
| `vflip` | `true`, `false` | `true` | `true`: the image is upright when sampled with `fragCoord / iResolution.xy` (Shadertoy's default) |
| `srgb` | `true`, `false` | `false` | `true`: the GPU decodes sRGB to linear when sampling (see [Colour](#colour)) |

`iChannelResolution[n]` is `vec3(width, height, 1.0)` of the image as stored (not affected
by the render scale), and `iChannelTime[n]` is 0 for images. Both are already declared.

### Validation (`Movie::parse`)

New serde fields, all with `deny_unknown_fields`: `MovieFile.channels`,
`MovieFile.textures` (`BTreeMap<String, TextureFile>`), `ShotFile.channels`.
`TextureFile` is untagged: a string, or `{ file, filter, wrap, vflip, srgb }`.

`parse` checks, with messages in the existing `shot "id": …` style:

- at most eight entries: `shot "sky": channels has 9 entries; there are 8 (iChannel0..7)`;
- a name must be in `[textures]`: `shot "wall": channels[1]: no texture called "noize" in [textures] (there are "brick", "grid", "noise")`;
- `[textures]` names are identifiers and don't shadow each other;
- paths follow `library::check_rel_path` (forward slashes, inside the film folder);
- a library path names a declared library: `[textures] noise: "<kat/…>" names library "kat", but [libraries] has "kit", "transitions"`;
- the extension is `.png`, `.jpg` or `.jpeg` (any case): `textures/sky.webp: shotglass reads PNG and JPEG images`;
- option values are from the tables above: `[textures] grid: filter "point" isn't one of "mipmap", "linear", "nearest"`;
- an unused `[textures]` entry is a warning, not an error (handy while editing).

The resolved `Shot` has `channels: [Option<Channel>; N_CHANNELS]`. The sketch below is
the original image-only shape; `Channel` is now `{ source, timing }`, with `source` one of
`Image`, `Text`, `Frames`, …:

```rust
pub struct Channel {
    pub file: PathBuf,     // absolute: film root or library root joined with the path
    pub label: String,     // as written, for messages: "textures/brick.png", "kit/textures/noise.png"
    pub filter: Filter,
    pub wrap: Wrap,
    pub vflip: bool,
    pub srgb: bool,
}
```

Library paths are joined with the library root in `resolve_libraries` (a bare `parse`
keeps them unresolved, like library includes). Texture files join `referenced_files`, so
`check_case` warns about case-only matches and the web preloader (#10) fetches them.

## Files

### Where they come from

- **The film folder**, committed with the film. No extra pinning: the film's own git
  history is the version.
- **A library**, as `<name/path.png>`. The library's sha256 in `shotglass.lock` already
  covers every file in it, binary ones included, so a library image is pinned exactly
  like its GLSL: a new release never changes a finished film, and an edited cache is an
  error. No change to the lock format.

That makes "an asset pack" for small images just a library: a folder with a
`library.toml` and some PNGs. The kit was meant to ship MIT or CC0 noise and pattern textures this way
(see [Decisions](#decisions)), but has none yet: its noise is procedural
(`core/noise.glsl`). Large assets that don't belong in git
(video, long samples) are #32's job: it will add content-addressed packs fetched over
HTTP with their own lock entries, and textures will be able to come from them too, with
the same `<name/path>` syntax.

Shadertoy's built-in media (its noise, "Organic", "Abstract" textures and so on) aren't
licensed for redistribution. The README will say so and point to CC0 sources.

**Git:** `.gitattributes` in this repo and in the `shotglass new` template get
`*.png binary` and `*.jpg binary` / `*.jpeg binary`. Git already detects these as binary,
so a CRLF checkout (the owner's) doesn't touch them, but being explicit costs nothing.
A Git LFS pointer committed instead of an image gets its own error message
(see [Errors](#error-messages)).

### Formats

**Version 1: PNG and JPEG**, decoded with the `image` crate (`png` feature, already used;
add `jpeg`, which is pure-Rust `zune-jpeg`, so no C dependency and it builds for wasm).
The format is detected from the content (`image::guess_format`); an extension that
disagrees is a warning.

Every image becomes **RGBA8**: grey becomes `(g, g, g, 1)`, grey+alpha `(g, g, g, a)`, RGB gets
`a = 1`, palette images are expanded, 16-bit channels are rounded to 8 bits (warning:
`textures/height.png is 16-bit; shotglass keeps 8 bits per channel for now`). Alpha is
straight (not premultiplied), as stored. PNG `gAMA`, `cHRM`, `sRGB` and `iCCP` chunks and
JPEG ICC profiles are ignored: texels are the stored numbers, as on Shadertoy.

Why only RGBA8: it's what Shadertoy gives shaders, every GL 3.3 / WebGL 2 implementation
can sample, filter and mipmap it, and one format keeps the upload path simple. 16-bit and
float data (height maps, LUTs) wait for a film that needs them (see [Decisions](#decisions)).

**Later: KTX2.** Worth it for big images (GPU-compressed formats: smaller downloads and
less GPU memory), float data, cubemaps and 3D textures. It needs Basis Universal
transcoding to whatever the GPU supports, which differs between desktop and browsers, and
compressed formats decode slightly differently on different GPUs. That's a separate
design once there's a film that needs it.

### Colour

Shadertoy uploads images as plain `RGBA8` (its per-channel "sRGB" checkbox is off by
default): a shader reads the stored, gamma-encoded numbers, and many pasted shaders do
`pow(col, vec3(2.2))` themselves. shotglass's output is also plain RGBA8 with no sRGB
framebuffer, so `srgb = false` (the default) means a pasted shader looks the same as on
Shadertoy.

`srgb = true` uploads as `SRGB8_ALPHA8`, so `texture()` returns linear values and
filtering happens in linear space (correct for photos you light in linear). Drivers
generate the mipmaps of an sRGB texture in linear space (GL 3.3 and ES 3.0 both allow
`glGenerateMipmap` on it). Both formats exist in GL 3.3 and WebGL 2.

`srgb = true` is only for shaders that encode their own output (for example ending with
`pow(col, vec3(1.0 / 2.2))`). shotglass's output isn't an sRGB framebuffer, so a shader
that writes an sRGB-decoded texel straight out shows it too dark. That's why the default
stays `false`: it's the one setting that's right whatever the shader does. GLSL itself
has no colour-space rules; engines default colour textures to sRGB because their output
path encodes too (a linear workflow). shotglass could offer that film-wide later (see
[Later](#later)).

### Size limits

- **Per image: at most 4096 × 4096.** Every desktop GPU and practically every WebGL 2
  implementation supports it. Bigger is an error with a "scale it down" message. The
  driver's own `GL_MAX_TEXTURE_SIZE` (already queried once at startup) is checked too,
  for the rare GPU below 4096. `check --web` warns about images over 2048 per side, the
  WebGL 2 minimum (some phones).
- **Per film: 256 MB of texture memory** (RGBA8, with mipmaps: a 4096² image with mipmaps
  is about 85 MB, a 1024² one about 5.3 MB). This is a budget for what is resident at once,
  not for the whole film; see [Memory](#memory). A film whose *single shot* needs more than
  the budget is refused by `check` and on load. Non-power-of-two sizes are fine everywhere
  (GL 3.3, WebGL 2), with mipmaps and `repeat`.

Neither limit is a `movie.toml` setting in version 1. They can become one if a film
needs it.

## Engine contract

Nothing changes in the assembled source: `SCENE_HEADER` already declares the samplers
and arrays, so existing programs, their hashes and the on-disk program cache stay valid,
and a film without `channels` renders bit for bit as before.

What's new is the `movie.toml` field and the promise that the channels carry the
images it names, with Shadertoy's orientation and colour defaults. That's an additive
change to the contract, so it is a **patch bump in 0.x: 0.1.0 → 0.1.1**.

- Existing films (`shotglass = "0.1"`, which means `>=0.1.0, <0.2.0`) keep loading.
- A film that uses `channels` should say `shotglass = "0.1.1"`; an older engine then says
  "this film needs shotglass 0.1.1 … install a newer shotglass" instead of serde's
  "unknown field `channels`". `check` warns when a film uses `channels` but its
  `shotglass` field is missing or allows 0.1.0, and `shotglass new` writes the current
  contract anyway.
- Sampler types are part of the contract from now on: `iChannel0..7` are `sampler2D`.
  A later cubemap or 3D channel would declare `samplerCube`/`sampler3D` for that shot,
  which changes the shot's source (and so its program key). That's fine, but it is a
  contract change of its own.

## Renderer (`gfx.rs`)

### Texture store

A new `TextureStore` owned by the `Renderer`, next to `programs`:

```rust
struct TexKey(u64);  // hash of (file path, length, mtime, vflip, srgb, mipmapped)

enum TexState {
    Idle,                                // known, nothing done yet
    Decoding,                            // a worker is reading and decoding the file
    Decoded(DecodedImage),               // RGBA8 levels on the CPU, waiting to upload
    Uploading { tex, next: (level, row) },
    Ready { tex, w, h, bytes },
    Failed(String),                      // the decode or size error, for the panel
}
```

The key covers how the texels are *made* (the file, flip, sRGB, whether there are mips).
How they're *sampled* (`filter`, `wrap`) isn't in the key: it lives in **sampler objects**
(`glGenSamplers`, GL 3.3 and WebGL 2), one per (filter, wrap) pair, created once. So one
file used with `nearest` in one shot and `mipmap` in another is uploaded once (with mips).

`set_movie` computes each shot's texture keys after assembling its sources. It `stat`s
each file (cheap; no read) for the length and mtime. A missing file is an error that
refuses the whole new version, like a missing `#include` (see
[Live reload](#live-reload)). `ShotKeys` gains `channels: [Option<(TexKey, Sampler)>; 4]`,
so the per-shot-id fallback remembers the textures that went with the last working
programs.

### Decoding off the render thread

Decoding a 4096² PNG takes 100 ms or more, so it never runs on the render thread. One
worker thread (two if there are many) takes jobs `(TexKey, path, vflip, srgb)` and
returns `Result<DecodedImage, String>` over a channel. The worker reads the file, decodes,
converts to RGBA8 and flips if `vflip`. `pump` collects results without blocking
(`try_recv`).

**The flip stays on the CPU.** It costs nothing that matters: the decoder writes rows
into the RGBA8 buffer anyway, so writing them bottom-up is the same work, on the worker
thread. GL has no free alternative (no negative `UNPACK_ROW_LENGTH`); flipping on the GPU
would need an extra render pass, and flipping in the shader would change the contract
(`texture(iChannel0, uv)` must mean the same thing as on Shadertoy).

**Mipmaps are made on the GPU** with `glGenerateMipmap`, once the last band of level 0
is submitted. That's the fast path: a third less to upload (64 MB instead of 85 MB for
4096²), no CPU filtering (tens of ms per 4096² image), the texture is ready sooner, and
on a real GPU the call costs well under a millisecond of GPU time. It's one command,
ordered like the uploads, so there's still nothing to wait for. The cost is that the
mip levels are the driver's: most use a 2×2 box filter, but the spec doesn't say, so
NVIDIA, llvmpipe, the Apple renderer and browsers may differ slightly at the smaller
levels (see [Determinism](#determinism)). On a software renderer (llvmpipe in CI) the
call is slow, but timings there mean nothing anyway. CPU mipmaps stay a possible
per-texture option later if a film ever needs bit-identical mips across machines.

On wasm there are no threads by default. The web player preloads every file before Play
anyway (#10), so it decodes there, before playback starts.

### Uploading without stalls

The per-frame rules from CLAUDE.md apply: no `glGetError`, no reads, no `finish`, no
waiting on fences on the render path.

- **Allocate** level 0 once with `tex_image_2d(…, None)` (`RGBA8` or `SRGB8_ALPHA8`);
  `glGenerateMipmap` allocates the rest. `glTexStorage2D` would be nicer but is GL 4.2 /
  ES 3.0 only; the allocation path is the same as `ensure_target`'s.
- **Upload in bands** with `tex_sub_image_2d` from client memory, at most a budget per
  frame (start at 8 MB of texels, about 1–3 ms of driver memcpy on the dev GPU; tune on
  the owner's machine). A 4096² texture takes about 8 frames, well inside the
  10 s lookahead. The driver copies client memory before the call returns, so there's
  nothing to wait for; the band size bounds that copy.
- **Ready means submitted.** GL executes commands in order, so a draw issued after the
  last band sees the data. No fence is needed, and none is polled.
- **Unpack state is restored**, like `PACK_*` for readbacks: set `UNPACK_ALIGNMENT` 1 and
  `UNPACK_ROW_LENGTH`/`SKIP_ROWS` for the band, put them back to 4/0/0 afterwards, and make
  sure `PIXEL_UNPACK_BUFFER` is unbound. The sound path binds that buffer for mix inputs,
  and a client-memory upload with it still bound would read a buffer offset instead of
  the slice. A selftest pins this.
- **Where it runs:** in `pump`, after the compile housekeeping. The player calls `pump`
  before rendering; uploads there are bounded by the budget, and `stream_sound` still runs
  after the frame's render is queued. The player already times `pump` as housekeeping, so
  a frame slowed by an upload counts as a stall, not a drop, and doesn't lower the auto
  scale; uploads also sit outside the GPU timer's begin/end in `render_at`. The
  Performance panel gets a "textures" share next to compiling and sound.
- A persistently mapped PBO for uploads (as the sound path does on GL 4.4) is a possible
  follow-up if the memcpy shows up in the Performance panel. Not in version 1.

### Priority and gating

Textures use the program priority: `priority(movie, t)` already ranks each shot by how
soon the playhead needs it. A texture gets the smallest distance of the shots that use it.
`pump` starts decodes and uploads for everything within `LOOKAHEAD` first, nearest first,
then (with `background`) the rest, a couple at a time, so errors show early, as for
compiles.

`usable(i, blocking)` treats textures like programs: a shot's keys are ready when its
programs are `Ready` **and** each bound texture is `Ready`.

- **Playing** (`blocking = false`): if the current version isn't ready, use the fallback
  (old programs and old textures); otherwise return None and the previous frame stays up.
  The player never shows a frame with a missing or half-uploaded texture.
- **Paused, scrubbing, headless** (`blocking = true`): decode and upload what the shot
  needs, now, on this thread. That's the only place a texture blocks, as for compiles. A
  counter `blocking_texture_loads` (like `blocking_audio_reads`) lets the selftest check
  that playback does none.

### Binding and texture units

| pass | units |
|---|---|
| scene (`draw_scene`) | 0–3: `iChannel0..3` |
| transition / copy (`composite`) | 0: `uFrom`, 1: `uTo` (2–5 reserved for transition textures later) |
| sound and mix (`submit_audio`) | 0: `sg_music`, 1: `sg_sfx` |

The passes are separate programs and separate draws, so unit 0 can mean different things
in each; each pass binds everything it reads before drawing. GL 3.3 and WebGL 2 both
guarantee 16 fragment units.

In `draw_scene`, for each channel *n*: `active_texture(TEXTURE0 + n)`, bind the texture
(or `dummy`), `bind_sampler(n, sampler)` (or 0 for `dummy`), and set
`iChannelResolution` with one `uniform_3_f32_slice`. The sampler uniforms
(`iChannelN = N`) never change, so they are set once per program right after linking (in
`finish_compile`, which also handles programs loaded from the binary cache, whose uniform
values reset) rather than every frame. All of this is state setting: no round trips.

**Freeze frames** render the outgoing shot once, with that shot's channels bound. The
freeze key is `(to, generation)`, and `set_movie` bumps `generation`, so a texture edit
(which goes through `set_movie`) re-renders the frozen frame.

`set_readback` (headless output drawn upside down) only flips the final composite, not
scenes, so it doesn't interact with `vflip`.

### Memory

Each `Ready` texture records its GPU bytes (all levels × 4). When the total is over the
256 MB budget, `pump` (every 30 frames, with `evict`) drops the least recently used
textures that no shot within 1.5 × `LOOKAHEAD` needs and no fallback holds, the same rule
as programs. They come back through the same decode and upload path, identically (see
the selftest). Decoded CPU copies are freed as soon as the upload finishes, and at most
two decoded images wait for upload at once, so CPU memory stays bounded too.

`check` prints the textures with size and memory, and the largest single-shot total.

## Live reload

- The watcher filter adds `png`, `jpg` and `jpeg`.
- A texture edit goes through the normal reload (`Movie::load` → `set_movie`). Shader
  sources don't change, so no program is recompiled; only the edited file's `TexKey`
  (its length/mtime changed) is new, and only it is decoded again.
- **A broken edit keeps the last working version on screen.** Editors save images in
  pieces, so the reload often sees a half-written PNG. A decode failure is per texture:
  the shot keeps drawing with its fallback (the old texture), the panel shows the error
  (`textures/brick.png: unexpected end of PNG data`), and the next save fixes it. Without
  a previous working version (first load), the shot can't be drawn while playing; when
  blocking it draws with that channel black and the error shows.
- **A missing file refuses the whole new version**, like a missing `#include`: it's an
  edit-level mistake (a typo in `movie.toml`, a renamed file), and the old version keeps
  playing.
- Library textures reload when their library is a working copy (`path` or an override),
  since the watcher already follows library folders.

## Web player and `check --web`

- **Files.** Textures are in `referenced_files`, so the preloader from #10 fetches them.
  `FileSource` (#10) needs a bytes read next to its text read; textures go through it,
  never `std::fs`. Same CORS rules as the rest of the film.
- **Decoding** uses the same Rust `image` code compiled to wasm, not the browser's
  `createImageBitmap` or `<img>`: browsers apply colour management and premultiplication
  and their JPEG decoders round differently, which would break "web and desktop render
  the same frames" (#2's acceptance list).
- **GLSL ES.** Nothing new: `version_line` already declares `precision highp sampler2D`,
  and `texture`, `textureLod`, `texelFetch` and `textureSize` exist in GLSL ES 3.00. The
  strict check (`validate.rs`) sees the same header as today.
- **WebGL 2** has sampler objects, NPOT mipmaps and repeat, `SRGB8_ALPHA8`, and
  `MIRRORED_REPEAT`. `check --web` warns about images over 2048 per side.
- **New `check` lint (all dialects):** a scene that mentions `iChannelN` while its shot
  binds nothing to *N* gets a warning, `shot "sky" (scenes/sky.glsl): reads iChannel0, but
  channels has nothing at 0 (it reads black)`. That's the common pasted-shader surprise.
  A text search of the shot's own files (not the header) is enough.
- `check --no-gpu` decodes every texture and checks the limits too, so CI catches a bad
  image without OpenGL.

## Determinism

A frame depends only on (movie, t, size, scale). Textures keep that true:

- **Same level 0 everywhere.** One decoder (Rust, same version on every platform
  including wasm), RGBA8, flipped on the CPU. The GPU gets identical bytes on every
  machine, and after an eviction and re-upload.
- **Mip levels are the driver's** (`glGenerateMipmap`, chosen for speed). The same driver
  makes the same mips every time, so on one machine playback, `still` and `export` still
  agree, and so does a re-upload after eviction. Across machines the smaller levels can
  differ slightly; where a frame samples them (minified textures, `filter = "mipmap"`),
  that adds to the GPU-to-GPU differences below. The golden references are llvmpipe
  renders checked on llvmpipe, so CI is unaffected; web vs desktop (#2) matches by
  tolerance, not bit for bit, as it already does for shader maths.
- **No history.** Texture contents depend on the file and the options only; sampler state
  is set from the movie on every draw; `iChannelTime` is 0 for images.
- **Playback shows only complete frames.** A shot is drawn only when all of its textures
  are uploaded, so playback never shows a frame that `still` wouldn't. `still`, `strip` and
  `export` render with `blocking = true`, which loads everything first.
- **What can still differ between GPUs** is the filtering arithmetic (LOD selection,
  bilinear weights' precision), as it already does for shader maths. The golden tolerance
  absorbs that. Anisotropic filtering stays off: its quality varies by vendor and it isn't
  in core GL 3.3 or WebGL 2.
- Render scale changes `iResolution` and so the derivatives, which picks coarser mips at
  lower scale. That's already part of "(…, scale)".

## Error messages

In the existing styles: `movie.toml` problems name the shot and field, file problems name
the file as written (relative to the film, or `kit/textures/noise.png` for a library
file), and they appear in `check`, in the player's panel and on stderr for headless
commands.

```
movie.toml: shot "sky": channels[2]: "textures/strs.png" not found (did you mean "textures/stars.png"?)
movie.toml: [textures] noise: "<kat/textures/noise.png>" names library "kat", but [libraries] has "kit"
textures/sky.png: 8192×4096 is larger than 4096×4096, the most every GPU and browser supports; scale it down
textures/sky.png: 4096×4096 is larger than this GPU's limit of 2048 (GL_MAX_TEXTURE_SIZE)
textures/brick.png: not a PNG or JPEG (it starts with "<!DOCTYPE"; an HTML page saved by mistake?)
textures/brick.png: this is a Git LFS pointer, not the image; run `git lfs pull`
textures/brick.png: unexpected end of PNG data (still being saved?)
shot "wall": its textures need 290 MB, more than the 256 MB budget: textures/sky.png 85 MB (4096×4096 with mipmaps), …
```

The "did you mean" is a case-insensitive or edit-distance-1 match in the same folder; the
case-only match is already a `check_case` warning.

## Tests

**Unit tests, `movie.rs`:** short and long forms; `""` gaps; the film default and a shot's
replacement, including `channels = []`; names vs paths vs `<lib/…>`; more than four
entries; unknown names, libraries, extensions and option values; unknown fields in a
texture table (`deny_unknown_fields`); path rules; unused-texture warning;
`referenced_files` includes textures. The proptest generator for near-valid `movie.toml`
gets `channels` and `[textures]`, so "the parser never panics" covers them.

**Unit tests, new `texture.rs`** (CPU side only, no GL): decoding tiny in-memory PNGs
(grey, grey+alpha, RGB, palette, 16-bit) and a JPEG to RGBA8; `vflip`; size and budget errors; the HTML and Git LFS messages; format sniffing
with a mismatched extension.

**Selftest checks** (each mutation-checked: break the code, see it fail):

1. *channels bind the right image to the right unit:* two 2×2 textures of known colours
   on channels 0 and 2; the scene writes `texture(iChannelN, …)` at known pixels;
   `iChannelResolution` matches; channel 1 reads black with resolution 0.
2. *orientation matches Shadertoy:* with the default `vflip`, the image's top-left texel
   lands at the top-left of the frame; with `vflip = false`, at the bottom-left.
3. *filter and wrap:* `nearest` vs `linear` at a texel boundary; `repeat`, `clamp` and
   `mirror` at u = 1.25; `mipmap` at a small size reads the generated mips (a
   checkerboard averaging to grey, within a tolerance since the filter is the driver's;
   for `srgb = true`, the grey of a linear-space average). One texture used with two samplers is uploaded once.
4. *playback never blocks on a texture:* a non-blocking render of a shot with a large
   texture returns None until the upload is done, spreads the upload over several frames
   (per-frame budget), and leaves `blocking_texture_loads` at 0; a blocking render draws
   it at once.
5. *a broken texture edit keeps the last working one:* overwrite the PNG with half its
   bytes, reload: the frame is unchanged and the error is reported; fix the file: the new
   image shows. A missing file refuses the new version.
6. *a texture-only edit recompiles nothing:* program states and compile counts unchanged.
7. *evicted textures come back identically* (force a tiny budget).
8. *frozen transitions use the outgoing shot's channels,* and re-freeze after a texture
   edit.
9. *uploads restore unpack state* and work while the sound path has used the unpack
   buffer (`PIXEL_UNPACK_BUFFER` unbound, alignment 4, row length 0).
10. Once #10 is in: the "downloaded file by file plays like the one on disk" fixture gets
    a texture.

The fixture images are generated by the selftest (tiny PNGs written with `image`), so
nothing binary is needed for them, and each fixture film passes the strict check at
330 and as GLSL ES (as all fixtures must).

**Golden image:** a new case in `tests/golden.rs`, `tests/films/channels/`: a small film
with a checked-in 64×64 PNG and a 64×64 JPEG (made for the purpose, CC0, a few KB each),
exercising `mipmap` (a receding plane), `repeat`, `nearest`, `clamp` and `vflip` over 8
frames at 320×180. Reference `tests/golden/channels.png`, blessed on llvmpipe as usual. It
must pass `check --web`, and `validate.rs` gets it in its "passes everywhere" list. The
demo and the template stay as they are (no re-bless), unless the owner wants a texture in
one of them.

## Implementation plan

Two PRs for #22, so that `gfx.rs` is touched by exactly one, plus a web follow-up.

**PR 1: syntax, decoding and `check` (no `gfx.rs`, `sound.rs` or `audio.rs`).**
Safe to run in parallel with other sessions (it touches the shared `movie.rs`, `main.rs`
and `validate.rs`, so expect a rebase).

- `movie.rs`: serde fields, resolved `Channel`, validation, unit tests, proptest generator,
  `referenced_files`.
- `texture.rs`: decode to RGBA8, flip, limits, error messages, unit tests;
  `image` gets the `jpeg` feature.
- `check` (and `--no-gpu`, `--web`): decode every texture, limits, budget per shot,
  unbound-channel lint, contract warning.
- Player watcher: add the image extensions (a reload only re-validates for now).
- `.gitattributes` for images; template `movie.toml` gets a commented example. No README
  section yet: the feature doesn't draw anything until PR 2, so loading a film that uses
  `channels` is refused with "iChannel textures arrive in the next version of shotglass";
  `check` validates it fully (see [Decisions](#decisions)).

**PR 2: the renderer (the one `gfx.rs` change).**

- `TextureStore`, decode worker, banded uploads in `pump` then `glGenerateMipmap`, sampler objects, binding and
  `iChannelResolution` in `draw_scene`, sampler uniforms set at link, gating in `usable`,
  textures in `ShotKeys`/fallback, eviction and budget, `blocking_texture_loads`.
- Selftest checks 1–9; golden case `channels`.
- README: a "Textures" section, the uniform table row, the CC0/licence note; template;
  CLAUDE.md map (`texture.rs`) and invariants (texture gating in `usable`, unpack state);
  contract 0.1.1 (`Cargo.toml` version).
- Owner tests on Windows/NVIDIA: upload budget, no stalls in the Performance panel.

**PR 3 (after #10 merges): web.** Bytes through `FileSource`, decode during preload,
the #10 selftest fixture with a texture, a browser spot check.

## Later

The syntax leaves room for other channel kinds as different inline tables, without
breaking version 1:

- `{ cubemap = "sky_{face}.png" }` or a KTX2 cubemap: declares `samplerCube iChannelN`
  for that shot (a contract change, see above).
- `{ volume = "…" }`: `sampler3D`, KTX2 only.
- `{ buffer = "A" }`: Shadertoy multipass (Buffer A–D), feedback from the previous frame.
  That's a big change to determinism (a frame would depend on earlier frames) and wants its
  own design.
- `{ video = "…" }`: #32 (decode thread to a texture, frame-accurate seek); `iChannelTime`
  becomes meaningful.
- Textures in transitions (luma wipes from an image) on units 2–5, and in sound shaders
  (wavetables), if a film needs them. Audio samples are #32's.
- KTX2 for big or compressed images; a persistently mapped upload PBO.
- A film-wide linear workflow, e.g. `color = "linear"`: textures default to `srgb = true`
  and the output is encoded to sRGB (an sRGB target or an encode in the composite pass).
  That's the engine convention, but it changes more than textures: transitions would blend
  in linear (a crossfade looks different), and export and the web player's output path
  are affected. A contract change with its own design.

## Decisions

These were open questions in the first draft. The owner said to go with the proposals
for now; any can be revisited during implementation.

1. **Syntax:** the positional `channels = [...]` array (Shadertoy's four slots), `""` for
   a gap, named `[textures]` and a film-wide default.
2. **Defaults:** Shadertoy's: `vflip = true`, `filter = "mipmap"`, `wrap = "repeat"`,
   `srgb = false` (see [Colour](#colour)).
3. **Contract:** 0.1.0 → 0.1.1. Additive, so films that say `shotglass = "0.1"` keep loading.
4. **Limits:** 4096 px per side and a 256 MB resident budget, not configurable in version 1.
5. **16-bit and float images:** 8 bits per channel only for now, with a warning. 16-bit
   (as `RGBA16F`, which WebGL 2 can filter) waits for a film that needs it.
6. **Golden case:** a dedicated `tests/films/channels/` film with its own small CC0
   images made for the test. The demo and the template keep their looks.
7. **Kit textures:** yes (not done yet: the kit has no `textures` library). shotglass-kit ships a `textures` library of MIT or CC0 images
   (RGBA noise at 64, 256 and 1024, a grey noise, a pattern or two) as the free
   replacement for Shadertoy's built-in media. Each image's source and licence goes in
   the library (`library.toml` `licences`, plus a note per file where they differ). This
   is a kit issue, independent of the engine PRs; until it lands, films use their own
   images.
8. **Between PR 1 and PR 2:** a film that uses `channels` is refused on load
   (`play`, `still`, `strip`, `export`, `wav`) with "iChannel textures arrive in the next
   version of shotglass". `check` validates it fully, so films can be prepared. PR 2
   removes the refusal.
9. **Missing vs broken files:** a missing file refuses the whole new version (like a
   missing include); a broken or half-saved file falls back to the last working texture.

## Implementation notes

Where the implementation settled something the design left open:

- **One PR, not two.** The syntax/`check` part and the renderer landed together, so there
  is no "textures arrive in the next version" refusal (decision 8 never shipped).
- **Every texture gets mipmaps,** whatever its `filter`, and the texture key has no
  "mipmapped" bit. That is what makes "one file used with `nearest` in one shot and
  `mipmap` in another is uploaded once" true; the cost is a third more memory for
  textures that are never minified, and the budget counts it.
- **`[textures]` and `channels` entries are parsed by hand** from TOML values rather than
  with an untagged serde enum, so an unknown option names itself
  (``unknown option `filtr` ``) instead of serde's "did not match any variant".
- **Missing files and the per-shot budget are checked in `Movie::load`** (with "did you
  mean"), so the player refuses such a version before the renderer sees it; `set_movie`
  still refuses a file that vanished in between.
- **The freeze key also holds the outgoing shot's keys,** so a frozen frame drawn with a
  fallback texture is redrawn once the edited texture is uploaded.
- **Headless commands fail on a broken texture** (`load_all` reports it like a compile
  error) rather than rendering black; the player shows black only without a previous
  working version, as designed.
- **Decode warnings** (16-bit, an extension that disagrees with the content) are printed
  by `check`; the player doesn't show them.
