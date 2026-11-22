# Design: layers and compositing

Status: **proposal** (nothing implemented; decisions to confirm are listed under
[Open questions](#open-questions)). Builds on the iChannel design
([ichannel.md](ichannel.md)): layers reuse its channel syntax, its texture gating and
its contract-bump rules.

## Summary

Today a film is one track: a sequence of opaque shots with transitions between them.
This design adds a stack of **layers** above that track, and later **comps**
(pre-composed sequences) that can stand in for a shot, feed a channel, or be a layer.

- **One concept, one list.** A layer is a scene shader (`mainImage`, the same header,
  uniforms, keyframes and `channels` as a shot) that is active over a span of the film
  and is composited over everything below it. A title writes alpha; a grade or grain
  pass reads what's below (`@input`) and writes an opaque result. Both are
  `[[layer]]`s, drawn bottom to top in file order. There is no separate "overlay" and
  "post" syntax.
- **The engine provides plumbing, not looks.** The only compositing the engine does
  itself is premultiplied source-over with an opacity (the identity of layering, in the
  same category as the internal copy pass). Every other blend mode, grain, grade, bloom,
  letterbox, vignette and picture-in-picture placement lives in shotglass-kit libraries
  (`fx/`, `blend/`), referenced like transitions: `type = "fx/grain"`,
  `blend = "blend/screen"`.
- **Tracks on the outside, a pass graph on the inside.** `movie.toml` stays a
  timeline (shots + an ordered layer list with spans). Each frame the engine resolves
  it into a small, pure render plan (a DAG of passes with target lifetimes) that
  `gfx.rs` executes. Backward references (`@<layer id>`, `@<comp>`) give the graph's
  expressiveness without a node-graph syntax.
- **Invariants carry over unchanged.** A frame is usable only when every program and
  texture of every active layer (and every comp they use) is ready; otherwise playback
  keeps the previous frame. One render per film frame; auto scale applies to every
  layer at `scale = "auto"`; fixed-scale layers are part of the movie, so frames stay
  a function of (movie, t, size, scale). Slow layers draw in viewport strips like
  shots. A broken layer edit falls back per layer id.
- **Contract:** additive, so a patch bump per phase (0.1.2 → 0.1.3 for the MVP).
  Films without layers assemble the same sources and render bit for bit as before.

MVP (phase 1): `[[layer]]` with spans, opacity/fades, `@input`, per-layer scale and the
built-in source-over. Then blend modes and `@<id>` references (phase 2), comps and
mattes (phase 3), layer transitions and float targets (phase 4).

Not in this design: temporal effects that read the previous frame (feedback, trails,
real motion blur) — they break deterministic frames; 3D/depth compositing; alpha in
exported video; sound on layers.

## What exists today

- `Movie::at(t)` returns `At::Shot` or `At::Transition { from, to, progress }`.
  `Renderer::render_at` draws one or two scenes into off-screen targets `a`/`b` (or
  the cached `freeze` target) at the scaled size `sw × sh`, then one composite pass
  (the transition program, or the internal `copy`) upsamples into `out` at `w × h`,
  drawn upside down when `set_readback(true)`.
- Targets are RGBA8, allocated at full size; a frame uses the bottom-left `used` part
  (`Target { alloc, used }`), which is how render scale works without reallocating.
  Transitions get `uFromScale`/`uFromMax` to sample only the used part.
- `SCENE_FOOTER` forces alpha to 1: shots are opaque.
- `ShotKeys { scene, trans, channels }` per shot; `keys_ready` requires programs and
  textures `Ready`; `usable(i, blocking)` falls back per shot id (`fallback`);
  `priority` orders compiles and uploads by distance from the playhead.
- `draw_scene` sets the Shadertoy uniforms from shot-local time, binds channels on units
  0–3 with sampler objects (unbound afterwards) and draws in `tiles` viewport strips when
  frames are slow.
- `cue_defines()` (`MOVIE_DURATION`, `CUE_<ID>_START/SETTLED/END`) goes into every
  shader, picture and sound, so they agree on timing.
- `channels_read(src)` already detects which `iChannelN` an assembled scene mentions.
- Transitions resolve `type` with `transition_kind`: `"<library>/<name>"` → `<lib>/<name>.glsl`,
  or a film file ending in `.glsl`.

## Use cases

| use case | how it's expressed |
|---|---|
| A title or lower third over a span that crosses a cut or a dissolve | `[[layer]] scene = "layers/title.glsl"`, `from = "moon"`, `to = "closeup.settled"`, `fade = [0.4, 0.4]` |
| A logo bug for the whole film | `[[layer]] type = "fx/place"`, `channels = ["logo"]` (a `[textures]` image), `scale = 1` |
| Film-wide post chain: grade → grain → letterbox | three `[[layer]]`s with `type = "fx/grade"`, `"fx/grain"`, `"fx/letterbox"`, no span |
| Post only on part of the film (a flashback in sepia) | `[[layer]] type = "fx/grade"`, `from = "flashback"`, `to = "flashback"` |
| Opacity animation | `opacity = { keys = [[0, 0], [2, 1]] }` (layer-local seconds, any ease) or the `fade` shorthand |
| Blend modes (screen a light leak, multiply a paper texture) | `blend = "blend/screen"` (phase 2, kit library) |
| Bloom (bright pass → blur → add) | three layers: `bright` (writes to its own output, `opacity = 0`), `blur` reading `@bright`, then `blend = "blend/add"`; or one `fx/bloom` single-pass approximation |
| Picture-in-picture | a comp rendered at its own size, placed by `type = "fx/place"` with `channels = ["@insert"]` (phase 3) |
| A pre-composed group of shots used as one shot | `[[shot]] comp = "intro"` (phase 3) |
| A sequence as a texture (a TV screen in a scene) | `channels = ["@tv"]` on a shot (phase 3) |
| Mattes (a title that reveals a sky; a shot seen through letters) | `matte = { source = "@letters", channel = "luma" }` (phase 3) |
| A title that irises in with the kit transition | `transition_in = { type = "transitions/iris", duration = 0.8 }` on the layer (phase 4) |

## Model

### The stack

```
 out  ◄── final copy (flip for readback)
  ▲
 layer N        ┐
 …              │ composited bottom to top, each only within its span
 layer 1        ┘
  ▲
 picture track: shots + transitions (unchanged)   ← or a comp, recursively
```

The picture track is layer 0 and is always opaque. Layers 1..N sit above it in file
order. At time `t` the *active* layers are those whose span contains `t`; inactive
layers cost nothing.

Each layer:

1. is a scene shader (`scene` = a film file, or `type` = a library pass / `.glsl` file,
   or later `comp` = a comp), with `uniforms`, `params`, `channels`, `time_offset`,
   `time_scale` as on shots;
2. writes **straight alpha** in `mainImage` (`fragColor = vec4(col, a)`, the natural
   Shadertoy way); the layer footer premultiplies and multiplies by the opacity. A
   layer that never writes alpha is opaque (the footer starts from alpha 1, as today);
3. may read the image below it through a channel bound to `@input`;
4. is composited onto the image below with `blend` (default `"over"`: premultiplied
   source-over) at `opacity`.

A grade is then simply an opaque layer that reads `@input`; at `opacity = 0.5` it is a
half-strength grade. A title is a layer with alpha that reads nothing.

### Time

- Layer-local time runs from 0 at the start of its span. `iTime = time_offset +
  local * time_scale` (as `Shot::scene_time`). `iShotTime`, `iShotDuration` and
  `iShotProgress` mean "this item" and are layer-local for layers. `iMovieTime`,
  `iFrame`, `iTimeDelta` are the frame's, unchanged.
- Keyframes (`uniforms`, `opacity`) are in layer-local seconds, evaluated with
  `anim.rs` exactly like shot keyframes. Editing keys changes uniforms only: no
  recompile.
- Cue defines gain `CUE_<LAYER>_START` and `CUE_<LAYER>_END` (movie seconds), so sound
  can sync with a title (a whoosh when it comes in). Layer ids share the shot id
  namespace (they must be unique across both: they are also the fallback keys and cue
  names). As with shots, editing a span changes the cue text and so recompiles every
  program; that's the existing cost of timing edits.

### Relationship to transitions

Transitions stay what they are: the picture track's (and later each comp's) cuts. The
transition interface (`getFromColor`/`getToColor`/`progress`/`ratio`, `transition(uv)`)
is unchanged. Layers sit above the track, so a title spanning a dissolve stays steady
while the shots dissolve underneath, and a film-wide grain is applied once to the
dissolved image (not twice, as including grain in each scene does today).

Phase 4 lets a layer *enter and leave* with a transition, reusing the interface as is:
`from` = the image below without the layer, `to` = the image with it. No new contract.

### Referencing outputs: `@` channel sources

Channel entries (ichannel.md) are a `[textures]` name, a film path, or a table. This
design adds sources that start with `@`, which can't be a texture name (identifiers)
or a path (paths contain `.` or `/`):

| source | meaning | phase |
|---|---|---|
| `@input` | the image below this layer (all layers under it, composited) | 1 |
| `@picture` | the picture track before any layer | 2 |
| `@<layer id>` | the output of an *earlier* layer, before compositing (its own premultiplied RGBA) | 2 |
| `@<comp name>` | a comp from `[comps]`, at the consumer's time | 3 |

Only backward references are allowed, so the stack is a DAG by construction and the
plan needs no cycle search (comps can nest; `Movie::parse` checks comp cycles).

**Channels never expose a partly used target.** Every `@` source a shader samples has
exactly the resolution `iChannelResolution[n]` reports, and uv 0..1 covers it, so
Shadertoy-style `texture(iChannel0, fragCoord / iResolution.xy)` just works. When a
source was rendered at a lower scale into a bigger allocation, the plan inserts a copy
pass that resolves it into an exactly sized target. (Alternative: hand shaders a uv
scale like `uFromScale`. Rejected: it breaks pasted Shadertoy code and every library
pass would have to remember it.) `@` sources use `filter = "linear"` (default) or
`"nearest"`; `"mipmap"` is refused in the MVP (it would need `glGenerateMipmap` per
frame).

### Ordering

File order of `[[layer]]` is bottom to top. One list rather than separate overlay and
post arrays, because the order between them matters and differs per film (grain usually
goes over titles, a letterbox over grain, a logo bug over the letterbox). TOML loses
the relative order of two different arrays of tables.

### Loop films

- Spans must lie within one period (`from < to`, both in `[0, duration]`): nothing
  crosses the seam, matching "no transition across the seam". A film-wide layer (no
  span) is the common case and is seamless as long as its shader is.
- Layers see wrapped time like shots (`FrameInfo::at` wraps in f64, `iFrame` by frame
  number), so frame n of loop k is still frame n of loop 0.
- A layer `transition_in` at 0 or `transition_out` at the end is allowed (it doesn't
  cross the seam; it repeats every period).
- No previous-frame inputs, ever: they'd make loop k differ from loop 0.

### Comps (nested sequences, phase 3)

A comp is a sequence of its own: shots, transitions and layers, no sound, no
`fps`/`aspect`/`loop` (the film's apply). It renders to a target and can be used:

- **as a shot:** `[[shot]] comp = "intro"` instead of `scene` (comp time = the shot's
  `scene_time(local)`, so `time_offset`/`time_scale` retime it);
- **as a channel:** `channels = ["@tv"]` (comp time = the consumer's `iTime` by default,
  or `{ source = "@tv", time_offset = 2 }`);
- **as a layer:** `[[layer]] comp = "lower-third"` (layer-local time);
- **as a matte source.**

Outside `[0, duration)` a comp holds its first/last frame (as `Movie::at` clamps); a
`loop = true` comp wraps (see open questions). Comp shaders get the film's cues plus
`COMP_DURATION` and `COMP_CUE_<ID>_START/SETTLED/END` in comp-local seconds (a comp can
be used at several movie times, so its own cues can't be movie times). Within one frame,
a comp needed twice at the same (time, size, scale) renders once.

### Mattes (phase 3)

`matte = { source = "@letters", channel = "alpha" | "luma", invert = false }` multiplies
the layer's premultiplied output by the matte's alpha or Rec. 709 luma in the layer
footer, before compositing. The source is an earlier layer or a comp. This is plumbing
(a multiply by a defined quantity), not a look. Fancier mattes (feathered, chroma keys)
are kit passes that read the source as an ordinary channel.

## movie.toml

### Syntax

The engine contract stays in `shotglass.toml` (`shotglass = "0.1.3"`); libraries too:

```toml
# shotglass.toml
shotglass = "0.1.3"
[libraries]
transitions = { source = "https://github.com/antonmarsden/shotglass-kit", subdir = "transitions" }
fx          = { source = "https://github.com/antonmarsden/shotglass-kit", subdir = "fx" }
blend       = { source = "https://github.com/antonmarsden/shotglass-kit", subdir = "blend" }   # phase 2
```

```toml
# movie.toml — shots exactly as today, then layers, bottom to top.

[[layer]]
id = "title"
scene = "layers/title.glsl"     # a film file: mainImage writes vec4(col, alpha)
from = "moon"                   # with shot "moon" (its start)
to = "closeup.settled"          # until the iris into "closeup" has finished
fade = [0.4, 0.6]               # fade in / out (s); multiplies `opacity`
scale = 1                       # full output resolution: crisp text (default "auto")
uniforms.uSize = 0.08
channels = ["font"]             # a [textures] name, as on shots

[[layer]]
id = "grade"
type = "fx/grade"               # library pass, resolved like a transition type
params = { lift = [0.02, 0.0, 0.04], gain = 1.05 }   # #defines, as transition params
uniforms.uExposure = { keys = [[0, -2.0], [3, 0.0]] } # animated: declared as a uniform

[[layer]]
id = "grain"
type = "fx/grain"
scale = 0.5                     # half resolution, whatever the auto scale
opacity = 0.6

[[layer]]
id = "bars"
type = "fx/letterbox"
params = { aspect = 2.39 }
from = "closeup"                # letterbox from the close-up to the end
```

### Fields

| field | meaning | default |
|---|---|---|
| `id` | required; unique across shots and layers (fallback key, cue name) | – |
| `scene` / `type` / `comp` | exactly one. `scene`: a film file (like a shot). `type`: `"<lib>/<name>"` or a film `.glsl` (like a transition). `comp`: phase 3 | – |
| `from`, `to` | span; a number (movie seconds) or `"<shot>"`, `"<shot>.start"`, `".settled"`, `".end"`, `"<layer>.start"`, `".end"`, each with an optional `+ s` / `- s` offset. A bare shot id means its start for `from` and its end for `to` | film start / end |
| `opacity` | float or keyframes (layer-local), 0..1 | 1 |
| `fade` | `[in, out]` seconds, smooth ease, multiplied into `opacity`; `in + out ≤` span | `[0, 0]` |
| `blend` | `"over"`, or `"<lib>/<name>"` / film `.glsl` (phase 2) | `"over"` |
| `scale` | `"auto"` (the player's auto scale, as shots), or a number in (0, 1]: a fixed fraction of the output size | `"auto"` |
| `uniforms`, `params` | as shots (`uniforms`, animatable) and transitions (`params`, `#define`s with `#ifndef` defaults in the pass) | – |
| `channels` | as shots, plus `@` sources. The film-wide `channels` default applies to shots only | none |
| `time_offset`, `time_scale` | as shots | 0, 1 |
| `matte` | phase 3 | – |
| `transition_in`, `transition_out` | phase 4, the transition table as on shots (no `freeze`) | – |

A library pass declares its animatable inputs as uniforms. The film may leave them out,
so for layers the engine adds `#define SG_HAS_<name>` for each uniform the film sets,
and a pass can default the rest:

```glsl
#ifndef SG_HAS_uExposure
const float uExposure = 0.0;
#endif
```

(Only layer sources get these defines, so shot sources and their program hashes don't
change.)

`[comps]` (phase 3) names comps the way `[textures]` names images:

```toml
[comps]
intro = "comps/intro.toml"                                # a file with [[shot]]/[[layer]]
insert = { file = "comps/insert.toml", size = [640, 360] } # own resolution (for PiP)
tv = { file = "comps/tv.toml", loop = true }
```

Paths inside a comp file are film-relative, like everything else (`path_rule`).

### Validation (`Movie::parse`)

Following the "movie.toml field" rules: serde structs with `deny_unknown_fields`, a
resolved `Layer` struct, validation in `Movie::parse`, unit tests, README and
`templates/new/movie.toml` (a commented-out layer).

- exactly one of `scene`/`type`/`comp`; `id` present, unique across shots and layers;
- spans: shot/layer references exist (the error lists the ids), offsets parse,
  `0 ≤ from < to ≤ duration` after resolution; loop films: nothing outside one period;
- `fade` fits the span; `opacity` keys in 0..1 (clamped with a warning otherwise);
- `scale` is `"auto"` or in (0, 1];
- `@` sources: `@input` only on layers; `@<layer>` only for an earlier layer; `@<comp>`
  names an entry of `[comps]`; mipmap filtering refused; comp references acyclic,
  nesting depth ≤ 4 (each level holds targets);
- `contract_warning` lists "layers, which need shotglass 0.1.3" (and comps' version).

## Engine contract

Additive, so a **patch bump in 0.x per phase**: 0.1.2 → 0.1.3 (layers), then 0.1.4
(blend interface, `@<id>`), 0.1.5 (comps, mattes). Old films keep loading
(`shotglass = "0.1"`); a film using layers should say `0.1.3` so an older engine says so
instead of "unknown field `layer`".

What becomes contract:

1. **Layer programs:** the scene header (unchanged), `mainImage` writing straight alpha,
   premultiplied storage, `SG_HAS_<uniform>` defines, `iShot*` meaning layer-local.
2. **`@` channel sources** and the guarantee that a sampled source has exactly
   `iChannelResolution` pixels.
3. **Layer cue defines** (`CUE_<LAYER>_START/END`), later `COMP_*`.
4. **The blend interface** (phase 2), a sibling of the transition interface:

   ```glsl
   // provided by the engine
   uniform float opacity;          // the layer's opacity (fades included)
   uniform float ratio;            // width / height
   vec4 getBase(vec2 uv);          // the image below, opaque
   vec4 getLayer(vec2 uv);         // this layer, premultiplied
   // written by the library: the new image (alpha is ignored, the result is opaque)
   vec4 blend(vec2 uv);
   ```

   It isn't the transition interface reused because the alpha semantics differ: a
   transition mixes two opaque images, a blend must honour the layer's alpha.
5. **Source-over** as the default composite and **matte multiply**: plumbing in the
   layer footer and GL blend state, like the internal `copy` pass. These are the only
   new engine-side GLSL lines; they define what "a layer at opacity o" means, and every
   look is in a library. The kit gets `fx/` (grade, grain, vignette, letterbox, place,
   bloom) and `blend/` (multiply, screen, add, overlay, soft light, difference) with
   `library.toml` peer requirements `shotglass >= 0.1.3` / `0.1.4`.

Unchanged: `SCENE_HEADER`, `SCENE_FOOTER` for shots, the transition interface, sound.
A film without layers assembles exactly today's sources, so program hashes, the on-disk
program cache and every golden reference stay valid.

## Renderer

`gfx.rs` takes one change at a time (CONTRIBUTING.md), so the plan is built by a new
pure module, `src/compose.rs`, and `gfx.rs` only executes it.

### The frame plan (`compose.rs`, no GL)

`plan(movie, t, size, scale) -> Plan`: the list of passes for this frame and the targets
they read and write, with lifetimes. Kinds:

| pass | reads | writes |
|---|---|---|
| `Scene` (shot, layer, comp shot) | channels (textures, `@` sources) | a scratch target at its scale |
| `Transition` / `Copy` (picture track composite) | two scene targets | working target `W0` at full size |
| `LayerOver` (direct) | channels, not `@input` | the current working target, GL blend `ONE, ONE_MINUS_SRC_ALPHA` |
| `LayerOverInput` | `@input` (as `sg_below`) + channels | the other working target; the footer composites `p + below * (1 - p.a)` |
| `LayerScaled` + `CopyOver` | layer at its scale → scratch; then copy with GL blend | current working target |
| `Blend` (phase 2) | working target + layer scratch | the other working target |
| `Resolve` | a scaled render | an exactly sized target (for `@` sources) |
| `Final` | last working target | `out` (flipped for readback) |

Which `Layer*` path a layer takes is decided from its assembled source with the existing
`channels_read` and its scale; the plan is cached per (movie generation, active layer
set, scale).

The footer composite samples a dedicated `sg_below` sampler on unit 4 (and `sg_matte` on
unit 5), not `iChannel0`, so a layer may bind its channels however it likes, and a shader
never samples the target it renders into.

With no active layer the plan is exactly today's path (scene(s) → composite → `out`),
so films without layers do no extra pass.

### Off-screen targets and memory

Targets come from a small pool keyed by allocation size instead of the fixed
`a`/`b`/`freeze`/`out` slots; the plan's lifetimes give the peak number alive.

| frame | RGBA8 targets alive | at 1920×1080 (8.3 MB each) |
|---|---|---|
| today, transition with freeze | a, b, freeze, out = 4 | 33 MB |
| MVP, transition + layers | + W0, W1, one scratch = 7 | 58 MB |
| phase 2, each retained `@<id>` output | + 1 while referenced | +8.3 MB |
| phase 3, per comp in use | + its output (+ its freeze), scratch shared | +8.3–17 MB |

At 4K multiply by 4. `check` reports the peak target memory of the worst frame at the
export size, and warns above 512 MB. Comps at their own `size` cost their size. The
256 MB texture budget is separate (images, not render targets). `ensure_target`'s
"reallocate on size change" stays; the pool shrinks when a new movie version needs fewer.

### Render scale per layer

- `scale = "auto"` layers (and the picture track, and comps used as shots) follow the
  frame's `info.scale`. Fixed-scale layers ignore it.
- The auto scale controller is unchanged: its budget is a film frame and it is driven by
  the whole frame's GPU time (one timer around all passes, as now; no per-layer queries,
  which would be extra driver round trips). Only auto layers shrink. If fixed-scale
  layers alone exceed the budget, the scale bottoms out at its floor and the player's
  stats line says which layers are fixed and how many pixels they cost.
- Export and `still` render at scale 1, so every layer is full size there. A fixed
  `scale = 0.5` grain is half size in export too: the scale is part of the movie, not
  a performance knob.

### Render once per film frame

Still one `render` per film frame while playing; a frame just has more passes. Layer
compiles and uploads are housekeeping like shot compiles: frames slowed by them are
stalls, not drops, and don't lower the scale.

### Compile and upload priority, `keys_ready`

- `ShotKeys` generalises to per-item keys: a layer has `scene` (its program),
  `blend` (phase 2), `trans_in`/`trans_out` (phase 4) and `channels`. Comps carry the keys
  of their shots and layers.
- `priority` adds every layer's programs and textures with distance = distance from `t`
  to its span (0 inside it); a comp's keys get the minimum distance over its users.
- **A frame is usable only if the picture track's needed shots, every active layer, and
  every comp they reference (recursively) are ready**, each through its own fallback.
  While playing, `render(.., blocking = false)` returns `None` otherwise and the previous
  frame stays up. Drawing the frame without a late layer was considered and rejected: a
  title popping in a second late is a frame `still`/`export` never render.
- `usable(.., blocking = true)` compiles and loads layer programs and textures on the
  spot as it does for shots, counted in `blocking_texture_loads` (still 0 while playing).
- Layer programs are compiled as soon as the movie loads, like shots, nearest first;
  film-wide layers are distance 0, so they compile with the first shot.

### Strips for slow frames

Layer and comp scenes go through `draw_scene`, so they are drawn in the same viewport
strips (never scissored ones) when frames are slow. Composite, blend, resolve and final
passes are single cheap draws. `LayerOverInput` is safe in strips because it reads a
different target than it writes. The strip count stays global, from the frame's GPU time.

### Broken edits: per-item fallback

`fallback` is keyed by item id: `wall` (shot), `title` (layer), `intro/wall` (a shot in a
comp, phase 3). A layer whose new version fails to compile keeps its last working program
on screen; a broken or half-saved image falls back per texture. Missing files (a
missing layer scene, an unknown library pass, a missing comp file) refuse the whole new
version, as for shots. Deleting a layer removes it from the next version at once (no
fallback to keep it alive).

### GL state

Layer draws restore what they change: GL blending is enabled only around `LayerOver` and
`CopyOver` and disabled after; sampler objects on units 0–5 are unbound after each scene
draw (as `draw_scene` does for 0–3); textures on units 4–5 are unbound so egui never sees
them. No new readbacks, `glGet`s, queries or fences on the render path.

## Deterministic frames

A frame still depends only on (movie, t, size, scale): layer spans, keyframes, fixed
scales and comp sizes are part of the movie; layer time is derived from `t` the same way
as shot time (`frame_at`, f64 wrap for loop films); there is no previous-frame input. So
`play`, `still`, `strip` and `export` agree. The selftest pins this for a film with
layers (below).

Precision note: working targets are RGBA8, as today. A long chain quantises at each step
and premultiplied 8-bit loses colour where alpha is small. Fine for titles and grain;
RGBA16F working targets are a phase 4 opt-in (see open questions).

## `check --web` (GLSL ES 3.00)

`validate_movie` assembles and checks every new program: each layer (scene header +
layer footer), each blend program, each comp's programs, in both dialects. The layer
footer uses only `texture()` on `sampler2D` and plain arithmetic, valid in 330 core and
300 es. RGBA8 is renderable in ES 3.00; RGBA16F targets need `EXT_color_buffer_float`
on WebGL 2, which is why float targets stay opt-in. Every kit `fx/` and `blend/` file
must pass `check --web` (the kit's CI and the canary).

## Live reload, errors, player

- The watcher already reloads on `glsl|toml`; comp files are `.toml` and are added to
  `referenced_files`.
- Errors name the item: `layer "title" (layers/title.glsl): …`, `layer "title": to =
  "closup.settled": no shot or layer "closup" (shots: wall, moon, closeup, end)`.
- Player (phase 2): the timeline shows a row per layer under the shots (span, fades),
  and a mute/solo toggle per layer for previewing. A muted layer makes the preview
  differ from `export`, so the transport shows "layers muted" while any is.

## Testing

### Unit tests (`movie.rs`, `compose.rs`)

- Parsing: every field, defaults, `deny_unknown_fields`, exactly-one-of
  `scene`/`type`/`comp`, duplicate ids across shots and layers.
- Span resolution: numbers, `"moon"` (start for `from`, end for `to`), `.settled`,
  `.end`, offsets, references to earlier layers; out-of-range and inverted spans; loop
  films (inside one period, refused across the seam).
- `fade` + `opacity` evaluation at the edges; `Movie::layers_at(t)` returns the active
  layers in order with their local times, at span boundaries (half-open `[from, to)`),
  and in wrapped time for loop films.
- `@` sources: `@input` refused on shots, forward `@<layer>` refused, unknown comp,
  comp cycles and depth.
- `cue_defines` includes layer cues only when layers exist (a film without layers has
  today's text, byte for byte); `SG_HAS_*` only in layer sources.
- `contract_warning` names layers/comps.
- Plan: path choice per layer (direct, over-input, scaled, blend), no plan reads a
  target before it is written or writes the one it reads, peak target count matches the
  table above, an empty layer list gives today's plan. Property test (proptest, as the
  existing ones): for random stacks and times, those properties hold and the number of
  passes is linear in active layers.

### Selftest checks (fixture films, mutation-checked)

1. **"layers: source-over"**: a solid picture plus a layer writing a known straight-alpha
   colour at a known opacity: read back and compare to the exact premultiplied result
   (±1/255). Break the premultiply or the blend func and it fails.
2. **"layers: @input"**: a layer that inverts `@input` at opacity 0.5 matches the
   arithmetic; and a layer binding `@input` to `iChannel2` gives the same frame.
3. **"layers: spans and fades"**: frames just before, inside and just after a span, and
   mid-fade.
4. **"layers: scale"**: a `scale = 0.5` layer and the resolve path: `@` sources report
   exact `iChannelResolution` and sample without stale edges at auto scale 0.3.
5. **"layers: never block while playing"**: a layer program held `Pending` (and a layer
   texture not uploaded) makes non-blocking `render` return `None`; `blocking_texture_loads`
   and `blocking_audio_reads` stay 0 while playing.
6. **"layers: fallback"**: a broken edit of a layer keeps its last working version; a
   missing layer file refuses the new version.
7. **"layers: strips"**: `set_tiles(Some(n))` draws layer scenes in n strips and the frame
   equals the single-draw frame.
8. **"layers: deterministic"**: player-style render at frame n equals `still`/export
   readback; for a loop film with layers, frame n of loop 3 equals frame n of loop 0.
9. **"layers: GL state"**: unpack/pack state and sampler bindings restored, blending
   disabled after the frame (extends the existing state checks).
10. Phase 2–4: blend fixture (a fixture-local blend shader; no kit dependency), `@<id>`
    lifetimes, comp-as-shot equals the comp rendered as a film, comp-as-channel at its
    `size`, matte alpha/luma/invert, layer transition equals the transition drawn by hand.

### Golden

- `tests/films/layers`: three shots with a cut and a dissolve, a title layer spanning the
  dissolve with a fade, a film-local vignette-like `@input` layer and a fixed-scale
  layer. Shaders are film-local (not kit) so the golden only moves when the engine does.
  An 8-frame strip chosen to hit: before the title, mid-fade, mid-dissolve under the
  title, after it. Blessed in the same commit as the MVP, PNG inspected.
- The demo and template goldens must not move in the MVP (proves films without layers
  are untouched). Adding a kit grain/letterbox to the demo is a later, deliberate
  re-bless, after the kit `fx/` library is pinned.
- The canary gets `tests/films/layers` with a kit `fx/` variant once the kit has it.

## Phased plan

Each phase is one issue and one branch (CONTRIBUTING.md); `gfx.rs` changes are serialised.

**Phase 1 — MVP: layers with source-over (contract 0.1.3)**
1. `movie.rs`: `[[layer]]` (serde + resolved `Layer`), span grammar, `fade`/`opacity`,
   `scale`, `@input` in channels, layer cues, `layers_at`, contract warning, unit tests.
2. `compose.rs`: the pure plan + unit/property tests.
3. `gfx.rs`: layer program assembly (layer footer, `SG_HAS_*`), target pool, plan
   execution (direct / over-input / scaled), priority, `keys_ready`/fallback per item,
   final copy. `validate.rs` and `check` (peak target memory).
4. Selftest checks 1–9, golden `tests/films/layers`, README section "Layers",
   commented layer in `templates/new/movie.toml`.
5. shotglass-kit (separate PR): `fx/` library — grade, grain, vignette, letterbox, place —
   each passing `check --web`.

**Phase 2 — blend modes and references (0.1.4)**: the blend interface, kit `blend/`,
`@picture` and `@<layer>`, kit `fx/bloom` as a documented layer recipe, timeline rows and
mute/solo in the player.

**Phase 3 — comps and mattes (0.1.5)**: `[comps]`, comp files, `[[shot]] comp`,
`[[layer]] comp`, `@<comp>` channels, comp cues, per-frame comp memoisation, `matte`.

**Phase 4 — polish**: layer `transition_in`/`transition_out` (existing interface), opt-in
RGBA16F working targets (`precision = "half"`), maybe alpha export of a comp.

## Alternatives considered

| alternative | for | against | verdict |
|---|---|---|---|
| **A free node graph** (`[[node]]` with named inputs, Nuke/Shadertoy-buffers style) | maximal flexibility; multi-pass effects natural | verbose for the common "title over shots" case; nodes have no time, so spans need a second concept; harder errors | rejected as syntax; the internal plan *is* a graph, and `@<id>` gives DAGs |
| **Separate `[[overlay]]` and `[[post]]` arrays** | self-describing names | fixed order between them (titles over or under grain?); two concepts for one mechanism | rejected |
| **Parallel shot tracks** (`track = 2` on `[[shot]]`, NLE style) | familiar from Premiere | needs gaps and alpha on shots, changes `Movie::at`, transitions per track; spans + opacity do the same with less | rejected |
| **Status quo: `#include` the post effect in every scene** | no engine change | duplicated per shot; applied twice during dissolves; can't span cuts; no separate scale | what we have; the motivation |
| **All blend modes as GL blend equations** (multiply, screen, add via `glBlendFunc`) | no shader pass | only a few modes; picking modes is a look in the engine; no soft light/overlay | rejected, except source-over |
| **Source-over as a separate composite pass per layer** instead of footer/GL blend | footer stays trivial | an extra full-screen pass and target per layer | rejected for "over"; used for scaled layers and blend modes |
| **Reuse the transition interface for blends** | no new interface | transitions ignore alpha; semantics would be muddled | rejected; a sibling interface instead |
| **Uv scale uniforms for `@` sources** instead of resolve passes | no extra copies | breaks Shadertoy convention and every pass must handle it | rejected |
| **Skip a not-ready layer while playing** | fewer held frames | shows frames export never renders | rejected (determinism invariant) |
| **Multi-pass library effects declared in `library.toml`** (a "pass group" per effect) | one line for bloom in the film | a mini graph language in library.toml, more contract | deferred; `@<id>` recipes first (open question) |

## Open questions

1. **Straight or premultiplied alpha in `mainImage`?** Proposed: authors write straight
   alpha, the footer premultiplies (natural for pasted Shadertoy code; correct filtering
   when a scaled layer is upsampled). Blend shaders see the layer premultiplied.
2. **Default layer `scale`: `"auto"` or `1`?** Proposed `"auto"` (predictable cost), with
   `scale = 1` recommended for text in the README. Text at 30% auto scale looks bad, so
   the alternative is defensible.
3. **Gamma-space vs linear compositing.** Working images are display-referred sRGB-encoded
   RGBA8 today; source-over in gamma space is what most tools default to. Linear blending
   would need float targets (phase 4) — decide with the `precision` option.
4. **Comp time outside its range:** hold (proposed) vs loop vs black; and whether a comp
   can be `loop = true` independent of the film.
5. **Sound in comps:** picture-only (proposed for phase 3) vs comp shot sounds mapped into
   the film's mix with the comp's time mapping.
6. **Multi-pass library effects:** `@<id>` recipes (phase 2) vs `library.toml` pass groups.
7. **Name:** `[[layer]]` vs `[[track]]` vs `[[overlay]]`. Proposed `[[layer]]` (it covers
   post passes too).
8. **`SG_HAS_<uniform>` defines** vs requiring films to set every uniform a pass uses
   (with a clear `check` error). Proposed: the defines, layers only.
9. **Layer transitions in phase 1?** They reuse the existing interface but need an extra
   working target; proposed phase 4 unless a film needs them sooner (fades cover most
   titles).

## Note on shotglass-kit

The kit repository wasn't available read-only to this session, so the packaging proposed
for `fx/` and `blend/` follows how the README and `shotglass.lock` describe the kit's
transitions (a subdir per library, `<lib>/<name>.glsl`, `library.toml` with a
`shotglass` peer requirement). It should be checked against the kit before phase 1.
