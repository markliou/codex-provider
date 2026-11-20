#!/bin/sh
# Formats, lints and tests the Rust crates inside Docker (AGENTS.md: no host
# toolchains). The check image is the Dockerfile's Rust stage plus rustfmt and
# clippy, so the checks compile with the same toolchain and musl target as the
# shipped binary. Keep its base and packages in step with that stage.
#
# The cargo registry and target directory live in Docker volumes so Codex's
# dependency tree is compiled once, not on every run. The names are test-only;
# never point them at codex-pool-data. The repository is mounted read-only (a
# test reads the Dockerfile's pinned Codex CLI version): a check
# must not rewrite files (run `cargo fmt` yourself) or Cargo.lock, which is
# seeded from the pinned Codex release (--locked).
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"

CHECK_IMAGE=${CHECK_IMAGE:-codex-pool-rust-check:test}
REGISTRY_VOLUME=${REGISTRY_VOLUME:-codex-pool-rust-registry}
# Codex's crates come from its git repository, which cargo keeps under
# cargo/git, not cargo/registry; without a volume of its own every run cloned
# the whole repository again.
GIT_VOLUME=${GIT_VOLUME:-codex-pool-rust-git}
TARGET_VOLUME=${TARGET_VOLUME:-codex-pool-rust-target}

for name in "$CHECK_IMAGE" "$REGISTRY_VOLUME" "$GIT_VOLUME" "$TARGET_VOLUME"; do
  case "$name" in
    codex-pool:local | codex-pool-data)
      echo "Refusing to run: '$name' belongs to the live provider." >&2
      exit 1
      ;;
  esac
done

docker build -q -t "$CHECK_IMAGE" - >/dev/null <<'DOCKERFILE'
FROM rust:1.99.1-alpine
RUN apk add --no-cache musl-dev perl make gcc linux-headers git && \
    rustup component add rustfmt clippy
DOCKERFILE

docker run --rm \
  -v "$ROOT:/repo:ro" \
  -v "$REGISTRY_VOLUME:/usr/local/cargo/registry" \
  -v "$GIT_VOLUME:/usr/local/cargo/git" \
  -v "$TARGET_VOLUME:/target" \
  -e CARGO_TARGET_DIR=/target \
  -e CARGO_NET_GIT_FETCH_WITH_CLI=true \
  -w /repo/upstream \
  "$CHECK_IMAGE" sh -ceu '
    cargo fmt --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked
  '
