#!/bin/sh
# Runs every integration test at once. The images are built here, once, with
# the same arguments scripts/build-local-image.sh gives a deploy of this commit,
# so the deploy afterwards reuses the build. Each test names its own network,
# containers and volume after its process id and publishes no host port, so
# they do not meet; run one of them alone to debug it.
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$repo_root"
. scripts/test/isolation.sh

tests="codex-config device-auth-failover codex-subagent routing-cache-observability"
logs=$(mktemp -d)

echo "Building $POOL_IMAGE and $MOCK_IMAGE"
CODEX_POOL_IMAGE="$POOL_IMAGE" sh scripts/build-local-image.sh -q >/dev/null
docker build -q -t "$MOCK_IMAGE" -f test/Dockerfile.mock . >/dev/null

pids=""
for test in $tests; do
  ITEST_SKIP_BUILD=1 sh "scripts/test/integration-$test.sh" > "$logs/$test.log" 2>&1 &
  pids="$pids $!:$test"
done

failed=0
for entry in $pids; do
  pid=${entry%%:*}
  test=${entry#*:}
  if wait "$pid"; then
    echo "ok   $test"
  else
    echo "FAIL $test (last lines of $logs/$test.log):"
    tail -20 "$logs/$test.log" | sed 's/^/    /'
    failed=1
  fi
done
echo "Logs: $logs"
exit "$failed"
