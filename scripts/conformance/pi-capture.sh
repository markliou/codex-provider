#!/bin/sh
# Measures what a native Pi sends on the Pi channel (SPEC.md 2.3.6) and holds
# upstream/testdata/pi-reference.json to it.
#
# A real Pi of the release in upstream/src/pool/pi_channel.rs (PI_RELEASE) runs
# inside Docker against a recording upstream that answers for api.openai.com
# and auth.openai.com with a throwaway CA, twice: signed in with "Sign in with
# ChatGPT" (an expired stored token, so its refresh is measured too), and
# pointed at a custom base URL the way a colleague's Pi points at the pool.
# Nothing reaches OpenAI; no credential is used.
#
#   sh scripts/conformance/pi-capture.sh check    compare with the reference
#   sh scripts/conformance/pi-capture.sh update   rewrite the reference
#
# A difference is a change in Pi to read, not to paper over: match
# pi_channel.rs to it, then update the reference.
set -eu
MODE=${1:-check}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
HARNESS="$ROOT/scripts/conformance/pi-harness"
OUT="$ROOT/scripts/conformance/out/pi-capture"
RELEASE=$(sed -n 's/^pub const PI_RELEASE: &str = "\(.*\)";/\1/p' "$ROOT/upstream/src/pool/pi_channel.rs")
NETWORK=codex-pool-pi-capture-net
SERVER=codex-pool-pi-capture
[ -n "$RELEASE" ] || { echo "PI_RELEASE not found" >&2; exit 1; }

rm -rf "$OUT"
mkdir -p "$OUT/certs" "$OUT/rec" "$OUT/pi" "$OUT/home1/.pi/agent" "$OUT/home2/.pi/agent" "$OUT/work"
cleanup() {
  docker rm -f "$SERVER" >/dev/null 2>&1 || true
  docker network rm "$NETWORK" >/dev/null 2>&1 || true
}
trap cleanup EXIT

docker run --rm -v "$OUT/certs:/c" -w /c alpine:3.20 sh -c '
  apk add --no-cache openssl >/dev/null 2>&1
  openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=pi-capture-ca" -keyout ca.key -out ca.crt 2>/dev/null
  openssl req -newkey rsa:2048 -nodes -subj "/CN=api.openai.com" -keyout server.key -out server.csr 2>/dev/null
  printf "subjectAltName=DNS:api.openai.com,DNS:auth.openai.com\nbasicConstraints=CA:FALSE\n" > ext
  openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 1 -extfile ext -out server.crt 2>/dev/null
  chmod 644 /c/*'
docker run --rm -u "$(id -u):$(id -g)" -e HOME=/tmp -v "$OUT/pi:/w" -w /w node:22-alpine \
  sh -c "npm init -y >/dev/null && npm install --no-audit --no-fund @earendil-works/pi-coding-agent@$RELEASE >/dev/null 2>&1"

# Signed in: an expired token, so Pi refreshes before it sends.
printf '%s\n' '{"openai":{"type":"oauth","access":"capture-access","refresh":"capture-refresh","expires":0,"clientId":"capture-client","scopes":["openid","profile","email","offline_access","resource.invoke","chatgpt.tokens.use.direct"]}}' > "$OUT/home1/.pi/agent/auth.json"
# Via the pool: the built-in OpenAI provider at a custom base URL, a key.
printf '%s\n' '{"providers":{"openai":{"baseUrl":"http://api.openai.com:8080/v1"}}}' > "$OUT/home2/.pi/agent/models.json"

cleanup
docker network create "$NETWORK" >/dev/null
docker run -d --name "$SERVER" --network "$NETWORK" \
  --network-alias api.openai.com --network-alias auth.openai.com \
  -u "$(id -u):$(id -g)" -e OUT=/rec/capture.jsonl \
  -v "$HARNESS/capture.mjs:/capture.mjs:ro" -v "$OUT/certs:/certs:ro" -v "$OUT/rec:/rec" \
  node:22-alpine node /capture.mjs >/dev/null
sleep 2
run_pi() {
  docker run --rm --network "$NETWORK" -u "$(id -u):$(id -g)" -e HOME=/home/u \
    -e NODE_EXTRA_CA_CERTS=/certs/ca.crt \
    -v "$1:/home/u" -v "$OUT/work:/work" -v "$OUT/pi:/pi:ro" -v "$OUT/certs:/certs:ro" -w /work \
    node:22-alpine node /pi/node_modules/.bin/pi --offline --no-session --model openai/gpt-5.5 "$2" "$3" -p "say hi" >/dev/null
}
run_pi "$OUT/home1" --thinking medium
run_pi "$OUT/home2" --api-key pool-key-placeholder

docker run --rm -v "$HARNESS:/h:ro" -v "$OUT/rec:/rec:ro" node:22-alpine \
  node /h/reference.mjs /rec/capture.jsonl "$RELEASE" > "$OUT/pi-reference.json"

REFERENCE="$ROOT/upstream/testdata/pi-reference.json"
case "$MODE" in
  update)
    cp "$OUT/pi-reference.json" "$REFERENCE"
    echo "pi reference updated from Pi $RELEASE"
    ;;
  check)
    # The system prompt names the run's working directory and date; the rest
    # must match exactly.
    if docker run --rm -v "$OUT:/o:ro" -v "$ROOT/upstream/testdata:/t:ro" node:22-alpine node -e '
      const fs = require("fs");
      const strip = (r) => { for (const k of ["responses", "viaPool"]) r[k].body.value.input = r[k].body.value.input.map((i) => i.role === "developer" ? { role: "developer", content: "<system prompt>" } : i); return JSON.stringify(r); };
      const a = strip(JSON.parse(fs.readFileSync("/o/pi-reference.json", "utf8")));
      const b = strip(JSON.parse(fs.readFileSync("/t/pi-reference.json", "utf8")));
      process.exit(a === b ? 0 : 1);'; then
      echo "ok   Pi $RELEASE sends what pi-reference.json records"
    else
      echo "FAIL Pi $RELEASE differs from pi-reference.json; diff $OUT/pi-reference.json $REFERENCE" >&2
      exit 1
    fi
    ;;
  *) echo "usage: pi-capture.sh check|update" >&2; exit 2 ;;
esac
