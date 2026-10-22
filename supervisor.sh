#!/bin/sh
set -eu

# One process since RUST_PORT.md step 4.6: the Rust pool takes the public
# address itself and sends upstream with Codex's own clients, so there is no
# helper to start or to stop after it. exec keeps the pool as the process
# Docker signals: on SIGTERM it closes its listener and drains in-flight
# requests, streams included, for up to CODEX_POOL_SHUTDOWN_GRACE before it
# exits (pool/serve.rs). A shell left in between would have to forward that
# signal, and an earlier version that stopped a helper first cut every open
# stream on every deploy. With arguments (hash-password) the same binary
# runs that command instead.
exec /usr/local/bin/codex-pool "$@"
