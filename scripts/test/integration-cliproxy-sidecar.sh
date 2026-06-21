#!/bin/sh
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"

NETWORK=${NETWORK:-codex-cliproxy-itest-$$}
POOL=${POOL:-codex-cliproxy-pool-itest-$$}
VOLUME=${VOLUME:-codex-cliproxy-data-itest-$$}
CLIENT_KEY=${CLIENT_KEY:-test-client-key}
ADMIN_PASSWORD=${ADMIN_PASSWORD:-test-admin-password}

cleanup() {
  docker rm -f "$POOL" >/dev/null 2>&1 || true
  docker network rm "$NETWORK" >/dev/null 2>&1 || true
  docker volume rm "$VOLUME" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

docker build -t codex-pool:local .
ADMIN_HASH=$(docker run --rm -e CODEX_POOL_ADMIN_PASSWORD="$ADMIN_PASSWORD" codex-pool:local hash-password)

docker network create --internal "$NETWORK" >/dev/null
docker volume create "$VOLUME" >/dev/null
docker run --rm -v "$VOLUME:/data" alpine:3.21 sh -ceu '
  mkdir -p /data/accounts/device-a/.codex
  cat >/data/config.json <<"EOF"
{"defaultModel":"gpt-test","modelAliases":{},"accounts":[{"id":"device-a","authType":"codex_device_auth","enabled":true,"inPool":true,"priority":100}]}
EOF
  printf "%s" "{\"auth_mode\":\"chatgpt\",\"tokens\":{\"access_token\":\"<test-device-access>\",\"refresh_token\":\"<test-device-refresh>\"}}" >/data/accounts/device-a/.codex/auth.json
  chmod 700 /data/accounts/device-a/.codex
  chmod 600 /data/accounts/device-a/.codex/auth.json
'

docker run -d --name "$POOL" --network "$NETWORK" --network-alias codex-pool -v "$VOLUME:/data" \
  -e CODEX_POOL_API_KEY="$CLIENT_KEY" \
  -e CODEX_POOL_ADMIN_PASSWORD_HASH="$ADMIN_HASH" \
  -e CODEX_POOL_DEFAULT_MODEL=gpt-test \
  -e CODEX_POOL_ADMIN_ADDR=0.0.0.0:8318 \
  -e CODEX_POOL_ALLOW_REMOTE_ADMIN=true \
  codex-pool:local >/dev/null

docker run --rm --network "$NETWORK" --entrypoint node codex-pool:local -e '
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
for (let i = 0; i < 80; i += 1) {
  try {
    const response = await fetch("http://codex-pool:8317/healthz", { headers: { Authorization: "Bearer '"$CLIENT_KEY"'" }, signal: AbortSignal.timeout(1000) });
    if (response.ok) process.exit(0);
  } catch (_) {}
  await wait(250);
}
throw new Error("pool did not become healthy");
'

docker exec "$POOL" node -e '
import fs from "node:fs";
const auth = JSON.parse(fs.readFileSync("/data/cliproxy/auths/device-a.json", "utf8"));
if (auth.type !== "codex" || auth.prefix !== "codex-pool-device-a") throw new Error("sidecar auth conversion missing account prefix");
if (!auth.access_token || !auth.refresh_token || auth.tokens) throw new Error("sidecar auth conversion has invalid token schema");
const key = fs.readFileSync("/data/cliproxy/internal-api-key", "utf8").trim();
const response = await fetch("http://127.0.0.1:8319/v1/models", { headers: { Authorization: `Bearer ${key}` }, signal: AbortSignal.timeout(3000) });
if (!response.ok) throw new Error(`sidecar models status ${response.status}`);
'

docker run --rm --network "$NETWORK" --entrypoint node codex-pool:local -e '
try {
  await fetch("http://codex-pool:8319/v1/models", { signal: AbortSignal.timeout(1000) });
  throw new Error("sidecar port is reachable from the Docker network");
} catch (error) {
  if (error.message === "sidecar port is reachable from the Docker network") throw error;
}
'

printf "%s\n" "CLIProxy sidecar integration passed."
