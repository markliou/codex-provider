FROM golang:1.24-bookworm AS build

WORKDIR /src
COPY go.mod ./
RUN go mod download
COPY cmd ./cmd
RUN --mount=type=cache,target=/root/.cache/go-build \
    CGO_ENABLED=0 go build -buildvcs=false -trimpath -ldflags='-s -w' -o /out/codex-pool ./cmd/codex-pool

FROM node:22-bookworm-slim AS codex-cli

RUN npm install --global @openai/codex@0.141.0

FROM alpine:3.21

RUN addgroup -S codexpool && adduser -S -G codexpool codexpool && apk add --no-cache nodejs su-exec
COPY --from=build /out/codex-pool /usr/local/bin/codex-pool
COPY --from=codex-cli /usr/local/lib/node_modules/@openai/codex /usr/local/lib/node_modules/@openai/codex
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN ln -sf ../lib/node_modules/@openai/codex/bin/codex.js /usr/local/bin/codex && \
    chmod 0755 /usr/local/bin/codex-pool /usr/local/bin/docker-entrypoint.sh
VOLUME ["/data"]
EXPOSE 8317 8318
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
