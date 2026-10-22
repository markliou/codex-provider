#!/bin/sh
set -eu

if [ "$#" -gt 0 ]; then
  exec /usr/local/bin/codex-pool "$@"
fi

HELPER_PID=""
POOL_PID=""

cleanup() {
  if [ -n "$HELPER_PID" ]; then
    kill "$HELPER_PID" 2>/dev/null || true
    wait "$HELPER_PID" 2>/dev/null || true
  fi
}

# A stop signal goes to the pool first, and the helper (codex-upstream) is
# stopped only after the pool exits. The pool drains in-flight
# requests on SIGTERM, and those streams still flow through the helper;
# stopping the helper first, as this script used to on every signal, cut every
# open stream on every deploy.
on_stop() {
  if [ -n "$POOL_PID" ] && kill -0 "$POOL_PID" 2>/dev/null; then
    kill -TERM "$POOL_PID" 2>/dev/null || true
    wait "$POOL_PID" 2>/dev/null || true
  fi
  exit 0
}

# wait_for_helper URL: fails if the helper exits before it answers.
wait_for_helper() {
  for _ in $(seq 1 40); do
    if ! kill -0 "$HELPER_PID" 2>/dev/null; then
      wait "$HELPER_PID"
      exit 1
    fi
    if node -e 'fetch(process.argv[1], {signal: AbortSignal.timeout(1000)}).then((response) => process.exit(response.status < 500 ? 0 : 1)).catch(() => process.exit(1))' "$1"; then
      return 0
    fi
    sleep 0.25
  done
}

run_pool() {
  trap cleanup EXIT
  trap on_stop INT TERM
  /usr/local/bin/codex-pool &
  POOL_PID=$!
  wait "$POOL_PID"
  STATUS=$?
  exit "$STATUS"
}

# The final hop goes through codex-upstream, Codex's own HTTP client, so the
# upstream connection is the one a native Codex CLI makes. An explicit
# CODEX_POOL_CODEX_TRANSPORT_URL (tests) is left alone.
#
# codex-upstream is also the front door (RUST_PORT.md step 3): it takes the
# pool's public address (CODEX_POOL_ADDR, as the pool would have bound it) and
# relays every client connection to the pool, which is moved to a loopback
# address inside the container. The stop order above still holds: the door
# lives in the helper, so it keeps relaying the pool's draining streams until
# the pool exits.
POOL_INTERNAL_ADDR=127.0.0.1:8318
if [ -z "${CODEX_POOL_CODEX_TRANSPORT_URL:-}" ] && [ -x /usr/local/bin/codex-upstream ]; then
  PUBLIC_ADDR=${CODEX_POOL_ADDR:-${CODEX_POOL_PUBLIC_ADDR:-:8317}}
  CODEX_UPSTREAM_ADDR=127.0.0.1:8320 \
    CODEX_FRONT_ADDR="$PUBLIC_ADDR" \
    CODEX_FRONT_POOL_ADDR="$POOL_INTERNAL_ADDR" \
    /usr/local/bin/codex-upstream &
  HELPER_PID=$!
  wait_for_helper http://127.0.0.1:8320/
  export CODEX_POOL_CODEX_TRANSPORT_URL=http://127.0.0.1:8320
  export CODEX_POOL_ADDR="$POOL_INTERNAL_ADDR"
fi
run_pool
