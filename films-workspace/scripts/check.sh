#!/usr/bin/env bash
# Check every film in the repository. Run from anywhere; Git Bash on Windows.
#
#   scripts/check.sh            strict GLSL check of every film (check --web --no-gpu)
#   scripts/check.sh --golden   ...then compare finished films with golden/<film>.png/.wav
#   scripts/check.sh --bless    ...or rewrite those references (look at / listen to them!)
#
# A film is a folder with a movie.toml. It is finished when golden/<film>.png exists.
# Golden references are Mesa llvmpipe renders (as in CI); on Linux without a display
# run under `xvfb-run -a`. SHOTGLASS (default: shotglass on PATH) picks the binary.
set -euo pipefail
cd "$(dirname "$0")/.."
SG=${SHOTGLASS:-shotglass}
# Every film reaches the shared lib/ through this override (relative to the film folder).
export SHOTGLASS_LIB_FILMS=${SHOTGLASS_LIB_FILMS:-../lib}

mode=check
case "${1:-}" in
  "") ;;
  --golden) mode=golden ;;
  --bless) mode=bless ;;
  *) echo "usage: $0 [--golden | --bless]" >&2; exit 2 ;;
esac

out=target/golden
mkdir -p "$out"
films=()
for m in */movie.toml; do films+=("${m%/movie.toml}"); done
[ ${#films[@]} -gt 0 ] || { echo "no films found" >&2; exit 1; }

failed=()
for f in "${films[@]}"; do
  echo "== $f"
  "$SG" check "$f" --web --no-gpu || { failed+=("$f (check)"); continue; }
  [ "$mode" = check ] && continue
  if [ ! -f "golden/$f.png" ] && [ "$mode" != bless ]; then
    echo "   not finished (no golden/$f.png): no golden comparison"
    continue
  fi
  if [ "$mode" = bless ] && [ ! -f "golden/$f.png" ] && [ -z "${BLESS_NEW:-}" ]; then
    echo "   not finished: skipped (BLESS_NEW=1 blesses unfinished films too)"
    continue
  fi
  extra=(); [ "$mode" = bless ] && extra=(--bless)
  "$SG" strip "$f" --count 8 --size 320x180 --cols 4 --out "$out/$f.png" \
      --compare "golden/$f.png" "${extra[@]}" || failed+=("$f (picture)")
  "$SG" wav "$f" --rate 8000 --out "$out/$f.wav" \
      --compare "golden/$f.wav" "${extra[@]}" || failed+=("$f (sound)")
done

if [ ${#failed[@]} -gt 0 ]; then
  echo
  echo "FAILED: ${failed[*]}"
  echo "actual strips/soundtracks and *.diff.png / *.diff.wav are in $out/"
  exit 1
fi
echo
echo "all ${#films[@]} films ok ($mode)"
