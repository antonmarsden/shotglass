# Design: feedback buffers in films

Status: **phase 1 built** (#134): `[[buffer]]`, history, resets, checkpoints; **phase 2
built** (#137): background fill, progressive scrub, `preview_lookback`, adaptive spacing,
`cache = "dense"`, slow-seek hints, per-frame invalidation; user docs in
[buffers.md](../buffers.md). Phase 3 (#138): **buffers in comps built**; warm loops and
`after` are still a proposal. Builds on the iChannel design
([ichannel.md](ichannel.md), which deferred `{ buffer = "A" }` to its own design) and on
layers and comps ([layers-compositing.md](layers-compositing.md), which ruled
previous-frame inputs out of layers).

## Summary

Shadertoy films often keep state from one frame to the next: a pass reads its own
previous output (a **feedback buffer**, drawn with two ping-pong targets). That gives
trails, echo, smear, accumulation, reaction-diffusion, cellular automata, particles and
simple fluids, none of which a pure `mainImage(t)` can do cheaply.

shotglass promises that a frame depends only on (movie, t, size, scale), so playback,
`still` and `export` agree and any frame can be shown on its own. Feedback breaks the
"on its own" part. This design keeps every other guarantee:

- **A buffer is a fold.** Its state at film frame n is its state at the last **reset**
  r ≤ n, stepped once per film frame from r + 1 to n. That is a pure function of
  (movie, n, size), so the frames are still deterministic: playback, `still`, `strip`
  and `export` agree, as before. Only the cost of reaching frame n changes.
- **Resets are part of the movie:** a buffer starts from black (or its `init` pass) at
  the start of its span, and at any reset points it names. Never across a cut unless
  the span says so.
- **State is fixed-size floats**, independent of render scale and of the player's
  window, so the auto scale never changes results.
- **Seeking is the engine's problem, solved with caches.** Checkpoints of the state
  (GPU memory, adaptively spaced under a fixed budget) keep a seek bounded.
  Catch-up happens only while paused or scrubbing, in budgeted slices, so playback
  never blocks and the player never freezes.
- **Contract:** additive (`[[buffer]]`, the `@prev` and `frame = -k` channel sources, buffer-local
  `iFrame`), so a patch bump: the next free version (0.1.9 if #73 hasn't taken it).
  Films without buffers take exactly the old path.

MVP (phase 1): `[[buffer]]` with spans, resets, `@prev`, a history of the last N
states (`frame = -k`), reading textures, text,
footage, `@picture` and other buffers; canonical fold from the reset; GPU checkpoints;
budgeted catch-up in the player. Then background fill and progressive scrub (phase 2),
then buffers in comps, seamless loop films and buffers inside the layer stack (phase 3).

Not in this design: buffers whose state depends on input or wall-clock time (that's
real-time mode, #60, which should reuse this contract); sound feedback (recursive
filters stay a sound-shader limitation); CPU-side simulation state.

## What exists today

- A frame is planned per film frame n (`FrameInfo::at`, `frame_at`): shots, a
  transition, layers bottom to top, comps drawn first into their own targets. Every
  pass reads only things of the same frame.
- `quality`'s `@prev`/`@next` render the neighbour frames independently; footage's
  `FrameMap::source_frame(n)` is a pure function of n. Both rely on frames being
  independent.
- The player renders once per film frame while playing and may skip frames (stalls)
  or jump (scrub, loop wrap); headless commands render the frames they're asked for, in
  any order (`strip` samples 8 frames across the film).
- Off-screen targets are RGBA8 (or float for `quality`), sized by the render scale.

Each of these assumes independence; the sections below say what changes.

## Use cases

1. **Trails / echo over a shot:** blend `@picture` into the decayed previous state.
2. **Accumulation:** paint strokes that build up, a long-exposure look, ink spreading.
3. **Simulation:** reaction-diffusion, Game of Life, a 2D fluid, particles stored in
   texels; several buffers that read each other (Shadertoy's Buffer A–D).
4. **Temporal smoothing:** a buffer that averages noisy raymarched frames (TAA-like),
   reset at every cut.
5. **Echo and frame blend:** distinct past frames rather than a decayed sum: After
   Effects' Echo, Nuke's FrameBlend, strobe trails, stutter, fake motion blur over a
   shutter, and at the far end slit-scan / time displacement (each row or pixel from a
   different past frame). Needs a [history](#history) of the last N frames.

Cases 1, 4 and 5 forget their past quickly; cases 2 and 3 never do. The seek strategy must
work for both, and be exact for both.

## Model

### Buffers

```toml
[[buffer]]
id = "trail"
scene = "buffers/trail.glsl"       # mainImage writes the new state
from = "moon"                      # span, as for layers; default: the whole film
to = "closeup.end"
size = [960, 540]                  # or scale = 0.5 (of the output size); fixed, never "auto"
format = "rgba16f"                 # default; "rgba32f" for simulations, "rgba8" for picture history
channels = ["@prev", "@picture"]   # iChannel0 = my previous state, iChannel1 = this frame's picture
reset = ["closeup"]                # also start over at the start of shot "closeup"

[[layer]]
id = "trails"
scene = "layers/show-trail.glsl"
channels = ["@trail"]              # this frame's state, after its step
```

- **Order:** buffers step in file order, after the picture track and before any layer.
  A buffer may read the current state of an *earlier* buffer (`@a`) and the previous
  state of any buffer, itself included (`@prev`, or `{ source = "@b", frame = "prev" }`).
  So Buffer A–D chains work and the per-frame graph stays a DAG.
  To get trails of something a layer would draw (a title, a brush tip), render it in a
  **comp** and read `@<comp>`: comps are drawn before the film, so they're available to
  buffers in phase 1.
- **History:** `history = N` keeps a ring of the buffer's last N states, read as
  `{ source = "@b", frame = -k }` for 1 ≤ k ≤ N (`@prev` and `frame = "prev"` are
  `frame = -1`; without `history`, N = 1). See [History](#history).
- **Inputs:** textures, text, footage, comps, `@picture` and buffers. Not `@input` or
  `@<layer>`: buffers step before layers. A buffer that reads `@picture` makes the
  picture part of its fold (see [Invalidation](#invalidation)).
- **Readers:** layers and later buffers read `@<buffer>`, at the buffer's size
  (`iChannelResolution`). Shots and transitions can't: they're drawn before buffers step.
- **Span:** `from`/`to` like layers. Outside its span a buffer is inactive and reads
  black; it resets at its span start.
- **Resets:** the span start, plus `reset = [...]`: shot ids or layer ids (their start),
  span-style expressions (`"moon + 2"`), or film seconds. Each is resolved to a film
  frame once at `Movie::load`. `reset = "cuts"` resets at every shot start inside the
  span (the TAA case).
- **Init:** at a reset the previous state is black, unless `init = "buffers/seed.glsl"`
  names a pass drawn once (it sees the frame's uniforms and channels, but no `@prev`).
- **Time:** `iTime` etc. are span-local like a layer's, and **`iFrame` counts frames
  since the last reset** (0 on the reset frame). `iTimeDelta` is `1/fps` exactly.
  `iMovieTime` and the cue defines are as usual.

### The fold

For a buffer with resets R and film frame n inside its span, let r = max{ x ∈ R : x ≤ n }.

```
state(r) = step(r, init)                 # init = black or the init pass
state(m) = step(m, state(m − 1))   for r < m ≤ n
```

`step(m, s)` draws the buffer's scene for film frame m with `@prev` = s. Every input is
read at frame m. This is the **canonical** state; every command shows it. The cost of
frame n is n − r steps from nothing, or fewer from a cache.

Consequences:

- **No step is ever skipped.** If playback stalls and goes from frame n to n + 3, the
  buffer steps n + 1, n + 2 and n + 3 before frame n + 3 is drawn. A buffer is
  integrated over film frames, never over displayed frames.
- **Size and format are part of the movie**, so `--size` on `still`/`export` doesn't
  change state unless the film uses `scale = …` (then the state is that fraction of the
  output size the command uses, like a fixed-scale layer).
- **Render scale doesn't touch buffers.** They never draw in strips at a lower scale;
  slow steps are drawn in viewport strips at full buffer size, like slow frames.

### History

One frame of feedback (`@prev`) covers every effect that remembers through a decayed
sum: trails, smear, accumulation, TAA. Effects that need *distinct* past frames read a
history:

```toml
[[buffer]]
id = "hist"
scene = "buffers/copy.glsl"        # copies @picture: the state is just the picture
scale = 0.5
format = "rgba8"
history = 8
channels = ["@picture"]

[[layer]]
id = "echo"
scene = "layers/echo.glsl"
channels = ["@input", "@hist", { source = "@hist", frame = -2 }, { source = "@hist", frame = -4 }]
```

How many frames typical effects need:

| effect | frames |
|---|---|
| trails, smear, accumulation, TAA / denoise | 1 (`@prev` + decay) |
| frame differencing, datamosh-like | 1–2 |
| frame blend / fake motion blur | 2–8 |
| discrete echo, strobe trails, stutter | 4–16 |
| slit-scan / time displacement | 30–120 |

- **The ring:** N + 1 targets (the N past states plus the one being drawn), rotated
  every step; no copies. `frame = -k` reads the state of film frame n − k.
- **Before a reset**, `frame = -k` with n − k < r reads what `@prev` reads on the reset
  frame: black, or the `init` state. So the fold stays exact and loop films stay
  seamless-or-reset as before.
- **Limited by memory, not count:** the ring is required memory ((N + 1) × state size,
  see [Memory](#memory)), checked at load, with a sanity cap of `history ≤ 64`. Full-HD
  float history fails at load with a message; slit-scan at half size in `rgba8` fits
  (960×540 RGBA8 × 60 ≈ 120 MB).
- **`format = "rgba8"`** is allowed on any buffer (picture history doesn't need floats);
  it halves the memory of `rgba16f`. Simulations should stay on float formats.
- **Checkpoints of a buffer with history** hold the whole ring, so they're N× bigger and
  spaced further apart under the same budget. A **pure history buffer** (one that
  doesn't read its own or another buffer's past: it only copies its inputs) needs no
  checkpoints at all: its state at n depends only on frames n − N .. n, so a seek
  rebuilds it exactly in N steps. The engine detects this from the channels (no `@prev`,
  no `frame = -k` reads) and skips its checkpoints.
- **Arbitrary random access** to the past (more than N frames back, or into the future)
  is out of scope: footage already has it through re-timing, and for shaders it would
  mean rendering past frames on demand.

### Buffers in the layer stack (phase 3)

Some effects want a partly composited picture: temporal denoise or smear of the final
picture (titles and grade included), or trails of one layer without wrapping it in a
comp. Phase 3 adds `after = "<layer id>"`: the buffer steps right after that layer is
composited and may read `@input` (the composite so far) and `@<layer>` of earlier
layers; later layers read it as usual. A field is needed because TOML loses the
relative order of `[[buffer]]` and `[[layer]]` entries. It's additive: buffers without
`after` keep stepping before all layers, so phase 1 doesn't block it.

### Loop films

The period's first frame (frame 0) is always a reset, so loop k equals loop 0 and the
"shaders only see time within one period" invariant holds. That means a buffer is
visibly empty at the seam. A seamless loop needs the state at frame 0 to equal the state
after a whole period: phase 3 adds `loop = "warm"`, which defines state(0) as the fold
from frame 0 of a *previous* period (one period of pre-roll, computed once and
checkpointed). That's exact for effects that forget and a fixed point only by luck for
simulations, so it stays opt-in.

### Comps

A comp's time is its user's and can jump (a `time_offset`, a hold, a wrap). Phase 1
refused `[[buffer]]` in comp files; phase 3 (#138, built) defines a comp buffer's state
as a **fold over comp-local frames**: its state at comp frame c is the fold from its
last reset r ≤ c (comp-local spans, resets and seconds; a loop comp's frame 0 is a
reset), so it's a function of the comp frame only, whoever shows the comp. A comp is
then exactly what it would be as a film of its own, buffers included; a hold holds the
state, a jump in comp time is a seek.

That leaves "one state per instance" against "used once" as a question of cost only.
Each comp gets **one fold** (rings, cursor, checkpoints), like the film's, and a comp
with buffers, directly or through a comp it shows, may be **shown in one place** (one
shot, layer, matte or channel in the film and its comps), checked at load: two uses
could ask for two comp frames in one film frame, which one cursor can't hold. Per-use
folds could lift that later without changing what a frame shows.

- **Order:** a comp instance's plan (`plan_inst`) advances the comp's fold to the comp
  frame before planning the comp's layers. Folds nest: a film buffer reading `@<comp>`
  (or `@picture` with a comp shot) steps the comp's fold from inside its own step;
  that's still canonical, since the comp's state depends only on the comp frame.
- **Size:** a comp buffer's `scale` is of the comp's `size`, else of the film's output
  size, never of the reader's (a film buffer reading the comp draws it at the
  buffer's size).
- **Invalidation:** each fold has its own key: a comp's reads its declaration, its
  buffers' programs and, if they read `@picture` or a comp, its shots (from their start)
  and every comp's items and buffers (frame 0). The film's key includes comps' buffers.
- **Caches:** one checkpoint budget for all folds; eviction scores every fold's
  checkpoints against that fold's own playhead. Progressive scrub and the background
  fill stay on the film's fold: a comp fold advanced from inside a film buffer's step
  must stay canonical.
- **Memory:** `Movie::load` adds each sequence's worst frame.

## Seeking and the caches

Playback forwards from a known state costs one step per frame. Everything else (a seek,
a scrub, `still` of frame 9000, `strip`'s 8 frames, `quality`'s `@prev`) needs the state
at an arbitrary n. Three mechanisms, in the order they're built, all in memory:

### 1. Canonical catch-up (phase 1)

Step from the nearest known state ≤ n (a checkpoint, or the reset). Headless commands
just do it, blocking: `still` of frame n costs n − r steps, `strip` walks forward once
through its sorted frames, `export` steps sequentially anyway.

In the player, catch-up runs **only while paused or scrubbing** (which may already
block) and in **slices**: at most a budget of steps per UI frame (the same idea as the
upload budget), so a long catch-up shows progress ("catching up: 41%") instead of
freezing. Until it's done, the frame isn't usable and the previous frame stays up, like
an unready texture. While playing, a seek forward or back first pauses the fold, catches
up, then resumes; the clock waits (a stall, not a drop).

### 2. Checkpoints (GPU memory)

Every K film frames (default: one second of frames), copy each buffer's state into a
checkpoint texture: a GPU copy, no readback, no sync. A seek restores the nearest
checkpoint ≤ n and steps at most K − 1 frames.

- Filled during playback for free, and while paused by a background walk forward from
  the playhead (budgeted, lowest priority, behind compiles and uploads).
- Kept in an LRU with its own budget (256 MB – 2 GB [from the GPU](#the-checkpoint-budget), 512 MB without it): a 960×540 RGBA16F state is ~4 MB,
  so a minute at one per second is ~240 MB per buffer. See [Memory](#memory) for what is
  evictable and how.
- Checkpoints never go to disk: evicted ones are dropped (see
  [Why no disk checkpoints](#why-no-disk-checkpoints)).

### 3. Progressive scrub (phase 2)

While dragging, the player shows the nearest ready state rather than waiting: the
picture is exact, the buffer is from checkpoint c ≤ n, and the transport says "buffer
approximate". When the drag stops, catch-up refines it to the canonical state. This is
preview-only, like muted layers: `still` and `export` never use it.

An optional `preview_lookback = 2.0` (seconds) lets the player approximate a seek far
from any checkpoint by stepping only the last 2 s from black. Effects that forget
(trails, TAA) look right; simulations don't, so it's off by default and never used
headless.

### Denser caching for a span

An "in-memory bake" is just checkpoints at K = 1, so it isn't a separate feature.
`cache = "dense"` on a buffer asks the LRU for a smaller spacing (down to every frame)
near the playhead, within the same budget: at ~4 MB per state, 512 MB holds about 4 s of
every-frame state at 30 fps, enough to scrub a short passage frame by frame. Raising the
budget in the player's settings stretches that.

## Memory

Buffer memory never grows without bound. Two kinds, treated differently:

- **Required: the ping-pong targets** (two per active buffer, at its size and format;
  N + 1 for a buffer with `history = N`).
  They can't be evicted while the buffer is active. `Movie::load` sums them for the
  worst frame (the most buffers active at once; each comp's worst frame added, #138) and refuses
  the film when that exceeds a fixed limit, with a message naming the buffers and their
  sizes, like `texture::shot_memory` does for textures.
- **Evictable: checkpoints.** They live in their own LRU under a budget sized once at
  startup (see [the budget](#the-checkpoint-budget) below). Eviction
  changes only how long a seek takes, never what it shows: a missing checkpoint means
  catching up from an earlier one or from the reset.

Rules for the checkpoint cache:

- **Separate budgets:** required targets, textures and checkpoints each have their own,
  so one can't starve the others.
- **Eviction order:** farthest from the playhead first. The checkpoint just before the
  playhead and the one just before each reset in view are kept longest, so stepping back
  a little and jumping to a cut stay cheap.
- **Adaptive spacing:** when the budget can't hold one checkpoint per K frames across a
  buffer's span, K doubles (every 2 s, then 4 s, …) for the part farther from the
  playhead instead of thrashing. The worst-case seek is then 2K steps, still bounded.
- **Allocation:** checkpoint textures are reused from a free list (like footage's
  `spare`), so filling and evicting doesn't churn GL allocations.
- **Slow seeks are reported:** when a catch-up would take more than about a second
  (estimated from measured step times), the player says so and suggests what helps: a smaller `size`,
  `cache = "dense"`, or reading a cheaper comp instead of `@picture`.

### The checkpoint budget

The budget only changes how fast a seek is, never what a frame shows, so it may follow
the GPU. It's decided once at startup (limits are asked once, never per frame):

1. **Your setting wins:** the player's settings, or `SHOTGLASS_BUFFER_CACHE_MB`.
2. **Otherwise, from total video memory** where the driver reports it:
   `GL_NVX_gpu_memory_info` (NVIDIA), `GL_ATI_meminfo` (AMD; Mesa drivers often expose
   one of the two). The budget is 1/8 of it, clamped to 256 MB – 2 GB: a 12 GB card gets
   ~1.5 GB, about 3× denser checkpoints than the fallback. Total memory, not free memory:
   free memory moves from moment to moment and isn't reliable enough to plan around.
3. **Otherwise 512 MB** (Apple, the web player, drivers without either extension).

The player shows the budget and where it came from (setting, video memory, fallback).
The texture LRU's fixed 256 MB could follow the same rule later; that's a separate
change.

### Why no disk checkpoints

With B checkpoints over a span of N frames, spacing settles near N/B, so a seek is at
most about N/B steps. At 960×540 RGBA16F (~4 MB) the 512 MB fallback budget holds ~128 (a 1.5 GB one, ~380):

| film | frames | spacing | worst seek, sim step ~0.5 ms | worst seek, step also renders `@picture` (~20 ms) |
|---|---|---|---|---|
| 1 min @ 30 fps | 1 800 | ~14 | ~7 ms | ~0.3 s |
| 10 min @ 30 fps | 18 000 | ~140 | ~70 ms | ~2.8 s |

That's enough for short films and pure simulations. Spilling evicted checkpoints to disk
would help only with long films whose steps are expensive (they read `@picture` or a
comp), with the first scrub after reopening a film, and with many buffers sharing the
budget. The background fill covers the second; for the first, the worst case (~3 s in a
10-minute film) is acceptable while the focus is short films, and a smaller buffer
`size`, `cache = "dense"` or reading a cheaper comp instead of `@picture` all shrink it.
Disk checkpoints also save nothing in the usual edit-scrub loop: an edit changes the
fold key and invalidates them anyway. They'd add a cache format, a location, eviction
and a readback path for little gain, so they're left out. The same goes for persistent
bakes (every frame's state written to disk): revisit both only if real films hit the
limit.

## Invalidation

Each buffer has a **fold key**: a hash of everything its fold reads.

- Its program(s) (scene and init), its declaration (size, format, span, resets).
- Its channels: textures, text and footage by their store keys (path + size + mtime /
  version), comps and `@picture` by the keys of every shot, transition and comp
  instance in its span (an edit to a shot changes the fold of a buffer that reads
  `@picture`), and earlier buffers by *their* fold keys.
- Its uniforms and keyframes, and the film's fps and duration.

A checkpoint is valid only for its fold key, and only for frames whose fold
started at the same reset. A live edit therefore invalidates the edited buffer and every
buffer downstream, from the first reset that sees the change (an edit to shot "closeup"
only invalidates checkpoints at or after the first frame that reads it, when nothing
before reads it). Phase 2 (#137) built it per frame, not per buffer: the fold key is a
list of parts (declarations, sizes, each buffer's keys, each item a buffer's `@picture`
or comp reads), each with the first frame that reads it (a buffer: its span start; a
shot: its start, or its incoming transition's, one frame early; comps and declarations:
frame 0). An edit keeps the cursor and the checkpoints before the earliest changed
part's frame, re-keyed; all buffers share one cursor, so a change to one buffer drops
the others' later states too.

**Broken edits** keep the last working version, as today: the old programs *and* their
checkpoints stay until the new version's frame n is ready.

## Engine contract

- `[[buffer]]` in `movie.toml` (fields above), refused by older engines through the
  contract version.
- Channel sources: `@prev` (buffers only), `@<buffer id>`, and
  `{ source = "@<id>", frame = "prev" | -k }`, classified in `movie::ChannelSource::of`.
- Buffer-local `iFrame` (frames since reset), `iTime` and the span uniforms as for
  layers. No new uniform names: a pasted Shadertoy Buffer A works with
  `channels = ["@prev"]`.
- Buffers get the scene header, but no layer footer: they write raw state (no
  premultiply, any values, including negative and > 1 in float formats).

## Renderer

- **Targets:** two per buffer (ping-pong) (a ring of N + 1 with `history = N`), at the buffer's size and format, allocated on
  first use in the span and freed after it. They count against a buffer budget, not the
  256 MB texture LRU.
- **Plan:** per frame, after the picture and before layers: for each active buffer, step
  if `state_frame + 1 == n`, else schedule catch-up (headless: do it; player: slices, not
  usable yet). `keys_ready` additionally requires every active buffer's state to be at
  frame n.
- **Stalls while playing:** missed frames are stepped before the frame is drawn (the GPU
  cost of k steps, counted as housekeeping, so the auto scale doesn't react).
- **No per-frame syncs:** steps and checkpoint copies are draws and copies on the GPU;
  nothing is read back.
- **Strips:** a slow step is drawn in viewport strips at full buffer size, like slow
  frames.

## Commands

| command | behaviour |
|---|---|
| `play` | forward stepping; checkpoints filled as it goes; budgeted catch-up when paused/scrubbing |
| `still`, `strip` | canonical fold from the nearest checkpoint or reset (blocking); `strip` walks its frames in order |
| `export` | sequential, steps every frame; the YUV pipeline (#116) stays non-blocking |
| `check` | compiles buffers; `check --deep` runs every buffer's fold once (also catches NaN/Inf in float state) |
| `quality` | `@prev`/`@next` take the fold through n − 1 / n + 1; `@reference` (N× render) upscales picture and layers, not the fixed-size state |

## Deterministic frames

The invariant becomes: **a frame depends only on (movie, n, size, scale), where a
buffer's state is the canonical fold from its last reset.** Same stack, same output,
bit for bit, whichever path reached frame n (forward playback, checkpoint, cold
`still`). Selftest pins this.

Across stacks, floats in a long fold can drift further apart than a single frame does
(errors compound). Goldens therefore use short folds (a few frames after a reset) and
the usual tolerances; a long-fold check compares paths on one stack, never a reference
image across stacks.

## `check --web`

Buffers are GLSL ES 3.00 scene passes like any other. Float render targets need
`EXT_color_buffer_float` on WebGL2 (widely available; the web player must check it and
refuse films with buffers without it). RGBA16F is the portable default; RGBA32F
filtering needs `OES_texture_float_linear`, so `rgba32f` buffers default to `nearest`.

## Testing

- **Unit (`movie.rs`):** parsing, span and reset resolution, `reset = "cuts"`, refusal
  of `@prev` outside buffers, forward-only references, buffers in comps (used once), loop
  films reset at 0.
- **Selftest (mutation-checked):**
  - "buffers: every path agrees": frame n by forward playback, by a cold `still`, from a
    checkpoint, after a seek back and forth, bit-identical on one stack.
  - "buffers: stalls don't skip steps": force a stall; state matches forward stepping.
  - "buffers: auto scale doesn't touch state".
  - "buffers: playback never blocks": 0 blocking catch-ups while playing.
  - "buffers: resets": state at a reset equals a fresh start.
  - "buffers: history": `frame = -k` equals the state k frames back, reads `init` before a reset, and a pure history buffer rebuilt after a seek equals one played into.
  - "buffers: edits invalidate": change the scene, a texture, an upstream shot.
- **Golden:** `tests/films/feedback`: a trail over a moving shape, an 8-frame echo and a few steps of
  reaction-diffusion, strip frames crossing a reset.

## Phased plan

1. **Buffers:** `[[buffer]]`, spans, resets, `init`, `@prev`, `@<buffer>`, `history`
   and `frame = -k`, `rgba8`,
   `@picture`/textures/text/footage inputs, canonical fold, GPU checkpoints, budgeted
   catch-up, selftest + golden. Contract patch bump.
2. **Faster scrubbing:** background fill, progressive scrub, `preview_lookback`,
   per-frame invalidation, slow-seek hints.
3. **Reach:** buffers in comps, `loop = "warm"`, `after = "<layer>"`
   ([buffers in the layer stack](#buffers-in-the-layer-stack-phase-3)), kit examples (trails, echo, TAA,
   reaction-diffusion) in shotglass-kit.

## Alternatives considered

- **Give up determinism for buffers** (state is whatever playback left behind). Simple,
  and what Shadertoy and most VJ tools do, but `still`, `export` and playback would
  disagree. That's real-time mode (#60), not films.
- **Declared lookback as the definition** (state(n) = fold of the last L frames from
  black, always). Every frame is cheap and independent, but each frame then costs L
  steps during playback too, and simulations can't be expressed. Kept only as a preview
  approximation.
- **Shot-local state only** (reset at every cut, no spans). Easier to reason about, but
  trails across a dissolve and film-wide accumulation are common asks. `reset = "cuts"`
  covers the shot-local case.
- **Feedback in layers** (`@self` on a `[[layer]]`). Mixes compositing with simulation
  state, and layers have opacity, fades and mattes that make "the previous state"
  ambiguous. Buffers are separate and layers read them.
- **Disk checkpoints and persistent bakes** (spill evicted checkpoints, or every frame's state, to disk). Bounded GPU checkpoints cover
  short films; the rest isn't worth the machinery yet; see
  [Why no disk checkpoints](#why-no-disk-checkpoints).

## Open questions

1. Is K = one second of frames the right starting spacing (before adaptive doubling)?
2. Should `check --deep` measure how fast a buffer forgets (the frame where fold-from-r
   and fold-from-(n − L) agree within 1/255) and suggest `preview_lookback`?
