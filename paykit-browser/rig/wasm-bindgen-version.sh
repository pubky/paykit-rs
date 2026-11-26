#!/usr/bin/env bash
# Print the wasm-bindgen version in Cargo.lock. wasm-bindgen-cli generates the bindings the test
# binary links against, so it must be exactly this version; any other version refuses the module.
# Fails when the lock holds more than one wasm-bindgen version, since the one the tests link
# against is then not known from the lock alone.
#
#   paykit-browser/rig/wasm-bindgen-version.sh [LOCK]
#
# LOCK defaults to the workspace Cargo.lock.

set -euo pipefail

lock=${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/Cargo.lock}
versions=$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print }' "$lock")
if [[ -z "$versions" ]]; then
  echo "error: no wasm-bindgen entry in $lock" >&2
  exit 1
fi
if [[ "$versions" == *$'\n'* ]]; then
  echo "error: more than one wasm-bindgen version in $lock: ${versions//$'\n'/, }" >&2
  exit 1
fi
echo "$versions"
