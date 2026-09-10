#!/usr/bin/env sh
# Runtime guard for the dashboard renderers.
#
# `node --check` only parses; it cannot see an undeclared identifier inside a
# template literal, and the Go suite never executes the browser bundle. A
# ReferenceError there is not cosmetic: refreshPublic() catches it, returns
# false, and the control page falls back to the login screen, so one bad
# reference takes the whole status page down. This script executes each renderer
# against a representative payload so that class of break fails in CI.
set -eu
repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$repo_root"
exec docker run --rm -v "$repo_root":/src -w /src node:22-alpine node scripts/test/render-web-assets.js
