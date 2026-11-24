#!/usr/bin/env bash
# Kit canary: checks that `shotglass new`'s template still works with shotglass-kit's
# current main branch, which is what a new film gets. The template follows the kit's
# head and has no golden reference (the demo keeps its own copies of the kit files it
# uses, so `cargo test` never depends on the kit). Run by .github/workflows/canary.yml;
# locally (Linux): xvfb-run -a .github/canary.sh
#
# Writes the template to a scratch folder, fetches the kit's newest commit, then runs
# the strict GLSL check (`check --web --no-gpu`), a GPU check of a frame, and renders
# a strip and the soundtrack (kept for a look, not compared). Every check runs even if
# an earlier one fails; the exit status is non-zero if any did. Output goes to
# target/tmp/canary/ (CI uploads it on failure).
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

"$bin" new "$out/template" >/dev/null || exit 1
"$bin" fetch "$out/template" || exit 1
echo "kit pins under test:"
grep -h -E '^(name|rev) ' "$out/template/shotglass.lock" | paste - -

run "template: strict GLSL check (330 + ES 3.00)"   "$bin" check "$out/template" --web --no-gpu
run "template: renders a frame"   "$bin" check "$out/template" --size 320x180 --frames 1
run "template: strip"   "$bin" strip "$out/template" --size 320x180 --count 8 --cols 4 --out "$out/template.png"
run "template: soundtrack"   "$bin" wav "$out/template" --rate 8000 --out "$out/template.wav"

if [ ${#failed[@]} -gt 0 ]; then
  echo
  echo "canary FAILED: shotglass new's template against shotglass-kit main:"
  printf '  - %s
' "${failed[@]}"
  echo "Fix the kit, or the template (templates/new). See CONTRIBUTING.md (Kit canary)."
  exit 1
fi
echo "canary passed: the template works with kit main"
