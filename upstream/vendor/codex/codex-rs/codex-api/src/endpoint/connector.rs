// Pool-owned, not a Codex file (codex-vendor.sh checks Codex has no file here).
//
// src/lib.rs includes Codex's endpoint/responses_websocket.rs with include!,
// and rustc resolves that file's `mod connector;` beside the included file,
// here, instead of at responses_websocket/connector.rs where Codex keeps it.
// This shim loads Codex's file from its own place, and passes it the items it
// imports through `super::`, so connector.rs itself stays unchanged.
use super::ResponsesWebsocketClient;
use super::ResponsesWebsocketConnection;
use super::connect_websocket;
use super::merge_request_headers;

#[path = "responses_websocket/connector.rs"]
mod codex;
