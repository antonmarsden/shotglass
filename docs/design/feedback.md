# Design: feedback buffers in films

Status: **proposal** (#134). Nothing is built yet. Builds on the iChannel design
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
  (GPU, then disk) make a seek cost at most K steps; a baked buffer costs none.
  Catch-up happens only while paused or scrubbing, in budgeted slices, so playback
  never blocks and the player never freezes.
- **Contract:** additive (`[[buffer]]`, the `@prev` channel source, buffer-local
  `iFrame`), so a patch bump: the next free version (0.1.9 if #73 hasn't taken it).
  Films without buffers take exactly the old path.

MVP (phase 1): `[[buffer]]` with spans, resets, `@prev`, reading textures, text,
footage, `@picture` and other buffers; canonical fold from the reset; GPU checkpoints;
budgeted catch-up in the player. Then disk checkpoints and progressive scrub (phase 2),
bakes (phase 3), buffers in comps and seamless loop films (phase 4).

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

Cases 1 and 4 forget their past quickly; cases 2 and 3 never do. The seek strategy must
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
format = "rgba16f"                 # default; "rgba32f" for simulations that need it
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

### Loop films

The period's first frame (frame 0) is always a reset, so loop k equals loop 0 and the
"shaders only see time within one period" invariant holds. That means a buffer is
visibly empty at the seam. A seamless loop needs the state at frame 0 to equal the state
after a whole period: phase 4 adds `loop = "warm"`, which defines state(0) as the fold
from frame 0 of a *previous* period (one period of pre-roll, computed once and
checkpointed). That's exact for effects that forget and a fixed point only by luck for
simulations, so it stays opt-in.

### Comps

A comp's time is its user's and can jump (a `time_offset`, a hold, a wrap), so a buffer
inside a comp would need a fold per instance, over comp-local frames. Phase 1 refuses
`[[buffer]]` in comp files. Phase 4 decides between "one state per instance" (memory per
use) and "comps with buffers may be used once".

## Seeking and the caches

Playback forwards from a known state costs one step per frame. Everything else (a seek,
a scrub, `still` of frame 9000, `strip`'s 8 frames, `quality`'s `@prev`) needs the state
at an arbitrary n. Four mechanisms, in the order they're built:

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

### 2. Checkpoints (phase 1 in GPU memory, phase 2 on disk)

Every K film frames (default: one second of frames), copy each buffer's state into a
checkpoint texture: a GPU copy, no readback, no sync. A seek restores the nearest
checkpoint ≤ n and steps at most K − 1 frames.

- Filled during playback for free, and while paused by a background walk forward from
  the playhead (budgeted, lowest priority, behind compiles and uploads).
- Kept in an LRU with its own budget (default 512 MB): a 960×540 RGBA16F state is ~4 MB,
  so a minute at one per second is ~240 MB per buffer. Checkpoints nearest the playhead
  are kept first.
- Phase 2 spills evicted checkpoints to disk (a raw f16/f32 file per checkpoint under
  `target`-style cache dir, keyed by the [fold key](#invalidation)), read back by the
  decode pool and uploaded in bands like footage frames. Then a reopened film scrubs
  fast from the first scrub.

### 3. Progressive scrub (phase 2)

While dragging, the player shows the nearest ready state rather than waiting: the
picture is exact, the buffer is from checkpoint c ≤ n, and the transport says "buffer
approximate". When the drag stops, catch-up refines it to the canonical state. This is
preview-only, like muted layers: `still` and `export` never use it.

An optional `preview_lookback = 2.0` (seconds) lets the player approximate a seek far
from any checkpoint by stepping only the last 2 s from black. Effects that forget
(trails, TAA) look right; simulations don't, so it's off by default and never used
headless.

### 4. Bakes (phase 3)

`shotglass bake <film> [--buffer trail]` runs the fold once and writes every frame's
state to the cache. A baked buffer is a lookup, exact-frame-or-stall, through the footage
machinery (#70: decode pool, banded uploads, `source_frame` = n). Baked state is float,
so the files are our own raw format (or EXR), not the PNG footage reader. The bake is
keyed by the fold key: editing the buffer marks it stale and the player falls back to
checkpoints. `bundle` doesn't include bakes (they're derived); `export` may use a fresh
one.

## Invalidation

Each buffer has a **fold key**: a hash of everything its fold reads.

- Its program(s) (scene and init), its declaration (size, format, span, resets).
- Its channels: textures, text and footage by their store keys (path + size + mtime /
  version), comps and `@picture` by the keys of every shot, transition and comp
  instance in its span (an edit to a shot changes the fold of a buffer that reads
  `@picture`), and earlier buffers by *their* fold keys.
- Its uniforms and keyframes, and the film's fps and duration.

A checkpoint or bake is valid only for its fold key, and only for frames whose fold
started at the same reset. A live edit therefore invalidates the edited buffer and every
buffer downstream, from the first reset that sees the change (an edit to shot "closeup"
only invalidates checkpoints at or after the first frame that reads it, when nothing
before reads it). Precise per-frame invalidation is phase 2; phase 1 invalidates the
whole buffer.

**Broken edits** keep the last working version, as today: the old programs *and* their
checkpoints stay until the new version's frame n is ready.

## Engine contract

- `[[buffer]]` in `movie.toml` (fields above), refused by older engines through the
  contract version.
- Channel sources: `@prev` (buffers only), `@<buffer id>`, and
  `{ source = "@<id>", frame = "prev" }`, classified in `movie::ChannelSource::of`.
- Buffer-local `iFrame` (frames since reset), `iTime` and the span uniforms as for
  layers. No new uniform names: a pasted Shadertoy Buffer A works with
  `channels = ["@prev"]`.
- Buffers get the scene header, but no layer footer: they write raw state (no
  premultiply, any values, including negative and > 1 in float formats).

## Renderer

- **Targets:** two per buffer (ping-pong), at the buffer's size and format, allocated on
  first use in the span and freed after it. They count against a buffer budget, not the
  256 MB texture LRU.
- **Plan:** per frame, after the picture and before layers: for each active buffer, step
  if `state_frame + 1 == n`, else schedule catch-up (headless: do it; player: slices, not
  usable yet). `keys_ready` additionally requires every active buffer's state to be at
  frame n.
- **Stalls while playing:** missed frames are stepped before the frame is drawn (the GPU
  cost of k steps, counted as housekeeping, so the auto scale doesn't react).
- **No per-frame syncs:** steps and checkpoint copies are draws and copies. Disk
  checkpoints read back through the PBO + fence path, polled oldest first, like audio.
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
| `bake` (phase 3) | writes the fold to the cache |

## Deterministic frames

The invariant becomes: **a frame depends only on (movie, n, size, scale), where a
buffer's state is the canonical fold from its last reset.** Same stack, same output,
bit for bit, whichever path reached frame n (forward playback, checkpoint, bake, cold
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
  of `@prev` outside buffers, forward-only references, buffers in comps refused, loop
  films reset at 0.
- **Selftest (mutation-checked):**
  - "buffers: every path agrees": frame n by forward playback, by a cold `still`, from a
    checkpoint, after a seek back and forth, bit-identical on one stack.
  - "buffers: stalls don't skip steps": force a stall; state matches forward stepping.
  - "buffers: auto scale doesn't touch state".
  - "buffers: playback never blocks": 0 blocking catch-ups while playing.
  - "buffers: resets": state at a reset equals a fresh start.
  - "buffers: edits invalidate": change the scene, a texture, an upstream shot.
- **Golden:** `tests/films/feedback`: a trail over a moving shape and a few steps of
  reaction-diffusion, strip frames crossing a reset.

## Phased plan

1. **Buffers:** `[[buffer]]`, spans, resets, `init`, `@prev`, `@<buffer>`,
   `@picture`/textures/text/footage inputs, canonical fold, GPU checkpoints, budgeted
   catch-up, selftest + golden. Contract patch bump.
2. **Faster scrubbing:** disk checkpoints, background fill, progressive scrub,
   `preview_lookback`, per-frame invalidation.
3. **Bakes:** `shotglass bake`, baked lookup through the footage machinery.
4. **Reach:** buffers in comps, `loop = "warm"`, kit examples (trails, echo, TAA,
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

## Open questions

1. Is K = one second of frames right, or should spacing adapt (dense near the playhead,
   sparse further away)?
2. Should buffers read `@input`-like full composites (after some layers)? That would
   need buffers interleaved with layers rather than a block before them.
3. Disk cache location and eviction (`target/`-like per film, or the shared library
   cache dir)?
4. Should `check --deep` measure how fast a buffer forgets (the frame where fold-from-r
   and fold-from-(n − L) agree within 1/255) and suggest `preview_lookback`?
5. Bake file format: raw f16 with a small header, or EXR (an extra dependency)?
