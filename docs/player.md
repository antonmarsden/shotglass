# The player

The interactive player, avoiding frame drops, and how long films stay fast.

[README](../README.md) · [Movies](movies.md) · [Channels](channels.md) · [Layers and blends](layers.md) · [Feedback buffers](buffers.md) · [Sound](sound.md) · [Loop films](loop-films.md) · [The player](player.md) · [Projects and libraries](libraries.md) · [Testing GLSL and quality metrics](testing-and-quality.md) · [Command line and export](cli.md) · [Testing shotglass](testing.md)

## The player

`shotglass play <movie> [--at 12.5 | --at moon | --at moon+2] [--scale auto | 1.0]`

Desktop playback compiles and runs GLSL from the film and its libraries on your GPU.
Play films and load libraries from sources you trust.

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
- **Feedback buffers** ([buffers.md](buffers.md#determinism-and-seeking)): while
  paused, the player takes checkpoints ahead of the playhead in the background; while
  you drag the timeline, a frame shows at once from the nearest saved state ("buffer
  approximate") and becomes exact when you let go. The System panel shows the
  checkpoints and sets their budget for the session.
- The Sound panel shows how much sound is buffered ahead and any underruns.
  The overlay shows shaders still compiling.
