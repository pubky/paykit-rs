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

# Install the cargo-ndk version used by the mobile release scripts.
CARGO_NDK_VERSION="3.5.4"
if ! command -v cargo-ndk &> /dev/null || ! cargo ndk --version | grep -q "cargo-ndk $CARGO_NDK_VERSION"; then
    echo "Installing cargo-ndk $CARGO_NDK_VERSION..."
    cargo install cargo-ndk --version "$CARGO_NDK_VERSION" --locked --force
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

find_readelf() {
    if command -v llvm-readelf >/dev/null 2>&1; then
        command -v llvm-readelf
        return
    fi

    if command -v readelf >/dev/null 2>&1; then
        command -v readelf
        return
    fi

    for ndk_dir in "${ANDROID_NDK_ROOT:-}" "${ANDROID_NDK_HOME:-}" "${NDK_HOME:-}"; do
        if [ -z "$ndk_dir" ] || [ ! -d "$ndk_dir/toolchains/llvm/prebuilt" ]; then
            continue
        fi

        ndk_readelf=$(find "$ndk_dir/toolchains/llvm/prebuilt" -path '*/bin/llvm-readelf' | head -n 1)
        if [ -n "$ndk_readelf" ]; then
            echo "$ndk_readelf"
            return
        fi
    done

    echo "Error: llvm-readelf or readelf is required to validate Android native debug symbols"
    exit 1
}

has_dwarf_debug_metadata() {
    "$READELF_BIN" -S "$1" | grep -Eq '\.debug_'
}

readelf_program_headers() {
    if "$READELF_BIN" -W -l "$1" >/dev/null 2>&1; then
        "$READELF_BIN" -W -l "$1"
        return
    fi

    "$READELF_BIN" -l "$1"
}

has_16kb_load_alignment() {
    alignments=$(readelf_program_headers "$1" | awk '$1 == "LOAD" { print $NF }')
    if [ -z "$alignments" ]; then
        return 1
    fi

    while read -r alignment; do
        if [ -z "$alignment" ]; then
            continue
        fi

        if [ "$((alignment))" -lt 16384 ]; then
            return 1
        fi
    done <<EOF
$alignments
EOF
}

validate_android_library() {
    lib="$1"
    if ! has_dwarf_debug_metadata "$lib"; then
        echo "Error: Android native library has no full DWARF debug metadata: $lib"
        exit 1
    fi

    if ! has_16kb_load_alignment "$lib"; then
        echo "Error: Android native library is not 16 KB page-size aligned: $lib"
        readelf_program_headers "$lib" | grep LOAD || true
        exit 1
    fi
}

validate_android_symbols() {
    READELF_BIN=$(find_readelf)

    for abi in armeabi-v7a arm64-v8a x86 x86_64; do
        lib="$JNILIBS_DIR/$abi/libpaykit.so"
        if [ ! -f "$lib" ]; then
            echo "Error: Android native library missing at $lib"
            exit 1
        fi

        validate_android_library "$lib"
    done
}

validate_android_aar_symbols() {
    READELF_BIN=$(find_readelf)
    aar=$(find "$ANDROID_LIB_DIR" -path '*/build/outputs/aar/*release.aar' -print | head -n 1)
    if [ -z "$aar" ]; then
        echo "Error: Android release AAR missing under $ANDROID_LIB_DIR"
        exit 1
    fi

    tmp_dir=$(mktemp -d)
    unzip -q "$aar" -d "$tmp_dir"

    for abi in armeabi-v7a arm64-v8a x86 x86_64; do
        lib="$tmp_dir/jni/$abi/libpaykit.so"
        if [ ! -f "$lib" ]; then
            echo "Error: Android release AAR native library missing at $lib"
            rm -rf "$tmp_dir"
            exit 1
        fi

        validate_android_library "$lib"
    done

    rm -rf "$tmp_dir"
}

echo "Building for Android architectures..."
export CARGO_PROFILE_RELEASE_DEBUG=2
export CARGO_PROFILE_RELEASE_STRIP=false
cargo ndk \
    -o "$JNILIBS_DIR" \
    --no-strip \
    -t armeabi-v7a \
    -t arm64-v8a \
    -t x86 \
    -t x86_64 \
    build --release
validate_android_symbols
unset CARGO_PROFILE_RELEASE_DEBUG
unset CARGO_PROFILE_RELEASE_STRIP

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
validate_android_aar_symbols

echo "Android build process completed successfully!"
echo ""
echo "Artifact: com.synonym:paykit-android:$CARGO_VERSION"
echo "Published to Maven Local for testing."
echo "To publish to GitHub Packages, create a release or run the gradle-publish workflow."
