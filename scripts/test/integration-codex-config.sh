#!/bin/sh
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"

NETWORK=${NETWORK:-codex-provider-itest-$$}
POOL=${POOL:-codex-pool-itest-$$}
MOCK=${MOCK:-codex-mock-itest-$$}
ADMIN_PASSWORD=${ADMIN_PASSWORD:-test-admin-password}
CLIENT_KEY=${CLIENT_KEY:-test-client-key}
UPSTREAM_KEY=${UPSTREAM_KEY:-upstream-test-key}

cleanup() {
  docker rm -f "$POOL" "$MOCK" >/dev/null 2>&1 || true
  docker network rm "$NETWORK" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

docker build -t codex-pool:local .
docker build -t codex-mock-upstream:local -f test/Dockerfile.mock .

ADMIN_HASH=$(docker run --rm -e CODEX_POOL_ADMIN_PASSWORD="$ADMIN_PASSWORD" codex-pool:local hash-password)

docker network create "$NETWORK" >/dev/null
docker run -d --name "$MOCK" --network "$NETWORK" --network-alias codex-mock codex-mock-upstream:local >/dev/null
docker run -d \
  --name "$POOL" \
  --network "$NETWORK" \
  --network-alias codex-pool \
  --tmpfs /data:rw,noexec,nosuid,size=64m \
  -e CODEX_POOL_API_KEY="$CLIENT_KEY" \
  -e CODEX_POOL_ADMIN_PASSWORD_HASH="$ADMIN_HASH" \
  -e CODEX_POOL_DEFAULT_MODEL="gpt-test" \
  -e CODEX_POOL_UPSTREAM_BASE_URL="http://codex-mock:4010/v1" \
  -e CODEX_POOL_UPSTREAM_API_KEY="$UPSTREAM_KEY" \
  -e CODEX_POOL_ADMIN_ADDR="0.0.0.0:8318" \
  -e CODEX_POOL_ALLOW_REMOTE_ADMIN=true \
  codex-pool:local >/dev/null

docker run --rm --network "$NETWORK" --entrypoint node codex-pool:local -e '
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
for (let i = 0; i < 60; i += 1) {
  try {
    const health = await fetch("http://codex-pool:8317/healthz", { headers: { Authorization: "Bearer '"$CLIENT_KEY"'" } });
    if (health.ok) break;
  } catch (_) {}
  await wait(500);
}
const fail = (message) => { throw new Error(message); };
const apiRootNoKey = await fetch("http://codex-pool:8317/");
if (apiRootNoKey.status !== 404) fail(`public API root without key status ${apiRootNoKey.status}`);
const apiRoot = await fetch("http://codex-pool:8317/", { headers: { Authorization: "Bearer '"$CLIENT_KEY"'" } });
if (apiRoot.status !== 404) fail(`public API root with key status ${apiRoot.status}`);
const healthNoKey = await fetch("http://codex-pool:8317/healthz");
if (healthNoKey.status !== 401) fail(`public health without key status ${healthNoKey.status}`);
const health = await fetch("http://codex-pool:8317/healthz", { headers: { Authorization: "Bearer '"$CLIENT_KEY"'" } });
if (health.status !== 200) fail(`public health with key status ${health.status}`);
const adminRoot = await fetch("http://codex-pool:8318/", { redirect: "manual" });
if (adminRoot.status !== 200) fail(`admin root status ${adminRoot.status}`);
const adminRootBody = await adminRoot.text();
if (!adminRootBody.includes(`id="dashboard-view"`) || !adminRootBody.includes(`id="login-view"`)) fail("admin root did not serve dashboard shell");
const publicResp = await fetch("http://codex-pool:8318/admin/api/public-dashboard");
if (publicResp.status !== 200) fail(`public dashboard status ${publicResp.status}`);
const unauth = await fetch("http://codex-pool:8318/admin/api/accounts");
if (unauth.status !== 401) fail(`management API without auth status ${unauth.status}`);
const modelsNoKey = await fetch("http://codex-pool:8317/v1/models");
if (modelsNoKey.status !== 401) fail(`models without key status ${modelsNoKey.status}`);
const models = await fetch("http://codex-pool:8317/v1/models", { headers: { Authorization: "Bearer '"$CLIENT_KEY"'" } });
if (models.status !== 200) fail(`models with key status ${models.status}`);
'

CODEX_OUTPUT=$(docker run --rm --network "$NETWORK" --entrypoint sh -e CODEX_POOL_API_KEY="$CLIENT_KEY" codex-pool:local -lc '
set -eu
mkdir -p /tmp/codex-home /tmp/work
cat >/tmp/codex-home/config.toml <<EOF
model = "gpt-test"
model_provider = "codex-pool"

[model_providers.codex-pool]
name = "Codex Pool integration test"
base_url = "http://codex-pool:8317/v1"
env_key = "CODEX_POOL_API_KEY"
wire_api = "responses"
EOF
CODEX_HOME=/tmp/codex-home codex exec --skip-git-repo-check --ephemeral --sandbox read-only -C /tmp/work "Reply with the exact token CODEX_PROVIDER_CONFIG_TOML_OK and nothing else."
')

printf "%s\n" "$CODEX_OUTPUT"
printf "%s\n" "$CODEX_OUTPUT" | grep -F "CODEX_PROVIDER_CONFIG_TOML_OK" >/dev/null

# model/list must pass through the bundled Codex decoder and surface gpt-test,
# which does not exist in its fallback catalog. A loose JSON-only check would
# miss required fields and allow the startup refresh loop to regress.
set +e
APP_SERVER_OUTPUT=$(docker run --rm --network "$NETWORK" --entrypoint sh -e CODEX_POOL_API_KEY="$CLIENT_KEY" codex-pool:local -lc '
set -eu
mkdir -p /tmp/codex-app-home
cat >/tmp/codex-app-home/config.toml <<EOF
model = "gpt-test"
model_provider = "codex-pool"

[model_providers.codex-pool]
name = "Codex Pool app-server integration test"
base_url = "http://codex-pool:8317/v1"
wire_api = "responses"
auth = { command = "/bin/echo", args = ["$CODEX_POOL_API_KEY"] }
EOF
{
  printf "%s\n" '\''{"method":"initialize","id":0,"params":{"clientInfo":{"name":"codex_pool_integration","title":"Codex Pool Integration","version":"1.0.0"}}}'\''
  printf "%s\n" '\''{"method":"initialized","params":{}}'\''
  printf "%s\n" '\''{"method":"model/list","id":1,"params":{}}'\''
  sleep 8
} | CODEX_HOME=/tmp/codex-app-home timeout 10 codex app-server 2>&1
')
APP_SERVER_STATUS=$?
set -e

printf "%s\n" "$APP_SERVER_OUTPUT"
if [ "$APP_SERVER_STATUS" -ne 0 ] && [ "$APP_SERVER_STATUS" -ne 124 ]; then
  printf "%s\n" "Codex app-server exited with status $APP_SERVER_STATUS." >&2
  exit "$APP_SERVER_STATUS"
fi
printf "%s\n" "$APP_SERVER_OUTPUT" | grep -F '"id":1' >/dev/null
printf "%s\n" "$APP_SERVER_OUTPUT" | grep -F '"model":"gpt-test"' >/dev/null
if printf "%s\n" "$APP_SERVER_OUTPUT" | grep -F "failed to decode models response" >/dev/null; then
  printf "%s\n" "Codex app-server rejected the provider model catalog." >&2
  exit 1
fi
printf "%s\n" "Codex config.toml integration passed."
