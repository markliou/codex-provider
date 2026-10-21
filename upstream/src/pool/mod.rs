//! Pool-owned compatibility primitives used by the later routing migration.
//!
//! These modules intentionally sit outside `vendor/codex`: the vendored tree
//! is a byte-for-byte copy of Codex, while hashes and persistent pool state are
//! this service's contracts. The Go implementation remains the writer until
//! migration step 4, so the fixtures here are the measured interchange format
//! rather than a second, independently invented schema.

pub mod hash;
pub mod state;
