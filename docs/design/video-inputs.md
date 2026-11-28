# Design: moving-image inputs (footage on iChannels)

Status: **phase 1 (image sequences) implemented**. Phase 2, video, is #73; RGBA16F/EXR
and other follow-ups are in #75. Builds on [ichannel.md](ichannel.md) (#22), which
left room for `{ video = "…" }` channels, and touches #32 (heavy assets) and #2 (web player).

**Where the build differs from this proposal:**
- Phase 2 (video) was revised after review (#73): video is **WebM only (AV1)**, demuxed and
  decoded **in-process** and seeked through the file's Cues, with no ffmpeg at playback and no
  disk cache. Other formats go through an explicit `shotglass conform`, which writes a WebM
  into the film folder. An automatic conform cache was considered and dropped: its disk use
  grows with the footage, which is unsafe where temp space is small (cloud machines). The
  sections on video below describe the revised design.
- Open questions 1, 2 and 4 are settled as proposed: `end = "hold"`, footage
  `filter = "linear"`, exact frame or stall (no preview mode).

## Summary

Today a channel carries a still PNG or JPEG. This design adds **footage**: a channel whose
contents change with film time. Two kinds of footage source:

1. **Image sequences** (`renders/fog_####.png`): one image file per frame. Phase 1, the MVP.
   No new dependencies: frames go through the existing `texture.rs` decoder, every frame
   is addressable on its own, so seeking is free and frame-exact. That covers Blender and
   other renderers, mattes, and re-using a shotglass `export --format png`.
2. **Video files** (`footage/surf.webm`): phase 2, **AV1 in WebM**, demuxed by our own
   Matroska reader and decoded in-process (rav1d, behind a feature), seeking to the
   keyframe before a frame through the Cues; constant-frame-rate clips only, YUV converted
   to RGBA in Rust on a worker thread. No ffmpeg at playback and no cache on disk; other
   formats are converted once with `shotglass conform` (ffmpeg), which writes a WebM the
   user keeps.

The core of the design is the same for both:

- **Frame-exact time mapping as a pure function** `source_frame(movie, shot, channel, n)`:
  film frame number → source frame number, computed in f64 from `frame_time`'s exact
  rational, never from an f32 `t * fps`. Per-channel `in`, `speed`, `end` and `clock`
  options; source fps from the file (video) or the entry (sequences).
- **A footage frame is a texture** keyed by `(footage key, source frame)`. Frames decode on
  worker threads ahead of the playhead, upload in bands in `pump` with their own budget,
  and count against the 256 MB texture budget. A shot is usable only when **the exact
  frame it needs** on every channel is `Ready`; while playing, otherwise the previous film
  frame stays up (a stall), exactly like an unfinished compile or texture. Playback never
  shows a wrong or approximate source frame, so play, `still` and `export` agree.
- **No built-in GLSL.** Colour conversion (YUV → RGB) runs on the CPU in Rust. Shaders see
  RGBA8 (or, phase 3, RGBA16F for EXR/16-bit), with the same `vflip`/`srgb`/`filter`/`wrap`
  options as images. `iChannelTime[n]` becomes the source time of the frame shown.
- **Clip audio is ignored** in phases 1–2 (shotglass's sound is GLSL). Using it is a
  phase-3 question that belongs with #32's audio samples.

## Use cases

| use | source | what it needs |
|---|---|---|
| Live-action or stock footage behind or through shaders (grading, displacement, glitch, a shader-made sky over a plate) | H.264/HEVC/VP9/AV1/ProRes clip | video decode, colour matrices, an in point, maybe slow motion |
| Passes from Blender, Houdini, After Effects (beauty, depth, mattes) | PNG/JPEG sequence; EXR later | sequences, a frame offset (`first = 1001`), linear float (phase 3) |
| A shotglass film as input to another (precomposed title, a pre-rendered expensive shot) | `export --format png` (`frame_000000.png`…) or `--format ffv1` | sequences (phase 1), lossless video (phase 2) |
| Alpha mattes, rotoscoped holdouts | RGBA PNG sequence, ProRes 4444, VP9 with alpha, or a grey matte clip read as `.r` | straight alpha kept as stored |
| Texture animation (flipbooks, caustics loops, scanned film grain) | short sequence or clip | `end = "loop"`, `clock = "movie"` |
| Freeze frame / a frame from a clip as a still | either | `speed = 0` |

Not covered: webcam/live inputs (not deterministic), nested films rendered live (see
[Alternatives](#alternatives)), and footage as sound input (see [Audio](#audio-in-clips)).

## movie.toml

### Syntax

Footage lives in the same places as images: `[textures]`, the film-wide `channels`, and a
shot's `channels`. Images keep `file`; footage uses `frames` or `video`.

```toml
shotglass = "0.1.6"            # the contract with footage (sequences)

[textures]
grain = "<kit/textures/grain_##.png>"                     # a library sequence (#: frame number)
fog   = "renders/fog_####.png"                            # a sequence in the film folder
matte = { frames = "renders/matte_####.png", fps = 24, first = 1001 }
surf  = "footage/surf.webm"                                # a video (phase 2)
plate = { video = "footage/plate.webm", in = 12.5, srgb = true, filter = "mipmap" }

[[shot]]
id = "beach"
scene = "scenes/beach.glsl"
duration = 6.0
channels = ["plate", "matte", "grain"]

[[shot]]
id = "beach-again"            # same clip, a later moment, slower
scene = "scenes/beach.glsl"
duration = 4.0
channels = [{ texture = "plate", in = 40.0, speed = 0.5 }, "matte"]

[[shot]]
id = "titles"
scene = "scenes/titles.glsl"
duration = 5.0
channels = [{ frames = "precomp/title_######.png", fps = "30000/1001", end = "black" }]
```

**Shorthand strings** keep working as in #22 and are told apart by their extension:

| written | is |
|---|---|
| `"brick"` | a name from `[textures]` |
| `"textures/brick.png"` | an image (`.png`, `.jpg`, `.jpeg`) |
| `"renders/fog_####.png"` | an image sequence: an image path with one run of `#` in its **file name** |
| `"footage/surf.webm"` | a video: `.webm` (any case). `.mp4`, `.m4v`, `.mov`, `.mkv`, `.avi` are refused with the `shotglass conform` command that converts them |
| `"<lib/…>"` | any of those from a library |

**Tables** take exactly one of `file` (image), `frames` (sequence), `video`, or `texture`
(a name from `[textures]`, re-timed: only the timing options below may follow it, so the
same clip can be used at different moments without naming it twice).

### Options

All of #22's options apply to footage: `filter`, `wrap`, `vflip`, `srgb`. New ones:

| option | applies to | values | default | meaning |
|---|---|---|---|---|
| `fps` | `frames`; `video` (override) | number or `"num/den"` (as the film's `fps`) | sequence: the film's fps; video: the file's rate | source frames per second. On a video it *reinterprets* the clip (a 23.976 clip played as 24) |
| `first` | `frames` | integer ≥ 0 | the lowest frame number found | the file number that is source frame 0 (`first = 1001` for a Blender range starting at 1001) |
| `in` | timing | seconds ≥ 0 (f64) | `0` | source time shown when the channel's clock is 0 |
| `in_frame` | timing | integer ≥ 0 | — | the same, as a source frame number (not with `in`) |
| `speed` | timing | number ≥ 0 | `1` | source seconds per clock second. `0` is a freeze frame at `in` |
| `clock` | timing | `"shot"`, `"movie"` | `"shot"` | `shot`: clock 0 is the shot's start; `movie`: clock 0 is the film's start, so one plate runs on across several shots like the music |
| `end` | timing | `"hold"`, `"loop"`, `"black"` | `"hold"` | past the last source frame: keep it, wrap to frame 0, or read black (`iChannelResolution` stays the clip's size) |
| `matrix` | `video` | `"bt601"`, `"bt709"`, `"bt2020"` | from the file's tags, else by height (see [Colour](#colour)) | YUV→RGB matrix override for badly tagged files |
| `range` | `video` | `"tv"`, `"pc"` | from the file's tags, else `"tv"` | limited (16–235) or full range |

Timing options can appear on a `[textures]` entry and on a shot's channel entry; a
channel entry's own values win. Footage defaults to `filter = "linear"` rather than
`"mipmap"`: a 1080p frame with mipmaps is a third more upload and memory every frame, and
footage is rarely minified. `filter = "mipmap"` works (one `glGenerateMipmap` per frame)
and is what the budget counts then. (Open question 2.)

Footage timing is independent of the shot's `time_offset` / `time_scale`: those set
`iTime` for the shader, these set which picture is on the channel. A shader can still
read `iChannelTime[n]` if it wants to follow the footage.

### Validation (`Movie::parse` / `Movie::load`)

Following "movie.toml field" in CLAUDE.md: serde stays `deny_unknown_fields` (entries are
parsed by hand from TOML values, as #22's implementation notes describe, so an unknown key
names itself), the resolved `Channel` grows a `source: Source` field, every rule below
gets a unit test, the proptest generator for near-valid `movie.toml` learns the new keys,
and README plus `templates/new/movie.toml` (a commented example) are updated.

```rust
pub enum Source {
    Image,                                   // as today
    Frames { pattern: Pattern, fps: Fps, first: u64, count: u64 },
    Video { info: VideoInfo, fps: Fps },     // phase 2: from the WebM headers
}
pub struct Timing { pub in_s: f64, pub speed: f64, pub clock: Clock, pub end: End }
```

`parse` checks (no file access): one of `file`/`frames`/`video`/`texture`; `#` only in the
file name, one run of them; `%04d` gets `write it as renders/fog_####.png`; timing keys
only on footage (`shot "wall": channels[0]: "in" is for footage; textures/brick.png is an
image`); `in` xor `in_frame`; `speed ≥ 0` and finite; `fps` parses and is > 0; enum values
from the tables; `texture = "…"` names a footage entry and carries only timing keys.

`load` (file access, like #22's missing-file and budget checks, so the player refuses a
broken version before the renderer sees it):

- **Sequences** are scanned once: list the folder, match `prefix` + exactly *n* digits
  (`####` = 4; more digits are allowed only when the number doesn't fit, as Blender
  writes) + `suffix`. `first` defaults to the lowest number; the frames must be
  contiguous from `first`. A gap is an error listing it (`renders/fog_####.png: frames
  1034–1036 are missing (between 1001 and 1100)`), since a sequence still being rendered
  usually has a gap at the end, not the middle; trailing frames simply aren't there yet
  (open question 5). Frame 0's dimensions are read (`texture::dimensions`), and every
  frame must match: checked lazily as frames decode (a mismatch is that frame's error),
  and fully by `check`.
- **Videos** are read by our WebM reader (see [Decoding](#decoding)): an AV1 video track,
  size, constant frame rate, frame count, bit depth, colour (Matroska Colour element and
  the AV1 sequence header), and the Cues. Reading headers and Cues is cheap (no decode);
  results are kept in memory keyed by (path, length, mtime) so reloads don't re-read.
- Size limits are #22's (4096 per side; `check --web` warns above 2048, and for footage
  above 1920×1080). The per-shot budget check counts footage by its streaming window
  (see [Memory](#memory)).
- Footage files and sequence folders join `referenced_files` (`check_case`, bundling, the
  web preloader). A sequence counts as its folder plus the matched names.

### Error messages

```
movie.toml: shot "beach": channels[1]: "renders/mate_####.png" matches no files (did you mean "renders/matte_####.png"?)
renders/fog_####.png: frames 1034–1036 are missing (between 1001 and 1100)
renders/fog_1042.png: 1280×720, but the sequence is 1920×1080 (frame 1001)
renders/fog_1042.png: unexpected end of PNG data (still being written?)
footage/surf.mp4: shotglass reads video as AV1 in WebM; convert it once: shotglass conform footage/surf.mp4
footage/surf.webm: variable frame rate (frame 212 at 7.071 s, expected 7.067 s); convert it once: shotglass conform footage/surf.webm
footage/plate.webm: VP9; shotglass decodes AV1 only: shotglass conform footage/plate.webm
footage/plate.webm: no Cues (seeking needs them); convert it: shotglass conform footage/plate.webm
footage/hdr.webm: HDR (smpte2084) isn't supported yet; convert to SDR BT.709 first
footage/surf.webm: keyframes up to 10.0 s apart: a paused seek decodes up to 300 frames; shotglass conform --keyint for faster scrubbing   [check note]
movie.toml: shot "titles": channels[0]: in = 12.0 is past the end of precomp/title_######.png (4.0 s, 120 frames)
shot "beach" (6.0 s) runs past the end of footage/surf.webm (5.2 s from in = 3.0); end = "hold" freezes the last frame   [check warning]
```

## Time mapping

### The function

Footage frame selection is one pure function in `movie.rs`, used by the renderer, `check`,
`export` and the tests:

```rust
/// The source frame channel `c` of shot `s` shows on film frame `n`, or None for black.
pub fn source_frame(&self, s: usize, c: usize, n: u64) -> Option<u64>
```

1. `n` is the **film frame** (for a loop film, already `wrap_frame(n)`, so frame n of
   loop k is frame n of loop 0, as for everything else). Renders at an arbitrary `t`
   (scrubbing) use `frame_at(t)`, the frame on screen, exactly as `FrameInfo::at` does
   for `iFrame`.
2. Film time in f64: `t = n · fps_den / fps_num` (the exact rational, not the f32
   `frame_time`).
3. Clock time: `clock = "shot"` → `t − shot.start` (the shot's f32 start widened to f64);
   `clock = "movie"` → `t`. During the incoming transition the clock is ≥ 0 from the
   shot's start; for the outgoing shot it runs past its duration, so footage keeps moving
   under a transition rather than freezing.
4. Source time: `s = in + speed · clock`.
5. Source frame: `k = floor_snap(s · src_num / src_den)`, where `floor_snap` is
   `frame_at`'s rule (round when within 1e-4·k of an integer, else floor), so exact
   ratios never drop a frame to rounding: film 30000/1001 with a 30000/1001 source is the
   identity for any length, and a 24 fps source in a 30 fps film shows
   0,0,1,2,3,4,4,5,6,7,8,8,… (one repeat every five film frames).
6. Negative `k` (only possible with `in` and `clock` arithmetic edge cases) clamps to 0.
   Past the last frame `L`: `hold` → `L`, `loop` → `k mod (L+1)`, `black` → None.

`iChannelTime[c]` = `k · src_den / src_num` (the displayed frame's source time, seconds,
f32), so a shader can tell where in the clip it is; `iChannelResolution[c]` = the clip's
size. Both are 0 for unbound channels, as now. Nothing depends on wall-clock time or on
which frames were decoded before: a frame is still a function of (movie, n, size, scale).

**Differing rates** need no special handling: the floor in step 5 is "the source frame
whose interval contains this film instant", the same rule `frame_at` applies to film
frames. No frame blending: a shader that wants motion-blurred conversion can read the
channel twice (open question 7 covers an opt-in `blend`).

**Loop films.** The mapping uses the wrapped film frame, so footage restarts every period
(with `clock = "shot"` the shot's clock restarts anyway). A clip shorter than the period
needs `end = "loop"` or holds; `check` warns when a looping channel's clip length doesn't
divide the period (`a visible jump at 4.0 s each loop`). The seam is handled like a cut
(see [Prefetch](#prefetch)).

### Video timestamps

A video's frame `k` must mean the same thing whether reached by decoding from the start or
by seeking. Phase 2 accepts **constant-frame-rate** streams only. The rate comes from the
track's `DefaultDuration`; frame `k` is the block whose timestamp, minus the first block's,
rounds to `k · den / num` seconds at the file's timestamp scale. Matroska timestamps are
usually whole milliseconds (our own export rounds to the nearest one), so the reader maps
a timestamp to the **nearest** frame, never the floor: at 30000/1001 a floor would put some
frames in the wrong slot. A block more than a quarter of a frame from its slot, a missing
slot or a duplicate makes the file VFR, refused with the `conform` hint. Frames are counted
in presentation order; AV1 in WebM stores them that way (hidden frames live inside a
temporal unit).

Rotation and cropping metadata aren't read in phase 2 (`conform` applies rotation).

## Decoding

### Phase 1: image sequences

A sequence frame is an image: `texture::load(path_of(k), label, vflip)` on a worker
thread, unchanged. Random access, no state, no seeking. What changes is throughput: a
1080p PNG decodes in roughly 20–40 ms and a JPEG in 5–10 ms, so 30 fps needs several
workers. The single texture decode thread becomes a small pool (`gfx::Decoder`) (`min(4, cores − 2)`,
at least 1) shared by images and footage; jobs carry a priority so the frame under the
playhead goes first. README guidance: JPEG (quality 95) for plates, PNG for mattes and
anything with alpha, and smaller frames on slow machines. (QOI, a fast lossless format the
`image` crate supports behind a feature, is a cheap later addition if PNG decode is the
bottleneck.)

### Phase 2: video, WebM + AV1 in-process

Revised after review (#73). The original proposal ran ffmpeg readers per seek position;
then an automatic intra-only conform cache was proposed. Both are replaced by this.

- **Container: WebM only.** Our Matroska code (`webm/mux.rs`, which already writes
  clusters with one Cue per keyframe and has a test reader) grows a real demuxer: EBML
  header, Tracks (the AV1 track's `CodecPrivate` = `av1C`, Colour, DefaultDuration), Cues,
  and Clusters read on demand (SimpleBlock and BlockGroup). A file without Cues is refused
  (seeking needs them).
- **Codec: AV1 only** in phase 2, 8- and 10-bit 4:2:0 (4:4:4 if the decoder offers it).
  Decoded by **rav1d** (the Rust port of dav1d, BSD-2) behind a `video` feature: it builds
  with cargo, so no meson on Windows. dav1d itself is the fallback if rav1d proves too slow
  or awkward. AV1 decoding is bit-exact by specification, threads or not.
- **No VP9 in-process.** VP9 WebMs go through `conform`, where the user's ffmpeg decodes
  them with its native decoder (ffvp9, the fastest VP9 decoder). ffvp9 itself can't be
  used in-process without linking libavcodec (ruled out: build pain, LGPL, version
  coupling), and its VP9-alpha support has historically needed libvpx anyway. If in-process
  VP9 alpha is ever really needed, libvpx (BSD) behind a feature is the route.
- **Alpha comes from matte clips:** a PNG sequence, or a second AV1 WebM whose luma is the
  alpha, bound on another channel and read as `.r`. That works with phase 1 today and with
  AV1, with no second codec.
- **Seeking.** To get frame `k`: the Cue at or before `k` gives the cluster that starts with
  its keyframe (our export writes closed GOPs, a cluster per keyframe); the decoder is reset
  there and frames are decoded and dropped up to `k`. A seek costs the distance back to the
  previous keyframe. **Fast scrubbing is not a requirement**: the rule is exact frame or
  stall, and paused/scrubbing renders may block. With 2 s keyframes (our export's default) a
  worst-case paused seek decodes ~60 frames, roughly 100–300 ms at 1080p on a desktop.
- **Readers.** A reader is (an open file, a decoder, its position). A request for `k` is
  served by a reader positioned at most one GOP before `k` (it decodes on); otherwise one
  seeks. Reading forward, playback and export decode each frame once. A small pool per clip
  (at most 4 readers, least recently used dropped) covers the cases that need separate
  playheads on one clip: two shots of the same clip at different `in` crossing in a
  transition, a comp channel with `time_offset`, `speed` above ~2. Readers run on the
  `gfx::Decoder` pool as a decode-job kind behind the same "give me frame k" interface as
  sequences (`trait FrameSource`); a reader belongs to one worker at a time.
- **The current GOP is kept.** Frames decoded on the way to `k` are offered to the texture
  store (within the footage budget and window) rather than dropped, so stepping back within
  a GOP is cheap; stepping back past a keyframe seeks again.
- **Back-pressure.** Readers decode only for requested frames (the window from
  `footage_order`), bounded by the CPU-side cap. Nothing spins.
- **Determinism.** Decoded YUV depends only on the file and `k`; the YUV→RGB step is
  shotglass's own Rust code. So a video frame's texels depend only on the file, `k` and the
  options, which is what play/`still`/`export` agreement needs. A selftest checks
  seek-vs-sequential equality (see [Tests](#tests)), and `check --deep` decodes every frame
  of the film's clips once, sequentially, and compares a sample against seeks.
- **Errors.** A decode error becomes the frame's `Failed` message with the file and frame.
  A build without the `video` feature refuses video channels at `Movie::load`, naming the
  feature. Long GOPs aren't an error: `check` notes keyframes more than 4 s apart.

#### `shotglass conform`

Explicit, never automatic: `shotglass conform footage/surf.mp4 [-o footage/surf.webm]`
writes a WebM next to the source (or where `-o` says), which the user keeps, commits (Git
LFS) or deletes. Nothing goes to a cache. ffmpeg is needed only here, as a decoder
(`SHOTGLASS_FFMPEG` or the PATH, export's install hints): it decodes to raw YUV at a
constant rate (`-fps_mode cfr`, autorotate applied) into a pipe, and our built-in encoder
(`webm::encode`, SVT-AV1, the `webm` feature) and muxer write the file. Options:

- `--keyint N` (default: export's ~2 s); smaller for faster scrubbing.
- `--crf` (default: higher quality than export's), `--lossless` where the encoder supports
  it, `--bits 8|10` (default: the source's depth, at most 10).
- Colour: the source's matrix, range and chroma siting are kept (no trip through RGB); HDR
  sources are refused.

Conforming re-encodes, so pixels change slightly (generation loss) unless `--lossless`, and
a VFR source gets frames duplicated or dropped onto the constant-rate grid. `conform`
prints what it did. Encoders aren't bit-exact across versions, so the conformed file, not
the source, is the film's input. A source that is already suitable (AV1, CFR, Cues) needs
no conform; `conform` says so and writes nothing.

**Our own export is already readable:** built-in `export --format webm` writes AV1, CFR
(frame n at n·den/num s, nearest millisecond), closed GOPs with a cluster and Cue per
keyframe, BT.709 limited range tagged in both the sequence header and the Colour element.
Its 10-bit default is fine (the reader handles 10-bit). `--keyint` becomes a command-line
option on both. AV1 in WebM has no standard alpha: an exported film with transparency is
read back as a PNG sequence (phase 1) or as a separate matte clip.

**The decoder is behind a trait** so hardware decoding can be added later without touching
readers or the renderer:

```rust
trait VideoDecoder {
    fn reset(&mut self);                                     // before a keyframe (a seek)
    fn send(&mut self, temporal_unit: &[u8]) -> Result<()>;
    fn receive(&mut self) -> Result<Option<YuvFrame>>;       // planes, 8 or 10 bit
}
```

Phase 2 ships one implementation, software (rav1d).

#### Hardware decoding (later)

AV1 decoding is bit-exact by specification, hardware included, so a conformant hardware
decoder gives the same YUV as rav1d and frames stay deterministic. AV1 decode is common
(NVIDIA RTX 30+, AMD RDNA2+, Intel Xe/Arc, Apple M3+). It pays off for 4K, 60 fps, several
video channels, laptops on battery and long-GOP seeks; at 1080p30 rav1d on a desktop CPU
is enough, so it is not part of phase 2.

- **One backend at a time**, each a `VideoDecoder`: D3D11 video on Windows first (all three
  GPU vendors, and the owner's machine), then VA-API (Linux) and VideoToolbox (macOS).
  Vulkan Video would be one API everywhere but is a second graphics stack beside OpenGL.
- **Readback first:** decode on the GPU, read NV12/P010 back to the CPU, then the same Rust
  YUV→RGBA and upload path. Zero-copy interop (WGL_NV_DX_interop, EGL dmabuf, IOSurface)
  only if profiling shows the readback hurts; it would also move YUV→RGBA to the GPU,
  which needs a contract-respecting answer (no built-in GLSL; GPUs differ slightly).
- **Used only when verified:** on first use per (device, driver, profile, bit depth), a
  small clip is decoded both ways and compared bit for bit; any mismatch, unsupported
  profile, session limit or decode error falls back to software, logged with the reason.
  `SHOTGLASS_VIDEO_DECODER=software|hardware` forces a choice; export and headless commands
  may default to software. Selftest: hardware and software agree on the video fixtures
  where a hardware decoder exists.

**Open for phase 2:** rav1d vs dav1d after measuring; the long-GOP note's threshold.

### Colour

**YUV → RGB on the CPU, in Rust**, on the reader's worker thread: fixed-point BT.601,
BT.709 or BT.2020 (non-constant luminance) matrices, limited or full range, chroma
upsampled with a fixed bilinear filter whose phase follows the stream's chroma siting
from the file (`left`, the AV1 and H.264/HEVC default for 4:2:0, when untagged). Output is RGBA8 with alpha 255
(or the clip's alpha plane), gamma-encoded as the file's transfer leaves it, i.e. the
same kind of numbers as a JPEG photo. So `srgb = false` (the default) reads the encoded
values as on Shadertoy, and `srgb = true` uploads as `SRGB8_ALPHA8` for linear sampling,
exactly as for images. (BT.709's transfer isn't sRGB's, but the difference is small and is
the convention every NLE and browser uses for SDR video on an sRGB display.)

Matrix and range come from the file's tags (the Matroska Colour element, else the AV1
sequence header); untagged files follow ffmpeg's rule (height ≥ 720 → BT.709, else
BT.601; YUV → limited range). `matrix`/`range` override.
HDR transfers (PQ, HLG) are refused in phase 2.

Why not on the GPU: a YUV→RGB pass would be engine-provided GLSL, which CLAUDE.md rules
out ("the engine provides a contract, not looks"), and its results would differ slightly
between GPUs. The CPU cost is a few ms per 1080p frame with plain Rust over rows (the
compiler vectorises it), on a worker thread. If bandwidth or CPU later bites, the
contract-respecting option is to expose the **planes** instead (`planes = true` binds
Y, U, V as three `R8` channels) and let a library (`<kit/footage/yuv.glsl>`) convert;
that's a contract addition, not built-in GLSL (open question 3).

**Alpha** is stored straight, as decoded (PNG sequences and ProRes 4444/VP9 alpha are
straight in practice). Premultiplied sources are the shader's business, as for images.

**Phase 3: 16-bit and EXR.** Blender's best output (EXR, 16-bit PNG) is linear or
high-precision. These upload as `RGBA16F` (filterable in GL 3.3 and WebGL 2; 8 bytes per
texel), decoded with the `image` crate's `exr` feature (pure Rust) and its 16-bit PNG
path. `srgb` doesn't apply (data is already linear for EXR; 16-bit PNGs are as stored).
This also settles #22's decision 5 for still images.

## Renderer (`gfx.rs`)

One `gfx.rs` change, in one PR (see [Plan](#phased-plan)). The shape follows #22's
texture store; the new parts are frame keys, windows and a separate upload budget.

### Keys and state

```rust
struct FootageKey(u64);        // hash of (source identity, vflip, srgb, matrix, range, mipmapped)
                               //   sequence identity: pattern, first, count, frame-0 len+mtime
                               //   video identity: path, len, mtime
struct FrameKey { footage: FootageKey, k: u64 }
```

`ChanKey` gains `footage: Option<FootageKey>` and a per-channel timing (`Timing` plus the
source rate and length), so the per-shot-id **fallback remembers the footage** that went
with the last working programs, as it does for textures. Each `FrameKey` has a
`TexState` (Idle → Decoding → Decoded → Uploading → Ready / Failed) exactly as images do;
`Ready` frames live in a per-footage **slot pool** of same-sized GL textures that are
reused rather than created and deleted per frame (allocation once per slot with
`tex_image_2d(…, None)`, then only `tex_sub_image_2d`).

### Gating

`keys_ready(k)` for a shot at film frame `n` checks its programs, its still textures, and
for each footage channel the `FrameKey` for `source_frame(s, c, n)` (None = black is
always ready). `usable(i, blocking)` therefore depends on `n`; the renderer passes it
through (`render_at` already knows the frame). Rules, as for everything else:

- **Playing** (`blocking = false`): the exact frame `Ready`, else the fallback version's
  exact frame `Ready`, else None: the previous film frame stays up and the player counts a
  stall. Never a neighbouring source frame: every frame on screen is one `still` would
  render (open question 4 discusses relaxing this).
- **Paused, scrubbing, headless** (`blocking = true`): decode and upload the frame now on
  this thread (sequences: `texture::load`; video: a seek reader, read one frame),
  counted in `blocking_footage_loads` (selftest: 0 while playing).

Freeze frames (frozen transitions) render the outgoing shot once at the transition's first
frame, so they need that one source frame; the freeze key includes it.

### Prefetch

`pump` already orders work by `priority(movie, t)` (wrap-aware `distance` for loop films).
Footage adds frames to that order:

- **The window:** for the shot(s) on screen at film frame `n`, the source frames for film
  frames `n … n + W`, where `W` covers about 0.5 s (and at least 3 frames), clipped by the
  memory share below. Frames are mapped through `source_frame`, so repeats (24 in 30)
  and `speed` are accounted for exactly, and `end = "loop"` wraps naturally.
- **Heads:** for each footage shot that starts within 2 s (`clock = "shot"`), its first
  `H` frames (at least the first 3). This is what makes **cuts, transitions and loop seams**
  stall-free: the incoming shot's first frames are ready before the playhead arrives. For
  a loop film the head of shot 0 is "within 2 s" near the end of each period (wrap-aware
  distance), so the seam is a cut like any other.
- **Paused:** the window around the playhead in both directions (±W) so stepping a frame
  either way is instant; no heads.
- Video: the window is what drives the reader positions; a head beyond the current
  reader's reach gets its own reader (spawned ≥ 2 s early: process start plus decode from
  the previous keyframe can take a few hundred ms for long-GOP files).

### Uploads

- **Footage has its own per-`pump` budget**, separate from images' 8 MB: enough for one
  frame per active footage channel per film frame, plus catch-up (twice that). A 1080p
  RGBA8 frame is 8.3 MB, i.e. 250 MB/s at 30 fps: comfortable as client-memory
  `tex_sub_image_2d` on desktop GPUs (#22 measured ~1–3 ms per 8 MB on the dev GPU), but
  it must be tuned on the owner's machine. Bands as today, so a 4K frame (33 MB) spreads
  over two film frames' pumps without one long call.
- **Unpack state** as #22 pinned: `PIXEL_UNPACK_BUFFER` unbound, alignment 1 per band then
  restored to 4, row length/skips 0. Same helper, same selftest.
- **Ready means submitted.** No fences on the client-memory path: GL orders the uploads
  before the draw. Reusing a slot is also safe without a fence (GL orders
  `tex_sub_image_2d` after earlier draws that sampled the old contents; the driver copies
  or renames as needed).
- **Phase 3: persistently mapped PBO ring** (GL 4.4 `ARB_buffer_storage`, as the sound
  path uses): the worker thread writes decoded frames straight into mapped memory, `pump`
  issues `tex_sub_image_2d` from the PBO offset, and a fence per slot (polled oldest-first
  with `glClientWaitSync(.., 0)`, batched, as `poll_audio` does) says when a PBO region may
  be reused. Drivers without 4.4 (macOS GL 4.1, the Apple Software Renderer) keep the
  client-memory path; selftest runs both, as it does for sound.
- **Where:** inside `pump`, timed as housekeeping, outside the GPU timer's begin/end, so
  upload time is a stall at worst and never lowers the auto scale. The Performance panel
  gets a "footage" share (decode lag, upload ms, frames missed while playing).

### Memory

Footage counts in the **256 MB** texture budget: each `Ready` frame's bytes (4 or 8 per
texel, ×4/3 with mipmaps). The window size per channel is chosen from a share of the
budget: `W = clamp(budget_share / frame_bytes, 3, 0.5 s of frames)`, where the share is
the budget minus the shot's still textures, divided among its footage channels and the
upcoming heads. A 1080p channel gets its full 15-frame window (125 MB); a 4K channel 3–4
frames. `check` and `load` refuse a shot whose minimum (3 frames per footage channel plus
its images, plus heads of the next shot) exceeds the budget, with #22's message style.

CPU memory: decoded frames waiting to upload are bounded by their own cap (64 MB, at
least 2 frames per channel); workers stop when it's full. Video readers' pipes add one
frame each.

Eviction: frames behind the playhead go first, then frames past the window, then heads of
shots no longer near; frames held by a fallback stay. Slots return to the pool rather
than being deleted, and pools are deleted when their footage leaves 1.5 × `LOOKAHEAD`.

### Performance expectations

- Real GPU, 1080p30 JPEG sequence or AV1 WebM: real time with a 3–4-thread pool or one
  reader; 4K60 is beyond the client-memory path's comfort and wants the PBO ring.
- llvmpipe: uploads and sampling are slow; CI only uses tiny frames (64×36) and judges
  correctness, never speed.
- When decode can't keep up while playing, the player stalls on those frames (the clock and
  sound carry on, as for any stall), the panel says which footage is behind, and README
  suggests JPEG, smaller frames, or `shotglass conform --keyint` for long-GOP files.

## Export, still, strip, check

- `export` renders frame `n` with `blocking = true` in order, so a video channel is read
  by one sequential reader for the whole export (no seeks; exports run as fast as decode
  allows).
- `still`/`strip` at arbitrary frames seek per frame (up to one GOP of decoding each).
- `check` lists footage with size, rate, length, frame count, memory per window, and
  warns about: a shot running past the clip end with `hold`, loop-length mismatches in loop
  films, VFR (error), untagged colour (note), keyframes more than 4 s apart (note), large files not in Git LFS (see below),
  reading an `iChannel` a shot doesn't bind (existing lint). `check --deep` decodes every
  sequence frame (sizes, corrupt files) and every video frame (sequentially), comparing a
  sample against seeks.
- `check --web` (phase 1): sequences pass (same decoder in wasm, preloaded); video is
  reported as not playable on the web yet (#2 decides), and footage larger than 1080p or
  more than ~200 MB total preload gets a warning.

## Live reload

- The watcher filter adds the video extensions (images are already there), with a
  debounce for sequences (a renderer writing frames triggers many events: wait ~300 ms of
  quiet before reloading).
- A footage edit goes through the normal reload. Only footage whose key changed is
  dropped; programs aren't recompiled.
- **A broken frame keeps the last working version**: a frame that fails to decode
  (half-written PNG) falls back to the previous version's same frame if the fallback has
  it, otherwise that shot isn't drawn while playing and the error shows; blocking renders
  fail headless (as `load_all` does for textures) and draw the channel black in the
  player, as #22 does.
- **A missing sequence, a gap, or a missing video refuses the whole new version** (an
  edit-level mistake), like a missing `#include`.
- Sequence keys use frame 0's length+mtime and the count, not every frame's stat (a
  10,000-frame folder would make every reload slow). A re-render of frames in place is
  caught because the watcher sees the files change: the reload bumps a per-footage
  generation for the changed frame numbers, so only those frames reload.

## Audio in clips

Phases 1–2 **ignore clip audio** (the reader skips other tracks; `conform` drops audio). shotglass's sound is GLSL synthesised in
chunks, deterministic and chunk-size independent; an audio file is a different kind of
input. When it's wanted, the natural design (with #32's audio samples) is a sound-side
input: `[sound] samples = { surf = "footage/surf.webm" }` decoded once to PCM at
the soundtrack rate, uploaded as a texture, and read by sound shaders through a contract
function (`sample(SURF, samp)`), with the same `in`/`speed` timing as the picture channel
so picture and sound stay locked. A sample lookup is a pure function of the sample index,
so the invariants (same sound whatever the chunk size, streamed = blocking) hold. It's
listed under phase 3 and needs its own design.

## Dependencies and licensing

- **Phase 1 adds nothing.** `image` already has `png` and `jpeg`.
- **Phase 2 adds rav1d** (BSD-2, Rust) behind a `video` feature, listed by `shotglass
  licenses` like `ENCODER_LICENCES`. The demuxer is ours. AV1 is royalty-free. Playback
  needs no ffmpeg; `shotglass conform` runs the user's ffmpeg as a process (as `export`
  can), so nothing is linked and there are no LGPL/GPL obligations.
- **Availability.** The video selftests and golden need only the `video` feature (the test
  clip is committed; fixtures are written by our own encoder with the `webm` feature), so
  they run on every CI platform. Only `conform`'s tests need ffmpeg and skip without it
  (Windows CI), as export's do.
- **Phase 3 EXR**: the `image` crate's `exr` feature (pure Rust, BSD-3), fine for wasm.
- **Footage in git.** Plates are large. `.gitattributes` in the template gains `*.mp4`,
  `*.mov`, `*.webm`, `*.mkv` as `binary`, and README recommends Git LFS (the existing
  LFS-pointer error extends to videos: an LFS pointer starts with
  `version https://git-lfs`). `check` warns about footage over 50 MB that isn't an LFS
  file. Library footage (`<kit/…>`) is pinned by the lock's sha256 like any library file,
  which costs a full read of each clip when hashing; #32's content-addressed packs are the
  long-term home for big footage, and footage will accept `<pack/…>` paths when they exist.

## Engine contract

Additive: `movie.toml` gains keys, and `iChannelTime[n]` gets a meaning (it was always 0).
Assembled shader source doesn't change, so program hashes and the program cache are
unaffected, and films without footage render bit for bit as before. A **patch bump** for
phase 1 (sequences), another for phase 2 (video), each with #22's "this film needs
shotglass 0.1.x" message and a `check` warning when a film uses footage but its
`shotglass` field allows an older engine. Phase 3's `RGBA16F` channels are still
`sampler2D` (no source change); `planes = true`, if adopted, is a contract addition.

## Invariants kept

| CLAUDE.md invariant | how |
|---|---|
| Playback never blocks on a compile or a texture | footage frames are `TexState`s; `keys_ready` needs the exact frame; only `blocking = true` loads on the spot, counted in `blocking_footage_loads` |
| Render once per film frame; auto scale from GPU time only | decode/upload in `pump` (housekeeping, outside the GPU timer); a missing frame is a stall, not a drop |
| No per-frame GPU→CPU syncs | client-memory uploads need no fence; the phase-3 PBO ring polls fences oldest-first with timeout 0, batched; no `glGetError`/reads added |
| Deterministic frames; play/still/export agree | `source_frame` is pure, from the frame number in f64; sequence frames use the one Rust decoder; video YUV is bit-exact by codec spec, converted in Rust; playback never substitutes another frame |
| Loop films see one period | the mapping takes `wrap_frame(n)`; heads make the seam a cut |
| A broken edit keeps the last working version | footage in `ChanKey`, so the per-shot fallback keeps old frames; missing files refuse the version; broken frames fall back per frame |
| GL 3.3 core / ES 3.00, macOS GL 4.1, llvmpipe | only `tex_sub_image_2d`, sampler objects, `RGBA8`/`SRGB8_ALPHA8` (`RGBA16F` phase 3, core in 3.3 and filterable in ES 3.0); persistent PBOs only where 4.4 exists |
| Unpack state restored | same upload helper as textures |
| No built-in GLSL | conversion on the CPU in Rust; no shader added |

## Tests

### Unit tests

- **`source_frame`** (`movie.rs`): identity at 30/30 and 30000/1001 over 10⁶ frames;
  24-in-30, 25-in-24, 60-in-24, 24000/1001-in-30000/1001 patterns against hand-computed
  sequences; `in`/`in_frame`; `speed` 0.5, 2, 0 (freeze); `end` hold/loop/black at and
  past the last frame; `clock = "movie"` across shots; the outgoing shot during a
  transition (clock past its duration); loop films (frame n + j·period gives the same
  answer, `wrap_frame` used); proptest: for `speed ≥ 0` the result is non-decreasing in
  `n` within a shot (before `loop` wraps) and never panics.
- **Parsing**: every key, shorthand detection (`#`, extensions), `texture =` re-timing,
  the error messages above, unknown keys, the near-valid-`movie.toml` proptest.
- **Sequence scanning** (temp dirs of empty files): `first` detection, gaps, padding
  overflow (`fog_10000.png` in `####`), case mismatches, `%04d` hint.
- **WebM reading** from small committed and generated files: CFR/VFR detection, Cues, nearest-ms timestamps, HDR refused, 10-bit,
  colour tags and the untagged rule.
- **YUV → RGB** (`footage.rs`): known colours (BT.601/709/2020 × tv/pc) to the expected
  RGBA8, chroma siting, alpha plane, odd widths/heights.

### Selftest checks

Fixture sequences are generated by the selftest (tiny PNGs, as #22's fixtures are), each
frame encoding its own number in a few pixels (e.g. `R = k`, `G = 255 − k`, a binary
stripe) so a check can read back exactly which source frame was drawn. Each check is
mutation-checked (break the code, see it fail), and every fixture passes the strict check
at 330 and ES 3.00.

1. *Footage frames map exactly:* a 30 fps film with a 24 fps sequence, an `in`, a
   `speed = 0.5` channel and an `end = "loop"` channel; blocking renders of a range of
   film frames read back `k` and `iChannelTime`, compared with `source_frame`.
2. *Playback never blocks on footage:* non-blocking renders paced over two seconds of a
   footage shot: `blocking_footage_loads == 0`, every drawn frame shows the exact `k`
   (never a neighbour), and a shot whose frame isn't ready returns None.
3. *Cuts and loop seams are prefetched:* playing into a footage shot (and across a loop
   film's seam) with a generous budget, after one warm-up pass, no stall at the cut.
4. *Play, still and export agree:* frames from the export path (sequential, blocking) equal
   `still` renders of the same `n`, including after seeking backwards and after eviction
   with a tiny budget (frames come back identically).
5. *Memory stays in budget:* with a tiny budget, resident footage bytes never exceed it,
   slots are reused (texture creations stay at the pool size), and a shot that can't fit
   its minimum window is refused.
6. *Broken frame / missing frame:* overwrite one frame with half its bytes → the previous
   version's frame is drawn and the error reported; delete a middle frame → the new
   version is refused.
7. *Uploads restore unpack state* with footage interleaved with sound streaming (extends
   #22's check).
8. Phase 2, *video*: the selftest encodes the generated sequence with our encoder (short
   and long `keyint`, 8- and 10-bit) and checks: frame `k` from a seek equals frame `k`
   read sequentially (bit for bit), from every position including across GOPs; the mapping
   matches `source_frame` at 30000/1001 (nearest-millisecond timestamps); playback of the
   clip has no `footage_misses` and no blocking loads; known YUV bars convert to within 1
   of the expected RGBA8. `conform` (skipped without ffmpeg): an already suitable WebM is
   left alone, and a conformed VFR clip reads as CFR.
9. Phase 3: the PBO path and the client-memory path give identical frames, with and
   without persistent buffers, as for sound.

### Golden strip

A new case in `tests/golden.rs`: `tests/films/footage/` with a **committed tiny sequence**
made for the test (CC0): 24 frames at 64×36, JPEG for the plate and RGBA PNG for a matte,
under 100 KB in total, with a visible frame counter so a diff shows a timing error at a
glance. The film exercises 24 fps in a 30 fps film, `in`, `speed = 0.5`, `end = "loop"`,
`clock = "movie"` across a cut, and the matte's alpha. Reference
`tests/golden/footage.png` (8 frames, 320×180, llvmpipe), and the film joins
`validate.rs`'s "passes everywhere" list. Phase 2 adds `tests/films/footage-video/` with a
tiny committed clip (AV1 in WebM written by our own export, about 20 KB, keyframes every 8
frames so the strip crosses a GOP), golden wherever the `video` feature is built. The demo and the template keep their looks (no re-bless).

## Phased plan

**Phase 1: image sequences (MVP).** Two PRs, so `gfx.rs` is touched by one.

- *PR 1a (no `gfx.rs`/`sound.rs`/`audio.rs`):* `movie.rs` syntax, `Source`/`Timing`,
  `source_frame` and its unit tests; `footage.rs` with sequence scanning and errors;
  `check` (+`--deep`, `--web`); watcher extensions; the committed golden sequence; the
  template's commented example. Loading a film with footage is refused with "footage
  channels arrive in the next version" until 1b (or 1a and 1b land together, as #22 did).
- *PR 1b (the `gfx.rs` change):* frame keys, slot pools, decode pool, footage budget,
  window/heads in `pump`, gating with the frame number, fallback, eviction,
  `blocking_footage_loads`, Performance panel share; selftests 1–7; golden case;
  README section, uniform table (`iChannelTime`), CLAUDE.md map and invariants; contract
  patch bump. Owner checks real-time 1080p JPEG playback on Windows/NVIDIA and tunes
  the budget.

**Phase 2: video, WebM + AV1 in-process (#73).** WebM demuxer (from `webm/mux.rs`'s
reader), rav1d behind a `video` feature, YUV→RGBA in Rust (`footage.rs`), readers that
seek through the Cues; the renderer side should need no change beyond a new decode job
kind (`trait FrameSource { fn frame(&mut self, k: u64) -> Result<Decoded> }`, sequential
sources cheap for k+1), a `VideoDecoder` trait (rav1d only; hardware backends later, own issues) and a reader pool per clip. `--keyint` on export; `shotglass
conform` (ffmpeg decode → our encoder). Selftest 8, video golden case, contract patch
bump, README "Footage" (WebM, conform, LFS).

**Phase 3 (each its own issue):** RGBA16F (EXR, 16-bit PNG); persistent PBO upload ring;
clip audio as sound samples (#32); reverse `speed`; frame blending; footage on the web
player (#2: sequences already work; video via WebCodecs or a wasm decoder, judged against
"web and desktop render the same frames"); `planes = true` if the CPU conversion becomes
the bottleneck.

## Alternatives

| option | for | against | verdict |
|---|---|---|---|
| **ffmpeg subprocess** (the first proposal) | every codec the user's ffmpeg has; nothing linked or bundled; same model and install story as export; bit-exact decoders | process per reader; seek latency of a process start plus a GOP; needs CFR for exact frame mapping; parsing ffprobe output | replaced by in-process WebM (#73); ffmpeg kept only for `conform` |
| **Automatic intra-only conform cache** (#73's first revision) | every seek is one frame; VFR handled once | disk use grows with the footage (unsafe with small temp space, e.g. cloud machines); a hidden re-encode changes pixels; encoder output differs across versions | dropped; `conform` is explicit and writes a file the user keeps |
| **Image sequences only** (phase 1) | zero dependencies, trivially frame-exact, random access, works on the web today, the format renderers write anyway | huge on disk for live-action; PNG decode is slow; users must convert clips themselves | the MVP |
| **`ffmpeg-next` / `video-rs` (linked libav)** | no process overhead, precise seeking by pts, VFR possible | C build pain on Windows/macOS (vcpkg, pkg-config), version coupling, LGPL (or GPL with x264-enabled builds) obligations when distributing binaries, doesn't build for wasm | not now; revisit if subprocess latency proves a problem |
| **Platform decoders** (Media Foundation, AVFoundation, VA-API) | hardware decode, no install | three code paths; colour conversion and rounding differ per OS, breaking cross-machine agreement; frame-accurate seeking is awkward | no |
| **Pure-Rust decoder** (rav1d for AV1 + a WebM demuxer) | no external tool, no process per seek, no disk cache, bit-exact everywhere | one codec (users must transcode to AV1, slow to encode), demuxing and seeking to write ourselves | **chosen for phase 2** (#73): WebM only, our demuxer (we already write WebM), `conform` for other formats; browsers decode the same files with WebCodecs (#2) |
| **GStreamer** | full pipeline, hardware decode | very heavy dependency, same linking/install issues as libav | no |
| **Hardware decode with GL interop** (NVDEC, VA-API → GL) | zero-copy 4K | vendor-specific, not on llvmpipe or macOS GL 4.1, nondeterministic conversion | no |
| **YUV→RGB in an engine GLSL pass** | a third of the upload bandwidth at 4:2:0 | built-in GLSL (against CLAUDE.md), GPU-dependent rounding | no; `planes = true` + a library conversion is the compliant version |
| **Approximate playback** (show the nearest ready frame while decode lags) | smoother on slow machines | playback could show frames `still` never renders, hiding timing bugs; breaks the invariant | no (open question 4) |
| **Nested films** (`{ film = "../intro" }` rendered live into a channel, like Remotion's compositions) | no pre-render step, edits propagate | another movie's full renderer state inside a channel, its sound, its own compile/texture budget; a large design | later, separately; precompose with `export --format png` meanwhile |

## Open questions

1. **Defaults for `end`:** `hold` (proposed: footage shouldn't jump unexpectedly) or `loop`
   (Shadertoy's videos loop)?
2. **Default `filter` for footage:** `linear` (proposed: no per-frame mipmap cost) or
   Shadertoy-style `mipmap` like images, for consistency?
3. **Is `planes = true` (Y/U/V as `R8` channels, converted by a library) acceptable later,**
   or should conversion always stay on the CPU?
4. **Strictness while playing:** keep "exact frame or stall" (proposed), or allow an opt-in
   preview mode that shows the nearest ready frame, clearly marked, on slow machines?
5. **Sequences being rendered:** treat missing trailing frames as "not there yet" (the
   sequence is as long as what exists, proposed) or require a declared `count`, so a
   half-finished render isn't silently shorter?
6. **Should footage follow `time_scale`/`time_offset`?** Proposed no (independent `in`/`speed`);
   a `follow_shot_time = true` could map `iTime` instead, which some ShaderToy-style films
   might expect.
7. **Frame blending** (`blend = true`, two frames mixed for rate conversion or smooth slow
   motion): engine-side would need a second texture per channel and a contract change;
   or leave it to shaders reading two channels with `in_frame` offsets.
8. **Probing at `Movie::load`:** acceptable start-up cost (cached), or probe lazily in the
   renderer and let `check` do the up-front validation?
9. **Footage storage:** Git LFS guidance for now, `<pack/…>` (#32) later; should
   `bundle` refuse or warn above a size?
10. **Clip audio:** wanted soon (drives #32's sound-sample design), or not at all?
