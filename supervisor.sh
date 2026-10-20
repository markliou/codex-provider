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
if [ -z "${CODEX_POOL_CODEX_TRANSPORT_URL:-}" ] && [ -x /usr/local/bin/codex-upstream ]; then
  CODEX_UPSTREAM_ADDR=127.0.0.1:8320 /usr/local/bin/codex-upstream &
  HELPER_PID=$!
  wait_for_helper http://127.0.0.1:8320/
  export CODEX_POOL_CODEX_TRANSPORT_URL=http://127.0.0.1:8320
fi
run_pool
