# Loop films

Films that play forever (`loop = true`), Shadertoy-style, with a seamless picture and sound.

[README](../README.md) · [Movies](movies.md) · [Channels](channels.md) · [Layers and blends](layers.md) · [Feedback buffers](buffers.md) · [Sound](sound.md) · [Loop films](loop-films.md) · [The player](player.md) · [Projects and libraries](libraries.md) · [Testing GLSL and quality metrics](testing-and-quality.md) · [Command line and export](cli.md) · [Testing shotglass](testing.md)

## Loop films

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
- **Feedback buffers** start over at frame 0, so a trail is empty at the seam.
  `loop = "warm"` (0.1.10) carries their state over it: see
  [Feedback buffers](buffers.md#seamless-loops-loop--warm).
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
