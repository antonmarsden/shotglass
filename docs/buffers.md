# Feedback buffers

State kept from one frame to the next: trails, echo, accumulation, reaction-diffusion,
cellular automata, simple fluids. Needs `shotglass = "0.1.9"`.

[README](../README.md) · [Movies](movies.md) · [Channels](channels.md) · [Layers and blends](layers.md) · [Feedback buffers](buffers.md) · [Sound](sound.md) · [Loop films](loop-films.md) · [The player](player.md) · [Projects and libraries](libraries.md) · [Testing GLSL and quality metrics](testing-and-quality.md) · [Command line and export](cli.md) · [Testing shotglass](testing.md)

## Buffers

A buffer is a scene shader whose output is kept and read back on the next frame, like
Shadertoy's Buffer A–D. Buffers step once per film frame, after the shots (the picture
track) and before any layer, in file order. Layers and later buffers read them:

```toml
[[buffer]]
id = "trail"
scene = "buffers/trail.glsl"       # mainImage writes the new state
from = "moon"                      # span, as for layers; default: the whole film
to = "closeup.end"
size = [960, 540]                  # or scale = 0.5 (of the output size); never "auto"
format = "rgba16f"                 # default; "rgba32f" for simulations, "rgba8" for picture history
channels = ["@prev", "@picture"]   # iChannel0 = my previous state, iChannel1 = this frame's picture
reset = ["closeup"]                # also start over at the start of shot "closeup"
uniforms.uDecay = 0.9

[[layer]]
id = "trails"
scene = "layers/show-trail.glsl"
channels = ["@trail"]              # this frame's state, after its step
```

```glsl
// buffers/trail.glsl
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  fragColor = max(texture(iChannel1, uv), texture(iChannel0, uv) * uDecay);
}
```

A pasted Shadertoy Buffer A works with `channels = ["@prev"]`: there are no new uniform
names.

- **State:** `mainImage` writes the raw state: no premultiply and no clamp, so float
  formats keep negative values and values above 1. `format` is `rgba16f` (default),
  `rgba32f` (read with `nearest` filtering by default: float filtering isn't portable)
  or `rgba8` (half the memory, fine for picture history).
- **Size:** `size = [w, h]` in pixels, or `scale` as a fraction of the output size
  (default 1). Never the player's render scale: the auto scale doesn't change what a
  buffer holds. With `scale`, `--size` on `still` and `export` changes the state's
  resolution, like a fixed-scale layer.
- **Span:** `from` / `to` as for [layers](layers.md#layers). Outside its span a buffer
  is inactive and reads black.
- **Resets:** a buffer starts over at the start of its span, and at every `reset`
  point: shot or layer ids (their start), span-style points (`"moon + 2"`) or film
  seconds. `reset = "cuts"` starts over at every shot start in the span (temporal
  smoothing that mustn't smear across a cut). A loop film's frame 0 is a reset, unless
  the film says `loop = "warm"` ([seamless loops](#seamless-loops-loop--warm)).
- **Init:** at a reset the previous state is black, unless `init = "buffers/seed.glsl"`
  names a pass drawn once instead (it sees the frame's uniforms and channels, but no
  `@prev`).
- **Time:** `iTime`, `iShotTime`, `iShotDuration` and `iShotProgress` are span-local,
  like a layer's; **`iFrame` counts frames since the last reset** (0 on the reset
  frame); `iTimeDelta` is exactly `1/fps`. `iMovieTime` and the cue defines are as
  usual.
- **Uniforms and params:** `uniforms` (with `SG_HAS_<name>` defines) and `params`
  (`#define`s), as for layers.

## Reading buffers

| channel | reads | who |
|---|---|---|
| `"@prev"` | this buffer's previous state | buffers |
| `"@<buffer>"` | that buffer's state this frame, after its step | layers, later buffers |
| `{ source = "@<buffer>", frame = "prev" }` | its previous state | layers, any buffer |
| `{ source = "@<buffer>", frame = -k }` | its state k frames ago (1 ≤ k ≤ `history`) | layers, any buffer |

- A buffer may read the current state of an *earlier* buffer and the previous state of
  any buffer, so Buffer A–D chains work. A later buffer's state this frame isn't drawn
  yet: read its previous state. On a later buffer's own reset frame, its previous state
  reads black from earlier buffers (its init pass hasn't run yet).
- Buffers read textures, text, footage, comps (including comps with
  [buffers of their own](#buffers-in-comps)), `@picture` (the picture track, rendered
  at the buffer's size) and other buffers; not `@input` or `@<layer>`, since layers draw
  after buffers step. For trails of something a layer would draw, render it in a
  [comp](layers.md#comps) and read `@<comp>`.
- Shots and transitions can't read buffers: they're drawn before buffers step.
- Reading a buffer that isn't active this frame gives black.

## History

`history = N` keeps the buffer's last N states (default 1, at most 64), read as
`frame = -k`. Effects that need distinct past frames (echo, frame blend, strobe trails,
stutter, slit-scan) use it:

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

Before a reset, `frame = -k` reads what `@prev` reads on the reset frame: black, or the
`init` state. A buffer that reads no buffer state at all (no `@prev`, no other buffer)
is a *pure history* buffer: its state at frame n depends only on frames n − N .. n, so
a seek rebuilds it in N steps and it needs no checkpoints.

## Memory

A buffer keeps `history + 1` states of its size and format while it's active (two for
`@prev`). `shotglass check` (and loading the film) refuses a film whose buffers need
more than 1 GB at once, naming them; `scale` buffers are counted at a 1080-pixel-high
output. A full-HD `rgba32f` history doesn't fit; a half-size `rgba8` slit-scan of 60
frames does (about 120 MB).

## Determinism and seeking

A buffer's state at film frame n is defined as its state at the last reset r ≤ n,
stepped once per film frame from r to n. That's a pure function of the film and n, so
playback, `still`, `strip`, `export` and `quality` show the same frame, bit for bit on
one machine, whichever way they reached it. No step is ever skipped: if playback
stalls from frame n to n + 3, the buffers step n + 1, n + 2 and n + 3 before frame
n + 3 is drawn.

Only the cost of reaching a frame changes. The engine starts from the nearest known
state: the current one if it's behind, a **checkpoint**, or the reset.

- Headless commands fold at once (`still` of frame n costs up to n − r steps; `strip`
  and `export` walk forward).
- The player copies every buffer's state into a checkpoint once a second of film, in
  GPU memory (no readback). Checkpoints far from the playhead are dropped first, and the
  spacing doubles away from it when the budget is full (a checkpoint that would be the
  first dropped isn't taken). The budget is the player's setting (System panel, for the
  session), else `SHOTGLASS_BUFFER_CACHE_MB`, else 1/8 of the video memory where the
  driver reports it (256 MB – 2 GB), else 512 MB. NVIDIA reports total memory; AMD's
  `GL_ATI_meminfo` reports only free texture memory, so there the budget follows what
  was free at startup.
- **Background fill:** while paused and idle (no compiles, uploads or catch-up), the
  player walks forward from the playhead taking the checkpoints it hasn't got yet, a few
  steps per redraw, until the end of the film or the budget is full.
- **`cache = "dense"`** on a buffer checkpoints every frame while it's active instead of
  every second, within the same budget: at ~4 MB a state, 512 MB holds about 4 s of
  30 fps, enough to scrub a short passage frame by frame.
- After a seek the player catches up a few steps per redraw ("catching up buffers 41%"),
  never freezing; while playing, the clock waits for it. Until then the previous frame
  stays up. When a catch-up would take more than about a second, the overlay suggests
  what would shorten it: a smaller `size`, `cache = "dense"`, or reading a cheaper comp
  instead of `@picture`.
- **Progressive scrub:** while you drag the timeline, a frame far from the buffers'
  state draws at once from the nearest saved state before it (the picture is exact, the
  buffers are from that earlier frame) and the overlay says "buffer approximate"; when
  you let go, it catches up to the exact frame. Preview only: `still`, `strip`,
  `export` and `quality` never approximate.
- **`preview_lookback = s`** (seconds, opt-in per buffer, player only): when no saved
  state is within s seconds of a scrubbed frame, step only the last s seconds from black
  instead (approximate, refined when you let go). Effects that forget (trails, temporal
  smoothing) look right; simulations don't, so leave it off for them. It applies only
  when every stateful buffer active at the frame has it.
- An edit keeps the states before the first frame that reads it: editing a buffer (or a
  texture it reads) starts its fold over from its span, editing a shot it reads
  (`@picture`) drops only checkpoints from that shot's start on. Changing a declaration
  (span, resets, size, format, the film's timing) or a comp it reads starts over. A
  broken edit keeps the last working version and its state.

Long folds over float state can drift further apart between GPUs and drivers than a
single frame does (errors compound), so compare long folds on one machine, not against
reference images from another.

## Buffers in comps

A [comp](layers.md#comps) file may have `[[buffer]]`s too (needs `shotglass = "0.1.10"`).
They work as in a film, inside the comp: they step after the comp's shots and before
its layers, `@picture` is the comp's picture track, the comp's layers read them, and
spans, resets and seconds are comp-local:

```toml
# comps/spark.toml
[[shot]]
id = "dot"
scene = "scenes/dot.glsl"
duration = 2.0

[[buffer]]
id = "trail"
scene = "buffers/trail.glsl"
size = [80, 45]
reset = [1.0]                      # 1 s of comp time
channels = ["@prev", "@picture"]   # the comp's picture track

[[layer]]
id = "show"
scene = "layers/show.glsl"
channels = ["@input", "@trail"]
```

- **The fold is over comp frames.** A comp buffer's state at comp frame c is the fold
  from its last reset to c, whoever shows the comp and whenever: exactly what the comp
  would show as a film of its own. A comp shown from 0.5 s in (`time_offset = 0.5`) starts
  at comp frame 15 (at 30 fps), with the state of 15 steps; a comp that holds its last
  frame holds its state too; a `loop = true` comp resets at its frame 0, like a loop film
  (`loop = "warm"` carries it over, [as for films](#seamless-loops-loop--warm)).
- **Shown in one place.** A comp with buffers (or that shows one) may be used once in
  the film and its comps: one shot, layer, matte or channel (a film buffer's included).
  Two uses could ask for two comp frames at once. For a second use, make a second comp
  file (they can share shaders). Comps without buffers can still be used anywhere.
- **Size:** `scale` is a fraction of the comp's `size`, or of the film's output size if
  it has none, never the size of whatever shows it.
- Seeking works as for the film's buffers (checkpoints under the same budget, catch-up
  in slices in the player). Progressive scrub and the background fill are for the
  film's buffers only: a comp's buffers always catch up to the exact frame.
- Memory: `shotglass check` adds the worst frame of the film's buffers and of every
  comp's, and refuses more than 1 GB in all (a sized comp's `scale` buffers are counted
  at its `size`).

## Seamless loops: `loop = "warm"`

In a [loop film](loop-films.md) frame 0 is a reset, so a trail is empty at the seam and
the loop shows a jump. `loop = "warm"` (needs `shotglass = "0.1.10"`) carries the state
over the seam instead:

```toml
loop = "warm"
```

- **What frame n shows:** the fold from the start of the *period before* (one period of
  pre-roll from a cold start), so frame 0 follows a whole period, as if the film had
  already played once. It's the same every loop, and `still`, `strip`, `export` and the
  player agree, as for any buffer.
- **Which buffers:** those that run from frame 0 to the end of the period (spanning the
  seam). For them the seam isn't a reset (not even one at frame 0 from `reset`). Other
  resets, and buffers that start later or end earlier, behave as in a plain loop.
  `shotglass check` warns when no buffer spans the seam (then warm changes nothing).
- **Seamless for effects that forget** (trails, smoothing, echo): after a period the
  pre-roll has gone, so frame 0 is exactly what the frame after the last would be. A
  simulation that never settles is only seamless by luck: frame 0 is its state after
  one period, not after many.
- **`iFrame`** of a spanning buffer counts on over the seam, from its last reset in the
  period before (so up to two periods' frames). Every other uniform stays within one
  period.
- **Cost:** reaching a frame from nothing folds up to two periods instead of one. The
  state at the seam is checkpointed and kept, so each wrap of the loop restores it
  instead of stepping a period again.
- A comp can be warm too: `loop = "warm"` on its `[comps]` entry.

## Not yet

Buffers inside the layer stack (`after = "<layer>"`) come later (#138); see
[the design](design/feedback.md).
`check --deep` doesn't yet look for NaN/Inf in float state.
