#!/bin/bash
set -eu
set -o pipefail
[[ $# == 0 ]] || { echo "Usage: $0" >&2; exit 2; }
source "$(dirname "$0")/build.sh"
[[ $(uname -s) == Darwin ]] || fail "The iOS build requires macOS and Xcode"
[[ -x $ffi/package-apple.sh ]] || fail "Missing executable: $ffi/package-apple.sh"
export IPHONEOS_DEPLOYMENT_TARGET=15.0

dist="$ffi/dist/ios"
mkdir -p "$dist"
work=$(mktemp -d "$dist/.build.XXXXXX")
trap 'rm -rf "$work"' EXIT
generate_bindings "$work/generated"
for arch in aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios; do
    cargo build --locked --release --target "$arch" -p paykit-ffi --lib
    require_file "$CARGO_TARGET_DIR/$arch/release/libpaykit.a"
done

mkdir "$work/simulator"
simulator="$work/simulator/libpaykit.a"
xcrun lipo -create "$CARGO_TARGET_DIR/aarch64-apple-ios-sim/release/libpaykit.a" \
    "$CARGO_TARGET_DIR/x86_64-apple-ios/release/libpaykit.a" -output "$simulator"
xcrun lipo "$simulator" -verify_arch arm64 x86_64
"$ffi/package-apple.sh" "$work/generated" "$CARGO_TARGET_DIR/aarch64-apple-ios/release/libpaykit.a" \
    "$simulator" "$work/package"
framework=PaykitFFI.xcframework
archive=Paykit.xcframework.zip
require_file "$work/package/$framework/Info.plist"
(
    cd "$work/package"
    export TZ=UTC
    find "$framework" -exec touch -t 198001010000 {} +
    find "$framework" -type f -print | LC_ALL=C sort | zip -Xq "$work/$archive" -@
)
unzip -tq "$work/$archive"

mkdir -p "$ffi/bindings/ios"
for name in Paykit.swift WalletRedaction.swift; do
    require_file "$work/package/Sources/Paykit/$name"
    cp "$work/package/Sources/Paykit/$name" "$ffi/bindings/ios/"
done
rm -rf "$dist/$framework"
mv "$work/package/$framework" "$dist/$framework"
mv "$work/$archive" "$dist/$archive"
echo "Built $dist/$archive"
