#!/usr/bin/env bash
# Run the paykit-browser tests in browser engines, through Playwright (runner.mjs).
#
#   paykit-browser/rig/test.sh [--switch] <chromium|firefox|webkit|chrome|all> [page|worker|both] [cargo test args...]
#   paykit-browser/rig/test.sh --harness <chromium|firefox|webkit|chrome|all>
#
# "all" is Playwright's chromium, firefox and webkit; "chrome" is the installed Google Chrome.
# The context defaults to both. Any further arguments go to `cargo test`, such as a test name
# filter. Needs wasm-bindgen-test-runner on PATH (or in WASM_BINDGEN_TEST_RUNNER) at the
# wasm-bindgen version of Cargo.lock, and `npm ci --ignore-scripts` run in this folder.
#
# Without --switch it runs the library's unit tests and every test target except the network
# ones (network_targets below), so it needs no database and no service. With --switch it runs
# the network targets, or the targets given with `--test`, against a real Pubky Switch service
# behind switch-rig.mjs, which terminates TLS and answers CORS on https://localhost:<rig port>,
# on 127.0.0.1 and ::1. The script picks the test targets: `--test` is the only cargo target
# option it takes, in either mode. This is test rig code only, never for production. The run
# starts the service, the rig and a fresh database, and removes them on every exit, including
# a failure and Ctrl-C. It reads:
#
#   PAYKIT_BROWSER_SWITCH_BIN           the pubky-switch service binary, at the version that
#                                       Cargo.lock pins (required)
#   PAYKIT_BROWSER_SWITCH_DATABASE_URL  a postgres:// URL of PostgreSQL 14 or later, with fsync
#                                       on, whose user may create databases; each run creates
#                                       its own database there and drops it (required)
#   PAYKIT_BROWSER_RIG_PORT             the rig's TLS port, in the origin. Default 18443.
#   PAYKIT_BROWSER_SWITCH_PORT          the service's API port. Default 18480.
#   PAYKIT_BROWSER_SWITCH_ADMIN_PORT    the service's admin port. Default 18490.
#   PAYKIT_BROWSER_RIG_CLOSED_PORT      a port on which nothing may listen, for the test of an
#                                       origin that does not answer. Default 18449.
#   PAYKIT_BROWSER_RIG_LOG_DIR          a folder that keeps the service and rig logs, replaced
#                                       at each run. Default: a temporary folder, removed.
#   PAYKIT_BROWSER_ENROLL_BIN           the native enrol program the rig runs for /rig/world.
#                                       Default: the switch-enroll example, built here.
#
# The network tests read the origin, https://localhost:<rig port>, from
# PAYKIT_BROWSER_RIG_ORIGIN, the closed port from PAYKIT_BROWSER_RIG_CLOSED_PORT, and the token
# the rig requires for POST /rig/world and for /rig/fault (GET, POST and DELETE) from
# PAYKIT_BROWSER_RIG_TOKEN, at compile time; this script sets them with --switch only. It makes
# a fresh random token for each run, and gives it to the rig in its environment, so a page or
# program that does not know it cannot make the rig enrol, nor arm, disarm or read its fault.
# Every port must be free on both 127.0.0.1 and ::1, the addresses a browser may resolve
# localhost to.
#
# PAYKIT_BROWSER_SKIP_KEYCHAIN_TESTS=1 is for local runs on macOS only (refused elsewhere), and
# CI never sets it: WebKit runs then leave out the tests that store a CryptoKey in IndexedDB,
# as Playwright's WebKit on macOS asks the user for Keychain access to store one and the test
# waits for the answer (runner.mjs, "Environment"). Each test left out is printed, and the
# summary counts them: such a run does not cover WebKit fully. 0 or unset runs every test. It
# is reported apart from the store targets that runner.mjs excludes in an engine build with no
# Origin Private File System (NO_OPFS in engines.mjs), which the summary lists as warnings.
#
# --harness runs the hosted test harness instead of the test targets: the harness bundle
# (rig/harness/, built by rig/harness/build.sh) in a dedicated module worker, under a strict
# Content Security Policy, against the real Switch service, in each engine given, through
# harness-runner.mjs (its header says what it checks). It starts the service, the database and
# the rig as --switch does, with the same variables, and has the rig also serve the harness on
# a second port, https://localhost:<harness port>; it then builds the bundle with the rig's
# origin and the run's token, runs harness-runner.mjs, and removes everything on every exit. It
# skips nothing: a bundle that does not build, a rig that does not start or any failed case
# fails the run. PAYKIT_BROWSER_SKIP_KEYCHAIN_TESTS does not apply: the harness stores no
# CryptoKey in WebKit on macOS. It also reads:
#
#   PAYKIT_BROWSER_HARNESS_PORT         the harness's TLS port, in its origin. Default 18444.
#   PAYKIT_BROWSER_HARNESS_PROFILE      the cargo profile of the bundle: dev, release or
#                                       browser-release. Default release.
#   PAYKIT_BROWSER_HARNESS_TIMINGS      a file harness-runner.mjs writes the timings to, as JSON.
#                                       Default: none; the timings are printed either way.
#
# The bundle goes where rig/harness/build.sh puts it:
# <cargo target directory>/paykit-browser-harness/<profile>/.

set -euo pipefail

# The test targets that need the Switch service; they run only with --switch.
network_targets=(fetch_transport switch_flow transport_faults switch_faults switch_http)

usage() {
  echo "usage: $0 [--switch] <chromium|firefox|webkit|chrome|all> [page|worker|both] [cargo test args...]" >&2
  echo "       $0 --harness <chromium|firefox|webkit|chrome|all>" >&2
  exit 2
}

die() {
  echo "error: $*" >&2
  exit 1
}

switch=0
harness=0
if [[ "${1:-}" == "--switch" ]]; then
  switch=1
  shift
elif [[ "${1:-}" == "--harness" ]]; then
  # The harness runs against the Switch service, so it starts it as --switch does.
  switch=1
  harness=1
  shift
  [[ $# -eq 1 ]] || usage
fi
[[ $# -ge 1 ]] || usage
case "$1" in
  all) engines=(chromium firefox webkit) ;;
  chromium | firefox | webkit | chrome) engines=("$1") ;;
  *) usage ;;
esac
case "${2:-both}" in
  both) contexts=(page worker) ;;
  page | worker) contexts=("$2") ;;
  *) usage ;;
esac
shift $(($# >= 2 ? 2 : 1))

skip_keychain=${PAYKIT_BROWSER_SKIP_KEYCHAIN_TESTS:-0}
case "$skip_keychain" in
  0 | 1) export PAYKIT_BROWSER_SKIP_KEYCHAIN_TESTS=$skip_keychain ;;
  *)
    echo "error: PAYKIT_BROWSER_SKIP_KEYCHAIN_TESTS must be 0 or 1" >&2
    exit 2
    ;;
esac
# The harness stores no CryptoKey in WebKit on macOS, so the setting does not apply to it.
if [[ $harness -eq 0 && $skip_keychain == 1 && $(uname -s) != Darwin ]]; then
  echo "error: PAYKIT_BROWSER_SKIP_KEYCHAIN_TESTS=1 is for macOS only" >&2
  exit 2
fi
if [[ $harness -eq 0 && $skip_keychain == 1 && " ${engines[*]} " == *" webkit "* ]]; then
  echo "PAYKIT_BROWSER_SKIP_KEYCHAIN_TESTS=1: WebKit runs leave out the tests that store a" \
    "CryptoKey in IndexedDB; this is not full WebKit coverage"
fi

rig=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$rig/../.." && pwd)

is_network_target() {
  local target
  for target in "${network_targets[@]}"; do
    [[ "$target" == "$1" ]] && return 0
  done
  return 1
}

# The `--test` targets among the cargo arguments, up to `--`, after which the arguments go to
# the test binaries.
given_targets=0
expect_target=0
for arg in "$@"; do
  [[ "$arg" == "--" ]] && break
  case "$arg" in
    --lib | --bins | --bin | --bin=* | --examples | --example | --example=* | --tests | \
      --benches | --bench | --bench=* | --all-targets | --doc)
      [[ $expect_target -eq 1 ]] ||
        die "$arg is refused: test.sh picks the test targets itself; name one with --test <target>"
      ;;
  esac
  if [[ $expect_target -eq 1 ]]; then
    target=$arg
    expect_target=0
  elif [[ "$arg" == "--test" ]]; then
    expect_target=1
    continue
  elif [[ "$arg" == --test=* ]]; then
    target=${arg#--test=}
  else
    continue
  fi
  given_targets=1
  if [[ $switch -eq 0 ]] && is_network_target "$target"; then
    die "the test target $target needs the Switch service; run it with --switch"
  fi
done

# Without `--test`, the targets of this mode, named one by one, so the network targets stay out
# of the other mode. Without --switch, `--lib` adds the library's unit tests (the store's format
# and logic, src/store/), which run in browsers too; with --switch, or given `--test`, the
# library is left out.
targets=()
if [[ $given_targets -eq 0 ]]; then
  if [[ $switch -eq 1 ]]; then
    for target in "${network_targets[@]}"; do
      targets+=(--test "$target")
    done
  else
    targets+=(--lib)
    # Cargo's test targets: tests/*.rs and tests/*/main.rs.
    for file in "$root"/paykit-browser/tests/*.rs "$root"/paykit-browser/tests/*/main.rs; do
      [[ -f "$file" ]] || continue
      case "$file" in
        */main.rs) target=$(basename "$(dirname "$file")") ;;
        *) target=$(basename "$file" .rs) ;;
      esac
      is_network_target "$target" || targets+=(--test "$target")
    done
  fi
fi

# The harness needs no test runner: rig/harness/build.sh checks the wasm-bindgen CLI it uses.
if [[ $harness -eq 0 ]]; then
  "$rig/wasm-bindgen-version.sh" --check "${WASM_BINDGEN_TEST_RUNNER:-wasm-bindgen-test-runner}" >/dev/null
fi
if [[ ! -d "$rig/node_modules/playwright" ]]; then
  die "Playwright is not installed; run: (cd $rig && npm ci --ignore-scripts)"
fi
# Cargo splits the runner setting on whitespace. The harness uses no cargo runner.
if [[ $harness -eq 0 && "$rig" =~ [[:space:]] ]]; then
  die "the path $rig contains whitespace, which a cargo runner setting cannot hold"
fi
export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER="node $rig/runner.mjs"
# A target that runs in one context only runs in the other context's run when its own context
# is not asked for (runner.mjs, "Target modes"), so a run of one context still covers it.
export PAYKIT_BROWSER_CONTEXTS="${contexts[*]}"

# Everything this run starts or creates, removed by `cleanup` on every exit.
work=""
tests_pid=""
switch_pid=""
rig_pid=""
database=""

descendants() {
  local child
  for child in $(pgrep -P "$1" || true); do
    echo "$child"
    descendants "$child"
  done
}

# Stop a process this script started with everything it started, such as a test run's cargo,
# runner, wasm-bindgen-test-runner and browser: SIGTERM, then SIGKILL after 10 s. The tree is
# listed first, since a child whose parent stops first moves to init.
stop_tree() {
  local pid i left
  local pids
  pids="$1 $(descendants "$1")"
  # shellcheck disable=SC2086 # one word per process ID
  kill -TERM $pids 2>/dev/null || true
  for ((i = 0; i < 100; i++)); do
    left=""
    for pid in $pids; do
      if kill -0 "$pid" 2>/dev/null; then left="$left $pid"; fi
    done
    [[ -n "$left" ]] || break
    sleep 0.1
  done
  if [[ -n "$left" ]]; then
    echo "warning: process$left did not stop on SIGTERM; killing it" >&2
    # shellcheck disable=SC2086 # one word per process ID
    kill -KILL $left 2>/dev/null || true
  fi
  wait "$1" 2>/dev/null || true
}

psql_admin() {
  psql "$PAYKIT_BROWSER_SWITCH_DATABASE_URL" -X -q -v ON_ERROR_STOP=1 "$@"
}

cleanup() {
  local code=$?
  trap '' INT TERM
  trap - EXIT
  [[ -z "$tests_pid" ]] || stop_tree "$tests_pid"
  [[ -z "$rig_pid" ]] || stop_tree "$rig_pid"
  [[ -z "$switch_pid" ]] || stop_tree "$switch_pid"
  # The service has stopped, so FORCE only ends a session it may have left behind.
  if [[ -n "$database" ]] &&
    ! psql_admin -c "DROP DATABASE IF EXISTS \"$database\" WITH (FORCE)" >&2; then
    echo "error: could not drop the database $database; drop it by hand" >&2
    [[ $code -ne 0 ]] || code=1
  fi
  [[ -z "$work" ]] || rm -rf "$work"
  exit "$code"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

work=$(mktemp -d "${TMPDIR:-/tmp}/paykit-browser-rig.XXXXXX")
# Each run's output, kept to check that some test target ran a test in each engine: a test
# target passes when a test name filter leaves it no test to run (runner.mjs), so a mistyped
# filter would otherwise pass every run without running anything. The check is per engine, not
# per context, as a filter may match only targets that run in one context.
log="$work/test-output.txt"

port_setting() {
  local value=${!1:-$2}
  if ! [[ "$value" =~ ^[1-9][0-9]{0,4}$ ]] || [[ $value -gt 65535 ]]; then
    die "$1 must be a port number, not \"$value\""
  fi
  echo "$value"
}

# Whether nothing accepts connections on a port, on 127.0.0.1 and on ::1 (curl exit 7: could
# not connect, which is also its answer when the host has no IPv6 loopback address).
port_free() {
  local address code
  for address in 127.0.0.1 "[::1]"; do
    code=0
    curl -s -o /dev/null --max-time 2 "http://$address:$1/" || code=$?
    [[ $code -eq 7 ]] || return 1
  done
}

tail_logs() {
  local file
  for file in "$logs/switch.log" "$logs/rig.log"; do
    [[ -f "$file" ]] || continue
    echo "-- last lines of $file" >&2
    tail -n 40 "$file" >&2
  done
}

die_with_logs() {
  tail_logs
  die "$@"
}

# Start the Switch service and the rig, and wait until both answer.
start_switch_rig() {
  local tool
  for tool in psql curl jq openssl; do
    command -v "$tool" >/dev/null 2>&1 || die "--switch needs $tool on PATH"
  done
  switch_bin=${PAYKIT_BROWSER_SWITCH_BIN:-}
  [[ -n "$switch_bin" ]] || die "--switch needs PAYKIT_BROWSER_SWITCH_BIN, the pubky-switch binary"
  [[ -x "$switch_bin" ]] || die "PAYKIT_BROWSER_SWITCH_BIN ($switch_bin) is not an executable file"
  [[ -n "${PAYKIT_BROWSER_SWITCH_DATABASE_URL:-}" ]] ||
    die "--switch needs PAYKIT_BROWSER_SWITCH_DATABASE_URL, a postgres:// URL whose user may create databases"
  rig_port=$(port_setting PAYKIT_BROWSER_RIG_PORT 18443)
  switch_port=$(port_setting PAYKIT_BROWSER_SWITCH_PORT 18480)
  admin_port=$(port_setting PAYKIT_BROWSER_SWITCH_ADMIN_PORT 18490)
  closed_port=$(port_setting PAYKIT_BROWSER_RIG_CLOSED_PORT 18449)
  local ports=("$rig_port" "$switch_port" "$admin_port" "$closed_port")
  # With --harness, the rig also serves the harness on its own port.
  local harness_args=()
  if [[ $harness -eq 1 ]]; then
    harness_port=$(port_setting PAYKIT_BROWSER_HARNESS_PORT 18444)
    ports+=("$harness_port")
    harness_args=(--harness-port "$harness_port" --harness-bundle "$harness_bundle")
  fi
  if [[ $(printf '%s\n' "${ports[@]}" | sort -u | wc -l) -ne ${#ports[@]} ]]; then
    die "the rig, service, admin, closed and harness ports must differ"
  fi
  local port
  for port in "${ports[@]}"; do
    port_free "$port" || die "something already listens on 127.0.0.1:$port or [::1]:$port"
  done

  # The binary reports its crate version but not the commit it was built from, so only the
  # version is compared with the pubky-switch package that Cargo.lock pins.
  local pinned_version pinned_rev binary_version
  pinned_version=$("$rig/switch-pin.sh" --version) || die "could not read the pubky-switch pin"
  pinned_rev=$("$rig/switch-pin.sh" --rev) || die "could not read the pubky-switch pin"
  binary_version=$("$switch_bin" --version | awk '{ print $2 }') ||
    die "$switch_bin --version failed"
  if [[ "$binary_version" != "$pinned_version" ]]; then
    die "$switch_bin is pubky-switch $binary_version, but Cargo.lock pins $pinned_version at $pinned_rev"
  fi
  echo "== pubky-switch crate version $binary_version matches Cargo.lock; the binary's commit is" \
    "not checked (Cargo.lock pins $pinned_rev)"

  local enroll_bin=${PAYKIT_BROWSER_ENROLL_BIN:-}
  if [[ -z "$enroll_bin" ]]; then
    echo "== building the switch-enroll example"
    local messages
    if ! messages=$(cargo build --locked --manifest-path "$root/Cargo.toml" -p paykit-browser \
      --example switch-enroll --message-format json-render-diagnostics); then
      die "could not build the switch-enroll example of paykit-browser"
    fi
    enroll_bin=$(jq -r 'select(.reason == "compiler-artifact" and .target.name == "switch-enroll")
      | .executable // empty' <<<"$messages" | tail -n 1)
    [[ -n "$enroll_bin" ]] || die "cargo reported no switch-enroll executable"
  fi
  [[ -x "$enroll_bin" ]] || die "the enrol program $enroll_bin is not an executable file"

  if [[ -n "${PAYKIT_BROWSER_RIG_LOG_DIR:-}" ]]; then
    logs=$PAYKIT_BROWSER_RIG_LOG_DIR
    mkdir -p "$logs"
    rm -f "$logs/switch.log" "$logs/rig.log" "$logs/switch-config.txt"
  else
    logs="$work/logs"
    mkdir "$logs"
  fi

  # The key store path goes into a TOML string unescaped.
  [[ "$work" =~ ^[A-Za-z0-9/._-]+$ ]] ||
    die "the temporary folder $work has characters the service configuration cannot hold"
  sed -e "s|@ADMIN_PORT@|$admin_port|g" -e "s|@SWITCH_PORT@|$switch_port|g" \
    -e "s|@RIG_PORT@|$rig_port|g" -e "s|@KEYS_PATH@|$work/keys|g" \
    "$rig/switch.toml.in" >"$work/switch.toml"
  if ! "$switch_bin" check-config --config "$work/switch.toml" >"$logs/switch-config.txt" 2>&1; then
    cat "$logs/switch-config.txt" >&2
    die "pubky-switch check-config refused the rig configuration"
  fi

  # Set before the database exists, so an interrupt during CREATE DATABASE still drops it.
  database="paykit_browser_rig_$(date +%s)_$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
  psql_admin -c "CREATE DATABASE \"$database\"" ||
    die "could not create a database through PAYKIT_BROWSER_SWITCH_DATABASE_URL"
  local database_url
  database_url=$(SOURCE_URL=$PAYKIT_BROWSER_SWITCH_DATABASE_URL DATABASE=$database node -e '
    const url = new URL(process.env.SOURCE_URL);
    url.pathname = `/${process.env.DATABASE}`;
    process.stdout.write(url.href);') ||
    die "PAYKIT_BROWSER_SWITCH_DATABASE_URL is not a URL"

  NO_COLOR=1 PUBKY_SWITCH_DATABASE_URL=$database_url "$switch_bin" --config "$work/switch.toml" \
    >"$logs/switch.log" 2>&1 &
  switch_pid=$!
  # A fresh token per run, in the rig's environment only: the process list shows arguments.
  rig_token=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n') ||
    die "could not make a random token for the rig"
  [[ "$rig_token" =~ ^[0-9a-f]{32}$ ]] || die "could not make a random token for the rig"
  PAYKIT_BROWSER_RIG_TOKEN=$rig_token node "$rig/switch-rig.mjs" --listen "$rig_port" \
    --upstream "$switch_port" --enroll-bin "$enroll_bin" ${harness_args[@]+"${harness_args[@]}"} \
    >"$logs/rig.log" 2>&1 &
  rig_pid=$!

  origin="https://localhost:$rig_port"
  local deadline=$((SECONDS + 60))
  # The rig's own ready line, so a rig left running by another run cannot answer for it. With
  # --harness, the rig prints it once the harness port serves too, and the harness page must
  # answer (index.html comes from the source tree, so it is there before the bundle is built).
  until curl -fsS -o /dev/null --max-time 2 "http://127.0.0.1:$admin_port/health/ready" 2>/dev/null &&
    grep -q '^\[switch-rig\] ready ' "$logs/rig.log" &&
    curl -kfsS -o /dev/null --max-time 2 "$origin/rig/health" 2>/dev/null &&
    { [[ $harness -eq 0 ]] || curl -kfsS -o /dev/null --max-time 2 \
      "https://localhost:$harness_port/harness/strict/index.html" 2>/dev/null; }; do
    kill -0 "$switch_pid" 2>/dev/null || die_with_logs "the Switch service exited while starting"
    kill -0 "$rig_pid" 2>/dev/null || die_with_logs "switch-rig.mjs exited while starting"
    [[ $SECONDS -lt $deadline ]] ||
      die_with_logs "the Switch service or the rig did not become ready in 60 s"
    sleep 0.2
  done
  echo "== Switch service ready behind $origin (database $database, logs in $logs)"
}

# Run a step of the harness in the background, so that a signal stops it at once through the
# traps above, and return its exit status.
run_step() {
  ("$@") &
  tests_pid=$!
  local status=0
  wait "$tests_pid" || status=$?
  tests_pid=""
  return "$status"
}

# --harness: start the service and the rig with the harness port, build the bundle against
# them, and run harness-runner.mjs in the engines given.
run_harness() {
  local profile=${PAYKIT_BROWSER_HARNESS_PROFILE:-release}
  case "$profile" in
    dev | release | browser-release) ;;
    *) die "PAYKIT_BROWSER_HARNESS_PROFILE must be dev, release or browser-release, not \"$profile\"" ;;
  esac
  [[ -x "$rig/harness/build.sh" ]] || die "$rig/harness/build.sh is missing or not executable"
  # The rig serves the bundle from where build.sh writes it. build.sh empties that folder
  # before it writes the new bundle and fails unless both files are there.
  harness_bundle=$("$rig/harness/build.sh" --print-out-dir "$profile") ||
    die "could not read the harness bundle folder"
  start_switch_rig

  echo "== building the harness bundle ($profile) for $origin"
  run_step env PAYKIT_BROWSER_RIG_ORIGIN="$origin" PAYKIT_BROWSER_RIG_CLOSED_PORT="$closed_port" \
    PAYKIT_BROWSER_RIG_TOKEN="$rig_token" "$rig/harness/build.sh" "$profile" ||
    die_with_logs "$rig/harness/build.sh $profile failed"

  echo "== hosted harness: ${engines[*]}, harness origin https://localhost:$harness_port"
  local status=0
  run_step env PAYKIT_BROWSER_RIG_ORIGIN="$origin" \
    PAYKIT_BROWSER_HARNESS_ORIGIN="https://localhost:$harness_port" \
    node "$rig/harness-runner.mjs" "${engines[@]}" ||
    status=$?
  # A harness run must never pass without its service: fail when either one stopped.
  kill -0 "$switch_pid" 2>/dev/null || { echo "error: the Switch service exited during the run" >&2; status=1; }
  kill -0 "$rig_pid" 2>/dev/null || { echo "error: switch-rig.mjs exited during the run" >&2; status=1; }
  if [[ $status -ne 0 ]]; then
    [[ -n "${PAYKIT_BROWSER_RIG_LOG_DIR:-}" ]] || tail_logs
    echo "failed: hosted harness (${engines[*]})" >&2
    exit 1
  fi
  echo "all passed: hosted harness, ${engines[*]}"
  exit 0
}

if [[ $harness -eq 1 ]]; then
  run_harness
fi

if [[ $switch -eq 1 ]]; then
  start_switch_rig
  export PAYKIT_BROWSER_RIG_ORIGIN=$origin
  export PAYKIT_BROWSER_RIG_CLOSED_PORT=$closed_port
  export PAYKIT_BROWSER_RIG_TOKEN=$rig_token
else
  unset PAYKIT_BROWSER_RIG_ORIGIN PAYKIT_BROWSER_RIG_CLOSED_PORT PAYKIT_BROWSER_RIG_TOKEN
fi

failed=()
# Test targets that run in one context only, and so were skipped in the other (runner.mjs).
skipped=()
# Store targets not run in an engine build without the Origin Private File System, with the
# reason (runner.mjs; NO_OPFS in engines.mjs).
excluded=()
# Tests left out as they need Keychain access (PAYKIT_BROWSER_SKIP_KEYCHAIN_TESTS).
not_run=()
for engine in "${engines[@]}"; do
  ran=no
  failed_before=${#failed[@]}
  for context in "${contexts[@]}"; do
    echo "== $engine, $context"
    # In the background, so that a signal stops the run at once, through the traps above.
    (
      PAYKIT_BROWSER_ENGINE=$engine PAYKIT_BROWSER_CONTEXT=$context \
        cargo test --locked --manifest-path "$root/Cargo.toml" -p paykit-browser \
        --target wasm32-unknown-unknown ${targets[@]+"${targets[@]}"} "$@" 2>&1 | tee "$log"
    ) &
    tests_pid=$!
    status=0
    wait "$tests_pid" || status=$?
    tests_pid=""
    if [[ $status -ne 0 ]]; then
      failed+=("$engine/$context")
    elif grep -q '^\[runner\] PASS ' "$log"; then
      ran=yes
    fi
    for target in $(sed -n 's/^\[runner\] SKIP \([^:]*\):.*/\1/p' "$log"); do
      skipped+=("$target@$engine/$context")
    done
    while IFS= read -r line; do
      excluded+=("$engine/$context: $line")
    done < <(sed -n 's/^\[runner\] EXCLUDED //p' "$log")
    for test in $(sed -n 's/^\[runner\] NOT RUN \([^ ]*\): needs Keychain access.*/\1/p' "$log"); do
      not_run+=("$test@$engine/$context")
    done
  done
  if [[ $ran == no && ${#failed[@]} -eq $failed_before ]]; then
    echo "error: no test ran in $engine (${contexts[*]}); check the test name filter" >&2
    failed+=("$engine")
  fi
done

if [[ ${#skipped[@]} -gt 0 ]]; then
  echo "not run, as they run only in the other context: ${skipped[*]}"
fi
for entry in ${excluded[@]+"${excluded[@]}"}; do
  echo "warning: not run, excluded in this engine build: $entry" >&2
  # A GitHub Actions annotation, so the exclusion shows on the run's summary page.
  [[ -z "${GITHUB_ACTIONS:-}" ]] || echo "::warning title=Store target not run::$entry"
done

if [[ $switch -eq 1 ]]; then
  # A network test must never pass without its service: fail when either one stopped.
  kill -0 "$switch_pid" 2>/dev/null || failed+=("the Switch service exited during the tests")
  kill -0 "$rig_pid" 2>/dev/null || failed+=("switch-rig.mjs exited during the tests")
fi

if [[ ${#not_run[@]} -gt 0 ]]; then
  echo "not run, as they need Keychain access for Playwright's WebKit on macOS" \
    "(PAYKIT_BROWSER_SKIP_KEYCHAIN_TESTS=1): ${#not_run[@]} tests"
fi

if [[ ${#failed[@]} -gt 0 ]]; then
  [[ $switch -eq 0 || -n "${PAYKIT_BROWSER_RIG_LOG_DIR:-}" ]] || tail_logs
  echo "failed: ${failed[*]}" >&2
  exit 1
fi
# What did not run is named in the summary line too, apart from the single-context skips,
# which run in the other context.
left_out=""
[[ ${#excluded[@]} -eq 0 ]] ||
  left_out="${#excluded[@]} target runs excluded in an engine build with no Origin Private File System"
[[ ${#not_run[@]} -eq 0 ]] ||
  left_out+="${left_out:+; }${#not_run[@]} tests not run as they need Keychain access"
if [[ -n $left_out ]]; then
  echo "all passed, except: $left_out: ${engines[*]} x ${contexts[*]}"
else
  echo "all passed: ${engines[*]} x ${contexts[*]}"
fi
