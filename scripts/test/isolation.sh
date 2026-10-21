# Sourced by every integration test. It fixes the test image tags and refuses
# any name the live provider uses.
#
# The live provider runs the image codex-pool:local as the container
# codex-pool on port 8317 with the volume codex-pool-data, and
# scripts/deploy.sh starts it with `docker compose up`. A test that builds
# codex-pool:local silently replaces the live image: the next compose up (or a
# restart after a host reboot) puts the test build into production. A test
# that reuses the container or volume name would stop the live provider or
# write into its accounts. Do not change these defaults to the live names, and
# do not drop the checks below to make a test run.

POOL_IMAGE=${POOL_IMAGE:-codex-pool:test}
MOCK_IMAGE=${MOCK_IMAGE:-codex-mock-upstream:test}

for name in "$POOL_IMAGE" "$MOCK_IMAGE" "${POOL:-}" "${VOLUME:-}"; do
  case "$name" in
    codex-pool:local | codex-pool | codex-pool-data)
      echo "Refusing to run: '$name' belongs to the live provider." >&2
      exit 1
      ;;
  esac
done
