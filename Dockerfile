FROM golang:1.24-bookworm AS build

WORKDIR /src
COPY go.mod ./
RUN go mod download
COPY cmd ./cmd
RUN --mount=type=cache,target=/root/.cache/go-build \
    CGO_ENABLED=0 go build -buildvcs=false -trimpath -ldflags='-s -w' -o /out/codex-pool ./cmd/codex-pool

FROM node:22-bookworm-slim AS codex-cli

RUN npm install --global @openai/codex@0.141.0

FROM eceasy/cli-proxy-api@sha256:b84deb08e59b061f664528deb3061660dda369891c47a3b6e373df7bb541da5d AS cliproxy

FROM node:22-bookworm-slim

RUN groupadd --system codexpool && \
    useradd --system --gid codexpool --create-home --home-dir /nonexistent --shell /usr/sbin/nologin codexpool && \
    apt-get update && \
    apt-get install --yes --no-install-recommends gosu && \
    rm -rf /var/lib/apt/lists/*
COPY --from=build /out/codex-pool /usr/local/bin/codex-pool
COPY --from=codex-cli /usr/local/lib/node_modules/@openai/codex /usr/local/lib/node_modules/@openai/codex
COPY --from=cliproxy /CLIProxyAPI/CLIProxyAPI /usr/local/bin/cliproxy-sidecar
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
COPY cliproxy-supervisor.sh /usr/local/bin/cliproxy-supervisor.sh
RUN ln -sf ../lib/node_modules/@openai/codex/bin/codex.js /usr/local/bin/codex && \
    chmod 0755 /usr/local/bin/codex-pool /usr/local/bin/cliproxy-sidecar /usr/local/bin/docker-entrypoint.sh /usr/local/bin/cliproxy-supervisor.sh
VOLUME ["/data"]
EXPOSE 8317 8318
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
