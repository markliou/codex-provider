#!/bin/sh
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"

NETWORK=${NETWORK:-codex-device-auth-itest-$$}
POOL=${POOL:-codex-device-pool-itest-$$}
MOCK=${MOCK:-codex-device-mock-itest-$$}
VOLUME=${VOLUME:-codex-device-data-itest-$$}
CLIENT_KEY=${CLIENT_KEY:-test-client-key}
ADMIN_PASSWORD=${ADMIN_PASSWORD:-test-admin-password}

cleanup() {
  docker rm -f "$POOL" "$MOCK" >/dev/null 2>&1 || true
  docker network rm "$NETWORK" >/dev/null 2>&1 || true
  docker volume rm "$VOLUME" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

docker build -t codex-pool:local .
docker build -t codex-mock-upstream:local -f test/Dockerfile.mock .
ADMIN_HASH=$(docker run --rm -e CODEX_POOL_ADMIN_PASSWORD="$ADMIN_PASSWORD" codex-pool:local hash-password)

docker network create --internal "$NETWORK" >/dev/null
docker volume create "$VOLUME" >/dev/null
docker run --rm -v "$VOLUME:/data" alpine:3.21 sh -ceu '
  mkdir -p /data/accounts/device-a/.codex /data/accounts/device-b/.codex
  cat >/data/config.json <<"EOF"
{"defaultModel":"gpt-test","modelAliases":{},"accounts":[
  {"id":"device-a","accountId":"test-acct-a","authType":"codex_device_auth","enabled":true,"inPool":true,"priority":100},
  {"id":"device-b","accountId":"test-acct-b","authType":"codex_device_auth","enabled":true,"inPool":true,"priority":10}
]}
EOF
  printf "%s" "{\"auth_mode\":\"chatgpt\",\"tokens\":{\"access_token\":\"<test-device-access-a>\"}}" >/data/accounts/device-a/.codex/auth.json
  printf "%s" "{\"auth_mode\":\"chatgpt\",\"tokens\":{\"access_token\":\"<test-device-access-b>\"}}" >/data/accounts/device-b/.codex/auth.json
  chmod 700 /data/accounts/device-a/.codex /data/accounts/device-b/.codex
  chmod 600 /data/accounts/*/.codex/auth.json
'

docker run -d --name "$MOCK" --network "$NETWORK" --network-alias codex-mock codex-mock-upstream:local >/dev/null
docker run -d --name "$POOL" --network "$NETWORK" --network-alias codex-pool -v "$VOLUME:/data" \
  -e CODEX_POOL_API_KEY="$CLIENT_KEY" \
  -e CODEX_POOL_ADMIN_PASSWORD_HASH="$ADMIN_HASH" \
  -e CODEX_POOL_DEFAULT_MODEL=gpt-test \
  -e CODEX_POOL_CODEX_GATEWAY_MODE=sidecar \
  -e CODEX_POOL_CLIPROXY_BASE_URL=http://codex-mock:4010/v1 \
  -e CODEX_POOL_CLIPROXY_API_KEY=cliproxy-test-key \
  -e CODEX_POOL_ADMIN_ADDR=0.0.0.0:8318 \
  -e CODEX_POOL_ALLOW_REMOTE_ADMIN=true \
  codex-pool:local >/dev/null

docker run --rm --network "$NETWORK" --entrypoint node codex-pool:local -e '
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
for (let i = 0; i < 60; i += 1) {
  try {
    const response = await fetch("http://codex-pool:8317/healthz", { headers: { Authorization: "Bearer '"$CLIENT_KEY"'" } });
    if (response.ok) process.exit(0);
  } catch (_) {}
  await wait(250);
}
throw new Error("pool did not become healthy");
'

CODEX_OUTPUT=$(docker run --rm --network "$NETWORK" --entrypoint sh -e CODEX_POOL_API_KEY="$CLIENT_KEY" codex-pool:local -lc '
set -eu
mkdir -p /tmp/codex-home /tmp/work
cat >/tmp/codex-home/config.toml <<EOF
model = "gpt-test"
model_provider = "codex-pool"
[model_providers.codex-pool]
name = "Codex Pool device-auth failover test"
base_url = "http://codex-pool:8317/v1"
env_key = "CODEX_POOL_API_KEY"
wire_api = "responses"
EOF
CODEX_HOME=/tmp/codex-home codex exec --skip-git-repo-check --ephemeral --sandbox read-only -C /tmp/work "Reply with the exact token DEVICE_AUTH_FAILOVER_B and nothing else."
')
printf "%s\n" "$CODEX_OUTPUT"
printf "%s\n" "$CODEX_OUTPUT" | grep -F DEVICE_AUTH_FAILOVER_B >/dev/null

docker run --rm --network "$NETWORK" --entrypoint node codex-pool:local -e '
const data = await (await fetch("http://codex-mock:4010/requests")).json();
const device = data.events.filter((event) => event.kind === "sidecar");
const seen = device.map((event) => `${event.account}:${event.status}`).join(",");
if (seen !== "test-acct-a:429,test-acct-b:200") throw new Error(`device failover sequence ${seen}`);
'
docker run --rm -v "$VOLUME:/data:ro" --entrypoint node codex-pool:local -e '
import fs from "node:fs";
const state = JSON.parse(fs.readFileSync("/data/state/runtime.json", "utf8"));
if (!state.cooldowns["device-a"]?.some((entry) => entry.reason === "rate_limited")) throw new Error("device-a cooldown missing");
if (!Object.values(state.stickySessions).some((entry) => entry.accountId === "device-b")) throw new Error("sticky binding was not moved to device-b");
'

printf "%s\n" "Device-auth failover integration passed."
