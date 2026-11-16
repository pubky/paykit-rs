#!/usr/bin/env bash
# THROWAWAY (#411): build the browser bindings and the browser test page.
#
# Needs, on PATH or in the environment:
# - wasm-bindgen CLI 0.2.115 (must equal wasm-bindgen in Cargo.lock);
# - node_modules with uniffi-bindgen-react-native 0.29.3-1, esbuild, prettier
#   (ubrn runs prettier when it finds it; without it the generated TypeScript
#   has `async public` modifiers that esbuild rejects);
# - optional WASM_OPT=/path/to/wasm-opt to also write an -Oz copy for sizing.
#
# Steps: seed the generated crate's Cargo.lock from the repo lock, run
# `ubrn build web --release` (native metadata build with --features web,
# TypeScript + shim crate generation, wasm32 build, wasm-bindgen), then bundle
# www/e2e.ts with esbuild.
set -euo pipefail
cd "$(dirname "$0")"

mkdir -p rust_modules/wasm
# Keep the generated crate on the repo's exact dependency versions.
cp ../Cargo.lock rust_modules/wasm/Cargo.lock

# --preserve-symlinks: lets the ubrn CLI find its own package root when
# node_modules is a symlink (as in the spike's scratch tooling).
node --preserve-symlinks --preserve-symlinks-main \
  node_modules/uniffi-bindgen-react-native/bin/cli.cjs build web \
  --config ubrn.config.yaml --release

# ubrn's shim gives 13 exported structs the same Rust name, and wasm-bindgen
# 0.2.115 merges them into one JS class, which breaks every async callback that
# returns a value (see scripts/patch_wasm_shim.py). Patch the generated shim,
# then rebuild the wasm crate and rerun wasm-bindgen exactly as ubrn did.
python3 -I scripts/patch_wasm_shim.py rust_modules/wasm/src/paykit_module.rs
TARGET_DIR="${CARGO_TARGET_DIR:-$(cd .. && pwd)/target}"
(cd rust_modules/wasm && cargo build --manifest-path Cargo.toml \
  --target wasm32-unknown-unknown --profile release)
wasm-bindgen --target web --omit-default-module-path --out-name index \
  --out-dir src/generated/wasm-bindgen \
  "$TARGET_DIR/wasm32-unknown-unknown/release/paykit_web_wasm.wasm"
if grep -q 'class ForeignFutureCompleteRustBuffer' src/generated/wasm-bindgen/index.js; then
  echo "shim patch OK: distinct ForeignFutureComplete* classes"
else
  echo "shim patch FAILED: completion classes are still merged" >&2
  exit 1
fi

# Belt and braces when prettier was not found: fix the modifier order.
if grep -q 'async public ' src/generated/paykit.ts; then
  sed -i.bak 's/async public /public async /g' src/generated/paykit.ts
  rm -f src/generated/paykit.ts.bak
fi

rm -rf www/dist
# Serve paykit-web/ as the web root: the wasm URL is absolute (/www/dist/...)
# so it resolves the same from the page and from the worker.
node_modules/.bin/esbuild www/e2e.ts www/worker.ts --bundle --format=esm \
  --target=es2022 --loader:.wasm=file --asset-names='[name]' \
  --public-path=/www/dist --outdir=www/dist
if [[ -n "${WASM_OPT:-}" ]]; then
  "$WASM_OPT" -Oz --enable-bulk-memory --enable-nontrapping-float-to-int \
    --enable-sign-ext --enable-mutable-globals --enable-reference-types \
    --enable-multivalue src/generated/wasm-bindgen/index_bg.wasm \
    -o src/generated/wasm-bindgen/index_bg.Oz.wasm
fi
ls -l src/generated/wasm-bindgen www/dist
