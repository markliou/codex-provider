#!/bin/sh
set -eu

if [ "$#" -gt 0 ]; then
  exec /usr/local/bin/codex-pool "$@"
fi

if [ "${CODEX_POOL_CODEX_GATEWAY_MODE:-sidecar}" = "direct" ]; then
  exec /usr/local/bin/codex-pool
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
SIDECAR_PID=$!

cleanup() {
  kill "$SIDECAR_PID" 2>/dev/null || true
  wait "$SIDECAR_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

for _ in $(seq 1 40); do
  if ! kill -0 "$SIDECAR_PID" 2>/dev/null; then
    wait "$SIDECAR_PID"
    exit 1
  fi
  if node -e 'fetch(process.argv[1], {headers: {Authorization: "Bearer " + process.argv[2]}, signal: AbortSignal.timeout(1000)}).then((response) => process.exit(response.ok ? 0 : 1)).catch(() => process.exit(1))' "$SIDECAR_URL/models" "$SIDECAR_KEY"; then
    break
  fi
  sleep 0.25
done

/usr/local/bin/codex-pool &
POOL_PID=$!
wait "$POOL_PID"
STATUS=$?
exit "$STATUS"
