#!/bin/bash
set -eu
set -o pipefail
[[ $# == 0 ]] || { echo "Usage: $0" >&2; exit 2; }
source "$(dirname "$0")/build.sh"
: "${ANDROID_HOME:?Set ANDROID_HOME to the Android SDK installation}"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
: "${ANDROID_NDK_HOME:?Set ANDROID_NDK_HOME to the Android NDK installation}"
: "${JAVA_HOME:?Set JAVA_HOME to the JDK 17 installation}"
require_file "$ANDROID_HOME/platforms/android-36/android.jar"
require_file "$ANDROID_NDK_HOME/source.properties"
[[ -x $JAVA_HOME/bin/java ]] || fail "Missing java under JAVA_HOME"
command -v cargo-ndk >/dev/null || fail "Install cargo-ndk 4.1.2"
command -v gradle >/dev/null || fail "Install Gradle 9.9.0"

android="$ffi/bindings/android"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
generate_bindings "$work"
kotlin=com/synonym/paykit/paykit.kt
require_file "$work/kotlin/$kotlin"
# Only bindings prepare-bindings.py prepared are packaged: it guards the generated callback
# wrappers and protects value descriptions.
[[ $(head -n 1 "$work/kotlin/$kotlin") == "// Prepared by paykit-ffi/prepare-bindings.py" ]] ||
    fail "The generated Kotlin was not prepared by prepare-bindings.py"
abis=(arm64-v8a armeabi-v7a x86_64)
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 --platform 28 -o "$work/jniLibs" \
    build --locked --release -p paykit-ffi --lib
for abi in "${abis[@]}"; do
    require_file "$work/jniLibs/$abi/libpaykit.so"
done

main="$android/lib/src/main"
rm -rf "$main/generated/kotlin" "$main/jniLibs"
mkdir -p "$main/generated/kotlin/$(dirname "$kotlin")"
cp "$work/kotlin/$kotlin" "$main/generated/kotlin/$kotlin"
for abi in "${abis[@]}"; do
    mkdir -p "$main/jniLibs/$abi"
    cp "$work/jniLibs/$abi/libpaykit.so" "$main/jniLibs/$abi/"
done

gradle -p "$android" --no-daemon :lib:assembleRelease :lib:compileDebugUnitTestKotlin
aar="$android/lib/build/outputs/aar/lib-release.aar"
require_file "$aar"
for abi in "${abis[@]}"; do unzip -tq "$aar" "jni/$abi/libpaykit.so"; done
unzip -p "$aar" classes.jar > "$work/classes.jar"
unzip -tq "$work/classes.jar" com/synonym/paykit/WalletHost.class
echo "Built $aar"
