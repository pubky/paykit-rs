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
"$ffi/package-apple.sh" "$work/generated" "$CARGO_TARGET_DIR/aarch64-apple-ios/release/libpaykit.a" \
    "$simulator" "$work/package"
framework=PaykitFFI.xcframework
archive=Paykit.xcframework.zip

# Each slice the XCFramework declares: its library, its headers, the architectures it claims
# and the platform every object in it was built for (2 is iOS, 7 the iOS simulator).
check_slice() {
    local slice="$work/package/$framework/$1" archs=$2 platform=$3 found arch
    for file in libpaykit.a Headers/PaykitFFI.h Headers/module.modulemap; do
        require_file "$slice/$file"
    done
    found=$(xcrun lipo -archs "$slice/libpaykit.a" | tr ' ' '\n' | LC_ALL=C sort | xargs)
    [[ $found == "$archs" ]] || fail "Slice $1 has architectures: $found"
    for arch in $archs; do
        found=$(xcrun otool -arch "$arch" -l "$slice/libpaykit.a" |
            awk '$1 == "cmd" { build = $2 == "LC_BUILD_VERSION" } build && $1 == "platform" { print $2 }' |
            LC_ALL=C sort -u | xargs)
        [[ $found == "$platform" ]] || fail "Slice $1 ($arch) is built for platforms: ${found:-none}"
    done
}
require_file "$work/package/$framework/Info.plist"
declared=$(python3 -I - "$work/package/$framework/Info.plist" <<'PLIST'
import plistlib, sys
with open(sys.argv[1], "rb") as file:
    libraries = plistlib.load(file)["AvailableLibraries"]
for library in sorted(libraries, key=lambda library: library["LibraryIdentifier"]):
    print(library["LibraryIdentifier"], library["LibraryPath"], library["HeadersPath"],
          library.get("SupportedPlatformVariant", "device"), *sorted(library["SupportedArchitectures"]))
PLIST
)
[[ $declared == $'ios-arm64 libpaykit.a Headers device arm64\nios-arm64_x86_64-simulator libpaykit.a Headers simulator arm64 x86_64' ]] ||
    fail "Unexpected XCFramework libraries: $declared"
check_slice ios-arm64 arm64 2
check_slice ios-arm64_x86_64-simulator "arm64 x86_64" 7
(
    cd "$work/package"
    export TZ=UTC
    find "$framework" -exec touch -t 198001010000 {} +
    find "$framework" -type f -print | LC_ALL=C sort | zip -Xq "$work/$archive" -@
)
unzip -tq "$work/$archive"

# The prepared Swift sources stay with the build output; a release build copies them to
# paykit-ffi/bindings/ios/, where the Swift package reads them.
rm -rf "$dist/Sources"
mkdir -p "$dist/Sources"
for name in Paykit.swift WalletRedaction.swift; do
    require_file "$work/package/Sources/Paykit/$name"
done
mv "$work/package/Sources/Paykit" "$dist/Sources/Paykit"
rm -rf "$dist/$framework"
mv "$work/package/$framework" "$dist/$framework"
mv "$work/$archive" "$dist/$archive"
echo "Built $dist/$archive"
