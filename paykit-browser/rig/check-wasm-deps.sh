#!/usr/bin/env bash
# Fail when the wasm32 build of paykit-browser would depend on a native networking or TLS crate.
# A browser build reaches the network only through the browser, so none of them belongs in it.
# They arrive through default features, such as the `http` feature of pubky-switch-client (its
# HTTPS transport on reqwest, hyper, tokio and rustls with ring), so this check catches a
# dependency change that turns one of those features back on.
#
#   paykit-browser/rig/check-wasm-deps.sh
#
# A crate that does not build for wasm32 already fails the build: mio and socket2 refuse the
# target at compile time. For those, this check only names the path that pulled them in. The
# list is here for the crates that do build for wasm32 but must not be in a browser build:
# - tokio builds for wasm32 with only its sync, macros, io-util, rt and time features, and then
#   its timers panic where the thread cannot block, as in a browser, and `spawn_blocking`
#   panics without threads (tokio's crate docs, "WASM support"). pubky-switch-client's `tokio`
#   feature runs every `StateStore` call through `spawn_blocking`.
# - reqwest and hyper are the HTTP stack of pubky-switch-client's native `http` feature.
#   reqwest builds for wasm32 over the browser's `fetch`, so either crate here means that
#   feature came back on.
# - rustls, ring and aws-lc-sys are TLS and its crypto backends, which a browser provides
#   itself. ring builds for wasm32 where the C compiler can target wasm32 (its build script
#   has a wasm32 case), so on such a machine the build alone does not catch it.

set -euo pipefail

banned=(mio socket2 ring aws-lc-sys reqwest hyper rustls tokio)

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
tree=(cargo tree --quiet --locked --manifest-path "$root/Cargo.toml"
  --target wasm32-unknown-unknown -p paykit-browser -e normal)

# One "name vX.Y.Z ..." line per package; keep "name@X.Y.Z" so `cargo tree -i` can tell
# versions apart.
packages=$("${tree[@]}" --prefix none | awk '{ sub(/^v/, "", $2); print $1 "@" $2 }' | sort -u)

found=0
for crate in "${banned[@]}"; do
  while read -r package; do
    found=1
    echo "error: the wasm32 dependency graph of paykit-browser contains $package, pulled in by:" >&2
    "${tree[@]}" -i "$package" >&2
  done < <(grep "^$crate@" <<<"$packages" || true)
done

[[ $found -eq 0 ]] || exit 1
echo "ok: the wasm32 dependency graph of paykit-browser has none of: ${banned[*]}"
