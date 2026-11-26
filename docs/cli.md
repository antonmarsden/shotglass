# Command line and export

Every `shotglass` command and its options, and exporting a film to video (WebM built in, other formats with ffmpeg).

[README](../README.md) · [Movies](movies.md) · [Channels](channels.md) · [Layers and blends](layers.md) · [Sound](sound.md) · [Loop films](loop-films.md) · [The player](player.md) · [Projects and libraries](libraries.md) · [Testing GLSL and quality metrics](testing-and-quality.md) · [Command line and export](cli.md) · [Testing shotglass](testing.md)

## Command line

```sh
shotglass play   <movie> [--at T] [--scale auto|0.1..1.0]
shotglass still  <movie> --at 3.5,moon,closeup+2 [--size 1280x720] [--out stills/]
shotglass strip  <movie> [--count 12 | --at ...] [--size 384x216] [--cols 4] [--out strip.png]
                 [--compare ref.png [--bless] [--tolerance 8] [--budget 0.1%] [--max-mean 0.25]]
                 # --compare: fail if the strip differs from ref.png (writes strip.diff.png)
shotglass check  <movie> [--web] [--strict] [--no-gpu] [--deep] [--size 1280x720] [--frames 8]
                 # validate movie.toml and the GLSL (strictly), compile, synth, time
                 # --deep: also decode every frame of every image sequence
shotglass wav    <movie> [--out sound.wav] [--rate 48000] [--allow-silence]
                 [--compare ref.wav [--bless] [--max-rms 0.0015] [--max-peak 0.01]]
                 # --compare: fail if the sound differs from ref.wav (writes sound.diff.wav)
shotglass export <movie> --out film.webm [--size 1920x1080] [--format webm|av1|ffv1|png|ffmpeg] [--rate 48000]
                 [--loops N]                               # loop = true films: N periods
                 [--quality 30 | --bitrate K] [--speed 6] [--depth 10|8] [--grain N]  # webm/av1
shotglass new    <dir>
shotglass fetch  <movie>                                   # download libraries, write shotglass.lock
shotglass update <movie> [library..]                       # move library pins to the newest allowed
shotglass licenses <movie>                                 # libraries the film includes, film fonts, and their licences
shotglass fonts add "Family" [..] [--film <dir>]           # Google Fonts families as libraries, plus [fonts] entries
shotglass bundle <movie> --out <dir>                       # self-contained copy with the library files it includes
shotglass shaders <film|file.test.glsl> [--web | --glsl <version>] --out <dir> # export assembled vertex and fragment GLSL
shotglass freeze <movie>                                   # copy the library files it includes into the film
shotglass unfreeze <movie>                                 # follow the project's libraries again
shotglass test   [file|folder] [filter] [--web [--strict]]                    # GLSL unit and property tests (*.test.glsl), metric tests (*.test.toml)
shotglass quality <movie> [--count 8 | --at ...] [--size 384x216] [--out quality-out/]
                 [--compare <dir> [--bless] [--budget 0.1%]] # heatmaps vs <dir>/<metric>.png
                 [--tighten [--margin 25%]]    # move limits towards the measured values
                 # metrics from quality.toml: report, quality.json, heatmaps; fails outside limits
shotglass selftest [filter]                                # GPU + sound checks on this machine
```

`still`, `strip`, `check`, `wav` and `export` render off-screen (`check --no-gpu` doesn't need OpenGL). On a Linux machine with
no display, run them under `xvfb-run`; Mesa's software OpenGL works.

For a film without music or shot sounds, `wav --allow-silence` writes a silent stereo
WAV for the full film duration at the requested sample rate. This path needs no
OpenGL and supports `--compare` and `--bless`.

## Export

`shotglass export` renders the film offline to a file. Playback is untouched: this
is a separate path that never drops a frame or lowers the resolution.

```sh
shotglass export movies/my-film --out my-film.webm                  # AV1 + Opus, no ffmpeg needed
shotglass export movies/my-film --out my-film.mkv --size 3840x2160  # lossless FFV1 + FLAC
shotglass export movies/my-film --out frames/                       # PNG sequence + sound.wav
```

- **Frame-accurate.** Frame *n* is rendered at exactly *n* / `fps` (`fps` can be a
  ratio, e.g. `"30000/1001"`, and is written to the file as one), at full size,
  waiting for every shader to compile. It's the same picture `shotglass still`
  renders at that time.
- **Sample-accurate.** The soundtrack is the same 16-bit PCM `shotglass wav` writes,
  `duration × rate` samples starting at sample 0 (a [loop film](loop-films.md#loop-films)'s
  period, rounded to whole samples, once per `--loops`). FLAC and WAV keep it bit for
  bit. Opus is lossy and has an encoder delay, which WebM records so players skip
  it and stay in sync; the built-in WebM also marks the padding at the end, so it
  decodes to exactly that many samples. Opus is always 48 kHz.
- **Colour.** Video formats are converted with the BT.709 matrix and tagged BT.709,
  limited range, so players don't shift the colours. FFV1 stores the RGB pixels
  as rendered (full range, tagged as RGB). VP9 and AV1 store colour at half
  resolution, so the size must be even. The built-in WebM converts on the GPU
  (chroma sited left, as MPEG and AV1 expect) and is 10-bit by default, so smooth
  gradients don't band.
- **Progress and Ctrl-C.** Progress goes to the terminal. Ctrl-C stops the export
  (a second Ctrl-C quits at once). Video exports replace the destination only
  after encoding succeeds.
- **PNG retries.** Export PNGs to a fresh folder for each retry. Frames and
  `sound.wav` are written directly into the folder, so an interrupted export can
  leave partial results and a shorter or silent export can leave older files there.

| `--format` | default for | video | audio | use it for |
|---|---|---|---|---|
| `webm` | `.webm` | AV1 (SVT-AV1), 10-bit, built in | Opus 160 kb/s | sharing, the web; needs no ffmpeg |
| `av1` | | the same, in `.webm` or `.mkv` (Matroska) | Opus 160 kb/s | AV1 in an `.mkv` |
| `ffv1` | `.mkv` | FFV1, lossless RGB | FLAC | archival, re-encoding later |
| `png` | a folder (no extension) | `frame_000000.png` … | `sound.wav` | editors and compositing; needs no ffmpeg |
| `ffmpeg` | | your `--ffmpeg-args` | | anything else your ffmpeg can do |

### WebM (built in)

`webm` (and `av1`) encode inside shotglass: frames are converted to YUV on the GPU,
read back without stalling the renderer, encoded by [SVT-AV1](https://gitlab.com/AOMediaCodec/SVT-AV1)
on its own threads, with the soundtrack in Opus ([libopus](https://opus-codec.org)),
and written by shotglass's own WebM writer. Keyframes come about every 2 seconds, so
browsers seek quickly. The flags only apply to these formats:

| flag | default | what it does |
|---|---|---|
| `--quality 0..63` | 30 | constant quality (SVT-AV1 CRF): lower is better and bigger |
| `--bitrate K` | | aim for K kbit/s of video instead (SVT-AV1 VBR); instead of `--quality`, for a size cap |
| `--speed 0..13` | 6 | encoder preset: higher is faster, a little bigger at the same quality |
| `--depth 10\|8` | 10 | bits per sample; 8 only for old decoders (gradients band) |
| `--grain 1..50` | off | film grain: removed before encoding and synthesised again by the player, so grainy films stay small |

```sh
shotglass export movies/my-film --out film.webm --quality 24 --speed 4   # better, slower
shotglass export movies/my-film --out film.webm --bitrate 4000           # about 4 Mbit/s
shotglass export movies/my-film --out film.mkv --format av1              # AV1 + Opus in Matroska
```

`shotglass licenses` lists the encoders' licences (SVT-AV1: BSD-3-Clause-Clear and
the AOMedia patent licence; libopus: BSD-3-Clause). A shotglass built without the
`webm` cargo feature has no built-in encoder: `--format webm` then goes through ffmpeg
as VP9 + Opus (it says so), and the flags above are refused.

For anything else, use ffmpeg with your own settings (and your own colour handling:
these convert with the BT.709 matrix and tag it, as the built-in path does). VP9, for
older players, and hardware AV1 (NVIDIA NVENC on RTX 40 and newer: faster, bigger):

```sh
shotglass export movies/my-film --out film.webm --format ffmpeg --ffmpeg-args \
  "-c:v libvpx-vp9 -crf 20 -b:v 0 -row-mt 1 -vf scale=out_color_matrix=bt709:out_range=tv,format=yuv420p \
   -colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range tv -c:a libopus -b:a 160k"
shotglass export movies/my-film --out film.webm --format ffmpeg --ffmpeg-args \
  "-c:v av1_nvenc -cq 30 -preset p5 -vf scale=out_color_matrix=bt709:out_range=tv,format=p010le \
   -colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range tv -c:a libopus -b:a 160k"
```

### Other formats

All the defaults are royalty-free codecs. To use another one, pass your own ffmpeg
output arguments; the container comes from the `--out` extension, and you choose
the colour handling yourself:

```sh
shotglass export movies/my-film --out film.mov --format ffmpeg \
  --ffmpeg-args "-c:v prores_ks -profile:v 3 -c:a pcm_s16le"
```

### Installing ffmpeg

Every format except `png` and the built-in `webm`/`av1` pipes raw frames to an
[ffmpeg](https://ffmpeg.org) process, so ffmpeg must be on your `PATH` (or set
`SHOTGLASS_FFMPEG` to its full path). shotglass links no codecs other than SVT-AV1
and libopus.

- **Windows:** `winget install Gyan.FFmpeg` (then open a new terminal), or download a
  build from <https://ffmpeg.org/download.html> and add its `bin` folder to `PATH`.
- **macOS:** `brew install ffmpeg`
- **Linux:** `sudo apt install ffmpeg` (Debian/Ubuntu), `sudo dnf install ffmpeg`
  (Fedora, with RPM Fusion), or your distribution's package.

Check with `ffmpeg -encoders`: `ffv1` needs `ffv1` and `flac` (built in); without the
`webm` feature, `webm` needs `libvpx-vp9` and `libopus`, and `av1` needs `libsvtav1` or
`libaom-av1`.

## Exporting assembled shaders

Run `shotglass shaders <film|file.test.glsl> --web --out <dir>` to export GLSL ES
3.00 sources for browser compile and link tests. With neither flag, the export
uses the film or test project's desktop dialect. Use `--glsl 330`, `--glsl 410`
or `--glsl 430` to override it for the export. The output folder must be new or
empty.

A film export includes the shaders assembled by `check`, including its quality
metrics. A `*.test.glsl` export resolves the test project's libraries and adds
the same assertion harness and dispatcher as `shotglass test`. Use a test file
that includes and calls the library function you want to check.

`manifest.json` has `schema` (currently 1), `dialect` and a list of `programs`.
Each program has a `label`, relative `vertex` and `fragment` filenames, and a
`sources` array that maps the fragment shader's GLSL source numbers to filenames.
Load each pair into a WebGL 2 program to compile and link it. The export command
only assembles the sources, so it can run without a GPU.
