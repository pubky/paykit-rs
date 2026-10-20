#!/usr/bin/env bash
# Install the example APK on the running emulator, launch it in `local` mode
# with autorun, and wait for the smoke result in logcat.
#
#   android-smoke.sh <apk> [timeout seconds]
#
# Needs the local stack running on the host (npm run local-stack). The
# emulator reaches the pkarr relay through 10.0.2.2. The homeserver publishes
# a 127.0.0.1 address, so its ports are forwarded with `adb reverse`.
set -euo pipefail

apk=$1
timeout_s=${2:-600}
app=com.synonym.paykit.rnexample

adb install -r "$apk"
for port in 6286 6287; do
  adb reverse "tcp:$port" "tcp:$port"
done
adb logcat -c
adb shell am start -n "$app/.MainActivity" \
  --es paykitMode local --es paykitAutorun true

smoke_lines() {
  adb logcat -d -s ReactNativeJS:V | grep -o 'SMOKE_.*' || true
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
