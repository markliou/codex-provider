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

staged_paths=$(git diff --cached --name-only --diff-filter=ACMR)

prohibited_paths=$(printf '%s\n' "$staged_paths" | grep -E '(^|/)(\.env($|\.)|\.codex/|auth\.json$|config\.json$|data/|accounts/|state/|logs/)|\.(db|sqlite|key|pem|p12|crt)$' | grep -E -v '(^|/)\.env\.example$' || true)
if [ -n "$prohibited_paths" ]; then
  printf '%s\n' "SECURITY AUDIT FAILED: prohibited credential or runtime-data paths staged:" >&2
  printf '%s\n' "$prohibited_paths" >&2
  failed=1
fi

added_lines=$(git diff --cached --no-ext-diff --unified=0 | sed -n '/^+[^+]/s/^+//p')
if printf '%s\n' "$added_lines" | grep -E '(sk-[A-Za-z0-9_-]{20,}|sess-[A-Za-z0-9_-]{20,}|codex_[A-Za-z0-9_-]{20,}|eyJ[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}|gh[pousr]_[A-Za-z0-9_]{20,}|pbkdf2-sha256:[0-9]+:[A-Za-z0-9+/=]{16,}:[A-Za-z0-9+/=]{32,})' >/dev/null; then
  fail "staged content appears to include an API key, session token, or JWT"
fi

named_token_lines=$(printf '%s\n' "$added_lines" | grep -E -i '"(access_token|refresh_token|id_token)"[[:space:]]*:[[:space:]]*"[^"]+"' || true)
if [ -n "$named_token_lines" ] && printf '%s\n' "$named_token_lines" | grep -E -i -v '"(access_token|refresh_token|id_token)"[[:space:]]*:[[:space:]]*"(\[REDACTED\]|<[^>]+>|example|replace[^\"]*)"' >/dev/null; then
  fail "staged content appears to include an OAuth token value"
fi

scan_added_lines_for_named_secrets() {
  path=$1
  case "$path" in
    scripts/security/precommit-security-audit.sh) return 0 ;;
  esac
  git diff --cached --no-ext-diff --unified=0 -- "$path" |
    sed -n '/^+[^+]/s/^+//p' |
    LC_ALL=C grep -E -I -n -i \
      '(^|[[:space:]])(OPENAI_API_KEY|CODEX_POOL_API_KEY|CODEX_POOL_UPSTREAM_API_KEY|CODEX_POOL_ADMIN_PASSWORD_HASH|client[_-]?secret|private[_-]?key)[[:space:]]*=[[:space:]]*['\''"]?[A-Za-z0-9_./+=:-]{8,}|"(apiKey|api_key|access_token|refresh_token|id_token|client_secret|private_key|authorization|cookie)"[[:space:]]*:[[:space:]]*"[^"]{8,}"' |
    grep -E -i -v '(\[REDACTED\]|redacted|<[^>]+>|replace|example|test-|dummy|placeholder|secret|pbkdf2-sha256:\.\.\.|pbkdf2_hash_from_hash-password|long-random)' || true
}

named_secret_hits=""
while IFS= read -r path; do
  [ -n "$path" ] || continue
  hits=$(scan_added_lines_for_named_secrets "$path")
  if [ -n "$hits" ]; then
    named_secret_hits="${named_secret_hits}${path}:${hits}
"
  fi
done <<EOF
$staged_paths
EOF

if [ -n "$named_secret_hits" ]; then
  printf '%s\n' "SECURITY AUDIT FAILED: staged content appears to assign or embed credential-like values:" >&2
  printf '%s\n' "$named_secret_hits" >&2
  failed=1
fi

if [ "$failed" -ne 0 ]; then
  exit 1
fi

if [ "$mode" != "--check" ] && [ "${CODEX_ACCOUNT_SECURITY_REVIEWED:-}" != "yes" ]; then
  printf '%s\n' "SECURITY AUDIT BLOCKED: inspect git diff --cached, then rerun the commit with CODEX_ACCOUNT_SECURITY_REVIEWED=yes." >&2
  exit 1
fi

printf '%s\n' "Codex account security audit passed."
