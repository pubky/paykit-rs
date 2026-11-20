#!/usr/bin/env bash
# Build the harness bundle that the hosted-page tests load in a worker. Test rig code only: the
# bundle is a test harness, not a proposal for the SDK's JavaScript surface (src/lib.rs).
#
#   paykit-browser/rig/harness/build.sh [dev|release|browser-release]
#   paykit-browser/rig/harness/build.sh --print-out-dir [dev|release|browser-release]
#
# The profile defaults to release. The script builds the paykit-browser-harness cdylib for
# wasm32-unknown-unknown with --locked, then runs `wasm-bindgen --target web --out-name harness`
# on it, into <target dir>/paykit-browser-harness/<profile>/, which it empties first; it holds
# harness.js and harness_bg.wasm. Needs wasm-bindgen on PATH (or in WASM_BINDGEN) at exactly the
# wasm-bindgen version of Cargo.lock, and jq. With --print-out-dir, it only prints that folder
# for the profile and builds nothing.
#
# The network scenarios read the rig's origin and token at compile time, from
# PAYKIT_BROWSER_RIG_ORIGIN and PAYKIT_BROWSER_RIG_TOKEN, as the network test targets do: the
# caller (test.sh) sets them for the build. Built without them, those scenarios fail with the
# code `build`.

set -euo pipefail

usage() {
  echo "usage: $0 [--print-out-dir] [dev|release|browser-release]" >&2
  exit 2
}

die() {
  echo "error: $*" >&2
  exit 1
}

print_out_dir=0
if [[ "${1:-}" == "--print-out-dir" ]]; then
  print_out_dir=1
  shift
fi
[[ $# -le 1 ]] || usage
profile=${1:-release}
case "$profile" in
  dev | release | browser-release) ;;
  *) usage ;;
esac

harness=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
rig=$(cd "$harness/.." && pwd)
root=$(cd "$rig/../.." && pwd)

command -v jq >/dev/null 2>&1 || die "jq is not on PATH"
target_dir=$(cargo metadata --locked --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" |
  jq -r .target_directory) || die "could not read the target directory"
[[ -n "$target_dir" && "$target_dir" != null ]] || die "cargo reported no target directory"
out="$target_dir/paykit-browser-harness/$profile"
if [[ $print_out_dir -eq 1 ]]; then
  echo "$out"
  exit 0
fi

bindgen=${WASM_BINDGEN:-wasm-bindgen}
"$rig/wasm-bindgen-version.sh" --check "$bindgen" >/dev/null

echo "== building paykit-browser-harness for wasm32-unknown-unknown, profile $profile"
messages=$(cargo build --locked --manifest-path "$root/Cargo.toml" -p paykit-browser-harness \
  --target wasm32-unknown-unknown --profile "$profile" --message-format json-render-diagnostics) ||
  die "could not build paykit-browser-harness"
wasm=$(jq -r 'select(.reason == "compiler-artifact" and .target.name == "paykit_browser_harness")
  | .filenames[] | select(endswith(".wasm"))' <<<"$messages" | tail -n 1)
[[ -n "$wasm" && -f "$wasm" ]] || die "cargo reported no paykit_browser_harness .wasm file"

rm -rf "$out"
mkdir -p "$out"
"$bindgen" --target web --out-name harness --no-typescript --out-dir "$out" "$wasm" ||
  die "wasm-bindgen failed"

for file in harness.js harness_bg.wasm; do
  [[ -f "$out/$file" ]] || die "wasm-bindgen wrote no $file"
done
echo "== harness bundle in $out:"
for file in "$out"/*; do
  [[ -f "$file" ]] || continue
  echo "   $(basename "$file") $(wc -c <"$file" | tr -d ' ') bytes"
done
