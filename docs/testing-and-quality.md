# Testing GLSL and quality metrics

`shotglass test` (GLSL unit/property tests, metric tests) and `shotglass quality` (picture and sound metrics with limits).

[README](../README.md) · [Movies](movies.md) · [Channels](channels.md) · [Layers and blends](layers.md) · [Sound](sound.md) · [Loop films](loop-films.md) · [The player](player.md) · [Projects and libraries](libraries.md) · [Testing GLSL and quality metrics](testing-and-quality.md) · [Command line and export](cli.md) · [Testing shotglass](testing.md)

## Testing GLSL

Golden strips show *that* a film changed. To pin down what a function promises (its
endpoints, range, identities, continuity) write tests in GLSL and run them on the GPU:

```glsl
// tests/unit/animate.test.glsl
#include <kit/animate.glsl>
#include <kit/color.glsl>

void test_back_out_endpoints() {                 // no parameter: runs once
    expect_near(kit_ease_back_out(0.0), 0.0, 1e-6);
    expect_near(kit_ease_back_out(1.0), 1.0, 1e-6);
}

void test_bounce_in_range(float t) {             // property: t sweeps [0, 1]
    float y = kit_ease_bounce_out(t);
    expect(y >= 0.0 && y <= 1.0 + 1e-6, y);
}

// samples: 262144
void test_srgb_round_trip(vec3 c) {              // property: c sweeps the unit cube
    expect_near(kit_srgb_decode(kit_srgb_encode(c)), c, 1e-5);
}
```

```
$ shotglass test tests/unit
tests/unit/animate.test.glsl
  ok    test_back_out_endpoints
  FAIL  test_bounce_in_range (4096 samples)
        tests/unit/animate.test.glsl:11: expect(y >= 0.0 && y <= 1.0 + 1e-6, y)
        at t = 0.3637: got 1.0213
        12 of 4096 samples failed
  ok    test_srgb_round_trip (262144 samples)

error: 2 passed, 1 failed (266241 samples) on llvmpipe (LLVM 20.1.2, 256 bits)
```

- `shotglass test [file|folder] [filter]` runs every `*.test.glsl` file at or under the
  folder (default `.`; hidden folders, `target/` and `shotglass-libs/` are skipped), or
  one file. The filter keeps only tests whose name contains it. It exits non-zero if
  anything fails.
- A test file is assembled like a scene, with the libraries of its
  [project](libraries.md#projects): the nearest `shotglass.toml` at or above the file. Libraries
  come from the lock, overrides and the cache as for films (missing ones are fetched).
  Keep test files **outside** library folders (e.g. `tests/unit/` next to them): a file
  inside a library ships with it and changes its sha256.
- **Tests** are top-level `void test_<name>()` functions, or property tests taking one
  `float`, `vec2` or `vec3`. A property test's parameter sweeps a regular grid over
  `[0, 1]` per component, both ends included; map it to the range you need
  (`mix(-2.0, 2.0, t)`). Samples: 4096 for a `float`, 256² for a `vec2`, 64³ for a
  `vec3`, or `// samples: N` on the line just above the function (up to 4 194 304; a
  grid rounds up to a whole number of points per axis). Test names must be unique.
- **Each test reports its first failure**: the call as written, with `file:line`, the
  input of the lowest failing sample, the values, and how many samples failed. A test
  where no assertion ran on any sample fails ("ran no assertions"), so an early `return`
  can't hide one. Some samples skipping their assertions is fine.
- **Strict first:** each file must pass the strict GLSL check (as GLSL 330 and as GLSL ES
  3.00, or at the version a library needs) before it runs on the driver. A file that
  doesn't assemble, validate or compile is reported with `file:line`, and the other
  files still run.
- Tests run on the real driver, so they catch driver differences too: run them on your
  GPU as well as in CI (software OpenGL).

### Assertions

The engine provides these to test files only (never to films); they are part of the
engine contract (semver). `T` is `float`, `vec2`, `vec3` or `vec4`, and comparisons are
per component.

| assertion | passes when |
|---|---|
| `expect(bool cond)` | `cond` |
| `expect(bool cond, T value)` | `cond`; shows `value` on failure |
| `expect_eq(T a, T b)` | `a == b` exactly; also `int`, `ivec2`–`ivec4`, `uint`, `uvec2`–`uvec4` and `bool` (integers are reported exactly, whatever their size) |
| `expect_near(T a, T b, float tol)` | `abs(a - b) <= tol`, and both are finite |
| `expect_rel(T a, T b, float rel)` | `abs(a - b) <= rel * max(abs(a), abs(b))`, and both are finite |
| `expect_finite(T x)` | no NaN or infinity |

NaN never passes `expect_near`, `expect_rel` or `expect_finite`. Call assertions in the
test file itself (in a test or in a helper function there): the engine numbers each call
before compiling, which is how a failure names the exact call, even with several on one
line. Names starting with `sgtest_` are reserved in test files.

### Metric tests

A [quality metric](#quality-metrics) can be tested on fixed inputs instead of a film:
`shotglass test` also runs `*.test.toml` files, whose cases each run one metric on
given pictures or samples and judge it with `quality.toml`'s limits, expecting it to
pass or to fail. A library keeps its must-pass and must-fail cases next to its unit
tests:

```toml
# tests/quality/banding.test.toml
use = "quality/banding"          # defaults for every case: use, params, range,
params = { STEP = 4 }            #   threshold, size, samples

[[case]]
name = "smooth ramp"
frame = "fixtures/ramp.png"      # @frame: a PNG, or GLSL (mainImage)
mean = { max = 0.01 }            # limits: mean, min, max, over, p1..p99

[[case]]
name = "posterised ramp"
frame = "fixtures/steps.glsl"
size = "128x32"                  # GLSL pictures' size (default 64x64)
expect = "fail"                  # some limit must fail (default "pass")
mean = { max = 0.01 }

[[case]]
name = "click"
use = "quality/clicks"
sound = "fixtures/click.wav"     # a sound metric: a WAV, or GLSL (mainSound)
max = { max = 0.05 }
expect = "fail"
```

```
$ shotglass test tests/quality
tests/quality/banding.test.toml
  ok    smooth ramp  mean 0.000412  max 0.003922  (2048 measured)
  ok    posterised ramp  mean 0.031250  max 0.062745  (4096 measured); fails as expected: mean 0.031250 > max 0.01
  ok    click  mean 0.019999  max 0.999969  (100 measured); fails as expected: max 0.999969 > max 0.05
```

- A picture case sets `frame` and, for each other variant its metric reads, `reference`,
  `prev`, `next` (a variant the metric reads must be given, and one it doesn't read is
  refused). PNGs give the size; a `.glsl` input defines `void mainImage(out vec4
  fragColor, in vec2 fragCoord)` and is drawn at `size` (or the frame's), stored at 8
  bits like a rendered frame.
- A sound case sets `sound`: a 48 kHz 16-bit stereo WAV (as `shotglass wav --rate 48000`
  writes), or a `.glsl` defining `vec2 mainSound(int samp, float time)`, run for
  `samples` samples (default 48000). Outside the input is silence.
- `use`, `params`, `range` and `threshold` are those of a `[[metric]]`; `use` is a
  library metric or a `.glsl` file, and input paths are relative to the test file.
  Uniforms are those of time 0, frame 0.
- A case passes when it does what `expect` says; a NaN/+Inf measure is an error either
  way (a must-fail case must fail by a limit), and so is `expect = "fail"` without
  limits. Metric and input shaders get the same strict check as test files.

## Quality metrics

`shotglass quality <film>` measures sample frames (picked like `strip`'s) with the
metrics listed in the film's `quality.toml`. It prints a report, writes `quality.json`
and one heatmap contact sheet per metric (`<name>.png`) into `--out` (default
`quality-out/`; a project gets one folder per film; a folder holding GLSL or a
`library.toml` is refused, so a library's `quality/` is never written into), and exits non-zero when a metric is outside its limits. What
counts as good depends on the film, so metrics are GLSL from libraries (the kit's
`quality/`) or from the film, not built into shotglass.

```toml
# quality.toml, next to movie.toml
count = 8            # sample frames (--count / --at override)
# at = ["intro+0.5", 3.25]   # or explicit times, as --at spells them (not with count)
size = "384x216"     # frame size (--size overrides)
reference = 4        # @reference: N×N samples per pixel (2-8)

[[metric]]
use = "quality/alias"          # a library metric, or a film file: "fx/mine.glsl"
name = "alias"                 # optional: names the heatmap; default from use
params = { GAIN = 4, ink = "#ff8000", palette = ["#000000", "#ffffff"] }
range = [0, 1]                 # histogram range, for percentiles (default [0, 1])
threshold = 0.1                # `over` counts pixels whose measure is above this
mean = { max = 0.02 }          # limits, over all frames: mean, min, max,
p95 = { max = 0.1 }            #   p1..p99 (percentiles),
over = { max = 0.01 }          #   over (share of pixels above threshold)
```

A metric defines `void mainMetric(out vec4 value, in vec2 fragCoord)`: `value.a` is the
per-pixel measure, `value.rgb` its heatmap colour. It reads render variants of the
sample frame:

| variant | read with | what |
|---|---|---|
| `@frame` | `getFrame(uv)` | the frame at 1× (8-bit, as exported) |
| `@reference` | `getReference(uv)` | the frame rendered at N×N size and box-filtered to 1× |
| `@prev` | `getPrev(uv)` | the previous film frame (n − 1; frame 0 at the start, the period's last frame in a loop film) |
| `@next` | `getNext(uv)` | the next film frame (n + 1; the last frame at the end, frame 0 in a loop film) |

Each variant other than `@frame` is rendered only for metrics that call its function.

`iResolution`, `iTime` and `iFrame` are those of the sample frame. Params become
`#define`s: a number is a float (an integer too: `STEP = 4` is `4.0`, so write
`int(STEP)` where GLSL wants an int), 2–4 numbers a `vecN`, `"#rrggbb"` a `vec3` (0–1,
as written), and a list of colours `vec3[n](…)` plus `<NAME>_COUNT`.

**Not measured.** A metric that applies to some pixels only (outline ink, frames between
animation steps) sets `value.a = notMeasured()` elsewhere (-Inf): that pixel is left out
of every reduction (mean, min, max, `over`, histogram) and counted as `unmeasured`, never
invalid, so the mean covers the measured pixels only. Sound metrics have it too. Test it
with [metric tests](#metric-tests).

Measures are reduced on the GPU per frame (mean, min, max, `over`) and counted into a
256-bin histogram over `range`; over frames the mean and `over` are pixel-weighted,
min and max are the extremes, and the histograms add up. Percentiles (p50, p95, p99
and any a limit names) come from the histogram, exact to a bin: (range width) / 256.
Values outside `range` count in the end bins. Two checks are fixed: **invalid** counts
NaN/+Inf measures, which are left out of everything else and always fail the metric;
**cost** reports the GPU time of each frame (never gated; meaningless on software
OpenGL). `check` validates metric shaders strictly like every other shader (`check
--web` too). `quality.json` (`schema` 4: adds `unmeasured`) records the engine and library versions, the
size, each frame (with the `@prev`/`@next` frames shown), each metric's variants,
per-frame values, totals (with percentiles and the histogram), limits and failures,
and the `sound` section (with its metrics).

**Sound.** A `[sound]` table measures the whole soundtrack (one period of a loop film),
synthesised at 48 kHz, in the engine, not in GLSL:

```toml
[sound]
integrated = { min = -24, max = -18 }   # LUFS
true_peak = { max = -1 }                 # dBTP
```

The measures are those of ITU-R BS.1770-4 (as in EBU R 128): `integrated` loudness in
LUFS (K-weighted, 400 ms blocks, gated at -70 LUFS and 10 LU below the ungated
level), `momentary_max` (400 ms) and `short_term_max` (3 s) in LUFS, `sample_peak` in
dBFS and `true_peak` in dBTP (4× oversampled). Each takes a `min`/`max`. A limit fails
if its measure is missing: a film without sound, silence, or one shorter than the
window. A `quality.toml` may have only `[sound]`; then no frames are rendered. The
report's `sound` line judges these limits only; each sound metric has its own line.

**Sound metrics.** `[[sound.metric]]` entries (inside `[sound]`, same fields as
`[[metric]]`: `use`, `name`, `params`, `range`, `threshold`, limits) are GLSL run over
every sample of the soundtrack at 48 kHz:

```glsl
// metrics/clicks.glsl (abridged)
void mainSoundMetric(out float value, in int samp) {
    vec2 d = getSound(samp) - 2.0 * getSound(samp - 1) + getSound(samp - 2);
    value = max(abs(d.x), abs(d.y));   // the jump in slope
}
```

`getSound(samp)` is the stereo sample (silence outside the film, wrapped in a loop
film); `iSampleRate` and `iSoundSamples` are uniforms. Samples are measured in blocks
of 2^20, and a metric can read 100 ms (4800 samples) either side of the sample it
measures; further reads are silent. The measures go through the same reductions as
pixels (mean, min, max, over, percentiles; "pixels" count samples), and the report
gives the time of the largest (`max_at`), so a click can be found.

**The ratchet.** `quality --tighten` runs the metrics and, if every one passes, moves
each limit in `quality.toml` to the measured value plus `--margin` (default 25%, at
least 1e-6), rounded outward to 3 significant digits. It only ever tightens: a max goes
down, a min goes up; comments and layout are kept. Leave room for other renderers: CI's
software OpenGL and real GPUs differ by several percent on some metrics, so tighten
with a margin that covers both (or on the renderer CI uses).
`[sound]` limits move to the measured value ± 0.5 dB, rounded outward to tenths.

The demo's `quality.toml` (`movies/big-shadow`) uses all of these: two film metrics
(aliasing against `@reference`, clipped highlights), loudness and true-peak limits,
and a clicks metric. Its metrics are there to test `shotglass quality`, not for reuse.
