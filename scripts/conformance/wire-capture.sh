#!/usr/bin/env sh
# Runs the same Codex turn twice against a fake chatgpt.com — once from a
# native Codex CLI signed in with ChatGPT, once from Codex behind a pool built
# from this working tree — over HTTP and over the Responses WebSocket, and
# compares what reached the fake upstream: TLS ClientHello (JA4) and ALPN,
# HTTP/2 SETTINGS, request order, header names, order and values, body
# encoding and body. A third run sends the pool's Codex provider config
# straight to the fake, so the report separates what the pool changes (exit
# non-zero) from what Codex itself sends differently to a custom provider
# (reported as the client gap; see SPEC.md 2.3.2).
#
#   sh scripts/conformance/wire-capture.sh
#
# The native Codex is the npm release matching the Codex tag pinned in
# upstream/Cargo.toml. Everything runs in Docker on an internal network with no
# route out; the only credential is a fake, unsigned one. Results land in
# scripts/conformance/out/wire-<time>/ (git-ignored).
#
# Run it after any change to how the pool talks to upstream, and on every Codex
# release bump (see AGENTS.md "Codex Wire Conformance").
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$repo_root"

pinned=$(sed -n 's/^codex-http-client = .*tag = "\([^"]*\)".*/\1/p' upstream/Cargo.toml)
codex_version=${pinned#rust-v}
model=${CONFORMANCE_MODEL:-gpt-5.5}
netshoot=nicolaka/netshoot:v0.13
stamp=$(date -u +%Y%m%dT%H%M%SZ)
out="$repo_root/scripts/conformance/out"
work="$out/wire-$stamp"
codex_dir="$out/codex-$codex_version"
suffix=$$
net="codex-conformance-$suffix"
fake="codex-conformance-fake-$suffix"
pool="codex-conformance-pool-$suffix"
sniff="codex-conformance-sniff-$suffix"
volume="codex-conformance-data-$suffix"
image="codex-pool:conformance"
# Throwaway values for the isolated capture network; run-codex.sh carries the
# same client key in the Codex provider config.
CLIENT_KEY=${CONFORMANCE_CLIENT_KEY:-conformance-client-key}
ADMIN_PASSWORD=${CONFORMANCE_ADMIN_PASSWORD:-conformance-admin}
mkdir -p "$work/logs" "$work/certs" "$work/pcap"
chmod 777 "$work/logs" "$work/pcap"

cleanup() {
  docker rm -f "$fake" "$pool" "$sniff" >/dev/null 2>&1 || true
  docker network rm "$net" >/dev/null 2>&1 || true
  docker volume rm "$volume" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

echo "Native Codex $codex_version vs this tree's pool, model $model -> $work"

if [ ! -x "$codex_dir/package/vendor/x86_64-unknown-linux-musl/bin/codex" ]; then
  echo "Fetching @openai/codex@$codex_version-linux-x64"
  mkdir -p "$codex_dir"
  docker run --rm -v "$codex_dir:/out" -w /out node:22-alpine sh -ceu "
    npm pack --silent @openai/codex@$codex_version-linux-x64 >/dev/null
    tar -xzf ./*.tgz && rm -f ./*.tgz"
fi
codex_vendor="$codex_dir/package/vendor/x86_64-unknown-linux-musl"

docker run --rm -v "$work/certs:/c" alpine:3.21 sh -ceu '
  apk add --no-cache openssl >/dev/null
  cd /c
  openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.pem -days 2 -subj "/CN=conformance-ca" 2>/dev/null
  printf "[req]\ndistinguished_name=dn\n[dn]\n[ext]\nsubjectAltName=DNS:chatgpt.com,DNS:*.chatgpt.com,DNS:conformance-direct,DNS:auth.openai.com,DNS:api.openai.com,DNS:*.openai.com\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n" > san.cnf
  openssl req -newkey rsa:2048 -nodes -keyout server.key -out server.csr -subj "/CN=chatgpt.com" 2>/dev/null
  openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out server.pem -days 2 -extfile san.cnf -extensions ext 2>/dev/null
  chmod 644 ./*'
for mode in current expiring; do
  docker run --rm -v "$repo_root/scripts/conformance/harness:/h:ro" node:22-alpine node /h/mkauth.mjs "$mode" > "$work/auth-$mode.json"
done
cp "$work/auth-current.json" "$work/auth.json"
cp scripts/conformance/harness/run-codex.sh "$work/run-codex.sh"

echo "Building $image from the working tree"
CODEX_POOL_IMAGE=$image sh scripts/build-local-image.sh -q >/dev/null

docker network create --internal "$net" >/dev/null
docker volume create "$volume" >/dev/null
docker run --rm -v "$volume:/data" -v "$work:/w:ro" alpine:3.21 sh -ceu "
  mkdir -p /data/accounts/conformance/.codex
  printf '%s' '{\"defaultModel\":\"$model\",\"modelAliases\":{},\"accounts\":[{\"id\":\"conformance\",\"authType\":\"codex_device_auth\",\"enabled\":true,\"inPool\":true,\"priority\":100}]}' > /data/config.json
  # About to expire, so the pool's first use of it is a token refresh.
  cp /w/auth-expiring.json /data/accounts/conformance/.codex/auth.json
  chmod 700 /data/accounts/conformance/.codex
  chmod 600 /data/accounts/conformance/.codex/auth.json
  chown -R 999:999 /data"
# The pool only serves once codex-upstream and the account load are up; a
# Codex started before that spends its turn reconnecting and records nothing.
wait_for_pool() {
  if ! docker run --rm --network "$net" alpine:3.21 sh -c '
    for i in $(seq 1 60); do wget -q -O /dev/null --header "Authorization: Bearer '"$CLIENT_KEY"'" http://conformance-pool:8317/healthz 2>/dev/null && exit 0; sleep 1; done; exit 1'; then
    docker logs "$pool" 2>&1 | tail -20
    echo "the pool did not become ready" >&2
    exit 1
  fi
}

start_fake() {
  docker rm -f "$fake" >/dev/null 2>&1 || true
  docker run -d --name "$fake" --network "$net" -e REJECT_WS="$1" -e PRIME_WINDOW="${PRIME_WINDOW:-0}" \
    -e SCENARIO="${SCENARIO:-}" -e FAIL_FROM="${FAIL_FROM:-}" -e CAPTURE_LOG=/logs/capture.jsonl \
    --network-alias chatgpt.com --network-alias auth.openai.com --network-alias api.openai.com --network-alias ab.chatgpt.com \
    --network-alias conformance-direct \
    -v "$repo_root/scripts/conformance/harness:/srv:ro" -v "$work/certs:/certs:ro" -v "$work/logs:/logs" \
    node:22-alpine node /srv/fake-upstream.mjs >/dev/null
  docker run -d --name "$sniff" --net "container:$fake" -v "$work/pcap:/pcap" "$netshoot" \
    tcpdump -i any -w "/pcap/$2.pcap" 'tcp port 443' >/dev/null
  sleep 3
}

stop_fake() {
  sleep 1
  docker stop "$sniff" >/dev/null
  docker rm "$sniff" >/dev/null
  docker run --rm -v "$work/pcap:/pcap" "$netshoot" tshark -r "/pcap/$1.pcap" -Y 'tls.handshake.type == 1' \
    -T fields -e ip.src -e tls.handshake.extensions_server_name -e tls.handshake.ja4 -e tls.handshake.extensions_alpn_str 2>/dev/null > "$work/tls-$1.txt"
}

run_turn() {
  : > "$work/logs/capture.jsonl"
  docker run --rm --network "$net" -v "$work:/harness:ro" -v "$work/certs:/certs:ro" -v "$codex_vendor:/codex:ro" \
    -e CONFORMANCE_CLIENT_KEY="$CLIENT_KEY" -e CONFORMANCE_PROMPT="${CONFORMANCE_PROMPT:-}" -e CONFORMANCE_EXTRA="${CONFORMANCE_EXTRA:-}" \
    alpine:3.21 sh /harness/run-codex.sh "$1" "$model" > "$work/$2.codex.log" 2>&1 || true
  cp "$work/logs/capture.jsonl" "$work/$2.jsonl"
}

# A scenario phase: the same turn from a native Codex and from Codex on the
# pool's native endpoint, against a fake answering in that scenario.
#   scenario_phase <label> <scenario> <reject-ws>
scenario_phase() {
  SCENARIO=$2 start_fake "$3" "$1"
  SCENARIO=$2 run_turn native "native-$1"
  docker run --rm --network "$net" alpine:3.21 wget -q --no-check-certificate -O /dev/null https://chatgpt.com/__conformance/reset || true
  SCENARIO=$2 run_turn native-pool "native-pool-$1"
  stop_fake "$1"
}

# Each failover phase starts with both pool accounts usable: the account an
# earlier phase refused as spent is rightly still cooling down, and would
# leave the next phase with only the account it refuses.
clear_pool_cooldowns() {
  docker stop "$pool" >/dev/null
  docker run --rm -v "$volume:/data" node:22-alpine node -e '
    const fs = require("fs"); const file = "/data/state/runtime.json";
    if (fs.existsSync(file)) { const s = JSON.parse(fs.readFileSync(file, "utf8")); s.cooldowns = {}; s.health = {}; fs.writeFileSync(file, JSON.stringify(s)); }'
  docker start "$pool" >/dev/null
  docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$pool" > "$work/pool-ip.txt"
  wait_for_pool
}

echo "Own calls: the pool starts (token refresh, usage, account check, reset credits), then a native Codex refreshes"
start_fake 0 own
: > "$work/logs/capture.jsonl"
admin_hash=$(docker run --rm -e CODEX_POOL_ADMIN_PASSWORD="$ADMIN_PASSWORD" "$image" hash-password)
# The prompt-cache passthrough settings mirror the production deployment, so
# the pool forwards Codex's prompt_cache_key and retention as Codex sent them.
docker run -d --name "$pool" --network "$net" --network-alias conformance-pool \
  -v "$volume:/data" -v "$work/certs:/certs:ro" \
  -e SSL_CERT_FILE=/certs/ca.pem -e CODEX_CA_CERTIFICATE=/certs/ca.pem \
  -e CODEX_POOL_API_KEY="$CLIENT_KEY" -e CODEX_POOL_ADMIN_PASSWORD_HASH="$admin_hash" \
  -e CODEX_POOL_ADDR=0.0.0.0:8317 -e CODEX_POOL_ALLOW_REMOTE_ADMIN=true \
  -e CODEX_POOL_PROMPT_CACHE_RETENTION="${CONFORMANCE_PROMPT_CACHE_RETENTION:-passthrough}" \
  -e CODEX_POOL_PROMPT_CACHE_KEY_MODE="${CONFORMANCE_PROMPT_CACHE_KEY_MODE:-passthrough}" \
  "$image" >/dev/null
docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$pool" > "$work/pool-ip.txt"
wait_for_pool
# The pool's first quota sweep runs at start; give it time to finish.
sleep 10
docker run --rm --network "$net" -v "$work:/harness:ro" -v "$work/certs:/certs:ro" -v "$codex_vendor:/codex:ro" \
  -e CONFORMANCE_CLIENT_KEY="$CLIENT_KEY" alpine:3.21 sh /harness/run-codex.sh native-refresh "$model" > "$work/native-own.codex.log" 2>&1 || true
cp "$work/logs/capture.jsonl" "$work/own.jsonl"
stop_fake own

echo "Prime: the pool primes an unanchored window by running the bundled Codex CLI"
PRIME_WINDOW=1 start_fake 0 prime
: > "$work/logs/capture.jsonl"
docker restart "$pool" >/dev/null
wait_for_pool
# The prime is one Codex turn; give it time to start, answer and exit.
sleep 30
cp "$work/logs/capture.jsonl" "$work/prime.jsonl"
docker logs "$pool" 2>&1 | grep -i 'quota window prime' | tail -3 > "$work/prime.log" || true
stop_fake prime

echo "HTTP: native Codex, then Codex through the pool"
start_fake 1 http
run_turn native native-http
run_turn client-http client-http
run_turn pool-http pool-http
run_turn native-pool native-pool-http
stop_fake http

echo "WebSocket: native Codex, then Codex through the pool"
start_fake 0 ws
run_turn native native-ws
run_turn client-ws client-ws
run_turn pool-ws pool-ws
run_turn native-pool native-pool-ws
stop_fake ws

echo "Subagent: a parent spawns one child, over HTTP and over the WebSocket"
subagent_config='[features.multi_agent_v2]
enabled = true
max_concurrent_threads_per_session = 3
non_code_mode_only = false'
CONFORMANCE_EXTRA="$subagent_config" CONFORMANCE_PROMPT="Use the spawn_agent tool exactly once, wait for the child, then reply with PARENT_OK." \
  scenario_phase subagent-http subagent 1
CONFORMANCE_EXTRA="$subagent_config" CONFORMANCE_PROMPT="Use the spawn_agent tool exactly once, wait for the child, then reply with PARENT_OK." \
  scenario_phase subagent-ws subagent 0

echo "Compaction: a low auto-compact limit makes Codex compact mid-turn"
CONFORMANCE_EXTRA='model_auto_compact_token_limit = 20000' scenario_phase compact compact 1

echo "Failover: the pool's first account is refused as spent and the request moves to its second"
docker stop "$pool" >/dev/null
docker run --rm -v "$repo_root/scripts/conformance/harness:/h:ro" node:22-alpine node /h/mkauth.mjs current acct-second > "$work/auth-second.json"
docker run --rm -v "$volume:/data" -v "$work:/w:ro" node:22-alpine sh -ceu '
  mkdir -p /data/accounts/conformance-second/.codex
  cp /w/auth-second.json /data/accounts/conformance-second/.codex/auth.json
  chmod 700 /data/accounts/conformance-second/.codex
  chmod 600 /data/accounts/conformance-second/.codex/auth.json
  node -e "const fs=require(\"fs\");const c=JSON.parse(fs.readFileSync(\"/data/config.json\",\"utf8\"));c.accounts.push({id:\"conformance-second\",authType:\"codex_device_auth\",enabled:true,inPool:true,priority:100});fs.writeFileSync(\"/data/config.json\",JSON.stringify(c))"
  chown -R 999:999 /data'
docker start "$pool" >/dev/null
docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$pool" > "$work/pool-ip.txt"
wait_for_pool
for transport in http ws; do
  reject=1
  [ "$transport" = ws ] && reject=0
  clear_pool_cooldowns
  FAIL_FROM=$(cat "$work/pool-ip.txt") scenario_phase "failover-$transport" failover "$reject"
done

docker logs "$pool" > "$work/pool.log" 2>&1 || true
status=0
docker run --rm -v "$work:/w:ro" -v "$repo_root/scripts/conformance/harness:/h:ro" node:22-alpine \
  node /h/compare-wire.mjs /w > "$work/report.txt" || status=$?
cat "$work/report.txt"
exit "$status"
