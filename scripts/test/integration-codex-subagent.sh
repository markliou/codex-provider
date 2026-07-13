#!/bin/sh
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"

NETWORK=${NETWORK:-codex-subagent-itest-$$}
POOL=${POOL:-codex-subagent-pool-itest-$$}
MOCK=${MOCK:-codex-subagent-mock-itest-$$}
VOLUME=${VOLUME:-codex-subagent-data-itest-$$}
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
  cat >/data/config.json <<"EOF"
{"defaultModel":"gpt-test","modelAliases":{},"accounts":[
  {"id":"preferred-account","label":"Preferred test account","authType":"provider_api_key","enabled":true,"inPool":true,"priority":100,"upstreamBaseUrl":"http://codex-mock:4010/v1","upstreamApiKey":"upstream-preferred-key","wireApi":"responses"},
  {"id":"parent-account","label":"Parent test account","authType":"provider_api_key","enabled":true,"inPool":true,"priority":10,"upstreamBaseUrl":"http://codex-mock:4010/v1","upstreamApiKey":"upstream-parent-key","wireApi":"responses"}
]}
EOF
'

docker run -d \
  --name "$MOCK" \
  --network "$NETWORK" \
  --network-alias codex-mock \
  -e CODEX_MOCK_SUBAGENT_INTEGRATION=true \
  codex-mock-upstream:local >/dev/null
docker run -d \
  --name "$POOL" \
  --network "$NETWORK" \
  --network-alias codex-pool \
  -v "$VOLUME:/data" \
  -e CODEX_POOL_API_KEY="$CLIENT_KEY" \
  -e CODEX_POOL_ADMIN_PASSWORD_HASH="$ADMIN_HASH" \
  -e CODEX_POOL_DEFAULT_MODEL=gpt-test \
  -e CODEX_POOL_MAX_RETRY_ACCOUNTS=2 \
  -e CODEX_POOL_PROMPT_CACHE_KEY_POLICY=preserve \
  -e CODEX_POOL_ADMIN_ADDR=0.0.0.0:8318 \
  -e CODEX_POOL_ALLOW_REMOTE_ADMIN=true \
  codex-pool:local >/dev/null

docker run --rm --network "$NETWORK" --entrypoint node codex-pool:local -e '
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
for (let i = 0; i < 80; i += 1) {
  try {
    const response = await fetch("http://codex-pool:8317/healthz", {
      headers: { Authorization: "Bearer '"$CLIENT_KEY"'" },
      signal: AbortSignal.timeout(1000),
    });
    if (response.ok) process.exit(0);
  } catch (_) {}
  await wait(250);
}
throw new Error("pool did not become healthy");
'

CODEX_OUTPUT=$(docker run --rm \
  --network "$NETWORK" \
  --entrypoint sh \
  -e CODEX_POOL_API_KEY="$CLIENT_KEY" \
  codex-pool:local -lc '
set -eu
mkdir -p /tmp/codex-home /tmp/work
cat >/tmp/codex-home/config.toml <<EOF
model = "gpt-test"
model_provider = "codex-pool"

[model_providers.codex-pool]
name = "Codex Pool subagent integration test"
base_url = "http://codex-pool:8317/v1"
env_key = "CODEX_POOL_API_KEY"
wire_api = "responses"

[features.multi_agent_v2]
enabled = true
max_concurrent_threads_per_session = 3
non_code_mode_only = false
EOF
CODEX_HOME=/tmp/codex-home timeout 90 codex exec \
  --skip-git-repo-check \
  --ephemeral \
  --sandbox read-only \
  -C /tmp/work \
  "Use the spawn_agent tool exactly once, wait for the child, then reply with the exact token SUBAGENT_PARENT_OK and nothing else."
')
printf "%s\n" "$CODEX_OUTPUT"
printf "%s\n" "$CODEX_OUTPUT" | grep -Fx SUBAGENT_PARENT_OK >/dev/null

docker run --rm --network "$NETWORK" --entrypoint node codex-pool:local -e '
const data = await (await fetch("http://codex-mock:4010/requests")).json();
const events = data.events.filter((event) => event.kind === "subagent-integration");
const failedPreferred = events.find((event) =>
  event.role === "parent" && event.account === "preferred-account" && event.status === 429);
const parentSpawn = events.find((event) =>
  event.role === "parent" && event.account === "parent-account" && event.spawnSchemaHasForkTurns === true);
const child = events.find((event) => event.role === "child");
const parentFollowup = events.find((event) =>
  event.role === "parent" && event.account === "parent-account" && event.sawSpawnOutput === true);
if (!failedPreferred) throw new Error("preferred-account failover event missing: " + JSON.stringify(events));
if (!parentSpawn) throw new Error("parent spawn event or fork_turns schema missing: " + JSON.stringify(events));
if (!child) throw new Error("child request missing: " + JSON.stringify(events));
if (child.account !== "parent-account") throw new Error("child ignored parent affinity: " + JSON.stringify(child));
if (!parentFollowup) throw new Error("parent did not receive the unchanged spawn output: " + JSON.stringify(events));
let spawnOutput;
try {
  spawnOutput = JSON.parse(parentFollowup.spawnOutput);
} catch (_) {
  throw new Error("parent spawn output was not preserved JSON: " + JSON.stringify(parentFollowup));
}
if (spawnOutput.task_name !== "/root/child_probe") {
  throw new Error("parent spawn output changed task identity: " + JSON.stringify(parentFollowup));
}
if (!parentSpawn.threadId || !child.threadId || child.threadId === parentSpawn.threadId) {
  throw new Error("main/child thread identities are not independent: " + JSON.stringify(events));
}
if (child.parentThreadId !== parentSpawn.threadId) {
  throw new Error("child parent thread " + child.parentThreadId + " does not match main " + parentSpawn.threadId);
}
if (!parentSpawn.promptCacheKey || !child.promptCacheKey || child.promptCacheKey === parentSpawn.promptCacheKey) {
  throw new Error("preserve policy did not retain independent native cache keys: " + JSON.stringify(events));
}
'

docker run --rm -v "$VOLUME:/data:ro" --entrypoint node codex-pool:local -e '
import fs from "node:fs";
const state = JSON.parse(fs.readFileSync("/data/state/runtime.json", "utf8"));
const bindings = Object.values(state.threadBindings ?? {});
const main = bindings.find((entry) => !entry.parentThreadId);
const child = bindings.find((entry) => entry.parentThreadId === main?.threadId);
if (!main || !child) throw new Error("main/child thread bindings missing: " + JSON.stringify(bindings));
if (main.stickyKey === child.stickyKey) throw new Error("main/child sticky keys collapsed: " + main.stickyKey);
if (main.accountId !== "parent-account" || child.accountId !== main.accountId) {
  throw new Error("parent affinity was not persisted: " + JSON.stringify(bindings));
}
if (child.lineageRootId !== main.threadId) {
  throw new Error("child lineage root " + child.lineageRootId + " does not match main " + main.threadId);
}
const responses = state.responseBindings ?? {};
if (responses.resp_subagent_child?.stickyKey !== child.stickyKey) {
  throw new Error("child response chain binding missing: " + JSON.stringify(responses));
}
if (responses.resp_subagent_parent?.stickyKey !== main.stickyKey) {
  throw new Error("parent response chain binding missing: " + JSON.stringify(responses));
}
const subagentMetric = Object.values(state.promptCache ?? {}).find((entry) =>
  entry.accountId === "parent-account" && entry.agentKind === "subagent");
if (!subagentMetric || subagentMetric.parentAffinityHitCount < 1) {
  throw new Error("subagent parent-affinity metric missing: " + JSON.stringify(state.promptCache));
}
'

printf "%s\n" "Codex subagent routing integration passed."
