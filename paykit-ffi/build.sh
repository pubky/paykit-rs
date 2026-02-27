#!/bin/bash

set -e

bump_version() {
    local bump_type="$1"

    local current_version=$(grep '^version = ' Cargo.toml | sed 's/version = "\(.*\)"/\1/')

    if [ -z "$current_version" ]; then
        echo "Error: Could not find version in Cargo.toml"
        exit 1
    fi

    IFS='.' read -ra VERSION_PARTS <<< "$current_version"
    local major="${VERSION_PARTS[0]}"
    local minor="${VERSION_PARTS[1]}"
    local patch="${VERSION_PARTS[2]}"

    case "$bump_type" in
        major)
            major=$((major + 1))
            minor=0
            patch=0
            ;;
        minor)
            minor=$((minor + 1))
            patch=0
            ;;
        patch)
            patch=$((patch + 1))
            ;;
        *)
            echo "Error: Unknown bump type: $bump_type (must be major, minor, or patch)"
            exit 1
            ;;
    esac

    local new_version="${major}.${minor}.${patch}"

    echo "Bumping $bump_type version from $current_version to $new_version"

    if sed --version >/dev/null 2>&1; then
        # GNU sed
        SED_INPLACE=(sed -i)
    else
        # BSD/macOS sed
        SED_INPLACE=(sed -i '')
    fi
    "${SED_INPLACE[@]}" "s/^version = \".*\"/version = \"$new_version\"/" Cargo.toml
    "${SED_INPLACE[@]}" "s/^version = \".*\"/version = \"$new_version\"/" ../paykit-lib/Cargo.toml
    "${SED_INPLACE[@]}" "s/let tag = \"v.*\"/let tag = \"v$new_version\"/" Package.swift
    "${SED_INPLACE[@]}" "s/^version=.*/version=$new_version/" bindings/android/gradle.properties

    echo "Version bumped to $new_version in all files"
}

BUILD_TARGET=""
DO_RELEASE=false
BUMP_TYPE="patch"

while [[ $# -gt 0 ]]; do
    case $1 in
        -r|--release)
            DO_RELEASE=true
            shift
            ;;
        --major|-M)
            BUMP_TYPE="major"
            shift
            ;;
        --minor|-m)
            BUMP_TYPE="minor"
            shift
            ;;
        --patch|-p)
            BUMP_TYPE="patch"
            shift
            ;;
        ios|android|all)
            BUILD_TARGET="$1"
            shift
            ;;
        *)
            echo "Usage: $0 [-r|--release] [--major|-M|--minor|-m|--patch|-p] {ios|android|all}"
            echo "  Version bump defaults to patch if --release is specified"
            exit 1
            ;;
    esac
done

if [ "$DO_RELEASE" = true ]; then
    bump_version "$BUMP_TYPE"
fi

case "$BUILD_TARGET" in
  "ios")
    ./build_ios.sh
    ;;
  "android")
    ./build_android.sh
    ;;
  "all")
    ./build_ios.sh && ./build_android.sh
    ;;
  *)
    echo "Usage: $0 [-r|--release] [--major|-M|--minor|-m|--patch|-p] {ios|android|all}"
    echo "  Version bump defaults to patch if --release is specified"
    exit 1
    ;;
esac
