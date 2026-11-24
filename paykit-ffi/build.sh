#!/bin/bash
set -eu
set -o pipefail

ffi=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$ffi/.."
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"

fail() { echo "$*" >&2; exit 1; }
require_file() { [[ -s $1 ]] || fail "Missing or empty file: $1"; }

# Generate from the host library before compiling the native release libraries.
generate_bindings() {
    local extension host="$CARGO_TARGET_DIR/release"
    case $(uname -s) in
        Darwin) extension=dylib ;;
        Linux) extension=so ;;
        *) fail "Binding generation requires a macOS or Linux host" ;;
    esac
    require_file "$ffi/prepare-bindings.py"
    [[ -x $ffi/generate-bindings.sh ]] || fail "Missing executable: $ffi/generate-bindings.sh"
    cargo build --locked --release -p paykit-ffi --features bindgen --lib --bin paykit-bindgen
    [[ -x $host/paykit-bindgen ]] || fail "Missing generator: $host/paykit-bindgen"
    require_file "$host/libpaykit.$extension"
    "$ffi/generate-bindings.sh" "$host/paykit-bindgen" "$host/libpaykit.$extension" "$1"
}

# The platform scripts use the same host generator and output checks.
if [[ ${BASH_SOURCE[0]} != "$0" ]]; then return; fi

usage() { fail "Usage: $0 [-r [--rc] [--patch|--minor]] all"; }
release=false
rc=false
bump=none
while [[ $# -gt 1 ]]; do
    case $1 in
        -r|--release) [[ $release == false ]] || usage; release=true ;;
        --rc) [[ $rc == false ]] || usage; rc=true ;;
        --patch|--minor) [[ $bump == none ]] || usage; bump=${1#--} ;;
        *) usage ;;
    esac
    shift
done
[[ $# == 1 && $1 == all ]] || usage
[[ $release == true || ( $rc == false && $bump == none ) ]] || usage

revision=$(git rev-parse --verify HEAD)
work=$(mktemp -d)
restore=false
manifests=(Cargo.toml Cargo.lock paykit-ffi/bindings/android/gradle.properties)
finish() {
    if [[ $restore == true ]]; then
        for path in "${manifests[@]}"; do cp "$work/$path" "$path"; done
    fi
    rm -rf "$work"
}
trap finish EXIT
trap 'exit 1' HUP INT TERM
if [[ $release == true ]]; then
    for path in "${manifests[@]}"; do
        mkdir -p "$work/$(dirname "$path")"
        cp "$path" "$work/$path"
    done
    restore=true
fi

python3 - "$release" "$bump" "$rc" <<'PY'
from pathlib import Path
import re
import subprocess
import sys
try:
    import tomllib
except ImportError:
    sys.exit("Building both packages requires Python 3.11 or newer")

release, bump, rc = sys.argv[1:]
cargo = Path("Cargo.toml")
lock = Path("Cargo.lock")
gradle = Path("paykit-ffi/bindings/android/gradle.properties")
files = {path: path.read_text() for path in (cargo, lock, gradle)}
workspace = tomllib.loads(files[cargo])["workspace"]
old = workspace["package"]["version"]
number = r"(0|[1-9][0-9]*)"
match = re.fullmatch(rf"{number}\.{number}\.{number}(?:-alpha\.{number}|-rc{number})?", old)
if not match:
    sys.exit(f"Unsupported workspace version: {old}")
new = old
if release == "true":
    major, minor, patch = map(int, match.group(1, 2, 3))
    if bump == "minor":
        minor, patch = minor + 1, 0
    elif bump == "patch" or (rc == "false" and "-" not in old):
        patch += 1
    new = f"{major}.{minor}.{patch}"
    if rc == "true":
        sequence = int(match[5] or 0) if bump == "none" else 0
        new += f"-rc{sequence + 1}"
    if subprocess.check_output(["git", "tag", "--list", f"v{new}"]).strip():
        sys.exit(f"Tag v{new} already exists")

def replace(path, before, after):
    if files[path].count(before) != 1:
        sys.exit(f"Expected exactly one {before.strip()!r} in {path}")
    files[path] = files[path].replace(before, after)

replace(cargo, f'\nversion = "{old}"\n', f'\nversion = "{new}"\n')
pin = 'paykit-lib = { path = "paykit-lib", version = "=%s" }\n'
replace(cargo, pin % old, pin % new)
for member in workspace["members"]:
    manifest = Path(member) / "Cargo.toml"
    package = tomllib.loads(manifest.read_text())["package"]
    if package["version"] != {"workspace": True}:
        sys.exit(f"{manifest} must inherit the workspace version")
    entry = f'[[package]]\nname = "{package["name"]}"\nversion = "%s"\n'
    replace(lock, entry % old, entry % new)
replace(gradle, f"\nversion={old}\n", f"\nversion={new}\n")
if release == "true":
    print(f"Building v{new} from v{old}", flush=True)
    for path, contents in files.items():
        path.write_text(contents)
PY

"$ffi/build_ios.sh"
"$ffi/build_android.sh"
[[ $(git rev-parse --verify HEAD) == "$revision" ]] || fail "The checkout changed during the build"
archive="$ffi/dist/ios/Paykit.xcframework.zip"
require_file "$archive"
checksum=$(swift package compute-checksum "$archive")
[[ $checksum =~ ^[0-9a-f]{64}$ ]] || fail "Unexpected Swift package checksum for $archive"
if [[ $release == true ]]; then
    # A release adds the Swift manifest at the repository root, for this tag and archive.
    version=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["workspace"]["package"]["version"])')
    template="$ffi/swift/Package.release.swift"
    require_file "$template"
    sed -e "s/@TAG@/v$version/" -e "s/@CHECKSUM@/$checksum/" "$template" > "$work/Package.swift"
    grep -Fxq "let checksum = \"$checksum\"" "$work/Package.swift" || fail "Missing checksum in Package.swift"
    grep -Fxq "let tag = \"v$version\"" "$work/Package.swift" || fail "Missing tag in Package.swift"
    mv "$work/Package.swift" Package.swift
fi
restore=false
echo "Built both packages. Paykit.xcframework.zip SHA-256: $checksum"
