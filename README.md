# Codex Pool Provider

Dockerized, single-user Codex/ChatGPT account-pool service. The service exposes one OpenAI-compatible `/v1` endpoint to Codex clients, while internally routing requests across Codex accounts authenticated with ChatGPT device auth.

No runtime, package manager, test tool, or service dependency is installed on the host. Build, password hashing, testing, and execution all occur in Docker.

## Build

```bash
docker build -t codex-pool:local .
```

Generate an admin password hash inside the image:

```bash
docker run --rm \
  -e CODEX_POOL_ADMIN_PASSWORD='choose-a-strong-password' \
  codex-pool:local hash-password
```

Copy the emitted `pbkdf2-sha256:...` value into `CODEX_POOL_ADMIN_PASSWORD_HASH` below.

## Run

```bash
docker run -d \
  --name codex-pool \
  --restart unless-stopped \
  -p <host-api-bind>:8317 \
  -p <host-admin-bind>:8318 \
  -v codex-pool-data:/data \
  -e CODEX_POOL_API_KEY='replace-with-a-long-random-client-key' \
  -e CODEX_POOL_ADMIN_PASSWORD_HASH='pbkdf2-sha256:...' \
  -e CODEX_POOL_DEFAULT_MODEL='gpt-5.5(xhigh)' \
  -e CODEX_POOL_SESSION_AFFINITY_TTL_MS=86400000 \
  -e CODEX_POOL_ADMIN_ADDR='0.0.0.0:8318' \
  -e CODEX_POOL_ALLOW_REMOTE_ADMIN=true \
  codex-pool:local
```

`CODEX_POOL_ALLOW_REMOTE_ADMIN=true` is required because Docker port forwarding reaches the container over its network interface. The published admin port remains loopback-only on the host through `-p 127.0.0.1:8318:8318`; do not publish that port to a public interface without TLS and additional access controls.

All persistent configuration, sticky sessions, cooldowns, and account data are stored in the `codex-pool-data` Docker volume at `/data`.

Open these paths after startup:

```text
Public API: http://<api-host>:<api-port>/v1
Admin UI:   http://<admin-host>:<admin-port>/admin
Health:     http://<api-host>:<api-port>/healthz
```

The admin root redirects to `/admin`. Public API root intentionally returns `404` so the API port does not advertise service details in a browser. Public API endpoints under `/v1` and `/healthz` require the configured API key.

### Add Codex Accounts

Open `/admin`, sign in, then select `Add account`. The UI does not ask for any account fields. It creates an independent Codex credential slot, starts device auth, and shows only the verification URL, user code, and a 15 minute countdown. After you complete the browser login, subscription tier, upstream account ID, email, and organization are stored as credential metadata. They are shown as secondary status information, but the slot ID remains the primary identity used by routing and management. Do not select models during onboarding; model access is discovered from the authenticated Codex credential and Codex clients can request the model they want later.

The container runs Codex CLI device auth with:

```text
CODEX_HOME=/data/accounts/<account-id>/.codex
```

The admin page shows the verification URL and user code. After you complete the browser login, Codex stores `auth.json` under the account's `/data/accounts/<account-id>/.codex` directory. Those credentials never belong in the Git repository or Docker image. Pool treats each completed device-auth credential as its own slot even when two slots report the same email address. Email, subscription tier, and organization fields are descriptive metadata only; they may be shown in the dashboard for recognition, but they are not used as local credential keys, routing keys, or storage paths.

### Bundled CLIProxy Sidecar

Codex device-auth inference is relayed through the pinned CLIProxyAPI binary bundled in the same container. It listens only on `127.0.0.1:8319`; it is not published with Docker and does not add an admin page or public endpoint. `codex-pool` remains the only public API and UI service.

For every selected account, Pool writes an isolated CLIProxy auth record under `/data/cliproxy/auths/<account-id>.json` with a unique model prefix. Pool selects that exact prefix, so it continues to own sticky bindings, quota-based exclusion, cooldowns, and account failover. CLIProxyAPI is the only process that refreshes the copied OAuth credential; quota polling reads that current sidecar record and does not race the refresh token. Do not edit either credential directory by hand.

`CODEX_POOL_CODEX_GATEWAY_MODE=sidecar` is the default. `direct` exists only as a compatibility/test override and bypasses the sidecar; it is not the normal deployment mode. `CODEX_POOL_CLIPROXY_BASE_URL` and `CODEX_POOL_CLIPROXY_API_KEY` are internal test/compatibility overrides and should not be set in a normal `docker run` deployment.

Quota is read from the authenticated Codex/ChatGPT backend after login and then refreshed every five minutes. The dashboard shows both short-window and weekly remaining percentages with reset times when upstream provides them. The optional `CODEX_POOL_CODEX_USAGE_URL` override exists for tests or backend compatibility; normal deployments use `CODEX_POOL_CODEX_BASE_URL + /wham/usage`.

### Prompt Cache Locality

Pool keeps requests sticky by project/session/model and automatically adds a hashed `prompt_cache_key` when the client did not provide one. The raw project or session value is not sent upstream. `prompt_cache_key` generation is controlled by `CODEX_POOL_PROMPT_CACHE_KEY_MODE=auto|off|passthrough`; `auto` is the default.

`CODEX_POOL_PROMPT_CACHE_RETENTION` is optional and defaults to passthrough so upstream organization data-retention defaults remain in control. Set it to `24h` or `in_memory` only when that retention policy is intended for every relayed request.

Successful responses update aggregate prompt-cache counters in the admin state from upstream `usage` fields, including input tokens and cached tokens. Response IDs are also bound to the selected account for the sticky TTL so follow-up `previous_response_id` requests stay on the original account.

### Preserve Pro Quota

Use the `Use Pro last` switch in the admin Console to defer Pro accounts until no eligible non-Pro account is available. When this mode is enabled, a session that temporarily moved to Pro because other accounts were cooling down moves back to a non-Pro account once one becomes eligible again. The switch is stored in `/data/config.json`; `CODEX_POOL_PRESERVE_PRO_QUOTA=true` only sets the initial default before the Console setting is saved.

### Remote Admin

Keep the admin port on loopback unless it is behind a private network or a reverse proxy with TLS and access controls. For remote administration, keep these two environment variables in the container configuration and publish the admin port only through your protected network path:

```bash
-e CODEX_POOL_ADMIN_ADDR='0.0.0.0:8318' \
-e CODEX_POOL_ALLOW_REMOTE_ADMIN=true
```

Do not forward the admin port directly from the Internet: the admin service does not terminate TLS itself. Use a reverse proxy with a valid TLS certificate, preserve the original `Host` header, and restrict the proxy to `/admin` and `/admin/api/` if the public API is served separately.

### Public Status And Protected Management

`GET /admin` opens the management page. Unauthenticated public pool status is disabled by default to reduce scan surface. If you need trusted account owners to join or leave the pool without the admin password, set `CODEX_POOL_PUBLIC_DASHBOARD=true`; the public JSON uses an opaque per-process account reference for this toggle and returns only a partially masked email for account recognition. It never returns full email addresses, account IDs, upstream URLs, API keys, sticky sessions, traffic details, quota error codes, or upstream error bodies.

Authenticate with the admin password to add accounts, remove accounts, restart device auth, and inspect or clear sticky sessions. Both views use the same admin port `8318`; no additional port is required. The dashboard auto-refreshes every five minutes; use `Refresh` for an immediate status read.

## Codex CLI

Create `~/.codex/config.toml` on the machine running Codex:

```toml
model = "gpt-5.5(xhigh)"
model_provider = "codex-pool"

[model_providers.codex-pool]
name = "Codex Pool"
base_url = "http://<api-host>:<api-port>/v1"
env_key = "CODEX_POOL_API_KEY"
wire_api = "responses"
```

Set `CODEX_POOL_API_KEY` in the Codex process environment to the same client key passed to the service container. The checked-in [test config](test/codex-config.toml) uses the same provider contract and was verified with Codex CLI `0.141.0`.

## Implemented Surface

- API-key authentication from Bearer, raw Authorization, `X-Goog-Api-Key`, `X-Api-Key`, `key`, and `auth_token`.
- `GET /v1/models`, including Codex client model-list mode.
- `POST /v1/responses` and `/v1/responses/compact`, with streaming passthrough.
- `POST /v1/chat/completions`, including translation to a Responses upstream.
- Model aliases and `(thinking-tier)` suffix translation.
- Sticky failover routing with idle TTL, per-model cooldowns, quota-exhaustion failover, optional Pro-quota preservation, prompt-cache-key routing, response-id continuation binding, and JSON persistence in `/data`. When an upstream account returns `429` or server errors, the request retries other configured accounts and successful failover rewrites the sticky binding.
- Bundled, loopback-only CLIProxyAPI sidecar for Codex device-auth requests. Pool pins each request to the selected account through a sidecar model prefix, while the sidecar owns OAuth refreshes.
- Public pool participation toggles on `/admin`, plus authenticated owner controls for add/remove account, device-auth login jobs, and sticky-session inspection. Account states are explicitly labeled `Ready`, `Low quota`, `Cooldown`, `Error`, `Login needed`, `Disabled`, or `Standby`.
- Codex quota refresh from `/backend-api/wham/usage`, including per-window percentages, reset times, plan-type updates, sanitized quota errors, and five-minute dashboard refresh.

Codex accounts are created through the admin UI/API as empty device-auth slots, then authenticated with device auth. The UI does not ask for email, subscription tier, or model selection during onboarding; account metadata is read from the authenticated Codex token after login. A legacy provider API-key gateway path remains for testing and advanced OpenAI-compatible providers, but it is not the default runtime path.

## Verification

The implementation is tested only with Docker:

```bash
docker run --rm -v "$PWD:/src" -w /src golang:1.24-bookworm \
  /usr/local/go/bin/go test -v -p 1 -buildvcs=false ./...
```

Run the Codex CLI/provider integration test with Docker as well:

```bash
sh scripts/test/integration-codex-config.sh
sh scripts/test/integration-cliproxy-sidecar.sh
sh scripts/test/integration-device-auth-failover.sh
```

That script builds the service and mock upstream images, starts them on an isolated Docker network, verifies public dashboard access, verifies protected management APIs, checks public API-key enforcement, then runs a real `codex exec` inside the service image with an ephemeral `CODEX_HOME/config.toml`. The test proves Codex can use:

```toml
[model_providers.codex-pool]
base_url = "http://codex-pool:8317/v1"
env_key = "CODEX_POOL_API_KEY"
wire_api = "responses"
```

The checked-in [test config](test/codex-config.toml) uses the same provider contract.

`integration-cliproxy-sidecar.sh` starts the real bundled sidecar and verifies its isolated auth conversion plus loopback-only binding. `integration-device-auth-failover.sh` runs a real `codex exec` through the Pool sidecar adapter, forces the first device-auth account to return `429`, and verifies that the request completes through the second account with a persisted cooldown and sticky binding.

## Commit Security

Install the repository hook once:

```bash
git config core.hooksPath .githooks
```

Before each commit, run the Docker-only test suite and the staged-change audit:

```bash
docker run --rm -v "$PWD:/src" -w /src golang:1.24-bookworm \
  /usr/local/go/bin/go test -v -p 1 -buildvcs=false ./...
sh scripts/test/integration-codex-config.sh
sh scripts/test/integration-cliproxy-sidecar.sh
sh scripts/test/integration-device-auth-failover.sh
sh scripts/security/precommit-security-audit.sh --check
```

The hook blocks credential/runtime-data paths, high-confidence API keys, session tokens, JWTs, and OAuth token values. It also requires an explicit review acknowledgement for every commit:

```bash
CODEX_ACCOUNT_SECURITY_REVIEWED=yes git commit -m "your message"
```

Never stage `.codex/`, `auth.json`, `.env`, `/data`, account data, OAuth responses, provider keys, or generated config files. See [AGENTS.md](AGENTS.md) for the mandatory Codex account security review checklist.
