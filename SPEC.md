# SPEC.md — Personal Codex Pool HTTP Service

## First Rule: Host-Free, Portable Operation

This rule takes precedence over every other requirement in this specification.

Do not install software, runtimes, package managers, libraries, build tools, test tools, or service dependencies on the host. Any required software must be installed and run inside a Docker image or container, including temporary build, development, and test tooling.

Portability is the first priority for every design and implementation decision. The service must be reproducible from a clean Docker host using only version-controlled source, the Docker build context, Docker runtime options, and the mounted `/data` volume. Do not rely on host-specific paths, host-installed commands, host configuration, or manually provisioned host dependencies.

Before release or deployment testing, verify the implementation with Docker-only commands:

1. Run the Go test suite in a Go Docker image.
2. Build the production image from the repository Dockerfile.
3. Run the Codex CLI/provider integration script against a mock upstream and prove Codex can use `config.toml` with `base_url`, `env_key`, and `wire_api = "responses"`; the same script must run `codex app-server` `model/list` and prove the provider catalog passes the bundled Codex decoder without falling back to bundled models.
4. Run the staged security audit and manually inspect staged changes before committing.

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
6. Default routing must balance new sticky sessions across healthy accounts without round-robining individual requests or moving existing sessions.

This is not a multi-tenant product. Assume one trusted owner/operator.

### 0.1 Intent comments and regression guardrails

Implementation changes must preserve source comments that explain non-obvious behavioral intent. Any change to fragile routing, failover, auth, device-auth refresh ownership, sticky sessions, duplicate-account handling, public/admin exposure, low-key UI wording, or code that was previously regressed must include a nearby comment explaining why the behavior exists and what future changes must not undo.

These comments are part of the product contract. They must explain purpose and regression risk, not merely describe the code. If a change alters one of these contracts, update this specification and the relevant source comment together.

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

### 1.2 Control UI and Admin API surface

This serves a public control page for account owners and a password-protected management mode for the owner.

Default bind:

```text
127.0.0.1:8318
```

UI paths:

```text
GET /
GET /admin
```

Admin API prefix:

```text
/admin/api/*
```

Public mode is visible without admin login and may show pool status plus join/leave controls. Management APIs under `/admin/api/*`, except login and public-dashboard endpoints, require strong password authentication. If remote admin is enabled, require explicit opt-in with `CODEX_POOL_ALLOW_REMOTE_ADMIN=true`.

Unauthenticated and login chrome must use deliberately neutral copy, such as `Service`, `Access`, and `Continue`, instead of obvious Codex, pool, provider, or admin labels. This is passive exposure reduction for casual browsing and keyword probes, not a security boundary; management APIs remain protected server-side.

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
| `CODEX_POOL_ADMIN_USERNAME` | no | `admin` | Internal admin session subject and legacy login API username. The UI uses password-only login. |
| `CODEX_POOL_ADMIN_PASSWORD_HASH` | yes | none | PBKDF2-HMAC-SHA256 hash emitted by the container's `hash-password` command. Do not require plaintext password. |
| `CODEX_POOL_DATA_DIR` | no | `/data` | Persistent runtime data root. |
| `CODEX_POOL_PUBLIC_ADDR` | no | `0.0.0.0:8317` | Public API bind address. |
| `CODEX_POOL_ADMIN_ADDR` | no | `127.0.0.1:8318` | Admin UI/API bind address. |
| `CODEX_POOL_ALLOW_REMOTE_ADMIN` | no | `false` | Required if admin address is non-loopback. |
| `CODEX_POOL_PUBLIC_DASHBOARD` | no | `true` | Enable unauthenticated public pool status and join/leave controls on the control page. Set to `false` to hide the public mode. |
| `CODEX_POOL_LOG_LEVEL` | no | `info` | `debug`, `info`, `warn`, `error`. |
| `CODEX_POOL_REDACT_LOGS` | no | `true` | Redact tokens, auth headers, API keys, refresh tokens. |
| `CODEX_POOL_DEFAULT_MODEL` | no | `gpt-5.5(xhigh)` | Default model when request omits model. |
| `CODEX_POOL_CODEX_BASE_URL` | no | `https://chatgpt.com/backend-api` | Codex/ChatGPT backend base URL used for quota reads and the legacy direct gateway. |
| `CODEX_POOL_CODEX_USAGE_URL` | no | `CODEX_POOL_CODEX_BASE_URL + /wham/usage` | Optional quota endpoint override for tests or compatible backends. |
| `CODEX_POOL_CODEX_GATEWAY_MODE` | no | `sidecar` | `sidecar` routes device-auth inference through the bundled loopback-only CLIProxyAPI executor. `direct` is a compatibility/test override. |
| `CODEX_POOL_ROUTING_STRATEGY` | no | `sticky_balanced` | `sticky_balanced` deterministically distributes new sessions across the highest-priority eligible account tier. `sticky_failover` preserves the legacy behavior that sends new sessions to the first preferred account. |
| `CODEX_POOL_SESSION_AFFINITY_TTL_MS` | no | `86400000` | Sticky session idle TTL. Successful requests refresh the binding expiry. |
| `CODEX_POOL_MAX_RETRY_ACCOUNTS` | no | `0` | Max account failover attempts per request. `0` means all configured accounts. |
| `CODEX_POOL_PROMPT_CACHE_KEY_MODE` | no | `auto` | `auto` injects a hashed `prompt_cache_key` when the client omitted one. `off`/`passthrough` leave the request unchanged. |
| `CODEX_POOL_PROMPT_CACHE_KEY_POLICY` | no | `preserve` | Upstream-key policy independent of sticky routing. `preserve` retains a client key and uses the legacy missing-key behavior. `lineage`, `project`, and `user` explicitly replace any client key with a deterministic hashed/bucketed key for that scope. |
| `CODEX_POOL_PROMPT_CACHE_KEY_SCOPE` | no | `auto` | Coarseness of the injected `prompt_cache_key`. `auto` groups by `X-Codex-Pool-Project` header, else API key, else per-conversation. `project`/`user` force that grouping; `conversation` keeps the historical per-conversation key. Coarser keys let sibling conversations reuse the same static-prefix cache. |
| `CODEX_POOL_PROMPT_CACHE_BUCKETS` | no | `4` | Number of buckets (1–256) a coarse cache key is split across, keyed by the stable per-conversation routing key, to stay under OpenAI's ~15 RPM-per-(prefix+key) cache-routing limit. Raise for hot pools; set `1` to maximize sharing. |
| `CODEX_POOL_PROMPT_CACHE_RETENTION` | no | `24h` | Upstream prompt cache retention. Defaults to `24h` (extended retention) to maximize cache hit rate across conversation turns. Set `passthrough` to leave requests untouched, or `in_memory` for the shorter built-in retention. |
| `CODEX_POOL_PRESERVE_PRO_QUOTA` | no | `false` | Initial default for the admin Console `Use Pro last` switch. Once saved in `/data/config.json`, the Console setting takes precedence. |

### 2.2 Startup safety checks

The service must refuse to start when:

1. No public API key is configured.
2. Admin password hash is missing.
3. Public API key equals a known example value.
4. Admin password hash equals a known example value.
5. Admin binds to `0.0.0.0` or another non-loopback address while `CODEX_POOL_ALLOW_REMOTE_ADMIN` is not `true`.

### 2.3 Bundled Codex sidecar

The image must bundle a pinned CLIProxyAPI executable and run it in the same container on `127.0.0.1:8319`. Do not expose or publish that port. The pool remains the only public OpenAI-compatible API and the only management UI.

For a device-auth account, Pool writes a separate CLIProxy auth record under `/data/cliproxy/auths/<account-id>.json` with a unique account prefix. Pool must include that prefix when relaying a selected account's request, so all account selection, sticky-session binding, quota exclusion, cooldown, and failover remain in Pool. Configure the sidecar with one retry credential and no independent cooling so a `429` or upstream server error returns to Pool for routing.

The sidecar owns refreshes of its copied OAuth credential. Pool must read the sidecar copy for quota polling and must not concurrently refresh the original Codex CLI auth file. Sidecar auth records, its internal API key, and generated config are runtime `/data` content and must never be committed.

The original Codex `auth.json` is still read for account metadata, routing eligibility, and sidecar sync. Codex CLI or the sidecar may rewrite that file while requests are selecting accounts, so Pool must use bounded retry when a read sees invalid or incomplete auth content. A missing auth file for a staged or unauthenticated slot must classify quickly as unavailable instead of retrying under the global state lock; a transient partial write must not make all device-auth slots look missing and produce a false `503 no eligible account` response.

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
      "default_reasoning_level": "medium",
      "supported_reasoning_levels": [
        {"effort": "low", "description": "Fast responses with lighter reasoning"},
        {"effort": "medium", "description": "Balances speed and reasoning depth for everyday tasks"},
        {"effort": "high", "description": "Greater reasoning depth for complex problems"},
        {"effort": "xhigh", "description": "Extra high reasoning depth for complex problems"}
      ],
      "shell_type": "shell_command",
      "visibility": "list",
      "supported_in_api": true,
      "priority": 0,
      "base_instructions": "You are Codex, a coding agent...",
      "supports_reasoning_summaries": true,
      "supports_reasoning_summary_parameter": true,
      "default_reasoning_summary": "none",
      "support_verbosity": true,
      "default_verbosity": "low",
      "apply_patch_tool_type": "freeform",
      "web_search_tool_type": "text_and_image",
      "truncation_policy": {"mode": "tokens", "limit": 10000},
      "supports_parallel_tool_calls": true,
      "supports_image_detail_original": true,
      "context_length": 272000,
      "context_window": 272000,
      "max_context_window": 272000,
      "effective_context_window_percent": 95,
      "experimental_supported_tools": [],
      "input_modalities": ["text", "image"],
      "additional_speed_tiers": [],
      "service_tiers": [],
      "availability_nux": null,
      "upgrade": null
    }
  ]
}
```

The response must include every non-defaulted field required by the bundled Codex client schema, even when the value is `false`, empty, or `null`. `base_instructions` must remain non-empty because remote metadata becomes authoritative after a successful refresh. Emit both `supports_reasoning_summaries` and `supports_reasoning_summary_parameter` while supported clients use both schema generations; unknown fields are ignored by the other generation.

Optional hidden models must include:

```json
{
  "visibility": "hide"
}
```

#### 5.2.1 Built-in Codex model lineup

The advertised catalog must always include the current Codex model lineup in addition to the configured default model, per-account `allowedModels`, and aliases. As of July 2026 that lineup, in picker order, is:

```text
gpt-5.6-sol
gpt-5.6-terra
gpt-5.6-luna
gpt-5.5
gpt-5.4
gpt-5.4-mini
gpt-5.3-codex-spark
gpt-5.2-codex
```

This keeps a stock Codex client from falling back to bundled model metadata (with its startup warning and conflicting-tool behavior, see 6.4.2) when the user selects a current model this pool was not explicitly configured for. Advertising a model is not an access grant: per-account model filters and upstream plan enforcement still apply (`gpt-5.3-codex-spark` is Pro-only upstream). Catalog `priority` ranks the configured default model first, then the lineup above, then operator-configured extras.

Reasoning levels are per model family: the `gpt-5.6` family additionally advertises `max` and `ultra`; older families must stay at `low`–`xhigh` so the client cannot submit an effort upstream rejects.

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
ultra
```

`max` and `ultra` are only advertised as catalog capability for the `gpt-5.6` family, but remain accepted as request-input suffixes for any model; upstream is the authority on whether the effort is valid.

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
sticky_balanced
```

Do not round-robin individual requests.

Reason: both quota distribution and prompt-cache locality are important. New
Codex sessions should be spread across healthy accounts so one credential does
not exhaust early and force several long-running sessions to restart cold on a
different account. Requests for the same Codex thread or fallback
project/session/model identity must continue using the same account until that
account becomes unavailable or enters cooldown.

For an unbound route, `sticky_balanced` must use a deterministic rendezvous-style
score derived from `(sticky key, upstream account identity)`. This is session
balancing, not mutable request round-robin:

1. Concurrent first requests for the same sticky key choose the same account
   even before the first successful response persists a binding.
2. Different sticky keys distribute statistically across the eligible accounts.
3. Existing TTL-live sticky bindings remain authoritative and are never
   rebalanced merely because another account becomes available.
4. Selection balances only within the highest-priority eligible tier, preserving
   explicitly configured lower-priority standby accounts.
5. When `preserveProQuota` is enabled, eligible non-Pro accounts form the
   preferred tier before the priority comparison.
6. Duplicate local slots for one upstream identity remain one balancing target.
   The hash identity must use the upstream identity when known so changing its
   healthy local representative does not create artificial capacity.
7. Failover excludes the failed upstream identity and deterministically chooses
   the next eligible candidate; success then persists the replacement sticky
   binding through the normal path.

`sticky_failover` remains an operator rollback mode. It preserves the former
strict priority/ID ordering for new sessions while retaining the same sticky,
quota, cooldown, duplicate-identity, parent-affinity, and failover contracts.

Sticky bindings have an idle TTL controlled by `CODEX_POOL_SESSION_AFFINITY_TTL_MS`. A successful request refreshes the binding expiry. Expired bindings are pruned and the next request selects a fresh account.

### 6.2 Sticky key derivation

Normalize Codex request identity from these sources, accepting both snake-case and compatible camel-case fields:

1. `client_metadata["x-codex-turn-metadata"]`
2. flat `client_metadata`
3. recognized compatibility headers and `X-Codex-Turn-Metadata`
4. top-level request fields

Malformed or absent metadata must be ignored rather than rejecting an otherwise valid Responses request. The routing source order is:

1. a live `previous_response_id` binding;
2. normalized Codex `thread_id`;
3. `X-Codex-Pool-Session`;
4. `X-Codex-Pool-Project`;
5. client `prompt_cache_key`;
6. `conversation`, `session_id`, or `conversation_id`;
7. an unbound `previous_response_id` hash;
8. hash of `(apiKeyId + model + normalized prompt prefix)`.

A live response binding is authoritative so a continuation cannot move to a different thread/account because of metadata version skew. Codex thread keys use:

```text
<model>:thread:<thread-id>
```

Final sticky key format:

```text
<model>:<stable-session-id>
```

Example:

```text
gpt-5.5:repo-main-worktree
```

### 6.2.1 Parent lineage and cache-key identity

Sticky routing identity, parent/child lineage, history inheritance, and the upstream cache key are separate contracts:

```text
sticky routing identity = thread_id
parent/child relationship = parent_thread_id + lineage_root_id
history inheritance = fork_turns, executed by Codex runtime
backend KV/prompt-cache behavior = upstream prompt prefix + prompt_cache_key
```

On the first request for an unbound child thread, resolve the parent's TTL-live thread binding and softly prefer its account. The preference must pass the same enabled, in-pool, authentication, quota, model, cooldown, duplicate-identity, and Pro-preservation checks as normal selection. If the parent account is ineligible or fails, use normal selection and failover. Success creates a separate child sticky binding; it must never merge the child route with the parent route.

Persist TTL-bounded thread bindings with thread, session, parent, lineage root, subagent kind, model, account, sticky key, upstream cache key, and activity timestamps. Reuse the sticky TTL and pruning lifecycle. Nested children inherit the known root; when only an immediate parent is known, use it as the temporary root and update the child binding after fuller parent state becomes available.

`CODEX_POOL_PROMPT_CACHE_KEY_POLICY` controls only the upstream key:

- `preserve`: retain a supplied client key. If absent, use `CODEX_POOL_PROMPT_CACHE_KEY_MODE` and `CODEX_POOL_PROMPT_CACHE_KEY_SCOPE` compatibility behavior.
- `lineage`: explicitly replace the client key with a hashed lineage-root key plus bucket.
- `project`: explicitly replace it with a hashed project key plus bucket, falling back to the user fingerprint when no project is supplied.
- `user`: explicitly replace it with a hashed API-key/user fingerprint plus bucket.

`preserve` is the compatibility default. No client key may be overwritten unless an explicit alternate policy is configured. Cache-key grouping must not alter sticky routing, lineage, or history inheritance.

### 6.2.2 MultiAgent tool and metadata forwarding

Pool is transparent to Codex MultiAgent V2 tool semantics. A `spawn_agent` schema containing `fork_turns` must reach upstream unchanged, and streamed or non-streamed tool-call arguments must return unchanged for `none`, positive integer strings, `all`, and omitted values. Codex decides whether to spawn and constructs the child history; Pool must not choose a fork tier, summarize history, or rewrite child input.

Forward only this Codex compatibility-header allowlist when present:

```text
X-Codex-Parent-Thread-ID
X-OpenAI-Subagent
X-Codex-Turn-Metadata
X-Codex-Window-ID
X-Codex-Installation-ID
```

Do not forward client authorization, API keys, hop-by-hop headers, or unrelated client-controlled headers.

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

Every successful request must classify the final routing result as exactly one
of:

```text
sticky_reuse
new_route_assignment
parent_affinity
parent_affinity_fallback
quota_failover
rate_limit_failover
auth_failover
transport_failover
repeated_5xx_failover
```

The first four describe selection without an upstream retry. A `*_failover`
outcome identifies why the successful request moved away from the prior or
failed account. Routing counters and request diagnostics must use these stable
values so operators can distinguish healthy distribution from cache-breaking
failover.

### 6.4 Failback behavior

Do not automatically fail back existing sticky sessions when the original account cooldown expires.

Use:

```text
failback = new_session_only
```

A session that failed over from `acct-a` to `acct-b` should remain on `acct-b` until it ends, unless `acct-b` also fails.

Optional exception: when `preserveProQuota` is enabled from the admin Console, if the sticky account is a Pro account and any eligible non-Pro account is available, select the non-Pro account and rewrite the sticky binding on success. This keeps Pro quota as the last-resort pool while preserving normal failover for deployments that leave the mode disabled.

On upstream quota exhaustion or rate limiting (`429`), mark the account/model in cooldown and retry the next eligible account in the same request. By default, the retry budget scales with the configured account count; `CODEX_POOL_MAX_RETRY_ACCOUNTS` can cap it.

Quota polling is advisory and must not become an inference availability gate for transient usage-endpoint failures. A quota refresh timeout, transport error, decode error, or upstream `5xx` may be retained for diagnostics, but it must not make an otherwise authenticated account ineligible. Only an explicit credential failure such as `401`, `403`, `invalid_token`, or a failed OAuth refresh that is classified as an auth failure may block routing. This distinction is required so exhaustion of a non-Pro account can still fail over to a healthy Pro account instead of returning a false initial `503`.

If at least one upstream account has already been selected and then failed in a request, exhausting the remaining failover candidates is an upstream failure (`502 bad_gateway`), not initial pool exhaustion (`503 no eligible account`). Initial `503 no eligible account` is reserved for the strict case where no account can be selected before any upstream attempt.

A transient upstream `5xx` without `Retry-After` must preserve sticky account locality for KV cache hit rate. Do not cool down or fail over the selected account on the first isolated `5xx`; return `502` for that request and let the next request retry the same sticky account. Only recent repeated `5xx` failures may cool down that account and move the sticky route to another upstream identity.

### 6.4.1 Codex model catalog compatibility

When Codex requests `/v1/models` with `client_version`, return the current Codex remote-model catalog shape rather than the generic OpenAI model-list shape. Every model record must include structured `supported_reasoning_levels` entries and a `default_reasoning_level`. Legacy reasoning aliases such as `model(high)` may remain accepted as request inputs, but the Codex catalog must collapse them into one canonical base model and advertise reasoning effort as capability metadata. Missing required model fields cause Codex's model manager to retry during app-server startup and must not be allowed to destabilize unrelated MCP client initialization.

The generic `/v1/models` response without `client_version` remains OpenAI-compatible and may continue exposing configured request aliases for non-Codex clients.

### 6.4.2 Hosted tool namespace conflicts

The ChatGPT Codex backend reserves the `image_gen` tool namespace implicitly: for current models it attaches its hosted image generation twin server-side even when the request declares no hosted tool. A Codex client running experimental features (multi-agent, image generation) or bundled fallback metadata can declare a client-side twin — verified against the live backend (2026-07), a `namespace` tool named `image_gen` inside an `additional_tools` input item is flattened upstream to `image_gen.imagegen` and the whole request is rejected with `Invalid Value: 'tools'. Function 'image_gen.imagegen' conflicts with a hosted tool in the same request.`

Before forwarding, the pool must drop client-declared tools whose name equals, or is namespaced under, a reserved namespace. Reserved namespaces are `image_gen` always, plus the namespace of any hosted tool declared in the same request (`image_generation`/`image_gen` reserves `image_gen`; `web_search`/`web_search_preview` reserves `web_search` — a bare `web_search` function without the hosted twin must pass through). Filtering must cover both places Codex declares tools — the top-level `tools` array and `additional_tools` items inside `input` (Codex 0.144+) — and both shapes: flat `function`/`custom` tools and `namespace` tools whose flattened `namespace.function` names collide. Hosted tools themselves are kept because upstream owns the namespace either way. Non-tool input items must pass through unmodified.

This sanitization is a compatibility guard for client behavior the end user cannot fix; it is not a substitute for catalog coverage (5.2.1), which keeps well-behaved clients from attaching conflicting tools in the first place.

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

Dashboard availability must mirror the same active routing gates. Historical health fields such as `LastFailureReason` and `ConsecutiveFailure` are diagnostic after their cooldown expires; they must not keep a slot labeled unavailable when auth, quota metadata, pool membership, model filters, and cooldown state are otherwise eligible.

---

## 7. Account Lifecycle

### 7.1 Account object

```json
{
  "id": "acct-org-a",
  "label": "us***er@example.com",
  "displayName": "us***er@example.com",
  "email": "us***er@example.com",
  "authType": "codex_device_auth",
  "enabled": true,
  "inPool": true,
  "priority": 100,
  "allowedModels": [],
  "excludedModels": [],
  "planType": "plus",
  "planRank": 200,
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
- Quota progress colors are an operational warning scale: healthy capacity uses the cool Jade/Wasabi range, then becomes amber, Persimmon, and finally red as remaining capacity approaches zero. Do not use one neutral color for all values or make a near-zero bar appear healthy.
- Admin API account responses must not expose full email addresses, upstream account IDs, upstream URLs, API keys, `codexHome`, or auth file paths. A device-auth credential slot is the primary local identity. Email, plan, upstream account ID, and organization values are descriptive credential metadata; browser-facing account labels may use masked email for recognition, but routing, storage, and management actions must use the local credential slot ID.

### 7.2 Add account

```http
POST /admin/api/accounts
Content-Type: application/json
```

Request:

```json
{}
```

The admin UI sends no user-entered account fields. It creates an empty Codex device-auth credential slot, keeps it disabled and out of the pool while login is pending, immediately starts device auth, and shows only the verification URL, user code, local copy controls for those two values, and a 15 minute countdown. The slot is enabled and added to the pool only after device auth succeeds, the sidecar auth record is prepared, and quota/account metadata refresh has run. Failed or cancelled login leaves the slot disabled and out of the pool. `id` and `label` are local slot identifiers and are not derived from email, plan, or upstream account metadata. `email`, `planType`, `planRank`, upstream account ID, organization, and model access are resolved after device auth from the authenticated Codex credential as metadata; routing treats an empty `allowedModels` list as no per-account model restriction.

Response:

```json
{
  "ok": true,
  "account": {
    "id": "acct-org-a",
    "label": "acct-org-a",
    "email": "",
    "planType": "",
    "enabled": false,
    "inPool": false
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
POST /admin/api/jobs/{jobId}/cancel
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
  "codeExpiresAt": "2026-06-20T16:15:00Z",
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

Public trusted-user endpoints:

```http
POST /admin/api/public-dashboard/accounts/{poolRef}/pool-add
POST /admin/api/public-dashboard/accounts/{poolRef}/pool-remove
```

`poolRef` is an opaque per-process account reference returned by the public dashboard. The public dashboard may return a partially masked email for account recognition, but must not expose full email addresses, account IDs, upstream URLs, API keys, sticky session keys, traffic details, quota error codes, or upstream error bodies.

Legacy authenticated owner endpoints:

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
  "email": "us***er@example.com",
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
      "email": "us***er@example.com",
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
    "hourly": {
      "percentage": 80,
      "resetAt": 1781934000,
      "windowMinutes": 300,
      "present": true
    },
    "weekly": {
      "percentage": 50,
      "resetAt": 1782016800,
      "windowMinutes": 10080,
      "present": true
    }
  },
  "usageUpdatedAt": "2026-06-21T12:30:00Z",
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
    "hourly": {
      "percentage": 80,
      "resetAt": 1781934000,
      "windowMinutes": 300,
      "present": true
    },
    "weekly": {
      "percentage": 50,
      "resetAt": 1782016800,
      "windowMinutes": 10080,
      "present": true
    }
  },
  "usageUpdatedAt": "2026-06-21T12:30:00Z"
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

Quota refresh must not block normal `/v1` request handling. Run refresh jobs in background with bounded concurrency. The service refreshes Codex quotas once after a successful device-auth login, once during startup, and then every five minutes. `remainingQuota` is a routing hint derived from the lowest present quota window.

### 9.1 Duplicate upstream identity guard

Device-auth slots are local management records, not proof of separate upstream capacity. If multiple enabled, in-pool slots resolve to the same upstream ChatGPT/Codex account identity, routing must treat only one local slot as the representative for that identity at a time. Duplicate slots must be shown as duplicate/standby in the dashboard and must not be selected as same-request failover capacity after the representative slot fails. This protects the pool from reusing the same upstream account through multiple local device-auth sessions from the same host/IP, which can amplify shared quota, refresh-token revocation, and team-workspace policy failures.

The representative is chosen from currently usable local credential copies, not permanently fixed to the highest-priority slot. ChatGPT/Codex device-auth copies for the same visible upstream identity can carry different quota snapshots or session-scoped rate limits. If the current representative has zero quota, an active cooldown, or a persisted auth/quota metadata error, a healthy duplicate credential slot may become the single representative for a later request. This avoids unnecessary fallback to Pro when a non-Pro identity still has a usable local credential copy.

Quota hints for duplicate slots participate only in representative selection for later requests. They must not allow another same-identity slot to be used as an immediate retry target within the same request after a 429, upstream 5xx, or auth failure from the representative.

A duplicate slot with a persisted auth/quota metadata error must not provide quota evidence for another same-identity representative. Manual or stale quota hints from that errored slot are not reliable capacity; otherwise a zero-quota representative can remain selectable and repeatedly return 429/503 instead of falling through to Pro or another upstream identity.

---

## 10. Usage Statistics

Prompt-cache aggregates must distinguish `main` and `subagent` traffic at
account + model + agent-kind granularity. Store request count,
usage-observed-request count, input tokens, cached tokens, cache-hit-request
count, cache-eligible-request count, cold starts, cache-write tokens,
cache-write input tokens, and cache-write-observed-request count for both
kinds, plus parent-affinity hits, parent-affinity fallbacks, lineage failovers,
and all routing failovers.

Metric definitions:

```text
token read hit rate = cached tokens / input tokens
request hit rate = cache-hit requests / usage-observed requests
cache write rate = cache-write tokens / input tokens for write-observed requests
cold eligible rate = cold requests / cache-eligible requests
cache eligible = input tokens >= 1024
```

An absent upstream cache-write field is unavailable data, not a confirmed zero.
The parser must accept nested and top-level compatible cache-read and
cache-write field names and preserve an explicitly observed streaming write
value if a later usage summary omits it.

Dashboard lifetime and reset-window views must expose these values. For
request-level diagnosis, persist a rolling list of successful routing/cache
events bounded to 500 entries and 24 hours. Raw request, response, thread,
lineage, sticky, and prompt-cache identifiers must be domain-separated hashes
before persistence. Prompt/input content, tool arguments, credentials, email,
and upstream account identities must never be stored in this event list. The
authenticated management projection must omit local/upstream account IDs, use
masked display labels plus opaque hashes, return newest first, and expose at
most the newest 50 events. The unauthenticated public dashboard must not return
this request-level event list because traffic timing, account selection, and
failover correlation are management diagnostics. This bounded event list is
operational correlation data, not a durable token ledger.

Dashboard metric provenance is a presentation contract. Raw usage values
reported by OpenAI/upstream (cache-read and cache-write tokens), counters
observed by Pool (requests, affinity, and failovers), and rates calculated by
Pool must be grouped and labeled separately. Color may reinforce the
distinction but must not be the only source indicator. An absent upstream
cache-write field must render as unavailable (`—`) rather than zero.

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
      "email": "us***er@example.com",
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
  "email": "us***er@example.com",
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
      "routingStrategy": "sticky_balanced",
      "preserveProQuota": false,
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
    "lanBaseUrl": "http://<lan-host>:<api-port>/v1",
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

### 11.1 Runtime Settings

```http
POST /admin/api/settings
Content-Type: application/json
```

Request:

```json
{
  "preserveProQuota": true
}
```

`preserveProQuota` backs the admin Console `Use Pro last` switch and is persisted in `/data/config.json`.

---

## 12. Public `/v1` Request Handling

### 12.1 `/v1/responses`

Accept OpenAI Responses-style JSON.

Required behavior:

1. Authenticate client API key.
2. Parse model and thinking suffix.
3. Resolve alias.
4. Normalize Codex thread/parent identity and derive an independent sticky route and upstream cache key.
5. Pick account using sticky failover, including soft parent affinity for a new child.
6. Refresh access token if expired.
7. Send the upstream request with only allowlisted Codex compatibility headers.
8. Stream SSE if request asks for stream without rewriting tool-call arguments.
9. Track usage, main/subagent cache metrics, thread/response bindings, and emit a usage event.
10. On quota exhaustion, mark account/model cooldown and retry the next available account.

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
  "accountEmail": "us***er@example.com",
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
      "email": "us***er@example.com",
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
  "routingStrategy": "sticky_balanced",
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

## 16. Dashboard And Admin UI Requirements

Build a minimal web UI on the single admin port.

### 16.1 Pages

Single-page dashboard is enough. `/` on the admin port and `/admin` serve the same page. Public mode is visible without admin login and may show pool status plus join/leave pool controls. Management mode is unlocked with the admin password and is reserved for account creation, deletion, device-auth repair, and sticky-session inspection.

The footer version must be injected from git-derived build metadata. Keep the HTML source as a placeholder and build local images through `scripts/build-local-image.sh` or an equivalent command that passes `CODEX_POOL_VERSION` and `CODEX_POOL_COMMIT`. Do not manually edit the footer string for releases; that has already caused deployed fixes to be indistinguishable from stale builds.

Sections:

1. Service status
2. Account pool
3. Account health/cooldowns and quota hints
4. Pool-wide cache reset window
5. Recent routing/cache diagnostics in management mode
6. Sticky sessions in management mode

### 16.2 Account table columns

```text
Account
Status
Quota
Routing / Pool
Main cache
Subagent cache
Affinity
Failovers
Last activity (management mode)
Actions
```

Main and subagent cache cells must show the calculated token read hit rate,
then separate OpenAI usage rows for cached/input tokens and
cache-write/cache-write-input tokens, with a Pool-calculated ratio on each
row. Cache-write values and ratio must be `—` when unavailable. Request and
cold counts belong in the diagnostic tooltip rather than the visible cell so
adjacent Affinity and Failovers columns cannot truncate them. Affinity is the
compact Pool-observed
`hit/fallback` count. Failovers is the total Pool-observed successful
routing-failover count.

The pool-wide cache window must show the total request count plus total
cache-read and cache-write tokens since reset. It must group and visibly label
OpenAI usage values, Pool-observed counters, and Pool-calculated read/write,
request-hit, and cold rates as distinct sources. The cold count is
Pool-observed; its rate is calculated and must not share one mixed-source
value. Main/subagent rates in this top summary must not append per-kind request
counts; the request total is the single pool-wide Requests metric.

The management-only recent routing/cache table must show time, agent kind,
masked account label, routing outcome, cache read, cache write, and input
tokens. It may expose domain-separated identifier hashes in authenticated UI
affordances, but never raw identifiers or account IDs. Public mode must neither
render nor receive request-level routing/cache events.

### 16.3 Account actions

Public mode:

```text
Add to pool
Remove from pool
```

Duplicate upstream accounts:

```text
Duplicate
```

Management mode:

```text
Add account
Login / Re-login
Delete account and purge credentials
Clear sticky session
```

### 16.4 Sticky session table

Management-mode Active routes must show enough information for the owner to identify each sticky session. The visible row must include the model, assigned account label, last-used time, expiry, and a masked route key. Do not regress this to model-only rows: without a route key, multiple active sessions for the same model become indistinguishable. Do not render the full route key as visible text because it may include project names, client session IDs, or prompt-derived routing hints; use the full key only for authenticated management actions such as clearing the route.

```json
{
  "key": "gpt-5.5:repo-main",
  "modelId": "gpt-5.5",
  "accountId": "acct-org-a",
  "createdAt": 1781930000000,
  "lastSuccessAt": 1781930500000,
  "expiresAt": 1782016900000,
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
11. Token expiration is refreshed by the account's sidecar credential on the same account, not treated as quota exhaustion or failover.
12. Admin UI displays account health, cooldown until time, quota windows, token usage stats, and recent errors.
13. Logs do not expose tokens, API keys, or auth files.
14. Public `/v1` API is protected by configured API key.
15. Admin UI is protected by password and defaults to loopback-only binding.
16. Main, child, and sibling Codex threads retain distinct sticky and response chains.
17. Eligible children softly prefer the parent account without bypassing routing safeguards.
18. Sticky routing and upstream prompt-cache keys remain independent; `preserve` never rewrites a client key.
19. Explicit `lineage`, `project`, and `user` policies generate deterministic hashed/bucketed keys.
20. MultiAgent `fork_turns` schema, arguments, and Codex-assembled child history pass through unchanged.
21. Thread/lineage bindings are TTL-pruned and main/subagent cache and affinity metrics are observable.
22. Unbound session keys distribute across equal-priority healthy accounts, while concurrent first requests for one key select the same account before persistence.
23. Existing sticky sessions never move solely for balancing, and `sticky_failover` remains available as an immediate rollback mode.
24. Cache observability distinguishes token-read hit rate, request-hit rate, cache-write availability/rate, and cold eligible rate; a missing write field is never treated as zero.
25. Successful requests expose stable routing outcomes for assignment, sticky/parent reuse, and quota/rate-limit/auth/transport/repeated-5xx failover.
26. Request-level routing/cache diagnostics are limited to 500 entries and 24 hours, persist only hashed operational identifiers, omit account IDs from authenticated browser responses, render only the newest 50 rows, and are absent from the unauthenticated public dashboard.

---

## 20. Important Behavioral Notes

1. Do not treat OAuth access-token expiration as quota exhaustion.
2. Balance new sessions, never individual requests; existing sticky sessions must not move solely for utilization balancing.
3. Do not fail back existing sessions automatically after cooldown expires.
4. Do not block live `/v1` requests while refreshing quota for all accounts.
5. Do not expose absolute remaining token quota unless upstream provides it reliably.
6. Show both quota-window percentages and actual token usage counters.
7. Keep account/model cooldowns separate.
8. Keep account credentials isolated by `CODEX_HOME` directory.
9. Use per-account locks when refreshing credentials.
10. Never print raw upstream error bodies if they may contain secrets.
