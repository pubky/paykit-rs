#!/bin/bash

set -e

echo "Starting iOS build process..."

# Workspace target directory (paykit-ffi is a workspace member, so target/ is at the root)
TARGET_DIR="../target"
IOS_BINDINGS_DIR="./bindings/ios"
IOS_DIST_DIR="./dist/ios"

rm -rf "$IOS_BINDINGS_DIR"/*
mkdir -p "$IOS_BINDINGS_DIR"
mkdir -p "$IOS_DIST_DIR"

export IPHONEOS_DEPLOYMENT_TARGET=13.4

echo "Building default target..."
cargo build --release

echo "Adding iOS targets..."
rustup target add aarch64-apple-ios-sim aarch64-apple-ios

echo "Building for iOS targets..."
cargo build --release --target=aarch64-apple-ios-sim
cargo build --release --target=aarch64-apple-ios

echo "Generating Swift bindings..."
rm -rf "$IOS_BINDINGS_DIR/paykit.swift"
rm -rf "$IOS_BINDINGS_DIR/paykitFFI.h"
rm -rf "$IOS_BINDINGS_DIR/paykitFFI.modulemap"
rm -rf "$IOS_BINDINGS_DIR/Headers"
rm -rf "$IOS_BINDINGS_DIR/ios-arm64"
rm -rf "$IOS_BINDINGS_DIR/ios-arm64-sim"

cargo run --bin uniffi-bindgen generate \
    --library "${TARGET_DIR}/release/libpaykit.dylib" \
    --language swift \
    --out-dir "$IOS_BINDINGS_DIR" \
    || { echo "Failed to generate Swift bindings"; exit 1; }

cp ./src/swift/PaykitPublicKeys.swift "$IOS_BINDINGS_DIR/PaykitPublicKeys.swift"
./postprocess_bindings.sh "$IOS_BINDINGS_DIR/paykit.swift" "$IOS_BINDINGS_DIR/paykitFFI.h"

echo "Handling modulemap file..."
if [ -f "$IOS_BINDINGS_DIR/paykitFFI.modulemap" ]; then
    mv "$IOS_BINDINGS_DIR/paykitFFI.modulemap" "$IOS_BINDINGS_DIR/module.modulemap"
else
    echo "Warning: modulemap file not found"
fi

echo "Normalizing generated Swift and header whitespace..."
find "$IOS_BINDINGS_DIR" -type f \( -name "*.swift" -o -name "*.h" -o -name "*.modulemap" \) -exec perl -0pi -e 's/[ \t]+(?=\n)//g; s/[ \t]+\z//; s/\n+\z/\n/; $_ .= "\n" unless /\n\z/' {} \;

echo "Type-checking generated Swift bindings..."
IOS_SIMULATOR_SDK=$(xcrun --sdk iphonesimulator --show-sdk-path)
SWIFT_MODULE_CACHE="${TARGET_DIR}/swift-binding-module-cache"
rm -rf "$SWIFT_MODULE_CACHE"
mkdir -p "$SWIFT_MODULE_CACHE"
xcrun --sdk iphonesimulator swiftc \
    -typecheck \
    -target arm64-apple-ios15.0-simulator \
    -sdk "$IOS_SIMULATOR_SDK" \
    -module-cache-path "$SWIFT_MODULE_CACHE" \
    -I "$IOS_BINDINGS_DIR" \
    "$IOS_BINDINGS_DIR/paykit.swift" \
    "$IOS_BINDINGS_DIR/PaykitPublicKeys.swift" \
    ./src/swift/AllowanceBindingsCompile.swift \
    || { echo "Failed to type-check generated Swift bindings"; exit 1; }

echo "Cleaning up existing XCFramework..."
rm -rf "$IOS_DIST_DIR/Paykit.xcframework"
rm -rf "$IOS_DIST_DIR/Headers"
rm -rf "$IOS_DIST_DIR/ios-arm64"
rm -rf "$IOS_DIST_DIR/ios-arm64-sim"

echo "Creating architecture-specific directories..."
mkdir -p "$IOS_DIST_DIR/ios-arm64/Headers/paykitFFI"
mkdir -p "$IOS_DIST_DIR/ios-arm64-sim/Headers/paykitFFI"

echo "Copying headers to architecture directories..."
cp "$IOS_BINDINGS_DIR/paykitFFI.h" "$IOS_DIST_DIR/ios-arm64/Headers/paykitFFI/"
cp "$IOS_BINDINGS_DIR/module.modulemap" "$IOS_DIST_DIR/ios-arm64/Headers/paykitFFI/"
cp "$IOS_BINDINGS_DIR/paykitFFI.h" "$IOS_DIST_DIR/ios-arm64-sim/Headers/paykitFFI/"
cp "$IOS_BINDINGS_DIR/module.modulemap" "$IOS_DIST_DIR/ios-arm64-sim/Headers/paykitFFI/"

echo "Creating XCFramework..."
xcodebuild -create-xcframework \
    -library "${TARGET_DIR}/aarch64-apple-ios-sim/release/libpaykit.a" -headers "$IOS_DIST_DIR/ios-arm64-sim/Headers" \
    -library "${TARGET_DIR}/aarch64-apple-ios/release/libpaykit.a" -headers "$IOS_DIST_DIR/ios-arm64/Headers" \
    -output "$IOS_DIST_DIR/Paykit.xcframework" \
    || { echo "Failed to create XCFramework"; exit 1; }

echo "Cleaning up temporary directories..."
rm -rf "$IOS_DIST_DIR/ios-arm64"
rm -rf "$IOS_DIST_DIR/ios-arm64-sim"

echo "Creating XCFramework zip file..."
rm -f "$IOS_DIST_DIR/Paykit.xcframework.zip"
find "$IOS_DIST_DIR/Paykit.xcframework" -exec touch -t 198001010000 {} \;
(
    cd "$IOS_DIST_DIR"
    find Paykit.xcframework -type f -print | LC_ALL=C sort | zip -X -q Paykit.xcframework.zip -@
) || { echo "Failed to create zip file"; exit 1; }

echo "Computing checksum..."
CHECKSUM=$(swift package compute-checksum "$IOS_DIST_DIR/Paykit.xcframework.zip") || { echo "Failed to compute checksum"; exit 1; }
echo "New checksum: $CHECKSUM"

echo "Updating Package.swift with new checksum..."
python3 ./update_package.py --checksum "$CHECKSUM" || { echo "Failed to update Package.swift"; exit 1; }

echo "iOS build process completed successfully!"
