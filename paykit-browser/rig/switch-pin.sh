#!/usr/bin/env bash
# Print the Pubky Switch that Cargo.lock pins, read straight from the lock file.
#
#   paykit-browser/rig/switch-pin.sh --rev [LOCK]      the pinned commit, 40 hex digits
#   paykit-browser/rig/switch-pin.sh --version [LOCK]  the version of the pubky-switch package
#
# Every pubky-switch package (pubky-switch and pubky-switch-*) must come from
# git+https://github.com/pubky/pubky-switch?rev=<commit>#<commit>, all at one commit; otherwise
# it fails, since the pin is then not one commit. --version also fails when the lock holds no
# pubky-switch package, or more than one. LOCK defaults to the workspace Cargo.lock.

set -euo pipefail

usage() {
  echo "usage: $0 --rev|--version [LOCK]" >&2
  exit 2
}

die() {
  echo "error: $*" >&2
  exit 1
}

[[ $# -ge 1 && $# -le 2 ]] || usage
case "$1" in
  --rev | --version) want=$1 ;;
  *) usage ;;
esac
lock=${2:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/Cargo.lock}
[[ -f "$lock" ]] || die "no lock file at $lock"

# One line per pubky-switch package: "<name> <version> <source>".
packages=$(awk '
  function flush() {
    if (name ~ /^pubky-switch(-.*)?$/) print name, version, source
    name = ""; version = ""; source = ""
  }
  /^\[\[package\]\]$/ { flush(); next }
  /^name = "/ { name = $3; gsub(/"/, "", name) }
  /^version = "/ { version = $3; gsub(/"/, "", version) }
  /^source = "/ { source = $3; gsub(/"/, "", source) }
  END { flush() }
' "$lock")
[[ -n "$packages" ]] || die "no pubky-switch package in $lock"

rev=""
version=""
while read -r name package_version source; do
  pattern='^git\+https://github\.com/pubky/pubky-switch\?rev=([0-9a-f]{40})#([0-9a-f]{40})$'
  if ! [[ "$source" =~ $pattern ]] || [[ "${BASH_REMATCH[1]}" != "${BASH_REMATCH[2]}" ]]; then
    die "$name in $lock is not pinned to one commit of github.com/pubky/pubky-switch: ${source:-no source}"
  fi
  if [[ -n "$rev" && "$rev" != "${BASH_REMATCH[1]}" ]]; then
    die "the pubky-switch packages in $lock are pinned to more than one commit: $rev, ${BASH_REMATCH[1]}"
  fi
  rev=${BASH_REMATCH[1]}
  if [[ "$name" == "pubky-switch" ]]; then
    [[ -z "$version" ]] || die "more than one pubky-switch package in $lock"
    version=$package_version
  fi
done <<<"$packages"

case "$want" in
  --rev) echo "$rev" ;;
  --version)
    [[ -n "$version" ]] || die "no pubky-switch package in $lock"
    echo "$version"
    ;;
esac
