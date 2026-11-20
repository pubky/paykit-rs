#!/usr/bin/env bash
# Print the wasm-bindgen version in Cargo.lock. wasm-bindgen-cli generates the bindings the test
# binary links against, so it must be exactly this version; any other version refuses the module.
# Fails when the lock holds more than one wasm-bindgen version, since the one the tests link
# against is then not known from the lock alone.
#
#   paykit-browser/rig/wasm-bindgen-version.sh [--check BIN] [LOCK]
#
# LOCK defaults to the workspace Cargo.lock. With --check, it also fails, with the command that
# installs the matching CLI, unless BIN (a wasm-bindgen-cli program such as wasm-bindgen or
# wasm-bindgen-test-runner, by name on PATH or by path) runs and reports that version.

set -euo pipefail

die() {
  echo "error: $*" >&2
  exit 1
}

check=""
if [[ "${1:-}" == "--check" ]]; then
  [[ $# -ge 2 ]] || die "--check needs a program"
  check=$2
  shift 2
fi

lock=${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/Cargo.lock}
versions=$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print }' "$lock")
[[ -n "$versions" ]] || die "no wasm-bindgen entry in $lock"
if [[ "$versions" == *$'\n'* ]]; then
  die "more than one wasm-bindgen version in $lock: ${versions//$'\n'/, }"
fi

if [[ -n "$check" ]]; then
  install_hint="cargo install wasm-bindgen-cli --version $versions --locked"
  command -v "$check" >/dev/null 2>&1 || die "$check not found; install it with: $install_hint"
  version_line=$("$check" --version) || die "$check --version failed; install it with: $install_hint"
  installed=$(awk '{ print $2 }' <<<"$version_line")
  if [[ "$installed" != "$versions" ]]; then
    die "$check is $installed, but Cargo.lock has wasm-bindgen $versions;" \
      "install the matching CLI with: $install_hint"
  fi
fi
echo "$versions"
