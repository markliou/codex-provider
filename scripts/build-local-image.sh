#!/usr/bin/env sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

commit_full=$(git rev-parse HEAD)
commit_short=$(git rev-parse --short=8 HEAD)
commit_date=$(git show -s --format=%cd --date=format:%Y.%m.%d HEAD)
dirty_suffix=""
# The admin UI uses this version to identify the deployed binary. Keep the
# dirty suffix so local rebuilds from uncommitted fixes are visibly distinct
# from clean commits instead of looking like an already-pushed release.
if ! git diff --quiet || ! git diff --cached --quiet; then
  dirty_suffix="-dirty"
fi

image="${CODEX_POOL_IMAGE:-codex-pool:local}"
version="v${commit_date}-${commit_short}${dirty_suffix}"
built_at=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

printf 'Building %s as %s (%s)\n' "$image" "$version" "$commit_full"
docker build \
  --build-arg CODEX_POOL_VERSION="$version" \
  --build-arg CODEX_POOL_COMMIT="$commit_full" \
  --build-arg CODEX_POOL_BUILT_AT="$built_at" \
  -t "$image" \
  "$@" \
  .
