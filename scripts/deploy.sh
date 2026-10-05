#!/usr/bin/env sh
# Build a new image, wait until the running pool has no provider request in
# flight, then recreate the container.
#
# Docker stops the old container before it starts the new one, so the port is
# closed for the whole stop. The pool's shutdown drain keeps in-flight streams
# alive but cannot accept new requests, so a deploy that lands mid-stream trades
# cut streams for refused connections. Waiting for an idle moment first keeps
# the swap to the few seconds a restart takes, which Codex's own request retries
# cover. The drain remains the backstop for a request that starts in between.
#
# Usage: sh scripts/deploy.sh [--force]
#   --force  restart even when the running pool cannot report its in-flight
#            count (a version that predates /internal/inflight).
# Environment:
#   CODEX_POOL_CONTAINER         container name (default codex-pool)
#   CODEX_POOL_DEPLOY_IDLE_WAIT  seconds to wait for idle before restarting
#                                anyway and relying on the drain (default 600)
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

container="${CODEX_POOL_CONTAINER:-codex-pool}"
idle_wait="${CODEX_POOL_DEPLOY_IDLE_WAIT:-600}"
force=false
if [ "${1:-}" = "--force" ]; then
  force=true
fi

# The endpoint answers loopback callers only, so it is read from inside the
# container. Node is already in the image for the supervisor's health check.
in_flight() {
  docker exec "$container" node -e '
    fetch("http://127.0.0.1:8317/internal/inflight", {signal: AbortSignal.timeout(2000)})
      .then((response) => response.ok ? response.json() : Promise.reject(new Error(String(response.status))))
      .then((body) => console.log(body.inFlight))
      .catch(() => process.exit(1))' 2>/dev/null
}

sh scripts/build-local-image.sh

if docker ps --format '{{.Names}}' | grep -qx "$container"; then
  if ! count=$(in_flight); then
    if [ "$force" != true ]; then
      echo "The running pool cannot report in-flight requests (it predates /internal/inflight)." >&2
      echo "Restarting it would cut any open stream. Re-run with --force at a quiet moment." >&2
      exit 1
    fi
    echo "In-flight count unavailable; restarting anyway (--force)."
  else
    waited=0
    while [ "$count" != "0" ]; do
      if [ "$waited" -ge "$idle_wait" ]; then
        echo "Still $count request(s) in flight after ${idle_wait}s; restarting and relying on the shutdown drain."
        break
      fi
      if [ $((waited % 15)) -eq 0 ]; then
        echo "Waiting for idle: $count request(s) in flight (${waited}s)..."
      fi
      sleep 1
      waited=$((waited + 1))
      count=$(in_flight || echo "?")
    done
    if [ "$count" = "0" ]; then
      echo "Pool is idle; restarting."
    fi
  fi
fi

docker compose up -d

ready=0
for _ in $(seq 1 60); do
  if in_flight >/dev/null; then
    ready=1
    break
  fi
  sleep 1
done
if [ "$ready" != 1 ]; then
  echo "The new container did not answer within 60s; check: docker logs $container" >&2
  exit 1
fi
echo "Deployed: $(docker inspect -f '{{.State.Status}} since {{.State.StartedAt}}' "$container")"
