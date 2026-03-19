#!/bin/bash
set -euo pipefail

# Copies pre-built bindings and binaries from paykit-ffi/ into the
# react-native-paykit package so consumers never need a Rust toolchain.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RN_DIR="$(dirname "$SCRIPT_DIR")"
FFI_DIR="$(dirname "$RN_DIR")/paykit-ffi"

echo "Updating bindings from: $FFI_DIR"
echo "Into:                   $RN_DIR"

# ---------------------------------------------------------------------------
# iOS
# ---------------------------------------------------------------------------
echo ""
echo "=== iOS ==="

# UniFFI-generated Swift binding (renamed to avoid case-insensitive
# collision with the hand-written Paykit.swift bridge on macOS)
cp "$FFI_DIR/bindings/ios/paykit.swift" "$RN_DIR/ios/PaykitUniFFI.swift"
echo "  Copied paykit.swift → PaykitUniFFI.swift"

# UniFFI-generated C header
cp "$FFI_DIR/bindings/ios/paykitFFI.h" "$RN_DIR/ios/"
echo "  Copied paykitFFI.h"

# Module map
if [ -f "$FFI_DIR/bindings/ios/module.modulemap" ]; then
  cp "$FFI_DIR/bindings/ios/module.modulemap" "$RN_DIR/ios/"
  echo "  Copied module.modulemap"
fi

# XCFramework (pre-built static library)
rm -rf "$RN_DIR/ios/Paykit.xcframework"
cp -r "$FFI_DIR/bindings/ios/Paykit.xcframework" "$RN_DIR/ios/"
echo "  Copied Paykit.xcframework"

# ---------------------------------------------------------------------------
# Android
# ---------------------------------------------------------------------------
echo ""
echo "=== Android ==="

# UniFFI-generated Kotlin bindings
KOTLIN_SRC="$FFI_DIR/bindings/android/lib/src/main/kotlin/com/synonym/paykit"
KOTLIN_DST="$RN_DIR/android/src/main/java/uniffi/paykit"
mkdir -p "$KOTLIN_DST"

for kt_file in "$KOTLIN_SRC"/*.kt; do
  if [ -f "$kt_file" ]; then
    cp "$kt_file" "$KOTLIN_DST/"
    echo "  Copied $(basename "$kt_file")"
  fi
done

# Pre-built native .so libraries
JNILIBS_SRC="$FFI_DIR/bindings/android/lib/src/main/jniLibs"
JNILIBS_DST="$RN_DIR/android/src/main/jniLibs"

for arch in armeabi-v7a arm64-v8a x86 x86_64; do
  if [ -f "$JNILIBS_SRC/$arch/libpaykit.so" ]; then
    mkdir -p "$JNILIBS_DST/$arch"
    cp "$JNILIBS_SRC/$arch/libpaykit.so" "$JNILIBS_DST/$arch/"
    echo "  Copied jniLibs/$arch/libpaykit.so"
  else
    echo "  WARNING: $JNILIBS_SRC/$arch/libpaykit.so not found"
  fi
done

echo ""
echo "Done! Bindings updated successfully."
