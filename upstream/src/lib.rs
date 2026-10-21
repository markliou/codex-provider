//! Codex's own reader of Responses streams and classifier of their failures.
//!
//! Codex keeps them crate-private in codex-api (the `sse` module and the
//! WebSocket wrapped-error parsing in `endpoint/responses_websocket.rs`), so
//! the files themselves are compiled here, vendored unchanged from the pinned
//! release under `vendor/codex` (scripts/conformance/codex-vendor.sh keeps
//! them byte-for-byte equal and fails on any edit). This root is the pool's:
//! it places each file where codex-api's own `lib.rs` places it, so every
//! `crate::` path inside them resolves to the same item it does in Codex.
//! What the pool needs differently is written outside `vendor/`, never in a
//! vendored file: an edited copy would be the pool's code again, and the
//! point is that upstream is read with Codex's.

// Root re-exports the vendored files name as `crate::...` (codex-api lib.rs).
pub(crate) use codex_client::TransportError;
pub(crate) use error::ApiError;
pub(crate) use telemetry::WebsocketTelemetry;

// Each file comes whole, while the pool uses part of it; an import that only
// codex-api's other modules use is unused here.
#[allow(dead_code)]
#[path = "../vendor/codex/codex-rs/codex-api/src/api_bridge.rs"]
pub(crate) mod api_bridge;
#[allow(dead_code)]
#[path = "../vendor/codex/codex-rs/codex-api/src/auth.rs"]
pub(crate) mod auth;
#[allow(dead_code)]
#[path = "../vendor/codex/codex-rs/codex-api/src/common.rs"]
pub(crate) mod common;
#[allow(dead_code)]
#[path = "../vendor/codex/codex-rs/codex-api/src/error.rs"]
pub(crate) mod error;
#[allow(dead_code, unused_imports)]
#[path = "../vendor/codex/codex-rs/codex-api/src/provider.rs"]
pub(crate) mod provider;
#[allow(dead_code)]
#[path = "../vendor/codex/codex-rs/codex-api/src/rate_limits.rs"]
pub(crate) mod rate_limits;
#[allow(dead_code)]
#[path = "../vendor/codex/codex-rs/codex-api/src/responses_headers.rs"]
pub(crate) mod responses_headers;
#[allow(dead_code)]
#[path = "../vendor/codex/codex-rs/codex-api/src/safety_buffering.rs"]
pub(crate) mod safety_buffering;
#[allow(dead_code, unused_imports)]
#[path = "../vendor/codex/codex-rs/codex-api/src/sse/mod.rs"]
pub(crate) mod sse;
#[allow(dead_code)]
#[path = "../vendor/codex/codex-rs/codex-api/src/telemetry.rs"]
pub(crate) mod telemetry;

pub(crate) mod endpoint {
    // Included rather than declared as a module file so that the pool's
    // accessors, written in this module, can reach the file's private
    // wrapped-error parsing. Its `mod connector;` resolves to the pool-owned
    // shim vendor/codex/codex-rs/codex-api/src/endpoint/connector.rs, which
    // loads Codex's connector.rs (see that file).
    #[allow(dead_code)]
    pub(crate) mod responses_websocket {
        // Before the include: the file ends with its test module.
        /// Codex's reading of one WebSocket message as a wrapped error, the
        /// first step of run_websocket_response_stream for a text message.
        pub(crate) fn pool_wrapped_error(text: &str) -> Option<ApiError> {
            let wrapped = parse_wrapped_websocket_error_event(text)?;
            map_wrapped_websocket_error_event(wrapped, text.to_string())
        }

        include!("../vendor/codex/codex-rs/codex-api/src/endpoint/responses_websocket.rs");
    }
}

// The pool's own code on top of the vendored files.
pub mod classify;
