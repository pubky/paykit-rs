#!/usr/bin/env bash
# Boot a simulator, install the example app, launch it in `local` mode with
# autorun, and wait for the smoke result in the simulator log.
#
#   ios-smoke.sh <PaykitExample.app> [timeout seconds]
#
# Needs the local stack running on the host (npm run local-stack). The
# simulator shares the host's loopback, so no forwarding is needed.
set -euo pipefail

app_path=$1
timeout_s=${2:-600}
app=com.synonym.paykit.rnexample
log=$(mktemp)

udid=$(xcrun simctl list devices available --json |
  python3 -I -c 'import json,sys
devices = json.load(sys.stdin)["devices"]
print(next(d["udid"] for runtime, ds in sorted(devices.items(), reverse=True)
           if "iOS" in runtime for d in ds if d["name"].startswith("iPhone")))')
xcrun simctl boot "$udid"
xcrun simctl bootstatus "$udid"
xcrun simctl install "$udid" "$app_path"

xcrun simctl spawn "$udid" log stream --style compact \
  --predicate 'eventMessage CONTAINS "SMOKE_"' >"$log" 2>&1 &
stream=$!
trap 'kill "$stream" 2>/dev/null || true; xcrun simctl shutdown "$udid" || true' EXIT
sleep 2
xcrun simctl launch "$udid" "$app" -paykitMode local -paykitAutorun true

smoke_lines() {
  grep -o 'SMOKE_.*' "$log" || true
}

deadline=$((SECONDS + timeout_s))
until smoke_lines | grep -q '^SMOKE_RESULT'; do
  if ((SECONDS > deadline)); then
    smoke_lines
    echo "No SMOKE_RESULT within ${timeout_s} s" >&2
    exit 1
  fi
  sleep 5
done
smoke_lines
smoke_lines | grep -q '^SMOKE_RESULT pass'
