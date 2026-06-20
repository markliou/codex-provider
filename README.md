# Codex Pool Provider

Dockerized, single-user OpenAI-compatible provider gateway for Codex. The service exposes `/v1` endpoints and routes requests to a configured provider API with sticky session affinity.

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
  -p 8317:8317 \
  -p 127.0.0.1:8318:8318 \
  -v codex-pool-data:/data \
  -e CODEX_POOL_API_KEY='replace-with-a-long-random-client-key' \
  -e CODEX_POOL_ADMIN_PASSWORD_HASH='pbkdf2-sha256:...' \
  -e CODEX_POOL_UPSTREAM_BASE_URL='https://api.openai.com/v1' \
  -e CODEX_POOL_UPSTREAM_API_KEY='upstream-provider-key' \
  -e CODEX_POOL_DEFAULT_MODEL='gpt-5.4' \
  -e CODEX_POOL_ADMIN_ADDR='0.0.0.0:8318' \
  -e CODEX_POOL_ALLOW_REMOTE_ADMIN=true \
  codex-pool:local
```

`CODEX_POOL_ALLOW_REMOTE_ADMIN=true` is required because Docker port forwarding reaches the container over its network interface. The published admin port remains loopback-only on the host through `-p 127.0.0.1:8318:8318`; do not publish that port to a public interface without TLS and additional access controls.

All persistent configuration, sticky sessions, cooldowns, and account data are stored in the `codex-pool-data` Docker volume at `/data`.

### Remote Admin Through A Router

Set the container up once with remote admin enabled, then use the router's port-forward rule as the on/off switch. Toggling that router rule does not restart or otherwise alter the container.

Replace the loopback admin mapping in the run command with the host's fixed LAN address:

```bash
-p <lan-host>:8318:8318
```

Keep these two environment variables in the container configuration:

```bash
-e CODEX_POOL_ADMIN_ADDR='0.0.0.0:8318' \
-e CODEX_POOL_ALLOW_REMOTE_ADMIN=true
```

Configure the router to forward an external TCP port only to the HTTPS reverse proxy in front of `<lan-host>:8318`. When remote administration is not needed, disable that router forwarding rule. The service continues serving the public API and remains ready for the next time the rule is enabled.

Do not forward TCP `8318` directly from the Internet: the admin service does not terminate TLS itself. Use a reverse proxy with a valid TLS certificate, preserve the original `Host` header, and restrict the proxy to `/admin` and `/admin/api/` if the public API is served separately.

### Public Status And Protected Management

`GET /admin` is a public, read-only pool status page. It shows only account labels, status, quota hints, configured models, and aggregate counts. It never returns account IDs, email addresses, upstream URLs, API keys, sticky sessions, traffic details, or upstream error bodies.

Select `Admin sign in` on the same page to authenticate. Only a signed-in admin can add, remove, enable, disable, add to pool, remove from pool, change quota hints, clear cooldowns, or inspect sticky sessions. Both views use the same admin port `8318`; no additional port is required.

## Codex CLI

Create `~/.codex/config.toml` on the machine running Codex:

```toml
model = "gpt-5.4"
model_provider = "codex-pool"

[model_providers.codex-pool]
name = "Codex Pool"
base_url = "http://<server-host>:8317/v1"
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
- Sticky failover routing, per-model cooldowns, and JSON persistence in `/data`.
- Authenticated admin dashboard at `/admin`, HttpOnly session cookie, CSRF checks, account CRUD/actions, and sticky-session inspection. Account states are explicitly labeled `Ready`, `Low quota`, `Cooldown`, `Error`, `Disabled`, or `Standby`.

Provider accounts can be initialized through `CODEX_POOL_UPSTREAM_*` environment variables or created through the admin API. API keys are redacted from all admin responses.

## Verification

The implementation is tested only with Docker:

```bash
docker run --rm -v "$PWD:/src" -w /src golang:1.24-bookworm \
  go test -v -p 1 -buildvcs=false ./...
```

The integration test performed during implementation built the service and mock upstream images, exercised model discovery, Responses forwarding, thinking-tier translation, Chat Completions translation, admin login/CSRF/account actions, persistence after restart, and a real `codex exec` call through `test/codex-config.toml`.

## Commit Security

Install the repository hook once:

```bash
git config core.hooksPath .githooks
```

Before each commit, run the Docker-only test suite and the staged-change audit:

```bash
docker run --rm -v "$PWD:/src" -w /src golang:1.24-bookworm \
  go test -v -p 1 -buildvcs=false ./...
sh scripts/security/precommit-security-audit.sh --check
```

The hook blocks credential/runtime-data paths, high-confidence API keys, session tokens, JWTs, and OAuth token values. It also requires an explicit review acknowledgement for every commit:

```bash
CODEX_ACCOUNT_SECURITY_REVIEWED=yes git commit -m "your message"
```

Never stage `.codex/`, `auth.json`, `.env`, `/data`, account data, OAuth responses, provider keys, or generated config files. See [AGENTS.md](AGENTS.md) for the mandatory Codex account security review checklist.
