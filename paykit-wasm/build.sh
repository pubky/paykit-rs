#!/usr/bin/env bash
# THROWAWAY (#411): build the wasm module and generate the JS package in pkg/.
# Needs wasm-bindgen-cli 0.2.115 (must match wasm-bindgen in Cargo.lock):
#   WASM_BINDGEN=/path/to/wasm-bindgen ./build.sh
# Optional: WASM_OPT=/path/to/wasm-opt to also produce pkg/paykit_wasm_bg.opt.wasm
set -euo pipefail
cd "$(dirname "$0")"
WASM_BINDGEN="${WASM_BINDGEN:-wasm-bindgen}"
TARGET_DIR="${CARGO_TARGET_DIR:-../target}"
cargo build -p paykit-wasm --target wasm32-unknown-unknown --release
rm -rf pkg
"$WASM_BINDGEN" --target web --out-dir pkg "$TARGET_DIR/wasm32-unknown-unknown/release/paykit_wasm.wasm"
if [[ -n "${WASM_OPT:-}" ]]; then
  "$WASM_OPT" -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext \
    --enable-mutable-globals --enable-reference-types --enable-multivalue \
    pkg/paykit_wasm_bg.wasm -o pkg/paykit_wasm_bg.opt.wasm
fi
ls -l pkg
