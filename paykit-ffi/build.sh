#!/bin/bash

set -e

set_version() {
    local new_version="$1"

    if sed --version >/dev/null 2>&1; then
        SED_INPLACE=(sed -i)
    else
        SED_INPLACE=(sed -i '')
    fi

    "${SED_INPLACE[@]}" "s/^version = \".*\"/version = \"$new_version\"/" Cargo.toml
    "${SED_INPLACE[@]}" "s/^version = \".*\"/version = \"$new_version\"/" ../paykit-lib/Cargo.toml
    "${SED_INPLACE[@]}" "s/let tag = \"v.*\"/let tag = \"v$new_version\"/" ../Package.swift
    "${SED_INPLACE[@]}" "s/^version=.*/version=$new_version/" bindings/android/gradle.properties
}

bump_version() {
    local bump_type="$1"
    local use_rc="$2"

    local current_version
    current_version=$(grep '^version = ' Cargo.toml | sed 's/version = "\(.*\)"/\1/')

    if [ -z "$current_version" ]; then
        echo "Error: Could not find version in Cargo.toml"
        exit 1
    fi

    local base_version rc_num=""
    if [[ "$current_version" =~ ^([0-9]+\.[0-9]+\.[0-9]+)(-rc([0-9]+))?$ ]]; then
        base_version="${BASH_REMATCH[1]}"
        rc_num="${BASH_REMATCH[3]}"
    else
        echo "Error: Could not parse version: $current_version"
        exit 1
    fi

    if [ "$bump_type" != "none" ]; then
        IFS='.' read -ra V <<< "$base_version"
        case "$bump_type" in
            major) V[0]=$((V[0] + 1)); V[1]=0; V[2]=0 ;;
            minor) V[1]=$((V[1] + 1)); V[2]=0 ;;
            patch) V[2]=$((V[2] + 1)) ;;
        esac
        base_version="${V[0]}.${V[1]}.${V[2]}"
        rc_num=""
    fi

    local new_version
    if [ "$use_rc" = true ]; then
        if [ -n "$rc_num" ] && [ "$bump_type" = "none" ]; then
            rc_num=$((rc_num + 1))
        else
            rc_num=1
        fi
        new_version="${base_version}-rc${rc_num}"
    else
        if [ -n "$rc_num" ] && [ "$bump_type" = "none" ]; then
            new_version="$base_version"
            echo "Finalizing $current_version → $new_version"
        else
            if [ "$bump_type" = "none" ]; then
                IFS='.' read -ra V <<< "$base_version"
                V[2]=$((V[2] + 1))
                new_version="${V[0]}.${V[1]}.${V[2]}"
            else
                new_version="$base_version"
            fi
        fi
    fi

    echo "Version: $current_version → $new_version"
    set_version "$new_version"
}

BUILD_TARGET=""
DO_RELEASE=false
BUMP_TYPE="none"
USE_RC=false

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
        --rc)
            USE_RC=true
            shift
            ;;
        ios|android|all)
            BUILD_TARGET="$1"
            shift
            ;;
        *)
            echo "Usage: $0 [-r|--release] [--major|-M|--minor|-m|--patch|-p] [--rc] {ios|android|all}"
            exit 1
            ;;
    esac
done

if [ "$DO_RELEASE" = true ]; then
    bump_version "$BUMP_TYPE" "$USE_RC"
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
    echo "Usage: $0 [-r|--release] [--major|-M|--minor|-m|--patch|-p] [--rc] {ios|android|all}"
    exit 1
    ;;
esac
