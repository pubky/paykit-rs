#!/bin/bash

set -e

echo "Starting Android build process..."

# Workspace target directory (paykit-ffi is a workspace member, so target/ is at the root)
TARGET_DIR="../target"

# Android Gradle project directories
ANDROID_LIB_DIR="./bindings/android"
BASE_DIR="$ANDROID_LIB_DIR/lib/src/main/kotlin/com/synonym/paykit"
JNILIBS_DIR="$ANDROID_LIB_DIR/lib/src/main/jniLibs"

echo "Installing gobley-uniffi-bindgen fork..."
cargo install --git https://github.com/ovitrif/gobley.git gobley-uniffi-bindgen --force

if ! command -v cargo-ndk &> /dev/null; then
    echo "Installing cargo-ndk..."
    cargo install cargo-ndk
fi

mkdir -p "$BASE_DIR"
mkdir -p "$JNILIBS_DIR"

echo "Removing previous build..."
rm -rf "$BASE_DIR"/*
rm -rf "$JNILIBS_DIR"/*

echo "Building host target for bindgen..."
cargo build --release

echo "Adding Rust Android targets..."
rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android

echo "Building for Android architectures..."
cargo ndk \
    -o "$JNILIBS_DIR" \
    -t armeabi-v7a \
    -t arm64-v8a \
    -t x86 \
    -t x86_64 \
    build --release

echo "Removing spurious intermediate .so files from jniLibs..."
find "$JNILIBS_DIR" -name "*.so" ! -name "libpaykit.so" -delete

case "$(uname -s)" in
    Darwin*) LIBRARY_PATH="${TARGET_DIR}/release/libpaykit.dylib" ;;
    *)       LIBRARY_PATH="${TARGET_DIR}/release/libpaykit.so" ;;
esac
if [ ! -f "$LIBRARY_PATH" ]; then
    echo "Error: Library file not found at $LIBRARY_PATH"
    echo "Available files in ${TARGET_DIR}/release/:"
    ls -l "${TARGET_DIR}/release/" | grep libpaykit
    exit 1
fi

echo "Generating Kotlin bindings..."
TMP_DIR=$(mktemp -d)

gobley-uniffi-bindgen \
    --library "$LIBRARY_PATH" \
    --config ./uniffi-android.toml \
    --out-dir "$TMP_DIR"

echo "Moving Kotlin files to final location..."
find "$TMP_DIR" -name "*.kt" -exec mv {} "$BASE_DIR/" \;
./postprocess_bindings.sh "$BASE_DIR"/*.kt

echo "Normalizing generated Kotlin whitespace..."
find "$BASE_DIR" -name "*.kt" -exec perl -0pi -e 's/[ \t]+(?=\n)//g; s/[ \t]+\z//; s/\n+\z/\n/; $_ .= "\n" unless /\n\z/' {} \;

echo "Cleaning up temporary files..."
rm -rf "$TMP_DIR"
rm -rf "$ANDROID_LIB_DIR/uniffi"

KT_COUNT=$(find "$BASE_DIR" -name "*.kt" | wc -l | tr -d ' ')
if [ "$KT_COUNT" -eq 0 ]; then
    echo "Error: No Kotlin bindings were generated"
    echo "Contents of $BASE_DIR:"
    ls -la "$BASE_DIR"
    exit 1
fi

echo "Generated $KT_COUNT Kotlin binding file(s):"
ls -la "$BASE_DIR"

echo "Syncing version from Cargo.toml..."
CARGO_VERSION=$(grep '^version = ' Cargo.toml | sed 's/version = "\(.*\)"/\1/' | head -1)
if sed --version >/dev/null 2>&1; then
    sed -i "s/^version=.*/version=$CARGO_VERSION/" "$ANDROID_LIB_DIR/gradle.properties"
else
    sed -i '' "s/^version=.*/version=$CARGO_VERSION/" "$ANDROID_LIB_DIR/gradle.properties"
fi

echo "Testing android library publish to Maven Local..."
"$ANDROID_LIB_DIR"/gradlew --project-dir "$ANDROID_LIB_DIR" clean publishToMavenLocal

echo "Android build process completed successfully!"
echo ""
echo "Artifact: com.synonym:paykit-android:$CARGO_VERSION"
echo "Published to Maven Local for testing."
echo "To publish to GitHub Packages, create a release or run the gradle-publish workflow."
