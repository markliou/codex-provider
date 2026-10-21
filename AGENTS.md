# Repository Rules

## Rust Port Branch

This is the `rust-port` branch. Before any work here, read `RUST_PORT.md` in full and follow it: it says what this branch may and may not touch (never the production container, its image tag, its data volume, `.env` or real account credentials, and never `scripts/deploy.sh`), the order of the migration steps, and the conditions every step must keep. Remove this section when the port is merged and finished.

## Portability

Do not install software, runtimes, package managers, libraries, build tools, or test tools on the host. All build, test, formatting, and service tooling must run inside Docker.

## Implementation Comments

Treat comments that explain behavioral intent as part of the implementation, not optional cleanup. When changing non-obvious behavior, fragile routing/auth/session logic, security or exposure boundaries, public/admin UI wording, or code that was previously changed incorrectly, add or update a nearby source comment explaining why the behavior exists and what must not be simplified away.

Comments should explain the purpose and regression risk, not restate the code. If the behavior is a product contract, update `SPEC.md` in the same change. Do not remove an intent comment unless the protected behavior is intentionally changed and the replacement code, comment, and spec text preserve the new intent.

## Codex Wire Conformance

The pool must behave like a native Codex of the release pinned in `upstream/Cargo.toml`, in both directions: what it sends upstream, and how it reads and acts on what upstream sends back. The source of truth is Codex's source at that release and measured traffic, never an assumption about either. See SPEC.md 2.3.2.

- Any change to how the pool talks to upstream (headers, body, transport, stream or WebSocket parsing, error classification, retry or failover on an upstream answer) must cite the Codex source it matches in a nearby comment, and must keep `sh scripts/conformance/codex-conformance.sh check` passing. When the change touches what is sent, also run `sh scripts/conformance/wire-capture.sh` and keep it passing.
- The native Codex endpoint (`/k/<key>/backend-api/codex/`) forwards Codex's request exactly as written. Never add a body or header rewrite to that path; a rewrite needed for custom-provider or foreign clients belongs behind `isNativeCodexRequest`, and `wire-capture.sh` must keep reporting the native endpoint equal to a native Codex.
- A test that fakes upstream must answer in shapes recorded in `cmd/codex-pool/testdata/codex-upstream-failures.json`. A shape that is not there is copied from Codex's source or tests first, with its file and a verbatim marker, never written from memory.
- Moving to a new Codex release: run `codex-conformance.sh diff <tag>`, read every hunk of the watched files, match the pool to each change or note why it does not apply, update the tag, `Cargo.lock`, the fixture's release and shapes together, then pass `check` and `wire-capture.sh` before deploying.
- Before every deploy that changes how the pool talks to upstream, the Go tests, `codex-conformance.sh check` and `wire-capture.sh` must all pass on the commit being deployed; there is no CI to run them for you.
- The pool's own upstream calls (refresh, usage, account metadata, reset credits, priming) are made the way a native Codex makes them (SPEC.md 2.3.2, own calls). Never add one from Go's own client, with browser headers, or with a body the pool composes; route it through `sendCodexOwnRequest` with the header order captured from a native Codex, or through the bundled Codex CLI.
- After a deploy, check the dashboard's Unrecognized failures and the log for `class=unknown`. Any occurrence means upstream sent a shape the pool does not read as Codex does; find it, compare it with Codex's source, and record it.
- Account semantics (identity, allowance, quota) are the pool's own and cannot be checked against Codex. Routing and the dashboard must share one definition of each, held together by a test, and an assumption about accounts must be checked against live account data (field names, hashes and timestamps only) before it ships.

## Codex Account Security

Treat every Codex, ChatGPT, OAuth, upstream provider, and admin credential as a release blocker.

Before every commit:

1. Run `sh scripts/security/precommit-security-audit.sh --check`.
2. Inspect `git diff --cached` for account identifiers, email addresses, tokens, auth state, upstream keys, and generated `/data` content.
3. Confirm no staged file contains `.codex/`, `auth.json`, `.env`, account data, `config.json`, OAuth responses, API keys, private keys, or session cookies.
4. Commit only after the review, using `CODEX_ACCOUNT_SECURITY_REVIEWED=yes git commit ...`.

Never override the hook or force-push credentials. If a secret is ever staged or pushed, revoke it before proceeding and remove it from repository history with the repository owner.
