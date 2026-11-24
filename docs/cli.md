# Command line and export

Every `shotglass` command and its options, and exporting a film to video with ffmpeg.

[README](../README.md) · [Movies](movies.md) · [Channels](channels.md) · [Layers and blends](layers.md) · [Sound](sound.md) · [Loop films](loop-films.md) · [The player](player.md) · [Projects and libraries](libraries.md) · [Testing GLSL and quality metrics](testing-and-quality.md) · [Command line and export](cli.md) · [Testing shotglass](testing.md)

## Command line

```sh
shotglass play   <movie> [--at T] [--scale auto|0.1..1.0]
shotglass still  <movie> --at 3.5,moon,closeup+2 [--size 1280x720] [--out stills/]
shotglass strip  <movie> [--count 12 | --at ...] [--size 384x216] [--cols 4] [--out strip.png]
                 [--compare ref.png [--bless] [--tolerance 8] [--budget 0.1%] [--max-mean 0.25]]
                 # --compare: fail if the strip differs from ref.png (writes strip.diff.png)
shotglass check  <movie> [--web] [--no-gpu] [--deep] [--size 1280x720] [--frames 8]
                 # validate movie.toml and the GLSL (strictly), compile, synth, time
                 # --deep: also decode every frame of every image sequence
shotglass wav    <movie> [--out sound.wav] [--rate 48000] [--allow-silence]
                 [--compare ref.wav [--bless] [--max-rms 0.0015] [--max-peak 0.01]]
                 # --compare: fail if the sound differs from ref.wav (writes sound.diff.wav)
shotglass export <movie> --out film.webm [--size 1920x1080] [--format webm|av1|ffv1|png|ffmpeg] [--rate 48000]
                 [--loops N]                               # loop = true films: N periods
shotglass new    <dir>
shotglass fetch  <movie>                                   # download libraries, write shotglass.lock
shotglass update <movie> [library..]                       # move library pins to the newest allowed
shotglass licenses <movie>                                 # libraries the film includes, film fonts, and their licences
shotglass fonts add "Family" [..] [--film <dir>]           # Google Fonts families as libraries, plus [fonts] entries
shotglass bundle <movie> --out <dir>                       # self-contained copy with the library files it includes
shotglass freeze <movie>                                   # copy the library files it includes into the film
shotglass unfreeze <movie>                                 # follow the project's libraries again
shotglass test   [file|folder] [filter]                    # GLSL unit and property tests (*.test.glsl), metric tests (*.test.toml)
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
shotglass export movies/my-film --out my-film.webm                  # VP9 + Opus
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
  it and stay in sync.
- **Colour.** Video formats are converted with the BT.709 matrix and tagged BT.709,
  limited range, so players don't shift the colours. FFV1 stores the RGB pixels
  as rendered (full range, tagged as RGB). VP9 and AV1 store colour at half
  resolution, so the size must be even.
- **Progress and Ctrl-C.** Progress goes to the terminal. Ctrl-C stops the export
  (a second Ctrl-C quits at once). Video exports replace the destination only
  after encoding succeeds.
- **PNG retries.** Export PNGs to a fresh folder for each retry. Frames and
  `sound.wav` are written directly into the folder, so an interrupted export can
  leave partial results and a shorter or silent export can leave older files there.

| `--format` | default for | video | audio | use it for |
|---|---|---|---|---|
| `webm` | `.webm` | VP9, CRF 20 | Opus 192 kb/s | sharing, the web |
| `av1` | | AV1 (SVT-AV1, else libaom) | Opus 192 kb/s | smaller files, slower encode |
| `ffv1` | `.mkv` | FFV1, lossless RGB | FLAC | archival, re-encoding later |
| `png` | a folder (no extension) | `frame_000000.png` … | `sound.wav` | editors and compositing; needs no ffmpeg |
| `ffmpeg` | | your `--ffmpeg-args` | | anything else your ffmpeg can do |

All the defaults are royalty-free codecs. To use another one, pass your own ffmpeg
output arguments; the container comes from the `--out` extension, and you choose
the colour handling yourself:

```sh
shotglass export movies/my-film --out film.mov --format ffmpeg \
  --ffmpeg-args "-c:v prores_ks -profile:v 3 -c:a pcm_s16le"
```

### Installing ffmpeg

Every format except `png` pipes raw frames to an [ffmpeg](https://ffmpeg.org)
process, so ffmpeg must be on your `PATH` (or set `SHOTGLASS_FFMPEG` to its full
path). shotglass doesn't link or ship any codecs.

- **Windows:** `winget install Gyan.FFmpeg` (then open a new terminal), or download a
  build from <https://ffmpeg.org/download.html> and add its `bin` folder to `PATH`.
- **macOS:** `brew install ffmpeg`
- **Linux:** `sudo apt install ffmpeg` (Debian/Ubuntu), `sudo dnf install ffmpeg`
  (Fedora, with RPM Fusion), or your distribution's package.

Check with `ffmpeg -encoders`: `webm` needs `libvpx-vp9` and `libopus`, `av1` needs
`libsvtav1` or `libaom-av1`, and `ffv1` needs `ffv1` and `flac` (built in).
