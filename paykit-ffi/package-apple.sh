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
for input in "$generated/Paykit.swift" \
    "$generated/WalletRedaction.swift" "$generated/PaykitFFI.h" \
    "$generated/PaykitFFI.modulemap" "$device" "$simulator"; do
    test -s "$input" || { echo "Missing package input $input" >&2; exit 1; }
done
# Only bindings that went through prepare-bindings.py are packaged: it patches the generated
# callback wrappers and protects value descriptions.
marker="// Prepared by paykit-ffi/prepare-bindings.py"
for prepared in "$generated/Paykit.swift" "$generated/WalletRedaction.swift"; do
    if [ "$(head -n 1 "$prepared")" != "$marker" ]; then
        echo "$prepared was not prepared by prepare-bindings.py" >&2
        exit 1
    fi
done
mkdir -p "$package"
headers=$(mktemp -d)
trap 'rm -rf "$headers"' EXIT
cp "$generated/PaykitFFI.h" "$headers/"
cp "$generated/PaykitFFI.modulemap" "$headers/module.modulemap"
xcodebuild -create-xcframework -library "$device" -headers "$headers" \
    -library "$simulator" -headers "$headers" -output "$package/PaykitFFI.xcframework"
mkdir -p "$package/Sources/Paykit"
cp "$generated/Paykit.swift" "$generated/WalletRedaction.swift" \
    "$package/Sources/Paykit/"
cp "$(dirname "$0")/swift/Package.swift" "$package/Package.swift"
