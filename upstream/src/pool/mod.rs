//! The pool: routing, accounts, quota, the control page and the server.
//!
//! These modules intentionally sit outside `vendor/codex`: the vendored tree
//! is a byte-for-byte copy of Codex, while hashes and persistent pool state are
//! this service's contracts. The fixtures in upstream/testdata (hash, state
//! and own-call vectors) were written from the Go pool this replaced, so the
//! hashes and files the running pool already holds keep their meaning; they
//! are the measured format, not a schema to rewrite. Keep a single copy.

pub mod admin;
pub mod admin_api;
#[cfg(test)]
mod admin_tests;
pub mod app;
#[cfg(test)]
mod asset_tests;
pub mod catalog;
pub mod codex_request;
pub mod credentials;
pub mod crypto;
pub mod dashboard;
#[cfg(test)]
mod dashboard_tests;
pub mod gojson;
pub mod gosort;
pub mod hash;
pub mod http_io;
pub mod installation_id;
pub mod listener;
pub mod log;
pub mod login;
#[cfg(test)]
mod login_tests;
pub mod metadata;
pub mod native;
pub mod outcome;
pub mod own_call;
pub mod plan;
pub mod prime;
pub mod proxy;
#[cfg(test)]
mod proxy_tests;
pub mod quota;
pub mod quota_refresh;
pub mod quota_schedule;
#[cfg(test)]
mod quota_tests;
pub mod reset_credit;
pub mod response_info;
pub mod responses_ws;
pub mod routes;
pub mod routing;
#[cfg(test)]
mod routing_tests;
pub mod serve;
pub mod server;
pub mod settings;
pub mod state;
pub mod status;
pub mod stream;
#[cfg(test)]
pub mod testing;
pub mod time;
pub mod tuning;
#[cfg(test)]
mod ws_tests;
