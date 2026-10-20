FROM golang:1.24-bookworm AS build

# .dockerignore keeps .git out of the image context, so release identity must be
# passed in by the build command. The admin UI depends on these args to avoid
# manual footer edits that can drift from the deployed commit.
ARG CODEX_POOL_VERSION=dev
ARG CODEX_POOL_COMMIT=unknown
ARG CODEX_POOL_BUILT_AT=unknown

WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY cmd ./cmd
RUN --mount=type=cache,target=/root/.cache/go-build \
    CGO_ENABLED=0 go build -buildvcs=false -trimpath \
    -ldflags="-s -w -X main.buildVersion=${CODEX_POOL_VERSION} -X main.buildCommit=${CODEX_POOL_COMMIT} -X main.buildBuiltAt=${CODEX_POOL_BUILT_AT}" \
    -o /out/codex-pool ./cmd/codex-pool

# codex-upstream sends the pool's Codex-backend requests with Codex's own HTTP
# client. Alpine's native musl target and Rust 1.95 match how the official
# Codex Linux release is built, and --locked keeps the Codex-seeded lock.
FROM rust:1.95-alpine AS upstream

RUN apk add --no-cache musl-dev perl make gcc linux-headers git
WORKDIR /src
COPY upstream/Cargo.toml upstream/Cargo.lock ./
COPY upstream/src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && \
    mkdir -p /out && cp target/release/codex-upstream /out/codex-upstream

FROM node:22-bookworm-slim AS codex-cli

# The bundled Codex CLI signs accounts in and primes quota windows, so it is
# the pinned Codex release, the same one codex-upstream is built from
# (upstream/Cargo.toml) and pinnedCodexVersion names; a test keeps them equal.
ARG CODEX_CLI_VERSION=0.163.0
RUN npm install --global @openai/codex@${CODEX_CLI_VERSION}

FROM node:22-bookworm-slim

RUN groupadd --system codexpool && \
    useradd --system --gid codexpool --create-home --home-dir /nonexistent --shell /usr/sbin/nologin codexpool && \
    apt-get update && \
    apt-get install --yes --no-install-recommends ca-certificates gosu && \
    rm -rf /var/lib/apt/lists/*
COPY --from=build /out/codex-pool /usr/local/bin/codex-pool
COPY --from=codex-cli /usr/local/lib/node_modules/@openai/codex /usr/local/lib/node_modules/@openai/codex
COPY --from=upstream /out/codex-upstream /usr/local/bin/codex-upstream
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
COPY supervisor.sh /usr/local/bin/supervisor.sh
RUN ln -sf ../lib/node_modules/@openai/codex/bin/codex.js /usr/local/bin/codex && \
    chmod 0755 /usr/local/bin/codex-pool /usr/local/bin/codex-upstream /usr/local/bin/docker-entrypoint.sh /usr/local/bin/supervisor.sh
VOLUME ["/data"]
EXPOSE 8317
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
