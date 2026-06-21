#!/bin/sh
set -eu

if [ "$(id -u)" = "0" ]; then
  mkdir -p /data
  chown -R codexpool:codexpool /data
  exec gosu codexpool /usr/local/bin/cliproxy-supervisor.sh "$@"
fi

exec /usr/local/bin/cliproxy-supervisor.sh "$@"
