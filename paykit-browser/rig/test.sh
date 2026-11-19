#!/usr/bin/env bash
# Run the paykit-browser tests in browser engines, through Playwright (runner.mjs).
#
#   paykit-browser/rig/test.sh <chromium|firefox|webkit|chrome|all> [page|worker|both] [cargo test args...]
#
# "all" is Playwright's chromium, firefox and webkit; "chrome" is the installed Google Chrome.
# The context defaults to both. Any further arguments go to `cargo test`, such as a test name
# filter. Needs wasm-bindgen-test-runner on PATH (or in WASM_BINDGEN_TEST_RUNNER) at the
# wasm-bindgen version of Cargo.lock, and `npm ci --ignore-scripts` run in this folder.

set -euo pipefail

usage() {
  echo "usage: $0 <chromium|firefox|webkit|chrome|all> [page|worker|both] [cargo test args...]" >&2
  exit 2
}

[[ $# -ge 1 ]] || usage
case "$1" in
  all) engines=(chromium firefox webkit) ;;
  chromium | firefox | webkit | chrome) engines=("$1") ;;
  *) usage ;;
esac
case "${2:-both}" in
  both) contexts=(page worker) ;;
  page | worker) contexts=("$2") ;;
  *) usage ;;
esac
shift $(($# >= 2 ? 2 : 1))

rig=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$rig/../.." && pwd)

locked=$("$rig/wasm-bindgen-version.sh")
install_hint="cargo install wasm-bindgen-cli --version $locked --locked"
runner_bin=${WASM_BINDGEN_TEST_RUNNER:-wasm-bindgen-test-runner}
if ! command -v "$runner_bin" >/dev/null 2>&1; then
  echo "error: $runner_bin not found; install it with: $install_hint" >&2
  exit 1
fi
if ! version_line=$("$runner_bin" --version); then
  echo "error: $runner_bin --version failed; install it with: $install_hint" >&2
  exit 1
fi
installed=$(awk '{ print $2 }' <<<"$version_line")
if [[ "$installed" != "$locked" ]]; then
  echo "error: $runner_bin is $installed, but Cargo.lock has wasm-bindgen $locked;" \
    "install the matching CLI with: $install_hint" >&2
  exit 1
fi
if [[ ! -d "$rig/node_modules/playwright" ]]; then
  echo "error: Playwright is not installed; run: (cd $rig && npm ci --ignore-scripts)" >&2
  exit 1
fi
# Cargo splits the runner setting on whitespace.
if [[ "$rig" =~ [[:space:]] ]]; then
  echo "error: the path $rig contains whitespace, which a cargo runner setting cannot hold" >&2
  exit 1
fi
export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER="node $rig/runner.mjs"

failed=()
for engine in "${engines[@]}"; do
  for context in "${contexts[@]}"; do
    echo "== $engine, $context"
    # `--tests` keeps the library out: given a test name filter, cargo runs the library test
    # binary too, despite its `test = false`, and that binary has no browser tests.
    if ! PAYKIT_BROWSER_ENGINE=$engine PAYKIT_BROWSER_CONTEXT=$context \
      cargo test --locked --manifest-path "$root/Cargo.toml" -p paykit-browser \
      --target wasm32-unknown-unknown --tests "$@"; then
      failed+=("$engine/$context")
    fi
  done
done

if [[ ${#failed[@]} -gt 0 ]]; then
  echo "failed: ${failed[*]}" >&2
  exit 1
fi
echo "all passed: ${engines[*]} x ${contexts[*]}"
