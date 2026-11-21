# shotglass-films

Short films made with [shotglass](https://github.com/antonmarsden/shotglass), while
they're being made. Private. The engine and the kit
([shotglass-kit](https://github.com/antonmarsden/shotglass-kit)) live in their own repos;
this one holds the films, and the GLSL they're trying out before it goes to the kit.

```
shotglass-films/
  lib/                 shared library "films/lib": primitives incubating before the kit
    library.toml
    shapes.glsl
  example/             one folder per film: a normal shotglass film
    movie.toml
    shotglass.lock     kit pins (commit it)
    scenes/ ...
  golden/              references of finished films: <film>.png, <film>.wav
  scripts/check.sh     check every film (CI runs this too)
  scripts/new-film.sh  start a film
  .github/workflows/ci.yml
```

Clone it next to the others (`C:\software\shotglass-films`, beside
`C:\software\shotglass` and `C:\software\shotglass-kit`).

## One-time setup: point films at the shared lib/

Every film lists the shared library as `films` and includes `<films/file.glsl>`.
`movie.toml` can't point outside the film folder (paths there must work for the web
player too), so each film declares a placeholder and a **per-machine override** supplies
the real path. Override paths are relative to the film folder, so one value, `../lib`,
works for every film. Set it once per machine:

```bat
setx SHOTGLASS_LIB_FILMS ../lib
```

(Windows; open a new terminal afterwards. Elsewhere: `export SHOTGLASS_LIB_FILMS=../lib`
in your shell profile.) CI sets the same variable.

Without it, loading a film fails with
`library "films": can't read example/lib/library.toml`: set the variable, or put the
override in that film's `movie.local.toml` instead:

```toml
[libraries.films]
path = "../lib"
```

Both are tested with shotglass `c864558` (main, Nov 2026): `check`, `check --web
--no-gpu`, `strip`, `wav` and `licenses` all see `lib/` through the override (and
live reload watches override folders). The one command that doesn't is `bundle`
(below). A cleaner, committed way is an engine question: see antonmarsden/shotglass#50.

## A new film

```sh
scripts/new-film.sh my-film       # shotglass new my-film + the films library entry
shotglass play my-film            # fetches the kit and writes my-film/shotglass.lock
```

That's `shotglass new` plus a `films` entry at the end of `[libraries]`:

```toml
films = { path = "lib" }          # placeholder; SHOTGLASS_LIB_FILMS=../lib supplies it
```

Uncomment the kit libraries in the new `movie.toml` that the film needs (`kit`,
`sound`; `transitions` is already there). Commit `shotglass.lock` with the film.

## While making a film

- `shotglass play <film>` from the repository root. Saving any file reloads, including
  files in `lib/` and in a local kit checkout.
- New GLSL that another film could use goes in `lib/`, not the film. Write it to the
  kit's [CONVENTIONS.md](https://github.com/antonmarsden/shotglass-kit/blob/main/CONVENTIONS.md)
  from the start, with `films_` instead of `kit_` (`films__` for internals): a header
  contract with units, no global state, valid GLSL ES 3.00, deterministic (no `sin()`
  hashes). Then graduating is a move and a rename.
- Film-specific GLSL stays in the film folder (`#include "common.glsl"`).

### Changing the kit alongside a film

When a film needs a kit change, work on a kit checkout and point the film at it with an
uncommitted `movie.local.toml` in the film folder (ignored by `.gitignore`):

```toml
[libraries.kit]
path = "C:/software/shotglass-kit/core"

[libraries.sound]
path = "C:/software/shotglass-kit/sound"

[libraries.transitions]
path = "C:/software/shotglass-kit/transitions"
```

(`SHOTGLASS_LIB_KIT=...` works too and wins over the file.) The film then plays the
working copy, live. Land the kit change through the kit's own PR and CI, then delete
`movie.local.toml` and move the film's pin:

```sh
shotglass update my-film          # or: shotglass update my-film kit
```

### Graduating a primitive from lib/ to the kit

1. **Kit conventions.** Rename `films_` to `kit_` (`films__` to `kit__`), pick the kit
   file it belongs in (`core/sdf.glsl`, `sound/envelope.glsl`, ...), and check the header
   against CONVENTIONS.md (names, parameter order, units, ranges, `// sound: yes`).
2. **Kit tests.** Call it from the kit's test film (`tests/film`, or `tests/sound` /
   `tests/instruments` / `tests/mix` for sound): the kit's CI fails if a public function
   isn't called. Re-bless the kit's golden picture / sound if it changes, and run
   `shotglass check tests/film --web --no-gpu`. Open the PR in shotglass-kit.
3. **Switch the include prefix.** While the PR is open, test it from the film with a
   `movie.local.toml` kit override (above). In each film that uses it, replace
   `#include <films/x.glsl>` with the kit file and `films_name` with `kit_name`.
   After the kit PR merges: delete the override, `shotglass update <film> kit`, and run
   `scripts/check.sh`.
4. **Remove it from lib/** once no film includes it (`grep -r films_name */`).
   A finished film that still includes it keeps working until then; its golden
   comparison shows whether the switch changed a pixel.

## When a film is done

1. **Pin.** Delete any `movie.local.toml` and run `shotglass update <film>` to pin the
   kit at its newest commit (or leave the existing pin), then `shotglass check <film>
   --web` and watch it once more. Commit `shotglass.lock`.
2. **Golden references** (marks the film finished for CI): references are Mesa
   llvmpipe renders, so make them on CI. Actions tab → Films → Run workflow, tick
   "bless" (it renders references for every film). Download the `golden-references`
   artifact, copy only `golden/<film>.png` and `.wav` of the film you're finishing into
   the repo, **look at the strip
   and listen to the WAV**, and commit them. On Linux (or WSL / Docker with Mesa):

   ```sh
   BLESS_NEW=1 xvfb-run -a scripts/check.sh --bless
   ```

3. From then on CI compares an 8-frame 320x180 strip and an 8 kHz soundtrack of the
   film with those references on every push. A failure means a change to `lib/`, the
   kit pin or the engine pin altered a finished film: fix it, or, if the change is
   meant, re-bless and say why in the commit. Never re-bless to make an unexplained
   diff go away. The actual strip, the WAV and `<film>.diff.png` (magenta = differs) /
   `<film>.diff.wav` are in the `golden-failures` artifact (locally:
   `target/golden/`).

`lib/` is not pinned per film: a finished film reads today's `lib/`. The golden
comparison is what keeps finished films safe when `lib/` changes.

## Checking everything

```sh
scripts/check.sh                       # strict GLSL (330 + ES 3.00) of every film, no GPU
xvfb-run -a scripts/check.sh --golden  # + golden comparison of finished films
```

On Windows, run it in Git Bash (no `xvfb-run`; your real GPU's renders won't match
llvmpipe references exactly, so treat a `--golden` failure there as a hint and trust CI).
`SHOTGLASS=path/to/shotglass` picks a binary other than the one on `PATH`.

## CI

`.github/workflows/ci.yml` builds shotglass at the commit in `SHOTGLASS_REV` (cached),
runs `scripts/check.sh` (strict check of every film), then `scripts/check.sh --golden`
under `xvfb-run` on Mesa llvmpipe. The engine is pinned like a library: bump
`SHOTGLASS_REV` (and `RUST_TOOLCHAIN` to shotglass's at that commit) on purpose, in a
commit of its own.

## Hosting a film (bundle)

`shotglass bundle` ignores overrides on purpose (a bundle must load on any machine), so
the `films` placeholder path must really exist. Copy `lib/` into the film for the bundle:

```sh
cp -r lib my-film/lib && shotglass bundle my-film --out site/my-film; rm -rf my-film/lib
```

The bundle then carries all of `lib/` (not only the included files) as a folder of the
film.

## The example film

`example/` is placeholder content that shows the wiring: two shots and a crossfade,
`films_ring` from `lib/shapes.glsl`, kit `color`/`noise` in the pictures and kit
`note`/`envelope` in the music. It's marked finished (`golden/example.*`) so CI's
golden path has something to run. Delete it (and its golden files) once a real film is
in place.
