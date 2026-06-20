#!/bin/sh
set -eu

mode=${1:-commit}
failed=0

fail() {
  printf '%s\n' "SECURITY AUDIT FAILED: $1" >&2
  failed=1
}

printf '%s\n' "Running Codex account security audit on staged changes..."

if ! git diff --cached --check; then
  fail "staged diff has whitespace errors"
fi

prohibited_paths=$(git diff --cached --name-only --diff-filter=ACMR | grep -E '(^|/)(\.env($|\.)|\.codex/|auth\.json$|config\.json$|data/|accounts/|state/|logs/)|\.(db|sqlite|key|pem|p12)$' | grep -E -v '(^|/)\.env\.example$' || true)
if [ -n "$prohibited_paths" ]; then
  printf '%s\n' "SECURITY AUDIT FAILED: prohibited credential or runtime-data paths staged:" >&2
  printf '%s\n' "$prohibited_paths" >&2
  failed=1
fi

added_lines=$(git diff --cached --no-ext-diff --unified=0 | sed -n '/^+[^+]/s/^+//p')
if printf '%s\n' "$added_lines" | grep -E '(sk-[A-Za-z0-9_-]{20,}|sess-[A-Za-z0-9_-]{20,}|codex_[A-Za-z0-9_-]{20,}|eyJ[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}|gh[pousr]_[A-Za-z0-9_]{20,})' >/dev/null; then
  fail "staged content appears to include an API key, session token, or JWT"
fi

named_token_lines=$(printf '%s\n' "$added_lines" | grep -E -i '"(access_token|refresh_token|id_token)"[[:space:]]*:[[:space:]]*"[^"]+"' || true)
if [ -n "$named_token_lines" ] && printf '%s\n' "$named_token_lines" | grep -E -i -v '"(access_token|refresh_token|id_token)"[[:space:]]*:[[:space:]]*"(\[REDACTED\]|<[^>]+>|example|replace[^\"]*)"' >/dev/null; then
  fail "staged content appears to include an OAuth token value"
fi

if [ "$failed" -ne 0 ]; then
  exit 1
fi

if [ "$mode" != "--check" ] && [ "${CODEX_ACCOUNT_SECURITY_REVIEWED:-}" != "yes" ]; then
  printf '%s\n' "SECURITY AUDIT BLOCKED: inspect git diff --cached, then rerun the commit with CODEX_ACCOUNT_SECURITY_REVIEWED=yes." >&2
  exit 1
fi

printf '%s\n' "Codex account security audit passed."
