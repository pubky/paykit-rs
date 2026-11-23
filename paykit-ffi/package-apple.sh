#!/bin/sh
# Assemble a local Swift package from the same revision's generated sources and native archives.
set -eu
if [ "$#" -ne 4 ]; then
    echo "Usage: $0 GENERATED_BINDINGS IOS_ARM64_LIBRARY SIMULATOR_LIBRARY PACKAGE_DIRECTORY" >&2
    exit 2
fi
generated=$1/swift
device=$2
simulator=$3
package=$4
for input in "$generated/PaykitV2.swift" \
    "$generated/WalletRedaction.swift" "$generated/PaykitV2FFI.h" \
    "$generated/PaykitV2FFI.modulemap" "$device" "$simulator"; do
    test -s "$input" || { echo "Missing package input $input" >&2; exit 1; }
done
mkdir -p "$package"
headers=$(mktemp -d)
trap 'rm -rf "$headers"' EXIT
cp "$generated/PaykitV2FFI.h" "$headers/"
cp "$generated/PaykitV2FFI.modulemap" "$headers/module.modulemap"
xcodebuild -create-xcframework -library "$device" -headers "$headers" \
    -library "$simulator" -headers "$headers" -output "$package/PaykitV2FFI.xcframework"
mkdir -p "$package/Sources/PaykitV2"
cp "$generated/PaykitV2.swift" "$generated/WalletRedaction.swift" \
    "$package/Sources/PaykitV2/"
cp "$(dirname "$0")/swift/Package.swift" "$package/Package.swift"
