FROM golang:1.24-bookworm AS build

WORKDIR /src
COPY go.mod ./
RUN go mod download
COPY . ./
RUN --mount=type=cache,target=/root/.cache/go-build \
    CGO_ENABLED=0 go build -buildvcs=false -trimpath -ldflags='-s -w' -o /out/codex-pool ./cmd/codex-pool

FROM alpine:3.21

RUN addgroup -S codexpool && adduser -S -G codexpool codexpool && apk add --no-cache su-exec
COPY --from=build /out/codex-pool /usr/local/bin/codex-pool
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod 0755 /usr/local/bin/codex-pool /usr/local/bin/docker-entrypoint.sh
VOLUME ["/data"]
EXPOSE 8317 8318
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
