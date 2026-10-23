#!/usr/bin/env sh
# Keeps upstream/vendor/codex a byte-for-byte copy of Codex source files at the
# release pinned in upstream/Cargo.toml.
#
#   codex-vendor.sh sync    copy every file in codex-vendor-files.txt from the
#                           pinned release (run after changing the pin)
#   codex-vendor.sh check   fail unless every vendored file equals the pinned
#                           release's and nothing else is in the vendor tree
#
# Codex keeps the functions that read a Responses stream and classify its
# failures crate-private (codex-api sse/, endpoint/responses_websocket.rs), so
# the pool compiles the files themselves. They are never edited: the
# point is that the pool reads upstream with Codex's own code, and a changed
# copy would be the pool's code again. Anything the pool needs differently is
# written outside upstream/vendor. Paths keep their place in the Codex
# repository; LICENSE and NOTICE come with them (Apache-2.0).
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$repo_root"

pinned=$(sed -n 's/^codex-http-client = .*tag = "\([^"]*\)".*/\1/p' upstream/Cargo.toml)
if [ -z "$pinned" ]; then
  echo "cannot read the pinned Codex tag from upstream/Cargo.toml" >&2
  exit 1
fi
list=scripts/conformance/codex-vendor-files.txt
vendor=upstream/vendor/codex
paths=$(grep -v '^[[:space:]]*#' "$list" | awk 'NF && $1 != "pool" { print $1 }' | tr '\n' ' ')
# Pool-owned files that must sit inside the vendor tree (each says why). They
# must not shadow a Codex file, and sync leaves them in place.
own=$(grep -v '^[[:space:]]*#' "$list" | awk '$1 == "pool" { print $2 }' | tr '\n' ' ')

# Same container and clone as codex-conformance.sh.
in_codex_repo() {
  docker run --rm -i \
    -v codex-conformance-git:/repo \
    -v "$repo_root:/work" \
    -w /work \
    node:22-alpine sh -ceu "
      apk add --no-cache git >/dev/null
      (cd / && git config --global --add safe.directory /repo)
      if [ ! -d /repo/.git ]; then
        git clone --quiet --filter=blob:none --no-checkout https://github.com/openai/codex /repo
      fi
      git -C /repo fetch --quiet --tags --force origin
      $1"
}

case "${1:-}" in
  sync)
    in_codex_repo "
      for file in \$(cd '/work/$vendor' 2>/dev/null && find . -type f | sed 's|^\./||'); do
        case ' $own ' in
          *\" \$file \"*) ;;
          *) rm -f \"/work/$vendor/\$file\" ;;
        esac
      done
      for path in $paths; do
        mkdir -p \"/work/$vendor/\$(dirname \"\$path\")\"
        git -C /repo show '$pinned':\"\$path\" > \"/work/$vendor/\$path\"
      done
      chown -R $(id -u):$(id -g) '/work/$vendor'"
    echo "Vendored $(echo $paths | wc -w) files from Codex $pinned into $vendor"
    ;;
  check)
    in_codex_repo "
      status=0
      for path in $paths; do
        if ! git -C /repo show '$pinned':\"\$path\" | cmp -s - \"/work/$vendor/\$path\"; then
          echo \"DIFFERS from $pinned: $vendor/\$path\"
          status=1
        fi
      done
      for path in $own; do
        if git -C /repo cat-file -e '$pinned':\"\$path\" 2>/dev/null; then
          echo \"Pool-owned file shadows a Codex file in $pinned: $vendor/\$path\"
          status=1
        fi
      done
      for file in \$(cd '/work/$vendor' && find . -type f | sed 's|^\./||'); do
        case ' $paths $own ' in
          *\" \$file \"*) ;;
          *) echo \"NOT a listed Codex file: $vendor/\$file\"; status=1 ;;
        esac
      done
      if [ \$status -eq 0 ]; then
        echo \"Vendored Codex files: ok ($(echo $paths | wc -w) files equal $pinned)\"
      fi
      exit \$status"
    ;;
  *)
    sed -n '2,16p' "$0"
    exit 2
    ;;
esac
