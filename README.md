# Codex Pool Provider

A Dockerized, single-owner pool of Codex/ChatGPT accounts. Codex clients (CLI,
Desktop, IDE extension) talk to the pool as if it were OpenAI's Codex backend;
the pool places each conversation on one of its accounts and sends the request
upstream exactly as a native Codex of the pinned release would.

- **Looks native upstream.** Requests leave through `codex-upstream`, a Rust
  process built from Codex's own HTTP client crate, so TLS, HTTP/2, header order
  and compression are those of a native Codex. A conformance harness measures
  this before every deploy (see [Development](#development)).
- **Keeps a conversation on one account.** A main agent and all its subagents
  stay bound to one account, so the upstream prompt cache keeps working; it moves
  only when that account can no longer serve.
- **Spreads conversations across accounts.** New sessions prefer idle accounts,
  weighted by remaining allowance and plan size, with extra weight on accounts
  whose reset credits are about to expire.
- **Docker only.** Nothing is installed on the host; build, tests and the
  service all run in containers. All state lives in the `codex-pool-data`
  volume at `/data`.

The full product contract is [SPEC.md](SPEC.md); repository rules are in
[AGENTS.md](AGENTS.md).

## Contents

1. [Quick start](#quick-start)
2. [Connecting Codex](#connecting-codex)
3. [Accounts](#accounts)
4. [Routing](#routing)
5. [Dashboard](#dashboard)
6. [How the pool talks upstream](#how-the-pool-talks-upstream)
7. [Exposure and security](#exposure-and-security)
8. [Configuration](#configuration)
9. [Development](#development)

## Quick start

1. Build the image (the script stamps the git version into the dashboard footer,
   e.g. `v2026.10.20-c722de9`, with `-dirty` for uncommitted changes):

   ```bash
   sh scripts/build-local-image.sh
   ```

2. Generate the admin password hash inside the image:

   ```bash
   docker run --rm -e CODEX_POOL_ADMIN_PASSWORD='choose-a-strong-password' \
     codex-pool:local hash-password
   ```

3. Copy [.env.example](.env.example) to `.env` and set at least
   `CODEX_POOL_API_KEY` (a long random client key) and
   `CODEX_POOL_ADMIN_PASSWORD_HASH` (the `pbkdf2-sha256:...` value above).
   Never commit `.env`.

4. Create the volume once and deploy:

   ```bash
   docker volume create codex-pool-data
   sh scripts/deploy.sh
   ```

   [compose.yaml](compose.yaml) publishes port `8317` and joins the
   `edge_proxy` network, where an HTTPS reverse proxy (Caddy) reaches the pool as
   `codex-pool:8317`; create that network or remove it if you have no proxy.

5. Open the control page and add accounts (see [Accounts](#accounts)).

| Path | What |
|---|---|
| `/` and `/admin` | Control page: public status, and management after password login |
| `/k/<key>/backend-api/codex/` | Native Codex endpoint (recommended for Codex) |
| `/v1` | OpenAI-compatible API for custom providers and other clients |
| `/healthz` | Health check (needs the client key) |

### Deploying updates

`sh scripts/deploy.sh` builds the image, waits until no provider request is in
flight, then recreates the container. If requests never pause it restarts after
`CODEX_POOL_DEPLOY_IDLE_WAIT` seconds (default 600; `0` restarts at once) and
relies on the shutdown drain, which lets in-flight requests finish for up to
`CODEX_POOL_SHUTDOWN_GRACE` (default `120s`). Codex reconnects on its own after a
restart. `--force` restarts a pool that predates in-flight tracking.

## Connecting Codex

### Native endpoint (recommended)

Keep Codex's built-in OpenAI provider and your own ChatGPT login (`codex login`),
and move only its base URL to the pool, with the pool's client key in the path:

```toml
# ~/.codex/config.toml (read by the Codex CLI, Desktop and IDE extension alike)
openai_base_url = "https://<pool-host>/k/<pool-api-key>/backend-api/codex"
```

Do not set `model_provider`, and use real model names (for example
`model = "gpt-5.5"` with `model_reasoning_effort`). Codex then sends the pool
exactly what it sends OpenAI, and the pool forwards it unchanged, replacing only
the credential with the selected account's. Your own login is still used,
directly with `chatgpt.com`, for the account, plugin and analytics calls a native
Codex makes. Responses over HTTP and WebSocket, compaction, models, image
generation and the other Codex backend paths are served; voice (realtime) is
not. Details: SPEC.md 2.3.3.

Use HTTPS for this endpoint: Codex sends your ChatGPT login to the pool in
`Authorization` (the pool drops it), and the key is in the URL, so no proxy in
front of the pool may log `/k/` paths.

### Custom provider (`/v1`)

Without changing `openai_base_url`, Codex can use the pool as a custom provider.
It works the same way, but Codex sends a custom provider a smaller request (no
MCP resource tools or `image_gen`, no internal metadata), which the pool cannot
restore:

```toml
model = "gpt-6.1-sol(xhigh)"
model_provider = "codex-pool"

[model_providers.codex-pool]
name = "Codex Pool"
base_url = "https://<pool-host>/v1"
env_key = "CODEX_POOL_API_KEY"
wire_api = "responses"
supports_websockets = true   # optional: the Responses WebSocket, cheapest for upstream
```

Set `CODEX_POOL_API_KEY` in Codex's environment to the pool's client key. Model
aliases and a `(thinking-tier)` suffix are translated on this path. The checked-in
[test config](test/codex-config.toml) is the same contract.

### Other clients

`/v1` also serves `GET /v1/models`, `POST /v1/responses` (and `/compact`),
`POST /v1/chat/completions` (translated to Responses) and image generation. The
client key is accepted as a Bearer token, a raw `Authorization`, `X-Api-Key`,
`X-Goog-Api-Key`, or the `key` / `auth_token` query parameters.

## Accounts

### Adding an account

On the control page select **Access**, sign in, then **Add account**. The pool
creates an empty credential slot, keeps it out of the pool, and runs Codex device
auth inside the container (`CODEX_HOME=/data/accounts/<slot>/.codex`). The page
shows only the verification URL, the user code and a 15-minute countdown. After
you finish the browser login, the pool reads the account's plan, email and
workspace from the credential, refreshes its quota, and adds it to the pool. No
fields or models are entered by hand.

Each slot is its own credential, even when two slots share an email. The plan,
email and workspace are shown for recognition only; the slot ID is what
management uses.

### Repairing a sign-in

When a credential expires, use **Repair sign-in** on the same row instead of
removing the account. Routing to the slot pauses while Codex rewrites the
credential; requests already bound to it get a retryable `503`
`account_authenticating` rather than moving and losing their cache. If the new
credential is the same upstream account, the slot keeps its bindings, counters and
history; otherwise those are reset. A repair that does not finish keeps the slot
out of routing, even across restarts.

On the public page, an invalid in-pool credential shows **Repair** so a shared
user can finish the device flow without the admin password. That path only
accepts the same upstream account and never exposes an admin-started job. Only
one device-auth job runs at a time.

### Duplicates and workspace members

Routing follows the upstream identity, which is the workspace **member**
(workspace plus email): members of one Business or Team workspace each hold
their own allowance and are separate targets. Several slots for the same login
are one target: they never count as backup capacity for each other, and a
healthy copy can stand in when the current one has failed.

### Per-account protection and notes

- **Quota protection** (off by default): stop new routing to a slot once its
  reported 5-hour window (else weekly window) is at or below a threshold.
  Missing or failed quota readings never block a slot.
- **Owner note**: an 80-character plain-text label, visible and editable on the
  public page so shared users can mark whose account it is. Never put secrets
  in it.
- **Join/Leave pool**: on the public page by default
  (`CODEX_POOL_PUBLIC_DASHBOARD=false` hides it).

## Routing

### Placing a new conversation

A new conversation is placed in steps:

1. **Red outline**: accounts holding a reset credit that expires within seven
   days come first, Pro accounts included, since an unspent credit is lost
   either way. While one of them can take the conversation it goes to one of
   them, even a busy one.
2. **Plan tier** (with **Use Pro last** on): ordinary accounts, then Pro 100,
   then Pro 200/500, applied within the red-outlined accounts or, when none can
   serve, within the rest.
3. **Idle first**: an account no conversation is using is preferred; only when
   all are busy is one shared, with the weight divided by (1 + sessions on it)².
4. **Weighted choice**: remaining allowance (the tighter window) × plan
   multiplier, and among red-outlined accounts × 2 at seven days out rising to
   × 7 at expiry. A weighted rendezvous hash makes the choice, so concurrent
   first requests for one conversation agree.

A conversation already bound stays where it is, with two exceptions that move it
(main agent and subagents together, at the cost of one prefill) on its next
request: **Use Pro last** moves it down to a lower plan tier, and the **red
outline** moves it from an account without one to a red-outlined account (a Pro
one included), so a failover caused by a passing overload does not keep it away from
an expiring credit. The pool never spends a reset credit; that stays the account
owner's decision.

| Plan (as Codex names it) | Shown as | Multiplier |
|---|---|---|
| `prolite` | Pro 100 | 5× |
| `pro` | Pro 200 | 10× (assumed; legacy 20× subscribers read low until 2026-10-29) |
| `promax` | Pro 500 | 25× |
| Business Premium seat | | `CODEX_POOL_PREMIUM_SEAT_MULTIPLIER` (default 5×, assumed) |
| everything else | | 1× |

A multiplier the account itself reports always wins over these assumptions, and
the dashboard marks every reading that rests on an assumption.

### Keeping a conversation together

A Codex thread, every request continuing one of its responses, and, with
**Keep subagents together** on (the default), its whole lineage of subagents are
strictly bound to one account. When that account can no longer serve any member,
the whole lineage moves to one replacement that serves all its models, and pays a
full prefill there once. It never falls back to its previous account. Turning the
switch off restores per-thread bindings where a subagent only prefers its
parent's account on its first request.

The pool reads thread, parent and lineage identity from Codex's
`client_metadata` and headers. It never changes `spawn_agent` arguments or
`fork_turns`; history inheritance belongs to Codex.

### Use Pro last

With **Use Pro last** on, accounts are spent in three tiers: ordinary accounts
first, then Pro 100 (Pro Lite), then Pro 200 and Pro 500. A conversation that had
to use a higher tier moves down as soon as a lower tier can serve it.
`CODEX_POOL_PRESERVE_PRO_QUOTA` sets only the initial default; the switch is
saved in `/data/config.json`.

### When upstream refuses

The pool keeps trying on the same account before it moves a conversation, as a
native Codex (which has only one account) would:

- **Overloaded or a server fault** (`server_is_overloaded`, `server_error` and
  similar): retried on the same account after 1 s, 2 s and 4 s.
- **Unreachable** (a connection that fails or drops before an answer): retried on
  the same account after 0.5 s and 1 s. A dropped WebSocket sets no cooldown, so
  Codex's reconnect returns to the same account.
- **Allowance spent, rate limit, revoked credential**: the account cools down and
  the request moves at once.
- **Every account refusing**: capacity retries wait and try again (up to
  `CODEX_POOL_CAPACITY_RETRY_LIMIT` rounds) before the refusal reaches Codex.

A streaming response is held until its first content, so a refusal before that is
retried invisibly; anything after content is relayed as is. Failure classes
follow Codex's own parsers; a failure the pool cannot classify is passed through
and counted as **Unrecognized failures**.

### Prompt cache key

`CODEX_POOL_PROMPT_CACHE_KEY_POLICY=preserve` (default) keeps the key Codex
sends. `lineage`, `project` and `user` replace it with a hashed, bucketed key.
These settings never affect routing, and the native endpoint never rewrites
anything.

## Dashboard

The control page refreshes every five minutes (**Refresh** reads at once).

- **Accounts**: state (`Ready`, `Low quota`, `Protected`, `Exhausted`,
  `Cooldown`, `Error`, `Login needed`, `Signing in`, `Duplicate`, `Disabled`,
  `Standby`), every quota window with its own percentage and reset time, credits,
  spend control, and reset credits with the nearest expiry (red within seven
  days). Quota is read from `/wham/usage` every five minutes; a value upstream
  does not report shows as `Not reported`.
- **Cache**: main and subagent cache reads per account, token and request hit
  rates, cold rate, and affinity/fallback counts. **Reset window** starts a new
  comparison window without deleting totals.
- **Recent routing & cache** (management only): the newest 50 requests with how
  each was routed (sticky reuse, new route, lineage follow, or which kind of
  failover) and which entry it came through (`native/http`, `native/ws`,
  `v1/http`, `v1/ws`). Identifiers are stored as hashes; at most 500 entries,
  24 hours.
- **Throughput**: up to 48 hours of output tokens per second against prompt-cache
  hit rate, kept in memory only.
- **Unrecognized failures**: upstream failures the pool could not classify;
  anything nonzero needs a look (also `class=unknown` in the log).

The public view shows pool status and join/leave only, with masked emails; it
never shows account IDs, keys, sessions or request-level details.

## How the pool talks upstream

The pool sends each request to the Codex backend itself, with the selected account's credential, and adds
only what a native Codex adds (`ChatGPT-Account-ID`, `X-Codex-Routing-Hint`). It
refreshes the account's token five minutes before expiry, and once more on a
`401` before treating the account as signed out.

The last hop is `codex-upstream`, a loopback-only Rust process in the same
container built from Codex's HTTP client crate at the pinned release
(`rust-v0.163.0` in [upstream/Cargo.toml](upstream/Cargo.toml)). It opens one
connection per request, as Codex does.

The pool's own calls also go out the way Codex makes them. Token refresh, usage,
account check and reset-credit reads use Codex's endpoints and header order, with
the identity of the Codex that last used the account. A quota-window prime is one
read-only turn of the bundled Codex CLI that holds no refresh token.

A bundled CLIProxyAPI sidecar used to be the gateway; it rewrote what Codex
sent and was removed (issue #32). Setting `CODEX_POOL_CODEX_GATEWAY_MODE=sidecar`
now stops startup with an error.

## Exposure and security

- **One listener.** The provider API and the control page share port `8317`.
  A non-loopback bind needs `CODEX_POOL_ALLOW_REMOTE_ADMIN=true`, acknowledging
  that the admin login is reachable wherever `/v1` is. Owner APIs still need the
  signed session and CSRF token.
- **HTTPS.** The pool does not terminate TLS. For anything beyond a trusted LAN,
  put it behind a TLS reverse proxy (`compose.yaml` joins Caddy's `edge_proxy`
  network) and sign in to the admin page only over HTTPS. Set
  `CODEX_POOL_TRUSTED_PROXIES` to the proxy's network so the login lockout counts
  real clients; `X-Forwarded-For` is believed only from there.
- **Credentials** live only in `/data/accounts/<slot>/.codex/auth.json` in the
  volume, never in the repository or the image. Logs redact tokens and keys.

## Configuration

Everything is set in `.env`; [.env.example](.env.example) lists the usual
settings and SPEC.md 2.1 lists them all. The ones most often changed:

| Variable | Default | Purpose |
|---|---|---|
| `CODEX_POOL_API_KEY` | (required) | Client key for `/v1`, `/healthz` and the native endpoint path |
| `CODEX_POOL_ADMIN_PASSWORD_HASH` | (required) | Output of `hash-password` |
| `CODEX_POOL_DEFAULT_MODEL` | `gpt-6.1-sol(xhigh)` | Model when a request names none |
| `CODEX_POOL_TRUSTED_PROXIES` | none | Reverse proxy addresses/CIDRs |
| `CODEX_POOL_SESSION_AFFINITY_TTL_MS` | `86400000` | Idle time after which a conversation's binding expires |
| `CODEX_POOL_ROUTING_STRATEGY` | `sticky_balanced` | `sticky_failover` restores strict priority order |
| `CODEX_POOL_QUOTA_FLOOR_PERCENT` | `1` | Share of a window held back so overage is not billed to credits |
| `CODEX_POOL_PREMIUM_SEAT_MULTIPLIER` | `5` | Assumed Business Premium seat size |
| `CODEX_POOL_PRESERVE_PRO_QUOTA` | `false` | Initial **Use Pro last** setting |
| `CODEX_POOL_LINEAGE_AFFINITY` | `true` | Initial **Keep subagents together** setting |
| `CODEX_POOL_SHUTDOWN_GRACE` | `120s` | Drain time on stop (keep `stop_grace_period` above it) |

### Data and logs

```text
/data/config.json                     accounts and Console settings
/data/accounts/<slot>/.codex/         each account's Codex credential
/data/state/runtime.json              bindings, cooldowns, health, quota, statistics
/data/logs/service.log                the pool's log, kept across deploys
```

Docker's own log goes with the container on every deploy, so
`/data/logs/service.log` is where to trace anything from before the last deploy.
It rotates at 20 MiB, keeping three older files.

## Development

All tooling runs in Docker.

```bash
# Go formatting, vet and tests (about four minutes)
docker run --rm -v "$PWD:/src" -w /src golang:1.24-bookworm sh -c \
  'test -z "$(/usr/local/go/bin/gofmt -l ./cmd ./test)" && /usr/local/go/bin/go vet ./... && /usr/local/go/bin/go test -p 1 -buildvcs=false ./...'

# The control page's JavaScript renderers
sh scripts/test/render-web-assets.sh

# The codex-upstream crate: rustfmt, clippy and cargo test (the first run
# compiles Codex's dependencies; later runs reuse cached Docker volumes)
sh scripts/test/rust-check.sh

# Integration tests: a real `codex exec` against a mock upstream. They build
# codex-pool:test, never the live codex-pool:local, and refuse the live
# container and volume names (scripts/test/isolation.sh).
sh scripts/test/integration-codex-config.sh
sh scripts/test/integration-device-auth-failover.sh
sh scripts/test/integration-codex-subagent.sh
sh scripts/test/integration-routing-cache-observability.sh
```

### Codex wire conformance

The pool must behave like a native Codex of the pinned release in both
directions (SPEC.md 2.3.2). Before any deploy that changes how it talks to
upstream, both checks must pass; there is no CI.

```bash
sh scripts/conformance/codex-conformance.sh check        # how the pool reads upstream, against Codex's source
sh scripts/conformance/wire-capture.sh                   # what the pool sends, against a native Codex
sh scripts/conformance/codex-conformance.sh releases     # Codex releases newer than the pin
sh scripts/conformance/codex-conformance.sh diff <tag>   # every change to the watched Codex files up to <tag>
```

- `check` confirms that every upstream shape in
  `cmd/codex-pool/testdata/codex-upstream-failures.json` is still in the Codex
  source it was copied from, and that every error code Codex parses has a shape
  there.
- `wire-capture.sh` runs the same turns from a native Codex CLI and through a pool
  built from the working tree, against a fake `chatgpt.com` on an internal
  network. It compares TLS (JA4), HTTP/2 settings, connections per request,
  header order and values, and bodies. It covers HTTP, the WebSocket, the native
  endpoint, a subagent, compaction and a failover between two accounts, plus the
  pool's own calls and a quota prime. It also reports the custom-provider client
  gap separately. Output goes to `scripts/conformance/out/` (git-ignored).

Moving to a new Codex release is described in AGENTS.md.

### Committing

Install the hook once with `git config core.hooksPath .githooks`. Before each
commit run the tests and the staged-change audit, review `git diff --cached`
for credentials, then commit with the review acknowledgement:

```bash
sh scripts/security/precommit-security-audit.sh --check
CODEX_ACCOUNT_SECURITY_REVIEWED=yes git commit -m "your message"
```

The hook blocks credential and runtime-data paths, API keys, session tokens, JWTs
and OAuth values. Never stage `.codex/`, `auth.json`, `.env`, `/data`, account
data, OAuth responses or provider keys. See AGENTS.md for the full checklist.
