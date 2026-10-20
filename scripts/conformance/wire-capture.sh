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
docker run --rm -v "$repo_root/scripts/conformance/harness:/h:ro" node:22-alpine node /h/mkauth.mjs > "$work/auth.json"
cp scripts/conformance/harness/run-codex.sh "$work/run-codex.sh"

echo "Building $image from the working tree"
CODEX_POOL_IMAGE=$image sh scripts/build-local-image.sh -q >/dev/null

docker network create --internal "$net" >/dev/null
docker volume create "$volume" >/dev/null
docker run --rm -v "$volume:/data" -v "$work:/w:ro" alpine:3.21 sh -ceu "
  mkdir -p /data/accounts/conformance/.codex
  printf '%s' '{\"defaultModel\":\"$model\",\"modelAliases\":{},\"accounts\":[{\"id\":\"conformance\",\"authType\":\"codex_device_auth\",\"enabled\":true,\"inPool\":true,\"priority\":100}]}' > /data/config.json
  cp /w/auth.json /data/accounts/conformance/.codex/auth.json
  chmod 700 /data/accounts/conformance/.codex
  chmod 600 /data/accounts/conformance/.codex/auth.json
  chown -R 999:999 /data"
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
# The pool only serves once codex-upstream and the account load are up; a
# Codex started before that spends its turn reconnecting and records nothing.
if ! docker run --rm --network "$net" alpine:3.21 sh -c '
  for i in $(seq 1 60); do wget -q -O /dev/null --header "Authorization: Bearer '"$CLIENT_KEY"'" http://conformance-pool:8317/healthz 2>/dev/null && exit 0; sleep 1; done; exit 1'; then
  docker logs "$pool" 2>&1 | tail -20
  echo "the pool did not become ready" >&2
  exit 1
fi

start_fake() {
  docker rm -f "$fake" >/dev/null 2>&1 || true
  docker run -d --name "$fake" --network "$net" -e REJECT_WS="$1" -e CAPTURE_LOG=/logs/capture.jsonl \
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
    -e CONFORMANCE_CLIENT_KEY="$CLIENT_KEY" alpine:3.21 sh /harness/run-codex.sh "$1" "$model" > "$work/$2.codex.log" 2>&1 || true
  cp "$work/logs/capture.jsonl" "$work/$2.jsonl"
}

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

docker logs "$pool" > "$work/pool.log" 2>&1 || true
status=0
docker run --rm -v "$work:/w:ro" -v "$repo_root/scripts/conformance/harness:/h:ro" node:22-alpine \
  node /h/compare-wire.mjs /w > "$work/report.txt" || status=$?
cat "$work/report.txt"
exit "$status"
