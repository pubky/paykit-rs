#!/usr/bin/env bash
# Run the paykit-browser tests in browser engines, through Playwright (runner.mjs).
#
#   paykit-browser/rig/test.sh [--switch] <chromium|firefox|webkit|chrome|all> [page|worker|both] [cargo test args...]
#
# "all" is Playwright's chromium, firefox and webkit; "chrome" is the installed Google Chrome.
# The context defaults to both. Any further arguments go to `cargo test`, such as a test name
# filter. Needs wasm-bindgen-test-runner on PATH (or in WASM_BINDGEN_TEST_RUNNER) at the
# wasm-bindgen version of Cargo.lock, and `npm ci --ignore-scripts` run in this folder.
#
# Without --switch it runs every test target except the network ones (network_targets below),
# so it needs no database and no service. With --switch it runs the network targets, or the
# targets given with `--test`, against a real Pubky Switch service behind switch-rig.mjs, which
# terminates TLS and answers CORS on https://localhost:<rig port>, on 127.0.0.1 and ::1. The
# script picks the test targets: `--test` is the only cargo target option it takes, in either
# mode. This is test rig code only, never for production. The run starts the service, the rig
# and a fresh database, and removes them on every exit, including a failure and Ctrl-C. It
# reads:
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

set -euo pipefail

# The test targets that need the Switch service; they run only with --switch.
network_targets=(fetch_transport switch_flow transport_faults switch_faults switch_http)

usage() {
  echo "usage: $0 [--switch] <chromium|firefox|webkit|chrome|all> [page|worker|both] [cargo test args...]" >&2
  exit 2
}

die() {
  echo "error: $*" >&2
  exit 1
}

switch=0
if [[ "${1:-}" == "--switch" ]]; then
  switch=1
  shift
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

# Without `--test`, the targets of this mode, named one by one. Naming them also keeps the
# library out: given a test name filter, `cargo test` would run the library test binary too,
# despite its `test = false`, and that binary has no browser tests.
targets=()
if [[ $given_targets -eq 0 ]]; then
  if [[ $switch -eq 1 ]]; then
    for target in "${network_targets[@]}"; do
      targets+=(--test "$target")
    done
  else
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

locked=$("$rig/wasm-bindgen-version.sh")
install_hint="cargo install wasm-bindgen-cli --version $locked --locked"
runner_bin=${WASM_BINDGEN_TEST_RUNNER:-wasm-bindgen-test-runner}
if ! command -v "$runner_bin" >/dev/null 2>&1; then
  die "$runner_bin not found; install it with: $install_hint"
fi
if ! version_line=$("$runner_bin" --version); then
  die "$runner_bin --version failed; install it with: $install_hint"
fi
installed=$(awk '{ print $2 }' <<<"$version_line")
if [[ "$installed" != "$locked" ]]; then
  die "$runner_bin is $installed, but Cargo.lock has wasm-bindgen $locked;" \
    "install the matching CLI with: $install_hint"
fi
if [[ ! -d "$rig/node_modules/playwright" ]]; then
  die "Playwright is not installed; run: (cd $rig && npm ci --ignore-scripts)"
fi
# Cargo splits the runner setting on whitespace.
if [[ "$rig" =~ [[:space:]] ]]; then
  die "the path $rig contains whitespace, which a cargo runner setting cannot hold"
fi
export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER="node $rig/runner.mjs"

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
# Each run's output, kept to check that some test target ran a test: a test target passes when
# a test name filter leaves it no test to run (runner.mjs), so a mistyped filter would
# otherwise pass every run without running anything.
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
  if [[ $(printf '%s\n' "${ports[@]}" | sort -u | wc -l) -ne ${#ports[@]} ]]; then
    die "the rig, service, admin and closed ports must differ"
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
    --upstream "$switch_port" --enroll-bin "$enroll_bin" >"$logs/rig.log" 2>&1 &
  rig_pid=$!

  origin="https://localhost:$rig_port"
  local deadline=$((SECONDS + 60))
  # The rig's own ready line, so a rig left running by another run cannot answer for it.
  until curl -fsS -o /dev/null --max-time 2 "http://127.0.0.1:$admin_port/health/ready" 2>/dev/null &&
    grep -q '^\[switch-rig\] ready ' "$logs/rig.log" &&
    curl -kfsS -o /dev/null --max-time 2 "$origin/rig/health" 2>/dev/null; do
    kill -0 "$switch_pid" 2>/dev/null || die_with_logs "the Switch service exited while starting"
    kill -0 "$rig_pid" 2>/dev/null || die_with_logs "switch-rig.mjs exited while starting"
    [[ $SECONDS -lt $deadline ]] ||
      die_with_logs "the Switch service or the rig did not become ready in 60 s"
    sleep 0.2
  done
  echo "== Switch service ready behind $origin (database $database, logs in $logs)"
}

if [[ $switch -eq 1 ]]; then
  start_switch_rig
  export PAYKIT_BROWSER_RIG_ORIGIN=$origin
  export PAYKIT_BROWSER_RIG_CLOSED_PORT=$closed_port
  export PAYKIT_BROWSER_RIG_TOKEN=$rig_token
else
  unset PAYKIT_BROWSER_RIG_ORIGIN PAYKIT_BROWSER_RIG_CLOSED_PORT PAYKIT_BROWSER_RIG_TOKEN
fi

failed=()
for engine in "${engines[@]}"; do
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
    elif ! grep -q '^\[runner\] PASS ' "$log"; then
      echo "error: no test ran in $engine/$context; check the test name filter" >&2
      failed+=("$engine/$context")
    fi
  done
done

if [[ $switch -eq 1 ]]; then
  # A network test must never pass without its service: fail when either one stopped.
  kill -0 "$switch_pid" 2>/dev/null || failed+=("the Switch service exited during the tests")
  kill -0 "$rig_pid" 2>/dev/null || failed+=("switch-rig.mjs exited during the tests")
fi

if [[ ${#failed[@]} -gt 0 ]]; then
  [[ $switch -eq 0 || -n "${PAYKIT_BROWSER_RIG_LOG_DIR:-}" ]] || tail_logs
  echo "failed: ${failed[*]}" >&2
  exit 1
fi
echo "all passed: ${engines[*]} x ${contexts[*]}"
