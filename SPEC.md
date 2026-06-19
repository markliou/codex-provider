# SPEC.md — Personal Codex Pool HTTP Service

## First Rule: Host-Free, Portable Operation

This rule takes precedence over every other requirement in this specification.

Do not install software, runtimes, package managers, libraries, build tools, test tools, or service dependencies on the host. Any required software must be installed and run inside a Docker image or container, including temporary build, development, and test tooling.

Portability is the first priority for every design and implementation decision. The service must be reproducible from a clean Docker host using only version-controlled source, the Docker build context, Docker runtime options, and the mounted `/data` volume. Do not rely on host-specific paths, host-installed commands, host configuration, or manually provisioned host dependencies.

## 0. Purpose

Build a Dockerized, single-user Codex account-pool service inspired by Cockpit Tools' Codex API Service / `cockpit-cliproxy` sidecar.

The service must expose one OpenAI-compatible HTTP API endpoint to clients, while internally routing requests across multiple Codex/ChatGPT accounts or organizations using sticky failover.

Primary goals:

1. Run as a single Docker container started with `docker run`, not Docker Compose.
2. Accept API credentials and admin password configuration through Docker runtime options.
3. Store all account credentials, device-auth state, sticky routing state, cooldown state, logs, and runtime data only in a mounted `/data` volume.
4. Provide a simple web admin UI for account lifecycle management.
5. Replicate Cockpit Tools' Codex API compatibility surface as closely as practical:
   - OpenAI-compatible `/v1/models`
   - OpenAI Responses API passthrough: `/v1/responses`
   - Chat Completions passthrough: `/v1/chat/completions`
   - image generation/edit relay compatibility
   - optional Claude/Gemini/Ollama bridge endpoints if implemented
   - account health, usage stats, quota/cooldown display, and model list behavior through admin APIs
6. Default routing must be sticky failover, not round-robin, to preserve prompt-cache locality.

This is not a multi-tenant product. Assume one trusted owner/operator.

---

## 1. Public vs Admin Surface

The service has two logical surfaces.

### 1.1 Public API surface

This is used by Codex CLI, IDEs, scripts, or OpenAI-compatible clients.

Default bind:

```text
0.0.0.0:8317
```

Public paths:

```text
GET  /v1/models
POST /v1/responses
POST /v1/responses/compact
POST /v1/chat/completions
POST /v1/images/generations
POST /v1/images/edits
POST /v1/messages
POST /v1/messages/count_tokens
GET  /v1beta/models
GET  /v1beta/models/*action
POST /v1beta/models/*action
GET  /api/version
GET  /api/tags
POST /api/show
POST /api/chat
```

Minimum MVP paths:

```text
GET  /v1/models
POST /v1/responses
POST /v1/chat/completions
```

### 1.2 Admin UI/API surface

This is used only by the owner to manage accounts, quota status, sticky sessions, cooldowns, and runtime settings.

Default bind:

```text
127.0.0.1:8318
```

Admin UI path:

```text
GET /admin
```

Admin API prefix:

```text
/admin/api/*
```

Do not expose `/admin` to the public Internet by default. If remote admin is enabled, require explicit opt-in with `CODEX_POOL_ALLOW_REMOTE_ADMIN=true` and strong password authentication.

---

## 2. Docker Runtime Contract

The container must be runnable with plain `docker run`.

Example:

```bash
docker run -d \
  --name codex-pool \
  --restart unless-stopped \
  -p 8317:8317 \
  -p 127.0.0.1:8318:8318 \
  -v /srv/codex-pool/data:/data \
  -e CODEX_POOL_API_KEY="cpk_replace_with_long_random_key" \
  -e CODEX_POOL_ADMIN_PASSWORD_HASH='pbkdf2_hash_from_hash-password' \
  -e CODEX_POOL_DATA_DIR="/data" \
  -e CODEX_POOL_PUBLIC_ADDR="0.0.0.0:8317" \
  -e CODEX_POOL_ADMIN_ADDR="0.0.0.0:8318" \
  -e CODEX_POOL_ALLOW_REMOTE_ADMIN=true \
  registry.gitlab.com/YOUR_NAMESPACE/codex-pool:latest
```

### 2.1 Supported environment variables

| Variable | Required | Default | Description |
|---|---:|---|---|
| `CODEX_POOL_API_KEY` | yes, unless `CODEX_POOL_API_KEYS` set | none | Single bearer key for public `/v1` access. |
| `CODEX_POOL_API_KEYS` | no | none | Comma-separated public API keys. |
| `CODEX_POOL_API_KEYS_FILE` | no | none | Path to file containing one public API key per line. |
| `CODEX_POOL_ADMIN_USERNAME` | no | `admin` | Admin UI username. |
| `CODEX_POOL_ADMIN_PASSWORD_HASH` | yes | none | PBKDF2-HMAC-SHA256 hash emitted by the container's `hash-password` command. Do not require plaintext password. |
| `CODEX_POOL_DATA_DIR` | no | `/data` | Persistent runtime data root. |
| `CODEX_POOL_PUBLIC_ADDR` | no | `0.0.0.0:8317` | Public API bind address. |
| `CODEX_POOL_ADMIN_ADDR` | no | `127.0.0.1:8318` | Admin UI/API bind address. |
| `CODEX_POOL_ALLOW_REMOTE_ADMIN` | no | `false` | Required if admin address is non-loopback. |
| `CODEX_POOL_LOG_LEVEL` | no | `info` | `debug`, `info`, `warn`, `error`. |
| `CODEX_POOL_REDACT_LOGS` | no | `true` | Redact tokens, auth headers, API keys, refresh tokens. |
| `CODEX_POOL_DEFAULT_MODEL` | no | implementation-defined | Default model when request omits model. |
| `CODEX_POOL_ROUTING_STRATEGY` | no | `sticky_failover` | Routing strategy. |
| `CODEX_POOL_SESSION_AFFINITY_TTL_MS` | no | `86400000` | Sticky session TTL. |
| `CODEX_POOL_MAX_RETRY_ACCOUNTS` | no | `3` | Max account failover attempts per request. |

### 2.2 Startup safety checks

The service must refuse to start when:

1. No public API key is configured.
2. Admin password hash is missing.
3. Public API key equals a known example value.
4. Admin password hash equals a known example value.
5. Admin binds to `0.0.0.0` or another non-loopback address while `CODEX_POOL_ALLOW_REMOTE_ADMIN` is not `true`.

The service should warn when:

1. `/data` has overly permissive permissions.
2. Debug logs are enabled.
3. Remote admin is explicitly enabled.
4. HTTP is used on a public bind address without TLS/reverse proxy.

---

## 3. Persistent Data Layout

All mutable and sensitive data must live under `/data`.

```text
/data/
  config.json
  accounts/
    acct-personal/
      meta.json
      .codex/
        auth.json
    acct-org-a/
      meta.json
      .codex/
        auth.json
  state/
    sticky-sessions.json
    cooldowns.json
    account-health.json
    usage-stats.json
    jobs.json
  logs/
    service.log
```

Never store these in the Git repository or Docker image:

```text
/data
accounts/
.codex/
auth.json
config.json
.env
logs/
*.db
*.sqlite
*.key
*.pem
*.crt
```

---

## 4. Public API Authentication

Public `/v1` APIs must accept API keys from the same places Cockpit's sidecar accepts client keys.

Supported key sources, in priority order:

1. `Authorization: Bearer <key>`
2. `Authorization: <key>`
3. `X-Goog-Api-Key: <key>`
4. `X-Api-Key: <key>`
5. URL query `?key=<key>`
6. URL query `?auth_token=<key>`

If no valid key is found, return:

```http
HTTP/1.1 401 Unauthorized
Content-Type: application/json
```

```json
{
  "error": {
    "message": "invalid or missing API key",
    "type": "invalid_request_error",
    "code": "invalid_api_key"
  }
}
```

---

## 5. Model Catalog and Thinking Tier

### 5.1 Model IDs

Expose model IDs through:

```text
GET /v1/models
```

Normal OpenAI-compatible response:

```json
{
  "object": "list",
  "data": [
    {
      "id": "gpt-5.5",
      "object": "model",
      "created": 0,
      "owned_by": "openai"
    },
    {
      "id": "gpt-5.5(high)",
      "object": "model",
      "created": 0,
      "owned_by": "openai"
    }
  ]
}
```

### 5.2 Codex client model-list mode

If the request is:

```text
GET /v1/models?client_version=...
```

Return a Codex-client-compatible model-list object.

Minimum shape:

```json
{
  "models": [
    {
      "id": "gpt-5.5",
      "slug": "gpt-5.5",
      "display_name": "GPT-5.5",
      "description": "GPT-5.5",
      "context_length": 272000,
      "max_context_window": 1000000,
      "priority": 1000,
      "additional_speed_tiers": [],
      "service_tiers": [],
      "availability_nux": null,
      "upgrade": null
    }
  ]
}
```

Optional hidden models must include:

```json
{
  "visibility": "hide"
}
```

### 5.3 Thinking tier model suffix

Support Cockpit-style model suffixes:

```text
<model>(<thinking-tier>)
```

Examples:

```text
gpt-5.5(low)
gpt-5.5(medium)
gpt-5.5(high)
gpt-5.5(none)
```

Supported tiers:

```text
none
auto
minimal
low
medium
high
xhigh
max
```

For Codex/OpenAI Responses requests, translate suffix into nested `reasoning.effort`:

Input:

```json
{
  "model": "gpt-5.5(high)",
  "input": "..."
}
```

Upstream body:

```json
{
  "model": "gpt-5.5",
  "reasoning": {
    "effort": "high"
  },
  "input": "..."
}
```

If suffix is `none`, remove or omit `reasoning.effort` unless upstream explicitly accepts `none`.

### 5.4 Model aliases

Support aliases:

```json
{
  "sourceModel": "gpt-5.5",
  "alias": "deep",
  "fork": false
}
```

Alias behavior:

```text
deep          -> gpt-5.5
deep(high)    -> gpt-5.5 + reasoning.effort=high
```

---

## 6. Request Routing

### 6.1 Default strategy

Default must be:

```text
sticky_failover
```

Do not round-robin by default.

Reason: prompt-cache locality is important. Requests for the same project/session/model should continue using the same account until that account becomes unavailable or enters cooldown.

### 6.2 Sticky key derivation

Use the first available source:

1. `X-Codex-Pool-Session`
2. `X-Codex-Pool-Project`
3. `prompt_cache_key` from JSON body
4. `session_id`, `conversation_id`, or `thread_id` from JSON body
5. Hash of `(apiKeyId + model + normalized prompt prefix)`

Final sticky key format:

```text
<model>:<stable-session-id>
```

Example:

```text
gpt-5.5:repo-main-worktree
```

### 6.3 Failover rules

Switch account only on:

```text
usage_limit_reached
rate_limit_exhausted
account_model_cooldown
account_disabled
missing_auth
refresh_failed
repeated_upstream_5xx
```

Do not switch on:

```text
normal access-token refresh
client 400 error
model_not_found
context_length_exceeded
single transient timeout
```

### 6.4 Failback behavior

Do not automatically fail back existing sticky sessions when the original account cooldown expires.

Use:

```text
failback = new_session_only
```

A session that failed over from `acct-a` to `acct-b` should remain on `acct-b` until it ends, unless `acct-b` also fails.

### 6.5 Candidate account filter

A selectable account must satisfy:

```text
enabled == true
inPool == true
authStatus == ready
account.status not in disabled/missing_auth/auth_error
model allowed by account
model allowed by API key
not in account-level cooldown
not in model-level cooldown for requested model
```

---

## 7. Account Lifecycle

### 7.1 Account object

```json
{
  "id": "acct-org-a",
  "label": "Org A",
  "email": "user@example.com",
  "accountId": "chatgpt-account-id-if-known",
  "authType": "codex_device_auth",
  "codexHome": "/data/accounts/acct-org-a/.codex",
  "enabled": true,
  "inPool": true,
  "priority": 100,
  "allowedModels": ["gpt-5.5"],
  "excludedModels": [],
  "planType": "plus",
  "planRank": 100,
  "remainingQuota": 82,
  "subscriptionExpiryMs": null,
  "createdAt": 1781930000000,
  "updatedAt": 1781930000000,
  "lastUsedAt": 1781930500000,
  "lastLoginAt": 1781930000000
}
```

Notes:

- `remainingQuota` is an integer routing hint. Treat it as a percentage or normalized score, not an exact token count.
- Actual consumed tokens are tracked in usage stats.
- Absolute remaining Codex token quota may not be available from upstream. Display both quota-window percentages and token usage counters.

### 7.2 Add account

```http
POST /admin/api/accounts
Content-Type: application/json
```

Request:

```json
{
  "id": "acct-org-a",
  "label": "Org A",
  "priority": 100,
  "enabled": false,
  "inPool": false,
  "allowedModels": ["gpt-5.5"]
}
```

Response:

```json
{
  "ok": true,
  "account": {
    "id": "acct-org-a",
    "label": "Org A",
    "enabled": false,
    "inPool": false,
    "status": "missing_auth"
  }
}
```

### 7.3 Start device-auth login

```http
POST /admin/api/accounts/{accountId}/login
```

Behavior:

1. Run Codex login with `CODEX_HOME=/data/accounts/{accountId}/.codex`.
2. Use device-auth flow.
3. Create a login job.
4. Return job ID immediately.

Response:

```json
{
  "ok": true,
  "jobId": "job-login-acct-org-a-1781930000",
  "status": "running"
}
```

### 7.4 Poll login job

```http
GET /admin/api/jobs/{jobId}
```

Waiting for user:

```json
{
  "ok": true,
  "jobId": "job-login-acct-org-a-1781930000",
  "type": "account_login",
  "status": "waiting_for_user",
  "accountId": "acct-org-a",
  "verificationUrl": "https://auth.openai.com/activate",
  "userCode": "ABCD-EFGH",
  "message": "Open the verification URL and enter the code."
}
```

Completed:

```json
{
  "ok": true,
  "jobId": "job-login-acct-org-a-1781930000",
  "type": "account_login",
  "status": "completed",
  "accountId": "acct-org-a"
}
```

Failed:

```json
{
  "ok": false,
  "jobId": "job-login-acct-org-a-1781930000",
  "type": "account_login",
  "status": "failed",
  "error": {
    "code": "login_failed",
    "message": "device auth failed or timed out"
  }
}
```

### 7.5 Enable/disable account

```http
POST /admin/api/accounts/{accountId}/enable
POST /admin/api/accounts/{accountId}/disable
```

Disable must also remove the account from pool and clear sticky sessions bound to that account.

### 7.6 Add/remove from pool

```http
POST /admin/api/accounts/{accountId}/pool-add
POST /admin/api/accounts/{accountId}/pool-remove
```

`pool-remove` keeps credentials but prevents scheduler selection. It should also clear sticky sessions bound to the account.

### 7.7 Soft delete and purge

```http
DELETE /admin/api/accounts/{accountId}
POST   /admin/api/accounts/{accountId}/purge
```

Soft delete behavior:

```text
/data/accounts/acct-org-a -> /data/accounts/.trash/acct-org-a-<timestamp>
```

Purge permanently deletes trashed credential data.

---

## 8. Account Health and Cooldown

### 8.1 Health object

```json
{
  "accountId": "acct-org-a",
  "email": "user@example.com",
  "available": true,
  "consecutiveFailures": 0,
  "lastSuccessAt": 1781930500000,
  "lastFailureAt": null,
  "lastFailureStatus": null,
  "lastFailureCategory": null,
  "lastFailureMessage": null,
  "imageGenerationStatus": "available",
  "imageGenerationCheckedAt": 1781930400000,
  "cooldowns": []
}
```

### 8.2 Cooldown object

```json
{
  "modelId": "gpt-5.5",
  "nextRetryAt": 1781934000000,
  "remainingMs": 3500000,
  "reason": "usage_limit_reached"
}
```

### 8.3 Health endpoint

```http
GET /admin/api/accounts/health
```

Response:

```json
{
  "ok": true,
  "accounts": [
    {
      "accountId": "acct-org-a",
      "email": "user@example.com",
      "available": false,
      "consecutiveFailures": 1,
      "lastSuccessAt": 1781930000000,
      "lastFailureAt": 1781930500000,
      "lastFailureStatus": 429,
      "lastFailureCategory": "usage_limit_reached",
      "lastFailureMessage": "usage limit reached",
      "imageGenerationStatus": "unknown",
      "imageGenerationCheckedAt": null,
      "cooldowns": [
        {
          "modelId": "gpt-5.5",
          "nextRetryAt": 1781934000000,
          "remainingMs": 3500000,
          "reason": "usage_limit_reached"
        }
      ]
    }
  ]
}
```

### 8.4 Clear cooldown

```http
POST /admin/api/accounts/{accountId}/cooldowns/clear
```

Optional request:

```json
{
  "modelId": "gpt-5.5"
}
```

---

## 9. Quota Refresh and Display

### 9.1 Upstream quota source

For Codex OAuth accounts, query:

```text
GET https://chatgpt.com/backend-api/wham/usage
```

Headers:

```text
Authorization: Bearer <access_token>
Accept: application/json
ChatGPT-Account-Id: <account-id-if-known>
```

### 9.2 Upstream usage response shape

Expected upstream fields:

```json
{
  "plan_type": "plus",
  "rate_limit": {
    "allowed": true,
    "limit_reached": false,
    "primary_window": {
      "used_percent": 20,
      "limit_window_seconds": 18000,
      "reset_after_seconds": 3600,
      "reset_at": 1781934000
    },
    "secondary_window": {
      "used_percent": 50,
      "limit_window_seconds": 604800,
      "reset_after_seconds": 86400,
      "reset_at": 1782016800
    }
  },
  "code_review_rate_limit": {
    "allowed": true,
    "limit_reached": false,
    "primary_window": null,
    "secondary_window": null
  }
}
```

### 9.3 Normalized quota object

Compute:

```text
remaining percentage = 100 - used_percent
reset time = reset_at if present, otherwise now + reset_after_seconds
window minutes = ceil(limit_window_seconds / 60)
```

Response object:

```json
{
  "accountId": "acct-org-a",
  "planType": "plus",
  "quota": {
    "hourlyPercentage": 80,
    "hourlyResetTime": 1781934000,
    "hourlyWindowMinutes": 300,
    "hourlyWindowPresent": true,
    "weeklyPercentage": 50,
    "weeklyResetTime": 1782016800,
    "weeklyWindowMinutes": 10080,
    "weeklyWindowPresent": true,
    "rawData": {}
  },
  "usageUpdatedAt": 1781930500,
  "quotaError": null
}
```

### 9.4 Refresh one account quota

```http
POST /admin/api/accounts/{accountId}/quota/refresh
```

Response:

```json
{
  "ok": true,
  "accountId": "acct-org-a",
  "planType": "plus",
  "quota": {
    "hourlyPercentage": 80,
    "hourlyResetTime": 1781934000,
    "hourlyWindowMinutes": 300,
    "hourlyWindowPresent": true,
    "weeklyPercentage": 50,
    "weeklyResetTime": 1782016800,
    "weeklyWindowMinutes": 10080,
    "weeklyWindowPresent": true,
    "rawData": {}
  }
}
```

### 9.5 Refresh all quotas

```http
POST /admin/api/accounts/quota/refresh-all
```

Response:

```json
{
  "ok": true,
  "results": [
    {
      "accountId": "acct-org-a",
      "ok": true,
      "quota": {}
    },
    {
      "accountId": "acct-org-b",
      "ok": false,
      "error": {
        "code": "quota_refresh_failed",
        "message": "API returned 401 [error_code:invalid_token]"
      }
    }
  ]
}
```

Quota refresh must not block normal `/v1` request handling. Run refresh jobs in background with bounded concurrency.

---

## 10. Usage Statistics

### 10.1 Usage stats object

```json
{
  "requestCount": 10,
  "successCount": 9,
  "failureCount": 1,
  "clientCanceledCount": 0,
  "upstreamResponseFailedCount": 0,
  "streamIncompleteCount": 0,
  "totalLatencyMs": 12345,
  "textRequestCount": 8,
  "imageRequestCount": 2,
  "imageGenerationRequestCount": 1,
  "imageEditRequestCount": 1,
  "imageGenerationCapabilityFailureCount": 0,
  "inputTokens": 100000,
  "outputTokens": 20000,
  "totalTokens": 120000,
  "cachedTokens": 70000,
  "reasoningTokens": 5000,
  "estimatedCostUsd": 1.23
}
```

### 10.2 Stats windows

Expose all-time, daily, weekly, and monthly windows:

```json
{
  "since": 1781900000000,
  "updatedAt": 1781930500000,
  "totals": {},
  "accounts": [
    {
      "accountId": "acct-org-a",
      "email": "user@example.com",
      "usage": {},
      "updatedAt": 1781930500000
    }
  ],
  "models": [
    {
      "modelId": "gpt-5.5",
      "usage": {},
      "updatedAt": 1781930500000
    }
  ],
  "apiKeys": [
    {
      "apiKeyId": "default",
      "label": "Default",
      "usage": {},
      "updatedAt": 1781930500000
    }
  ]
}
```

### 10.3 Full stats response

```http
GET /admin/api/stats
```

Response:

```json
{
  "ok": true,
  "stats": {
    "since": 1781900000000,
    "updatedAt": 1781930500000,
    "totals": {},
    "accounts": [],
    "models": [],
    "apiKeys": [],
    "daily": {},
    "weekly": {},
    "monthly": {},
    "events": []
  }
}
```

### 10.4 Usage event object

```json
{
  "timestamp": 1781930500000,
  "requestId": "req_xxx",
  "accountId": "acct-org-a",
  "email": "user@example.com",
  "apiKeyId": "default",
  "apiKeyLabel": "Default",
  "modelId": "gpt-5.5",
  "gatewayMode": "sidecar",
  "requestKind": "text",
  "success": true,
  "httpStatus": 200,
  "errorCategory": "",
  "errorMessage": "",
  "latencyMs": 1200,
  "inputTokens": 1000,
  "outputTokens": 200,
  "totalTokens": 1200,
  "cachedTokens": 800,
  "reasoningTokens": 50,
  "estimatedCostUsd": 0.01,
  "inputUsdPerMillion": 1.25,
  "outputUsdPerMillion": 10,
  "cachedInputUsdPerMillion": 0.125
}
```

### 10.5 Usage event pagination

```http
GET /admin/api/usage-events?page=1&pageSize=50
```

Response:

```json
{
  "ok": true,
  "events": [],
  "total": 123,
  "page": 1,
  "pageSize": 50,
  "totalPages": 3
}
```

---

## 11. Runtime State Endpoint

```http
GET /admin/api/state
```

Response:

```json
{
  "ok": true,
  "state": {
    "collection": {
      "enabled": true,
      "port": 8317,
      "apiKey": "redacted",
      "apiKeys": [],
      "accessScope": "lan",
      "clientBaseUrlHost": "localhost",
      "imageGenerationMode": "enabled",
      "gatewayMode": "sidecar",
      "upstreamProxyUrl": null,
      "routingStrategy": "sticky_failover",
      "customRoutingRules": [],
      "accountModelRules": [],
      "modelAliases": [],
      "modelPricings": [],
      "excludedModels": [],
      "sessionAffinity": true,
      "sessionAffinityTtlMs": 86400000,
      "maxRetryCredentials": 3,
      "maxRetryIntervalMs": 3000,
      "timeouts": {},
      "activeTimeoutPresetId": "long_wait",
      "timeoutPresets": [],
      "disableCooling": false,
      "restrictFreeAccounts": true,
      "debugLogs": false,
      "boundOauthAccountId": null,
      "accountIds": ["acct-org-a"],
      "createdAt": 1781930000000,
      "updatedAt": 1781930000000
    },
    "running": true,
    "defaultProfile": null,
    "apiPortUrl": "http://127.0.0.1:8317/v1",
    "baseUrl": "http://localhost:8317/v1",
    "lanBaseUrl": "http://<lan-host>:8317/v1",
    "modelIds": ["gpt-5.5", "gpt-5.5(high)"],
    "modelPricingPresets": [],
    "lastError": null,
    "memberCount": 2,
    "stats": {},
    "accountHealth": []
  }
}
```

All secrets must be redacted in state responses.

---

## 12. Public `/v1` Request Handling

### 12.1 `/v1/responses`

Accept OpenAI Responses-style JSON.

Required behavior:

1. Authenticate client API key.
2. Parse model and thinking suffix.
3. Resolve alias.
4. Pick account using sticky failover.
5. Refresh access token if expired.
6. Send upstream request.
7. Stream SSE if request asks for stream.
8. Track usage and emit usage event.
9. On quota exhaustion, mark account/model cooldown and retry next available account.

### 12.2 `/v1/chat/completions`

Accept OpenAI Chat Completions-style JSON.

If upstream requires Responses format, translate request.

If using provider gateway with `wireApi=chat_completions`, forward as Chat Completions.

### 12.3 Streaming

For streaming responses:

- Preserve `text/event-stream` semantics.
- Flush chunks immediately.
- Detect stream idle timeout.
- On stream error, emit SSE `error` event where possible.
- Track incomplete streams separately.

### 12.4 Error response shape

Use OpenAI-style error envelope:

```json
{
  "error": {
    "message": "model gpt-x is not available",
    "type": "invalid_request_error",
    "code": "model_not_available"
  }
}
```

Common error codes:

```text
invalid_api_key
model_not_available
not_found
bad_gateway
upstream_timeout
usage_limit_reached
all_accounts_cooling_down
account_auth_failed
streaming_not_supported
invalid_request
```

---

## 13. Request Diagnostics and Events

Expose admin SSE stream:

```http
GET /admin/api/events
```

### 13.1 Startup event

```json
{
  "type": "startup",
  "stage": "runtime_started"
}
```

### 13.2 Request started event

```json
{
  "type": "request_started",
  "requestId": "req_xxx",
  "method": "POST",
  "path": "/v1/responses",
  "requestKind": "text",
  "model": "gpt-5.5",
  "apiKeyId": "default",
  "apiKeyLabel": "Default",
  "transport": "sse",
  "startedAtMs": 1781930500000
}
```

### 13.3 Request completed event

```json
{
  "type": "request_completed",
  "requestId": "req_xxx",
  "method": "POST",
  "path": "/v1/responses",
  "requestKind": "text",
  "model": "gpt-5.5",
  "apiKeyId": "default",
  "apiKeyLabel": "Default",
  "transport": "sse",
  "status": 200,
  "latencyMs": 1200,
  "completedAtMs": 1781930501200,
  "aborted": false,
  "errorMessage": ""
}
```

### 13.4 Usage event

```json
{
  "type": "usage",
  "requestId": "req_xxx",
  "provider": "codex",
  "model": "gpt-5.5",
  "alias": "deep",
  "accountId": "acct-org-a",
  "accountEmail": "user@example.com",
  "authId": "acct-org-a",
  "apiKeyId": "default",
  "apiKeyLabel": "Default",
  "requestKind": "text",
  "success": true,
  "status": 200,
  "errorCategory": "",
  "errorMessage": "",
  "latencyMs": 1200,
  "usage": {
    "inputTokens": 1000,
    "outputTokens": 200,
    "reasoningTokens": 50,
    "cachedTokens": 800,
    "totalTokens": 1200
  },
  "requestedAtMs": 1781930501200
}
```

---

## 14. Provider Gateway Support

Support API-key-based upstream providers in addition to Codex OAuth accounts.

Provider gateway object:

```json
{
  "baseUrl": "https://provider.example.com/v1",
  "apiKey": "redacted",
  "upstreamModel": "provider-model",
  "upstreamModels": ["provider-model", "provider-model-large"],
  "wireApi": "responses",
  "supportsVision": true,
  "modelCapabilities": {
    "provider-model": {
      "supportsVision": true
    }
  },
  "visionRoutingModel": "provider-model-vision"
}
```

Supported `wireApi` values:

```text
responses
chat_completions
```

Aliases accepted for `chat_completions`:

```text
chat_completions
chat-completions
openai_chat
openai-chat
chat
```

Everything else defaults to `responses`.

---

## 15. Manifest Compatibility

Internally support a Cockpit-like manifest shape for config export/import.

```json
{
  "apiKeys": [
    {
      "id": "default",
      "label": "Default",
      "key": "redacted",
      "providerGateway": null,
      "modelPrefix": null,
      "allowedModels": ["gpt-5.5"],
      "excludedModels": [],
      "enabled": true
    }
  ],
  "accounts": [
    {
      "id": "acct-org-a",
      "email": "user@example.com",
      "authId": "acct-org-a",
      "upstreamApiKey": null,
      "planRank": 100,
      "remainingQuota": 80,
      "subscriptionExpiryMs": null
    }
  ],
  "modelIds": ["gpt-5.5", "gpt-5.5(high)"],
  "modelAliases": [
    {
      "sourceModel": "gpt-5.5",
      "alias": "deep",
      "fork": false
    }
  ],
  "excludedModels": [],
  "routingStrategy": "sticky_failover",
  "customRoutingRules": [
    {
      "accountId": "acct-org-a",
      "priority": 100,
      "weight": 1
    }
  ],
  "debugLogs": false
}
```

---

## 16. Admin UI Requirements

Build a minimal web UI.

### 16.1 Pages

Single-page admin dashboard is enough.

Sections:

1. Service status
2. Account pool
3. Account health/cooldowns
4. Quota windows
5. Sticky sessions
6. Usage stats
7. Recent events
8. Model aliases and model catalog
9. Settings

### 16.2 Account table columns

```text
ID
Label
Email
Enabled
In Pool
Status
Priority
Plan
Remaining Quota
Hourly Reset
Weekly Reset
Sticky Sessions
Last Success
Last Error
Actions
```

### 16.3 Account actions

```text
Login / Re-login
Enable
Disable
Add to pool
Remove from pool
Refresh quota
Clear cooldown
Soft delete
Purge
```

### 16.4 Sticky session table

```json
{
  "key": "gpt-5.5:repo-main",
  "modelId": "gpt-5.5",
  "accountId": "acct-org-a",
  "createdAt": 1781930000000,
  "lastSuccessAt": 1781930500000,
  "failoverFrom": null
}
```

Endpoints:

```http
GET    /admin/api/sticky-sessions
DELETE /admin/api/sticky-sessions/{key}
POST   /admin/api/sticky-sessions/{key}/move
```

Move request:

```json
{
  "accountId": "acct-org-b"
}
```

---

## 17. Security Requirements

### 17.1 Repository safety

The public GitLab repo must include:

```text
Dockerfile
README.md
SPEC.md
.env.example
.gitignore
.dockerignore
examples/
```

The repo must not include:

```text
.env
config.json
auth.json
/data
accounts/
.codex/
logs/
state/
```

### 17.2 Secret redaction

Always redact these in logs and admin responses:

```text
Authorization
X-Api-Key
X-Goog-Api-Key
access_token
refresh_token
id_token
auth.json
apiKey
CODEX_POOL_API_KEY
CODEX_POOL_ADMIN_PASSWORD_HASH
```

Redacted value format:

```text
[REDACTED]
```

### 17.3 Admin session

Admin UI must use:

1. Password verification against hash.
2. HttpOnly session cookie.
3. SameSite cookie.
4. CSRF token for mutating methods.
5. Login rate limit.
6. No token display in UI.

---

## 18. Implementation Phases

### Phase 1 — MVP

Required:

1. Docker image.
2. `docker run` env-based config.
3. `/v1/models`.
4. `/v1/responses` streaming passthrough.
5. `/v1/chat/completions` basic passthrough/translation.
6. Account CRUD in admin UI.
7. Device-auth login jobs.
8. Sticky failover scheduler.
9. Account health and cooldown state.
10. Quota refresh from `wham/usage`.
11. Usage stats counters.
12. Secret redaction.

### Phase 2 — Cockpit compatibility expansion

Add:

1. `/v1/responses/compact`
2. `/v1/images/generations`
3. `/v1/images/edits`
4. Anthropic messages bridge
5. Gemini bridge
6. Ollama bridge
7. Provider gateway
8. Model pricing presets
9. Usage event pagination
10. Admin SSE events

### Phase 3 — Hardening

Add:

1. Export/import manifest.
2. Account profile backup/restore.
3. SQLite storage option.
4. Reverse proxy TLS examples.
5. Secret-scanning CI.
6. Integration tests against mock upstream.
7. Chaos tests for cooldown/failover.

---

## 19. Acceptance Criteria

The implementation is acceptable when:

1. `docker run` starts the service without Compose.
2. No secrets are present in the image or repo.
3. `/v1/models` returns OpenAI-compatible model list.
4. `/v1/models?client_version=x` returns Codex-client-compatible model list.
5. `gpt-5.5(high)` is translated to `model=gpt-5.5` plus `reasoning.effort=high`.
6. Multiple accounts can be added through admin UI.
7. Each account can complete device-auth login into its own `CODEX_HOME`.
8. Accounts can be enabled, disabled, added to pool, removed from pool, soft-deleted, and purged.
9. Sticky sessions keep using the same account across requests.
10. Quota exhaustion triggers account/model cooldown and failover.
11. Token expiration triggers refresh on the same account, not failover.
12. Admin UI displays account health, cooldown until time, quota windows, token usage stats, and recent errors.
13. Logs do not expose tokens, API keys, or auth files.
14. Public `/v1` API is protected by configured API key.
15. Admin UI is protected by password and defaults to loopback-only binding.

---

## 20. Important Behavioral Notes

1. Do not treat OAuth access-token expiration as quota exhaustion.
2. Do not round-robin healthy accounts by default.
3. Do not fail back existing sessions automatically after cooldown expires.
4. Do not block live `/v1` requests while refreshing quota for all accounts.
5. Do not expose absolute remaining token quota unless upstream provides it reliably.
6. Show both quota-window percentages and actual token usage counters.
7. Keep account/model cooldowns separate.
8. Keep account credentials isolated by `CODEX_HOME` directory.
9. Use per-account locks when refreshing credentials.
10. Never print raw upstream error bodies if they may contain secrets.
