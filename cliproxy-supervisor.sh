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

# A stop signal goes to the pool first, and the helper (codex-upstream or the
# sidecar) is stopped only after the pool exits. The pool drains in-flight
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

# wait_for_helper URL [BEARER]: fails if the helper exits before it answers.
wait_for_helper() {
  for _ in $(seq 1 40); do
    if ! kill -0 "$HELPER_PID" 2>/dev/null; then
      wait "$HELPER_PID"
      exit 1
    fi
    if node -e 'fetch(process.argv[1], {headers: process.argv[2] ? {Authorization: "Bearer " + process.argv[2]} : {}, signal: AbortSignal.timeout(1000)}).then((response) => process.exit(response.status < 500 ? 0 : 1)).catch(() => process.exit(1))' "$1" "${2:-}"; then
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

if [ "${CODEX_POOL_CODEX_GATEWAY_MODE:-direct}" = "direct" ]; then
  # Direct mode sends the final hop through codex-upstream, Codex's own HTTP
  # client, so the upstream connection is the one a native Codex CLI makes.
  # An explicit CODEX_POOL_CODEX_TRANSPORT_URL (tests) is left alone.
  if [ -z "${CODEX_POOL_CODEX_TRANSPORT_URL:-}" ] && [ -x /usr/local/bin/codex-upstream ]; then
    CODEX_UPSTREAM_ADDR=127.0.0.1:8320 /usr/local/bin/codex-upstream &
    HELPER_PID=$!
    wait_for_helper http://127.0.0.1:8320/
    export CODEX_POOL_CODEX_TRANSPORT_URL=http://127.0.0.1:8320
  fi
  run_pool
fi

DATA_DIR=${CODEX_POOL_DATA_DIR:-/data}
SIDECAR_DIR="$DATA_DIR/cliproxy"
AUTH_DIR="$SIDECAR_DIR/auths"
KEY_FILE="$SIDECAR_DIR/internal-api-key"
CONFIG_FILE="$SIDECAR_DIR/config.yaml"

mkdir -p "$AUTH_DIR"
chmod 700 "$SIDECAR_DIR" "$AUTH_DIR"

if [ ! -s "$KEY_FILE" ]; then
  umask 077
  head -c 48 /dev/urandom | base64 | tr -d '\n' >"$KEY_FILE"
fi
chmod 600 "$KEY_FILE"
SIDECAR_KEY=$(cat "$KEY_FILE")

umask 077
cat >"$CONFIG_FILE" <<EOF
host: "127.0.0.1"
port: 8319
auth-dir: "$AUTH_DIR"
api-keys:
  - "$SIDECAR_KEY"
debug: false
commercial-mode: true
usage-statistics-enabled: false
logging-to-file: false
request-retry: 0
max-retry-credentials: 1
disable-cooling: true
force-model-prefix: true
remote-management:
  allow-remote: false
  secret-key: ""
  disable-control-panel: true
routing:
  strategy: "fill-first"
  session-affinity: false
EOF
chmod 600 "$CONFIG_FILE"

export CODEX_POOL_CLIPROXY_BASE_URL=${CODEX_POOL_CLIPROXY_BASE_URL:-http://127.0.0.1:8319/v1}
export CODEX_POOL_CLIPROXY_API_KEY=${CODEX_POOL_CLIPROXY_API_KEY:-$SIDECAR_KEY}
SIDECAR_URL=http://127.0.0.1:8319/v1

/usr/local/bin/cliproxy-sidecar -config "$CONFIG_FILE" &
HELPER_PID=$!
wait_for_helper "$SIDECAR_URL/models" "$SIDECAR_KEY"
run_pool
