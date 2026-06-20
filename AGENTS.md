# Repository Rules

## Portability

Do not install software, runtimes, package managers, libraries, build tools, or test tools on the host. All build, test, formatting, and service tooling must run inside Docker.

## Codex Account Security

Treat every Codex, ChatGPT, OAuth, upstream provider, and admin credential as a release blocker.

Before every commit:

1. Run `sh scripts/security/precommit-security-audit.sh --check`.
2. Inspect `git diff --cached` for account identifiers, email addresses, tokens, auth state, upstream keys, and generated `/data` content.
3. Confirm no staged file contains `.codex/`, `auth.json`, `.env`, account data, `config.json`, OAuth responses, API keys, private keys, or session cookies.
4. Commit only after the review, using `CODEX_ACCOUNT_SECURITY_REVIEWED=yes git commit ...`.

Never override the hook or force-push credentials. If a secret is ever staged or pushed, revoke it before proceeding and remove it from repository history with the repository owner.
