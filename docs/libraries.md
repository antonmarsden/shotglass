# Projects and libraries

Projects (`shotglass.toml`), shared GLSL libraries, `shotglass.lock`, local overrides, licences, freezing and bundling.

[README](../README.md) · [Movies](movies.md) · [Channels](channels.md) · [Layers and blends](layers.md) · [Feedback buffers](buffers.md) · [Sound](sound.md) · [Loop films](loop-films.md) · [The player](player.md) · [Projects and libraries](libraries.md) · [Testing GLSL and quality metrics](testing-and-quality.md) · [Command line and export](cli.md) · [Testing shotglass](testing.md)

## Projects

A project is a folder with a `shotglass.toml`: the engine contract and the
[libraries](#libraries) for every film in it. A film's project is the nearest folder at
or above the film that has one. `shotglass new` makes a film that is a project of its
own (`shotglass.toml` next to `movie.toml`); a film with no `shotglass.toml` anywhere
above it is one too, with no libraries.

For several films that share code, put `shotglass.toml` in the folder above them:

```
my-films/
├── shotglass.toml          shotglass = "0.1", [libraries] for every film
├── shotglass.lock          one lock: every film uses the same pins
├── shotglass.local.toml    uncommitted overrides, e.g. a local kit checkout
├── lib/                    library.toml, …: GLSL shared by the films
├── first-film/movie.toml   films: folders with a movie.toml, no [libraries]
└── second-film/movie.toml
```

```toml
# my-films/shotglass.toml
shotglass = "0.1"
[libraries]
kit   = { source = "https://github.com/antonmarsden/shotglass-kit", subdir = "core" }
films = { path = "lib" }    # relative to the project folder
```

- `check`, `licenses`, `fetch` and `update` take the project folder too: `shotglass
  check my-films --web --no-gpu` checks every film (the root, if it has a `movie.toml`,
  and every folder directly under it that has one) and fails if any fails. `update`
  moves the pins once for all films.
- Commands for one film (`play`, `still`, `strip`, `wav`, `export`, `bundle`, `freeze`)
  take a film folder and refuse a project folder, naming its films.
- A finished film can be [frozen](#freezing-a-finished-film), so later changes to the
  project's libraries never change it.
- `movie.toml` has no `shotglass` or `[libraries]`; loading one that does says to move
  them to `shotglass.toml`.

## Libraries

Shared GLSL comes from versioned libraries that a project declares by name in its
`shotglass.toml` (see [Projects](#projects)). The engine provides only the contract
(uniforms, entry points, the transition interface, cue defines), so upgrading shotglass
doesn't change how a finished film looks or sounds.

```toml
# shotglass.toml
shotglass = "0.1"        # engine contract the films need; other engines refuse with a clear message

[libraries]
kit  = { version = "0.4", source = "https://github.com/antonmarsden/shotglass-kit", subdir = "core" }
sfx  = { version = "0.3", source = "https://github.com/antonmarsden/shotglass-kit", subdir = "sounds" }
toon = { version = "1.2", source = "https://github.com/someone/toon-kit" }
edge = { source = "https://github.com/someone/fx-kit" }   # no version: follow the default branch
mine = { path = "lib/mykit" }            # a library inside the project folder
lobster = { google = "Lobster" }         # a Google Fonts family (see "Getting fonts")
```

```glsl
#include <kit/noise.glsl>
#include <toon/outline.glsl>
```

- **The project chooses the name.** The include prefix is the local name from
  `[libraries]`, so two publishers' libraries can both be called "kit" without clashing.
  Every film of the project uses the same names.
- **`version`** is a semver range (`"0.4"` means `>=0.4.0, <0.5.0`, as in Cargo),
  matched against the source's git tags (`v0.4.2` or `0.4.2`).
- **No `version`** follows a branch instead of releases: the repository's default
  branch, or `branch = "name"`. Fetching pins the branch's newest commit in
  `shotglass.lock`, so the film still doesn't change until `shotglass update`. Handy
  for a library that has no releases yet; prefer a version once it does.
- **`source`** is the https URL of a git repository. **`subdir`** picks a folder of
  it as the library root, so one repository can publish several libraries that a film
  pins separately. Every folder shares the repository's tags.
- **`path`** is a library inside the project folder, relative to it. It may not climb
  above it.
- Errors inside a library file show `kit/noise.glsl:12`. A missing include lists
  every place that was searched.
- **Clashes are named.** If two libraries (or a library and the film) define the same
  function or struct, a failed compile starts with "`hash21(vec2)` is defined by both
  library "toon" (toon/outline.glsl:12) and library "kit" (kit/noise.glsl:3)" rather
  than only the driver's "redefinition". Overloads with different parameters are fine.
  Library functions should start with the library's own prefix (`kit_`) to avoid this.

### library.toml

Each library root has one:

```toml
name = "shotglass-kit/core"        # canonical name
version = "0.4.2"
requires_shotglass = "0.1"         # engine contract it needs (optional)
licences = ["MIT"]                 # SPDX identifiers
glsl = 430                         # optional: needs more than the GLSL 3.30 baseline (410 or 430)

[requires]                         # peer libraries: checked, not installed
"shotglass-kit/sounds" = ">=0.3"
```

Libraries are pure GLSL and data: no scripts, build steps or plugins. Inside a
library, includes of *other* libraries use canonical names
(`#include <shotglass-kit/sounds/bell.glsl>`), which shotglass maps to whatever the
project calls them. A peer requirement means the project must list that library itself;
shotglass checks the versions and says what's missing, but never adds anything.

A library with `glsl = 430` only loads in a film that opted in to at least that version
(`glsl = "430"` in `movie.toml`); otherwise loading the film says which library needs it.
Such a film needs a driver with that GLSL and won't play on the web.

### Fetching and shotglass.lock

```sh
shotglass fetch  <film|project>           # download what's missing, write shotglass.lock
shotglass update <film|project> [name..]  # move pins to the newest version in range (or commit)
```

`fetch` (also run automatically by `play`, `still`, `strip`, `check` and `wav` when
something isn't downloaded yet) resolves each library to an exact version and writes
`shotglass.lock` next to the project's `shotglass.toml` (one lock for all its films):
version, source, tag, `subdir`, and a sha256 of the library's files (for a followed
branch, `tag` is the ref and `rev` the commit). **Commit it.** A new library release never
changes a finished film; only `update` moves a pin. If the files don't match the lock
(a moved tag, an edited cache), that's an error. Loading a film (including live
reload) only reads the lock and the cache and never touches the network.

Downloads use `git` and are cached per user, shared between films and between
libraries from the same repository and version, and work offline afterwards:
`%LOCALAPPDATA%\shotglass\libs` on Windows, `~/Library/Caches/shotglass/libs` on macOS,
`~/.cache/shotglass/libs` elsewhere (`SHOTGLASS_CACHE` overrides it).

The hash is sha256 over the library root's files, sorted by their `/`-separated path
relative to the root and skipping hidden files and folders (`.git`); CRLF becomes LF in
UTF-8 files so Windows checkouts hash the same. For each file it hashes the path, a 0
byte, the content length as 8 little-endian bytes, then the content.

### Working on a library locally

Point a library at a working copy without putting machine paths in the project:

```sh
SHOTGLASS_LIB_KIT=../shotglass-kit/core shotglass play movies/my-film
```

or put an uncommitted `shotglass.local.toml` next to `shotglass.toml` (`shotglass new`
adds it to `.gitignore`). One file covers every film of the project:

```toml
[libraries.kit]
path = "C:/src/shotglass-kit/core"
```

The variable is `SHOTGLASS_LIB_` plus the local name in capitals, with `-` as `_`.
It wins over `shotglass.local.toml`. Relative override paths are relative to the project
folder. The version range in `shotglass.toml` still has to match the working copy's
`library.toml`. Live reload watches library folders, overrides included. Overrides never
apply to a [frozen](#freezing-a-finished-film) film.

### Licences

`licences` in `library.toml` takes SPDX identifiers. To see what a film actually uses:

```sh
shotglass licenses <movie>
```

It lists every library the film includes something from (local name, canonical name,
version, licences, and where the files come from: source and pin, a folder in the film,
or an override) and flags non-commercial licences (any `-NC` one, e.g.
`CC-BY-NC-SA-4.0`): a film that includes one can't be used commercially. Libraries in
`[libraries]` that nothing includes are named separately, and so are libraries that
declare no licence.

### Freezing a finished film

```sh
shotglass freeze   <film>     # copy exactly the library files the film includes into it
shotglass unfreeze <film>     # follow the project's libraries again
```

In a project of several films, a finished film would otherwise change whenever the
project's libraries do: a new kit pin, an edit in a shared `lib/`. `freeze` copies
exactly the files the film includes from each library (every `#include`d file and
library texture), its `library.toml` and its licence files (`LICENSE*`, `LICENCE*`,
`COPYING*`, `NOTICE*`, from the library folder and, for a `subdir`, the repository above
it) into `<film>/shotglass-libs/<name>/`, and writes a `shotglass.toml` marked frozen
and a `shotglass.lock` with the sha256 of each copy. The film is then a project of its
own: it loads with no project, network, cache or overrides, and never changes. An edited
frozen file is an error, like an edited cache. Commit the three with the film.

`update` refuses a frozen film. `unfreeze` removes the three, and the film follows its
project again (it refuses if the project can no longer load the film, before removing
anything). A one-film project has nothing to freeze: its `shotglass.lock` already pins
everything. Use `bundle` to host it.

### Bundling for hosting

```sh
shotglass bundle <film> --out site/my-film
```

writes a self-contained copy of the film to a new (or empty) folder outside it: the film
folder (without hidden files, project, lock and override files), plus the same frozen
libraries, `shotglass.toml` and `shotglass.lock` that `freeze` writes, and
`LIBRARY-LICENSES.md`, which lists each library's licences and licence files. The bundle
loads with no project, network, cache or overrides, and never changes. `movie.toml` is
unchanged. A library folder inside the film folder travels with the film as it is.

### Portable paths

The same `movie.toml` has to work on every OS and in a future web player, so paths in
it use forward slashes and stay inside the film folder (paths in `shotglass.toml`,
inside the project folder): no absolute paths, drive
letters, backslashes or `..` that climbs out. URLs only go in `source`. File names are
case-sensitive: `check` (and every command) warns when a name only matches a file by
ignoring case, which works on Windows and macOS but not on Linux or the web.
