# Runs one Codex turn for wire-capture.sh inside an Alpine container.
#   sh run-codex.sh native <model>        Codex signed in with ChatGPT, talking to
#                                         the fake chatgpt.com directly
#   sh run-codex.sh pool-http <model>     Codex pointed at the pool over HTTP
#   sh run-codex.sh pool-ws <model>       Codex pointed at the pool with
#                                         supports_websockets = true
#   sh run-codex.sh native-refresh <model> as native, with an access token about to
#                                         expire, so Codex refreshes it first
#   sh run-codex.sh native-pool <model>   Codex's built-in OpenAI provider with
#                                         ChatGPT auth, its base URL moved to the
#                                         pool's native endpoint (SPEC.md 2.3.3)
#   sh run-codex.sh client-http <model>   the pool's provider config, pointed
#   sh run-codex.sh client-ws <model>     straight at the fake: what Codex sends
#                                         the pool before the pool touches it
# The Codex binary is mounted at /codex, the capture CA at /certs and the
# fake auth.json at /harness/auth.json. CONFORMANCE_PROMPT replaces the prompt
# and CONFORMANCE_EXTRA is TOML appended to config.toml (a scenario's own
# settings, such as multi-agent).
set -eu
target=$1
model=$2
export CODEX_HOME=/tmp/codex-home PATH=/codex/codex-path:$PATH
mkdir -p "$CODEX_HOME" /tmp/work
cd /tmp/work
case "$target" in
  native|native-refresh|native-pool)
    cat /certs/ca.pem >> /etc/ssl/certs/ca-certificates.crt
    export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt CODEX_CA_CERTIFICATE=/certs/ca.pem
    if [ "$target" = native-refresh ]; then
      cp /harness/auth-expiring.json "$CODEX_HOME/auth.json"
    else
      cp /harness/auth.json "$CODEX_HOME/auth.json"
    fi
    cat > "$CODEX_HOME/config.toml" <<CFG
model = "$model"
model_reasoning_effort = "xhigh"
CFG
    if [ "$target" = native-pool ]; then
      echo "openai_base_url = \"http://conformance-pool:8317/k/$CONFORMANCE_CLIENT_KEY/backend-api/codex\"" >> "$CODEX_HOME/config.toml"
    fi
    printf '%s\n' "${CONFORMANCE_EXTRA:-}" >> "$CODEX_HOME/config.toml"
    ;;
  pool-http|pool-ws|client-http|client-ws)
    websockets=false
    base_url=http://conformance-pool:8317/v1
    case "$target" in *-ws) websockets=true ;; esac
    case "$target" in
      client-*)
        cat /certs/ca.pem >> /etc/ssl/certs/ca-certificates.crt
        export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt CODEX_CA_CERTIFICATE=/certs/ca.pem
        # Not chatgpt.com: Codex adds internal metadata for that host alone
        # (model-provider/src/provider.rs include_internal_metadata), so a
        # baseline aimed there would not be what Codex sends the pool.
        base_url=https://conformance-direct/backend-api/codex
        ;;
    esac
    cat > "$CODEX_HOME/config.toml" <<CFG
model_provider = "pool"
model = "$model"
model_reasoning_effort = "xhigh"
[model_providers.pool]
name = "Codex API Service"
base_url = "$base_url"
wire_api = "responses"
requires_openai_auth = false
experimental_bearer_token = "$CONFORMANCE_CLIENT_KEY"
supports_websockets = $websockets
CFG
    ;;
  *)
    echo "unknown target $target" >&2
    exit 2
    ;;
esac
timeout 180 /codex/bin/codex exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox "${CONFORMANCE_PROMPT:-run echo hi}" 2>&1 | tail -5
