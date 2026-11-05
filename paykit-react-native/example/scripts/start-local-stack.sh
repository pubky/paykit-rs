#!/usr/bin/env bash
# Start the local Pubky stack (local-stack.mjs) in the background, and wait
# for it separately, so other work can run while it builds and starts.
#
#   start-local-stack.sh start <dir>
#       Start it; write its PID to <dir>/local-stack.pid and its output to
#       <dir>/local-stack.log.
#   start-local-stack.sh wait <dir> [timeout seconds]
#       Wait until the log has PAYKIT_LOCAL_STACK_READY (default 1200 s).
#       Fails if the stack exits first.
set -euo pipefail

usage() {
  echo "Usage: $0 start <dir> | wait <dir> [timeout seconds]" >&2
  exit 2
}

[ $# -ge 2 ] || usage
mode=$1
dir=$2
log="$dir/local-stack.log"
pid_file="$dir/local-stack.pid"

case $mode in
  start)
    mkdir -p "$dir"
    nohup node "$(dirname "$0")/local-stack.mjs" >"$log" 2>&1 &
    echo $! >"$pid_file"
    echo "Local stack starting: PID $(cat "$pid_file"), log $log"
    ;;
  wait)
    deadline=$((SECONDS + ${3:-1200}))
    until grep -q PAYKIT_LOCAL_STACK_READY "$log"; do
      if ! kill -0 "$(cat "$pid_file")" 2>/dev/null; then
        tail -50 "$log"
        echo "The local stack exited before it was ready" >&2
        exit 1
      fi
      if ((SECONDS > deadline)); then
        tail -50 "$log"
        echo "The local stack was not ready within ${3:-1200} s" >&2
        exit 1
      fi
      sleep 2
    done
    grep PAYKIT_LOCAL_STACK_READY "$log"
    ;;
  *)
    usage
    ;;
esac
