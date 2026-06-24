# Repository Rules

## Portability

Do not install software, runtimes, package managers, libraries, build tools, or test tools on the host. All build, test, formatting, and service tooling must run inside Docker.

## Implementation Comments

Treat comments that explain behavioral intent as part of the implementation, not optional cleanup. When changing non-obvious behavior, fragile routing/auth/session logic, security or exposure boundaries, public/admin UI wording, or code that was previously changed incorrectly, add or update a nearby source comment explaining why the behavior exists and what must not be simplified away.

Comments should explain the purpose and regression risk, not restate the code. If the behavior is a product contract, update `SPEC.md` in the same change. Do not remove an intent comment unless the protected behavior is intentionally changed and the replacement code, comment, and spec text preserve the new intent.

## Codex Account Security

Treat every Codex, ChatGPT, OAuth, upstream provider, and admin credential as a release blocker.

Before every commit:

1. Run `sh scripts/security/precommit-security-audit.sh --check`.
2. Inspect `git diff --cached` for account identifiers, email addresses, tokens, auth state, upstream keys, and generated `/data` content.
3. Confirm no staged file contains `.codex/`, `auth.json`, `.env`, account data, `config.json`, OAuth responses, API keys, private keys, or session cookies.
4. Commit only after the review, using `CODEX_ACCOUNT_SECURITY_REVIEWED=yes git commit ...`.

Never override the hook or force-push credentials. If a secret is ever staged or pushed, revoke it before proceeding and remove it from repository history with the repository owner.
