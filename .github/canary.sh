#!/usr/bin/env bash
# Kit canary: checks the engine against shotglass-kit's current main branch instead of
# the commit pinned in movies/big-shadow/shotglass.lock (which is what `cargo test`
# uses). Run by .github/workflows/canary.yml; locally (Linux): xvfb-run -a .github/canary.sh
#
# Copies the demo and `shotglass new`'s template to a scratch folder, moves their
# library pins to the kit's newest commit (`shotglass update` / `shotglass fetch`),
# then runs the strict GLSL check (`check --web --no-gpu`) and the same golden
# strip/soundtrack comparisons as tests/golden.rs. Every check runs even if an earlier
# one fails; the exit status is non-zero if any did. Actual and diff images / WAVs go to
# target/tmp/canary/ (CI uploads them).
#
# Env: SHOTGLASS (binary, default target/release/shotglass, built if missing),
#      CANARY_DIR (scratch + output folder, default target/tmp/canary).
set -uo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
bin="${SHOTGLASS:-target/release/shotglass}"
out="${CANARY_DIR:-target/tmp/canary}"
if [ ! -x "$bin" ]; then
  cargo build --release || exit 1
fi
rm -rf "$out"
mkdir -p "$out"

failed=()
run() { # run <label> <command...>
  echo "::group::$1"
  if "${@:2}"; then
    echo "::endgroup::"
  else
    echo "::endgroup::"
    echo "::error::canary: $1 failed"
    failed+=("$1")
  fi
}

# The films, pinned to kit main.
cp -r movies/big-shadow "$out/big-shadow"
"$bin" update "$out/big-shadow" || exit 1
"$bin" new "$out/template" >/dev/null || exit 1
rm -f "$out/template/shotglass.lock"
"$bin" fetch "$out/template" || exit 1
echo "kit pins under test:"
grep -h -E '^(name|rev) ' "$out/big-shadow/shotglass.lock" "$out/template/shotglass.lock" | paste - -

# Same sizes and rates as tests/golden.rs.
for film in big-shadow template; do
  run "$film: strict GLSL check (330 + ES 3.00)" \
    "$bin" check "$out/$film" --web --no-gpu
  run "$film: golden strip" \
    "$bin" strip "$out/$film" --size 320x180 --count 8 --cols 4 \
    --out "$out/$film.png" --compare "tests/golden/$film.png"
  run "$film: golden soundtrack" \
    "$bin" wav "$out/$film" --rate 8000 \
    --out "$out/$film.wav" --compare "tests/golden/$film.wav"
done

if [ ${#failed[@]} -gt 0 ]; then
  echo
  echo "canary FAILED against shotglass-kit main:"
  printf '  - %s\n' "${failed[@]}"
  echo "Bump the kit pin (shotglass update movies/big-shadow) and re-bless the golden"
  echo "references if the change is intended, or fix the kit. See CONTRIBUTING.md (CI)."
  exit 1
fi
echo "canary passed: the demo and the template match their references on kit main"
