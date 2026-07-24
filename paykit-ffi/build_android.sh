#!/bin/bash

set -e

echo "Starting Android build process..."

# Workspace target directory (paykit-ffi is a workspace member, so target/ is at the root)
TARGET_DIR="../target"

# Android Gradle project directories
ANDROID_LIB_DIR="./bindings/android"
BASE_DIR="$ANDROID_LIB_DIR/lib/src/main/kotlin/com/synonym/paykit"
JNILIBS_DIR="$ANDROID_LIB_DIR/lib/src/main/jniLibs"
NATIVE_DEBUG_SYMBOLS_ZIP="$ANDROID_LIB_DIR/native-debug-symbols.zip"

GOBLEY_REV="82a0f93ad552d0c45e185f728f14c3c60b1ed707"
GOBLEY_INSTALL_ROOT="$TARGET_DIR/build-tools/gobley-$GOBLEY_REV"
GOBLEY_BINDGEN_BIN="$GOBLEY_INSTALL_ROOT/bin/gobley-uniffi-bindgen"

echo "Installing pinned gobley-uniffi-bindgen fork into $GOBLEY_INSTALL_ROOT..."
cargo install \
    --git https://github.com/ovitrif/gobley.git \
    --rev "$GOBLEY_REV" \
    --root "$GOBLEY_INSTALL_ROOT" \
    --locked \
    --force \
    gobley-uniffi-bindgen
"$GOBLEY_BINDGEN_BIN" --version

# Install the cargo-ndk version used by the mobile release scripts.
CARGO_NDK_VERSION="3.5.4"
echo "Checking cargo-ndk $CARGO_NDK_VERSION..."
if ! command -v cargo-ndk &> /dev/null || ! cargo ndk --version | grep -q "cargo-ndk $CARGO_NDK_VERSION"; then
    echo "Installing cargo-ndk $CARGO_NDK_VERSION..."
    cargo install cargo-ndk --version "$CARGO_NDK_VERSION" --locked --force
fi

EXPECTED_ANDROID_NDK_REVISION="28.2.13676358"
if [ -z "${ANDROID_NDK_HOME:-}" ] || [ -z "${ANDROID_NDK_ROOT:-}" ]; then
    echo "Error: ANDROID_NDK_HOME and ANDROID_NDK_ROOT must both select Android NDK $EXPECTED_ANDROID_NDK_REVISION"
    exit 1
fi
ANDROID_NDK_HOME_PATH=$(cd "$ANDROID_NDK_HOME" && pwd -P)
ANDROID_NDK_ROOT_PATH=$(cd "$ANDROID_NDK_ROOT" && pwd -P)
if [ "$ANDROID_NDK_HOME_PATH" != "$ANDROID_NDK_ROOT_PATH" ]; then
    echo "Error: ANDROID_NDK_HOME and ANDROID_NDK_ROOT select different NDKs: HOME=$ANDROID_NDK_HOME_PATH ROOT=$ANDROID_NDK_ROOT_PATH"
    exit 1
fi
if [ ! -f "$ANDROID_NDK_HOME_PATH/source.properties" ]; then
    echo "Error: Android NDK source.properties is missing at $ANDROID_NDK_HOME_PATH"
    exit 1
fi
cat "$ANDROID_NDK_HOME_PATH/source.properties"
if ! grep -Fx "Pkg.Revision = $EXPECTED_ANDROID_NDK_REVISION" "$ANDROID_NDK_HOME_PATH/source.properties"; then
    echo "Error: Android NDK $EXPECTED_ANDROID_NDK_REVISION is required: path=$ANDROID_NDK_HOME_PATH"
    exit 1
fi
echo "Using Android NDK $EXPECTED_ANDROID_NDK_REVISION at $ANDROID_NDK_HOME_PATH"
ANDROID_NDK_LLVM_READELF=$(find "$ANDROID_NDK_HOME_PATH/toolchains/llvm/prebuilt" -path '*/bin/llvm-readelf' -print -quit)
if [ -z "$ANDROID_NDK_LLVM_READELF" ]; then
    echo "Error: llvm-readelf is missing from Android NDK $EXPECTED_ANDROID_NDK_REVISION at $ANDROID_NDK_HOME_PATH"
    exit 1
fi
ANDROID_NDK_LLVM_BIN=$(dirname "$ANDROID_NDK_LLVM_READELF")
echo "Using Android NDK LLVM tools from $ANDROID_NDK_LLVM_BIN"

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
    if [ -x "$ANDROID_NDK_LLVM_BIN/llvm-readelf" ]; then
        echo "$ANDROID_NDK_LLVM_BIN/llvm-readelf"
        return
    fi

    echo "Error: llvm-readelf is missing from Android NDK $EXPECTED_ANDROID_NDK_REVISION at $ANDROID_NDK_HOME_PATH"
    exit 1
}

find_strip() {
    if [ -x "$ANDROID_NDK_LLVM_BIN/llvm-strip" ]; then
        echo "$ANDROID_NDK_LLVM_BIN/llvm-strip"
        return
    fi

    echo "Error: llvm-strip is missing from Android NDK $EXPECTED_ANDROID_NDK_REVISION at $ANDROID_NDK_HOME_PATH"
    exit 1
}

find_llvm_tool() {
    local tool_name="$1"
    local tool_path

    tool_path="$ANDROID_NDK_LLVM_BIN/$tool_name"
    if [ -x "$tool_path" ]; then
        echo "$tool_path"
        return
    fi

    echo "Error: $tool_name is missing from Android NDK $EXPECTED_ANDROID_NDK_REVISION at $ANDROID_NDK_HOME_PATH"
    exit 1
}

has_dwarf_debug_metadata() {
    "$READELF_BIN" -S "$1" | grep -Eq '\.debug_'
}

has_dwarf_sections() {
    "$READELF_BIN" -S "$1" | grep -Eq '\.debug_'
}

readelf_program_headers() {
    if "$READELF_BIN" -W -l "$1" >/dev/null 2>&1; then
        "$READELF_BIN" -W -l "$1"
        return
    fi

    "$READELF_BIN" -l "$1"
}

validate_16kb_segments() {
    local abi="$1"
    local lib="$2"
    local display_path="${3:-$lib}"
    local headers
    local load_alignments
    local load_summary=""
    local relro_fields
    local relro_start="missing"
    local relro_memsz="missing"
    local relro_end="missing"
    local alignment
    local alignment_value
    local alignment_hex
    local relro_start_value
    local relro_memsz_value
    local relro_end_value
    local invalid=false

    headers=$(readelf_program_headers "$lib")
    load_alignments=$(printf '%s\n' "$headers" | awk '$1 == "LOAD" { print $NF }')
    if [ -z "$load_alignments" ]; then
        invalid=true
        load_summary="missing"
    else
        while read -r alignment; do
            if [ -z "$alignment" ]; then
                continue
            fi

            alignment_value=$((alignment))
            printf -v alignment_hex '0x%x' "$alignment_value"
            if [ -n "$load_summary" ]; then
                load_summary="$load_summary,"
            fi
            load_summary="$load_summary$alignment_hex"

            if [ "$alignment_value" -lt 16384 ]; then
                invalid=true
            fi
        done <<EOF
$load_alignments
EOF
    fi

    relro_fields=$(printf '%s\n' "$headers" | awk '$1 == "GNU_RELRO" { print $3, $6; exit }')
    if [ -z "$relro_fields" ]; then
        invalid=true
    else
        read -r relro_start_value relro_memsz_value <<EOF
$relro_fields
EOF
        relro_end_value=$((relro_start_value + relro_memsz_value))
        printf -v relro_start '0x%x' "$((relro_start_value))"
        printf -v relro_memsz '0x%x' "$((relro_memsz_value))"
        printf -v relro_end '0x%x' "$relro_end_value"

        if [ "$((relro_end_value % 16384))" -ne 0 ]; then
            invalid=true
        fi
    fi

    if [ "$invalid" = true ]; then
        echo "Error: Android 16 KB ELF validation failed: abi=$abi path=$display_path LOAD_ALIGNMENTS=[$load_summary] GNU_RELRO_START=$relro_start GNU_RELRO_MEMSZ=$relro_memsz GNU_RELRO_END=$relro_end required_LOAD_min=0x4000 required_GNU_RELRO_end_alignment=0x4000"
        printf '%s\n' "$headers" | grep -E '(^|[[:space:]])(LOAD|GNU_RELRO)[[:space:]]' || true
        return 1
    fi

    echo "Android 16 KB ELF validation passed: abi=$abi path=$display_path LOAD_ALIGNMENTS=[$load_summary] GNU_RELRO_START=$relro_start GNU_RELRO_MEMSZ=$relro_memsz GNU_RELRO_END=$relro_end"
}

validate_android_library() {
    local abi="$1"
    local lib="$2"
    local display_path="${3:-$lib}"
    if ! has_dwarf_debug_metadata "$lib"; then
        echo "Error: Android native library has no full DWARF debug metadata: abi=$abi path=$display_path"
        exit 1
    fi

    if ! validate_16kb_segments "$abi" "$lib" "$display_path"; then
        exit 1
    fi
}

validate_stripped_android_library() {
    local abi="$1"
    local lib="$2"
    local display_path="${3:-$lib}"
    if has_dwarf_sections "$lib"; then
        echo "Error: Android release native library still contains .debug_* sections: abi=$abi path=$display_path"
        exit 1
    fi

    if ! validate_16kb_segments "$abi" "$lib" "$display_path"; then
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

        validate_android_library "$abi" "$lib"
    done
}

create_native_debug_symbols_archive() {
    tmp_dir=$(mktemp -d)

    for abi in armeabi-v7a arm64-v8a x86 x86_64; do
        mkdir -p "$tmp_dir/$abi"
        cp "$JNILIBS_DIR/$abi/libpaykit.so" "$tmp_dir/$abi/"
    done

    rm -f "$NATIVE_DEBUG_SYMBOLS_ZIP"
    archive_path="$PWD/$NATIVE_DEBUG_SYMBOLS_ZIP"
    (
        cd "$tmp_dir"
        zip -qr "$archive_path" armeabi-v7a arm64-v8a x86 x86_64
    )
    zip -T "$NATIVE_DEBUG_SYMBOLS_ZIP" >/dev/null
    rm -rf "$tmp_dir"
}

strip_android_libraries() {
    STRIP_BIN=$(find_strip)

    for abi in armeabi-v7a arm64-v8a x86 x86_64; do
        "$STRIP_BIN" --strip-unneeded "$JNILIBS_DIR/$abi/libpaykit.so"
    done
}

validate_stripped_android_symbols() {
    READELF_BIN=$(find_readelf)

    for abi in armeabi-v7a arm64-v8a x86 x86_64; do
        validate_stripped_android_library "$abi" "$JNILIBS_DIR/$abi/libpaykit.so"
    done
}

validate_android_aar_symbols() {
    READELF_BIN=$(find_readelf)
    LLVM_NM_BIN=$(find_llvm_tool llvm-nm)
    LLVM_OBJDUMP_BIN=$(find_llvm_tool llvm-objdump)
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

        validate_stripped_android_library "$abi" "$lib" "$aar!/jni/$abi/libpaykit.so"
        validate_uniffi_integrity_library "$abi" "$lib" "$aar!/jni/$abi/libpaykit.so"
    done

    rm -rf "$tmp_dir"
}

extract_kotlin_contract_version() {
    local kotlin_file="$BASE_DIR/paykit.android.kt"
    local contract_version

    contract_version=$(sed -nE \
        's/^[[:space:]]*val bindings_?[Cc]ontract_?[Vv]ersion = ([0-9]+).*$/\1/p' \
        "$kotlin_file" | head -n 1)
    if [ -z "$contract_version" ]; then
        echo "Error: Unable to extract the UniFFI contract version from $kotlin_file"
        exit 1
    fi

    echo "$contract_version"
}

extract_kotlin_checksum_manifest() {
    local kotlin_file="$BASE_DIR/paykit.android.kt"
    local manifest

    manifest=$(perl -ne \
        'print "$1 $2\n" if /if \((?:lib\.)?(uniffi_paykit_checksum_[A-Za-z0-9_]+)\(\) != ([0-9]+)\.toShort\(\)\)/' \
        "$kotlin_file")
    if [ -z "$manifest" ]; then
        echo "Error: Unable to extract UniFFI API checksums from $kotlin_file"
        exit 1
    fi

    echo "$manifest"
}

extract_native_constant() {
    local abi="$1"
    local lib="$2"
    local symbol="$3"
    local symbol_fields
    local symbol_address
    local symbol_size
    local start_address
    local stop_address
    local disassembly
    local immediate
    local objdump_args=()

    symbol_fields=$("$LLVM_NM_BIN" -D -S "$lib" | awk -v symbol="$symbol" '$NF == symbol { print $1, $2; exit }')
    if [ -z "$symbol_fields" ]; then
        echo "Error: UniFFI integrity symbol missing: abi=$abi symbol=$symbol path=$lib"
        exit 1
    fi

    read -r symbol_address symbol_size <<EOF
$symbol_fields
EOF
    start_address=$((16#$symbol_address))
    stop_address=$((start_address + 16#$symbol_size))
    if [ "$abi" = "armeabi-v7a" ]; then
        objdump_args+=(--triple=thumbv7-none-linux-android)
    fi

    disassembly=$("$LLVM_OBJDUMP_BIN" \
        -d \
        --no-show-raw-insn \
        "--start-address=$start_address" \
        "--stop-address=$stop_address" \
        "${objdump_args[@]}" \
        "$lib")
    immediate=$(printf '%s\n' "$disassembly" \
        | grep -E 'movs?[[:space:]]+r0,[[:space:]]*#0x|movw[[:space:]]+r0,[[:space:]]*#0x|mov\.w[[:space:]]+r0,[[:space:]]*#0x|mov[[:space:]]+w0,[[:space:]]*#0x|mov[wl][[:space:]]+\$0x[0-9a-fA-F]+,[[:space:]]*%e?ax' \
        | grep -Eo '[#$]0x[0-9a-fA-F]+' \
        | head -n 1 \
        | sed -E 's/^[#$]//')
    if [ -z "$immediate" ]; then
        echo "Error: Unable to decode UniFFI integrity symbol: abi=$abi symbol=$symbol path=$lib"
        printf '%s\n' "$disassembly"
        exit 1
    fi

    echo "$((immediate))"
}

validate_uniffi_integrity_library() {
    local abi="$1"
    local lib="$2"
    local display_path="${3:-$lib}"
    local native_contract_version
    local checksum_symbol
    local expected_checksum
    local native_checksum
    local expected_checksum_symbols
    local native_checksum_symbols
    local checksum_count

    native_contract_version=$(extract_native_constant "$abi" "$lib" "ffi_paykit_uniffi_contract_version")
    if [ "$KOTLIN_CONTRACT_VERSION" -ne "$native_contract_version" ]; then
        echo "Error: UniFFI Kotlin/native contract mismatch: abi=$abi Kotlin=$KOTLIN_CONTRACT_VERSION native=$native_contract_version path=$display_path"
        exit 1
    fi

    expected_checksum_symbols=$(printf '%s\n' "$KOTLIN_CHECKSUM_MANIFEST" | awk '{print $1}' | sort)
    native_checksum_symbols=$("$LLVM_NM_BIN" -D "$lib" \
        | awk '$NF ~ /^uniffi_paykit_checksum_/ {print $NF}' \
        | sort)
    if [ "$expected_checksum_symbols" != "$native_checksum_symbols" ]; then
        echo "Error: UniFFI checksum symbol set mismatch: abi=$abi path=$display_path"
        diff -u \
            <(printf '%s\n' "$expected_checksum_symbols") \
            <(printf '%s\n' "$native_checksum_symbols") || true
        exit 1
    fi

    checksum_count=0
    while read -r checksum_symbol expected_checksum; do
        if [ -z "$checksum_symbol" ]; then
            continue
        fi
        native_checksum=$(extract_native_constant "$abi" "$lib" "$checksum_symbol")
        if [ "$expected_checksum" -ne "$native_checksum" ]; then
            echo "Error: UniFFI API checksum mismatch: abi=$abi symbol=$checksum_symbol Kotlin=$expected_checksum native=$native_checksum path=$display_path"
            exit 1
        fi
        checksum_count=$((checksum_count + 1))
    done <<EOF
$KOTLIN_CHECKSUM_MANIFEST
EOF

    echo "UniFFI Kotlin/native integrity validation passed: abi=$abi contract=$KOTLIN_CONTRACT_VERSION checksums=$checksum_count path=$display_path"
}

validate_uniffi_integrity() {
    LLVM_NM_BIN=$(find_llvm_tool llvm-nm)
    LLVM_OBJDUMP_BIN=$(find_llvm_tool llvm-objdump)
    KOTLIN_CONTRACT_VERSION=$(extract_kotlin_contract_version)
    KOTLIN_CHECKSUM_MANIFEST=$(extract_kotlin_checksum_manifest)
    echo "UniFFI Kotlin integrity manifest: contract=$KOTLIN_CONTRACT_VERSION checksums=$(printf '%s\n' "$KOTLIN_CHECKSUM_MANIFEST" | wc -l | tr -d ' ')"

    for abi in armeabi-v7a arm64-v8a x86 x86_64; do
        validate_uniffi_integrity_library "$abi" "$JNILIBS_DIR/$abi/libpaykit.so"
    done
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
create_native_debug_symbols_archive
strip_android_libraries
validate_stripped_android_symbols
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

"$GOBLEY_BINDGEN_BIN" \
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
validate_uniffi_integrity

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
