//! Pool-owned compatibility primitives used by the later routing migration.
//!
//! These modules intentionally sit outside `vendor/codex`: the vendored tree
//! is a byte-for-byte copy of Codex, while hashes and persistent pool state are
//! this service's contracts. The Go implementation remains the writer until
//! migration step 4, so the fixtures (cmd/codex-pool/testdata, written by
//! hash_state_vectors_test.go from the production Go code) are the measured
//! interchange format rather than a second, independently invented schema.
//! Keep a single copy of them there; a second copy here could drift.

pub mod hash;
pub mod installation_id;
pub mod own_call;
pub mod plan;
pub mod quota;
pub mod quota_schedule;
pub mod state;
pub mod time;
pub mod tuning;
