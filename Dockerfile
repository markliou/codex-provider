# The pool: one Rust binary that routes and sends
# upstream with Codex's own HTTP and WebSocket clients. Alpine's native musl
# target and Rust 1.99.1 match how the official Codex Linux release is built,
# and --locked keeps the Codex-seeded lock.
FROM rust:1.99.1-alpine AS pool

RUN apk add --no-cache musl-dev perl make gcc linux-headers git
# .dockerignore keeps .git out of the image context, so release identity must be
# passed in by the build command. The admin UI depends on these args to avoid
# manual footer edits that can drift from the deployed commit. They come after
# the toolchain step and carry no build time: every argument in scope is part of
# the release build's cache key, so a timestamp made wire-capture, the
# integration tests and the deploy each rebuild the same commit from scratch
# (several minutes each, with fat LTO). Same commit, same arguments, one build.
ARG CODEX_POOL_VERSION=dev
ARG CODEX_POOL_COMMIT=unknown
WORKDIR /build/upstream
COPY upstream/Cargo.toml upstream/Cargo.lock ./
COPY upstream/src ./src
# The control page, compiled into the binary (pool/admin.rs).
COPY upstream/web ./web
# Codex's stream reader and failure classifier, vendored unchanged (src/lib.rs).
COPY upstream/vendor ./vendor
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/upstream/target \
    CODEX_POOL_BUILD_VERSION="${CODEX_POOL_VERSION}" \
    CODEX_POOL_BUILD_COMMIT="${CODEX_POOL_COMMIT}" \
    cargo build --release --locked && \
    mkdir -p /out && cp target/release/codex-pool /out/codex-pool

FROM node:22-bookworm-slim AS codex-cli

# The bundled Codex CLI signs accounts in and primes quota windows, so it is
# the pinned Codex release, the same one the pool is built from
# (upstream/Cargo.toml) and PINNED_CODEX_VERSION names; a test keeps them equal.
ARG CODEX_CLI_VERSION=0.165.0
RUN npm install --global @openai/codex@${CODEX_CLI_VERSION}

FROM node:22-bookworm-slim

RUN groupadd --system codexpool && \
    useradd --system --gid codexpool --create-home --home-dir /nonexistent --shell /usr/sbin/nologin codexpool && \
    apt-get update && \
    apt-get install --yes --no-install-recommends ca-certificates gosu && \
    rm -rf /var/lib/apt/lists/*
COPY --from=pool /out/codex-pool /usr/local/bin/codex-pool
COPY --from=codex-cli /usr/local/lib/node_modules/@openai/codex /usr/local/lib/node_modules/@openai/codex
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
COPY supervisor.sh /usr/local/bin/supervisor.sh
RUN ln -sf ../lib/node_modules/@openai/codex/bin/codex.js /usr/local/bin/codex && \
    chmod 0755 /usr/local/bin/codex-pool /usr/local/bin/docker-entrypoint.sh /usr/local/bin/supervisor.sh
VOLUME ["/data"]
EXPOSE 8317
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
