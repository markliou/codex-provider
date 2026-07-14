#!/bin/sh
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"

NETWORK=${NETWORK:-codex-routing-cache-itest-$$}
POOL=${POOL:-codex-routing-cache-pool-itest-$$}
MOCK=${MOCK:-codex-routing-cache-mock-itest-$$}
VOLUME=${VOLUME:-codex-routing-cache-data-itest-$$}
POOL_IMAGE=${POOL_IMAGE:-codex-pool:issue4-itest}
MOCK_IMAGE=${MOCK_IMAGE:-codex-mock-upstream:issue4-itest}
CLIENT_KEY=${CLIENT_KEY:-routing-cache-client-key}
ADMIN_PASSWORD=${ADMIN_PASSWORD:-routing-cache-admin-password}

cleanup() {
  docker rm -f "$POOL" "$MOCK" >/dev/null 2>&1 || true
  docker network rm "$NETWORK" >/dev/null 2>&1 || true
  docker volume rm "$VOLUME" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

# Keep integration tags separate from codex-pool:local: the latter is the live
# provider image used by this development session and must not be retagged by a
# test build.
docker build -t "$POOL_IMAGE" .
docker build -t "$MOCK_IMAGE" -f test/Dockerfile.mock .
ADMIN_HASH=$(docker run --rm -e CODEX_POOL_ADMIN_PASSWORD="$ADMIN_PASSWORD" "$POOL_IMAGE" hash-password)

docker network create --internal "$NETWORK" >/dev/null
docker volume create "$VOLUME" >/dev/null
docker run --rm --entrypoint sh -v "$VOLUME:/data" "$POOL_IMAGE" -ceu '
  cat >/data/config.json <<"EOF"
{"defaultModel":"gpt-test","modelAliases":{},"accounts":[
  {"id":"routing-primary","label":"Primary","authType":"provider_api_key","enabled":true,"inPool":true,"priority":100,"upstreamBaseUrl":"http://routing-cache-mock:4010/v1","upstreamApiKey":"routing-primary-key"},
  {"id":"routing-secondary","label":"Secondary","authType":"provider_api_key","enabled":true,"inPool":true,"priority":10,"upstreamBaseUrl":"http://routing-cache-mock:4010/v1","upstreamApiKey":"routing-secondary-key"}
]}
EOF
  chmod 600 /data/config.json
'

docker run -d --name "$MOCK" --network "$NETWORK" --network-alias routing-cache-mock \
  -e CODEX_MOCK_ROUTING_CACHE_INTEGRATION=true \
  "$MOCK_IMAGE" >/dev/null
docker run -d --name "$POOL" --network "$NETWORK" --network-alias codex-pool -v "$VOLUME:/data" \
  -e CODEX_POOL_API_KEY="$CLIENT_KEY" \
  -e CODEX_POOL_ADMIN_PASSWORD_HASH="$ADMIN_HASH" \
  -e CODEX_POOL_DEFAULT_MODEL=gpt-test \
  -e CODEX_POOL_ROUTING_STRATEGY=sticky_failover \
  -e CODEX_POOL_ADMIN_ADDR=0.0.0.0:8318 \
  -e CODEX_POOL_ALLOW_REMOTE_ADMIN=true \
  "$POOL_IMAGE" >/dev/null

docker run --rm --network "$NETWORK" --entrypoint node "$POOL_IMAGE" -e '
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
for (let i = 0; i < 60; i += 1) {
  try {
    const response = await fetch("http://codex-pool:8317/healthz", { headers: { Authorization: "Bearer '"$CLIENT_KEY"'" } });
    if (response.ok) break;
  } catch (_) {}
  await wait(250);
}

const headers = {
  Authorization: "Bearer '"$CLIENT_KEY"'",
  "Content-Type": "application/json",
  "X-Codex-Pool-Session": "integration-raw-sticky-session",
};
for (let index = 0; index < 3; index += 1) {
  const response = await fetch("http://codex-pool:8317/v1/responses", {
    method: "POST",
    headers,
    body: JSON.stringify({ model: "gpt-test", input: `request-${index}` }),
  });
  if (!response.ok) throw new Error(`proxy request ${index} returned ${response.status}: ${await response.text()}`);
}

const publicDashboard = await (await fetch("http://codex-pool:8318/admin/api/public-dashboard")).json();
if (Object.prototype.hasOwnProperty.call(publicDashboard.dashboard || {}, "routingCacheEvents")) {
  throw new Error("public dashboard exposed request-level routing events");
}
const loginResponse = await fetch("http://codex-pool:8318/admin/api/login", {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({ password: "'"$ADMIN_PASSWORD"'" }),
});
if (!loginResponse.ok) throw new Error(`admin login returned ${loginResponse.status}: ${await loginResponse.text()}`);
const cookie = String(loginResponse.headers.get("set-cookie") || "").split(";")[0];
if (!cookie) throw new Error("admin login omitted session cookie");
const stateResponse = await fetch("http://codex-pool:8318/admin/api/state", { headers: { Cookie: cookie } });
if (!stateResponse.ok) throw new Error(`admin state returned ${stateResponse.status}: ${await stateResponse.text()}`);
const dashboard = (await stateResponse.json()).state || {};
const events = dashboard.routingCacheEvents || [];
const outcomes = events.slice(0, 3).map((event) => event.routingOutcome).join(",");
if (outcomes !== "rate_limit_failover,sticky_reuse,new_route_assignment") {
  throw new Error(`routing outcomes ${outcomes}`);
}
const failover = events[0];
if (failover.accountLabel !== "Secondary" || failover.failoverFromAccountLabel !== "Primary") {
  throw new Error(`failover accounts ${JSON.stringify(failover)}`);
}
if (failover.cacheWriteTokens !== 1800 || failover.cachedTokens !== 0 || !failover.coldCacheEligible) {
  throw new Error(`failover cache result ${JSON.stringify(failover)}`);
}
const sticky = events[1];
if (sticky.cacheReadRate !== 0.75 || sticky.cacheWriteTokens !== 100) {
  throw new Error(`sticky cache result ${JSON.stringify(sticky)}`);
}
const serialized = JSON.stringify(dashboard.routingCacheEvents);
for (const raw of ["routing-primary", "routing-secondary", "integration-raw-sticky-session"]) {
  if (serialized.includes(raw)) throw new Error(`public dashboard leaked ${raw}`);
}
'

docker run --rm --network "$NETWORK" --entrypoint node "$POOL_IMAGE" -e '
const data = await (await fetch("http://routing-cache-mock:4010/requests")).json();
const seen = data.events
  .filter((event) => event.kind === "routing-cache")
  .map((event) => `${event.account}:${event.status}`)
  .join(",");
if (seen !== "primary:200,primary:200,primary:429,secondary:200") {
  throw new Error(`upstream routing sequence ${seen}`);
}
'

docker run --rm -v "$VOLUME:/data:ro" --entrypoint node "$POOL_IMAGE" -e '
import fs from "node:fs";
const state = JSON.parse(fs.readFileSync("/data/state/runtime.json", "utf8"));
if (state.routingCacheEvents?.length !== 3) throw new Error(`persisted event count ${state.routingCacheEvents?.length}`);
const encoded = JSON.stringify(state.routingCacheEvents);
for (const raw of ["integration-raw-sticky-session", "request-0", "request-1", "request-2"]) {
  if (encoded.includes(raw)) throw new Error(`persisted event leaked ${raw}`);
}
'

printf "%s\n" "Routing/cache observability integration passed."
