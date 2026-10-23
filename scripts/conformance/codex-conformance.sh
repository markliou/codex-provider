#!/usr/bin/env sh
# Compare the pool's model of Codex with Codex's source. All tooling runs in
# Docker; the Codex repository is kept as a blobless clone in a Docker volume.
#
#   codex-conformance.sh check [tag]   verify the shape fixture and watchlist
#                                      against a Codex release (default: the
#                                      release pinned in upstream/Cargo.toml)
#   codex-conformance.sh diff <tag>    print every change to the watched Codex
#                                      files between the pinned release and
#                                      <tag>, then run check against <tag>
#   codex-conformance.sh releases      list Codex releases newer than the pin
#
# Moving to a new Codex release: run `diff <tag>`, read every hunk, match the
# pool to it (or note why it does not apply), update the tag in
# upstream/Cargo.toml, reseed upstream/Cargo.lock, update codexRelease and the
# shapes in upstream/testdata/codex-upstream-failures.json, re-copy the
# vendored Codex files (scripts/conformance/codex-vendor.sh sync), then run
# `check` and scripts/conformance/wire-capture.sh. See AGENTS.md.
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$repo_root"

pinned=$(sed -n 's/^codex-http-client = .*tag = "\([^"]*\)".*/\1/p' upstream/Cargo.toml)
if [ -z "$pinned" ]; then
  echo "cannot read the pinned Codex tag from upstream/Cargo.toml" >&2
  exit 1
fi
fixture=upstream/testdata/codex-upstream-failures.json
watchlist=scripts/conformance/codex-watchlist.txt
out_dir=scripts/conformance/out
mkdir -p "$out_dir"

# Runs a shell script inside a container that holds the Codex clone at /repo.
in_codex_repo() {
  docker run --rm -i \
    -v codex-conformance-git:/repo \
    -v "$repo_root:/work" \
    -w /work \
    node:22-alpine sh -ceu "
      apk add --no-cache git >/dev/null
      # From /: in a git worktree /work/.git is a file pointing at a path
      # outside the container, and git aborts any command started inside it.
      (cd / && git config --global --add safe.directory /repo)
      if [ ! -d /repo/.git ]; then
        git clone --quiet --filter=blob:none --no-checkout https://github.com/openai/codex /repo
      fi
      git -C /repo fetch --quiet --tags --force origin
      $1"
}

# Paths to export: the watchlist plus every file a fixture shape cites.
export_paths() {
  {
    grep -v '^[[:space:]]*#' "$watchlist" | awk 'NF { print $2 }'
    sed -n 's/.*"source": "\([^"]*\)".*/\1/p' "$fixture"
  } | sort -u | tr '\n' ' '
}

check() {
  tag=$1
  paths=$(export_paths)
  in_codex_repo "
    rm -rf /tmp/codex && mkdir -p /tmp/codex
    for path in $paths; do
      git -C /repo archive '$tag' -- \"\$path\" 2>/dev/null | tar -x -C /tmp/codex || true
    done
    node scripts/conformance/harness/check-codex-shapes.mjs /tmp/codex '$fixture' '$watchlist'"
}

case "${1:-}" in
  check)
    tag=${2:-$pinned}
    echo "Checking the pool's Codex shapes against $tag"
    check "$tag"
    # The Codex files codex-upstream compiles must be the pinned release's.
    sh scripts/conformance/codex-vendor.sh check
    ;;
  diff)
    tag=${2:?usage: codex-conformance.sh diff <tag>}
    paths=$(export_paths)
    report="$out_dir/codex-$pinned..$tag.diff"
    in_codex_repo "git -C /repo diff --stat '$pinned' '$tag' -- $paths; git -C /repo diff '$pinned' '$tag' -- $paths > '/work/$report'"
    echo "Full diff of the watched files: $report"
    echo "Read every hunk; then check the shapes against $tag:"
    check "$tag"
    ;;
  releases)
    in_codex_repo "git -C /repo tag --list 'rust-v*' --sort=-v:refname | grep -v -- '-alpha\|-beta' | awk -v pin='$pinned' '\$0 == pin { exit } { print }'"
    echo "(pinned: $pinned)"
    ;;
  *)
    sed -n '2,18p' "$0"
    exit 2
    ;;
esac
