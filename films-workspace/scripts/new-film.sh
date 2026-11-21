#!/usr/bin/env bash
# Start a new film in this repository: `shotglass new` plus the shared lib/ entry.
#   scripts/new-film.sh my-film
set -euo pipefail
cd "$(dirname "$0")/.."
name=${1:?usage: $0 <film-folder-name>}
SG=${SHOTGLASS:-shotglass}
"$SG" new "$name"
# The repository's .gitignore and .gitattributes cover every film.
rm -f "$name/.gitignore" "$name/.gitattributes"
cat >> "$name/movie.toml" <<'TOML'
# The repository's shared lib/ (#include <films/file.glsl>). The path is a placeholder:
# SHOTGLASS_LIB_FILMS=../lib (or movie.local.toml) supplies the real one; see README.md.
films = { path = "lib" }
TOML
echo "new film in $name/: shotglass play $name (with SHOTGLASS_LIB_FILMS=../lib set)"
