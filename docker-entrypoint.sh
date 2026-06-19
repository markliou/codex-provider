#!/bin/sh
set -eu

if [ "$(id -u)" = "0" ]; then
  mkdir -p /data
  chown -R codexpool:codexpool /data
  exec su-exec codexpool /usr/local/bin/codex-pool "$@"
fi

exec /usr/local/bin/codex-pool "$@"
