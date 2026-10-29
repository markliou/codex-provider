//! How much one client key may send through the pool (SPEC.md 2.3.6).
//!
//! The request gate (codex_request.rs) checks what a request says, never how
//! many arrive. An app stuck in a loop, or one that retries at once on every
//! error, sends requests that each pass the gate, and upstream would see one
//! address flooding it. So each client key has two limits: responses running
//! at once, and requests started in the last minute. Over either, the client
//! gets a 429 with `retry-after` and nothing reaches upstream.
//!
//! The answer is the one a Codex waits on rather than fails on: a 429 whose
//! error is not a usage limit becomes RetryLimit with the server's delay
//! (codex-api/src/api_bridge.rs), and RetryLimit is retried after that delay
//! within the turn's retry budget (protocol/src/error.rs `retry_delay`). The
//! WebSocket form is the wrapped error event with the header in `headers`
//! (codex-api/src/endpoint/responses_websocket.rs
//! `map_wrapped_websocket_error_event`). Do not drop the header: without a
//! delay a Codex treats the 429 as final and the turn stops.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

const WINDOW: Duration = Duration::from_secs(60);

/// A Codex session runs its main agent and up to six subagents at once
/// (Codex core/src/config/mod.rs DEFAULT_AGENT_MAX_THREADS), so the default
/// leaves room for two full sessions on one key.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 16;
/// Each Codex request takes seconds of generation, so even seven agents stay
/// well under this; a loop does not.
pub const DEFAULT_MAX_PER_MINUTE: usize = 120;
/// How long a client over the in-flight limit is asked to wait: long enough
/// for a running response to finish, short enough not to stall a turn.
const IN_FLIGHT_RETRY_AFTER: Duration = Duration::from_secs(5);

#[derive(Default)]
struct KeyUse {
    in_flight: usize,
    started: VecDeque<Instant>,
    // A looping app is held back many times a second; the log says so once
    // a minute per key.
    logged: Option<Instant>,
}

pub struct ClientLimits {
    max_in_flight: usize,
    max_per_minute: usize,
    keys: Mutex<HashMap<String, KeyUse>>,
}

/// Why a request was held back, and when to ask again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Over {
    InFlight,
    PerMinute { retry_after: Duration },
}

impl Over {
    pub fn retry_after(self) -> Duration {
        match self {
            Self::InFlight => IN_FLIGHT_RETRY_AFTER,
            Self::PerMinute { retry_after } => retry_after,
        }
    }

    /// Whole seconds, at least one, as `retry-after` carries them.
    pub fn retry_after_seconds(self) -> u64 {
        self.retry_after().as_secs_f64().ceil().max(1.0) as u64
    }

    pub fn message(self) -> String {
        match self {
            Self::InFlight => "Too many requests from this client key are running at once. Wait for one to finish; if no request should be running, check the app for a loop.".to_string(),
            Self::PerMinute { .. } => "Too many requests from this client key in the last minute. Wait and retry; if the app retries at once on errors, give it a delay.".to_string(),
        }
    }

    pub fn reason(self) -> &'static str {
        match self {
            Self::InFlight => "in_flight",
            Self::PerMinute { .. } => "per_minute",
        }
    }

    /// The HTTP error body: an OpenAI-style error whose type and code are not
    /// one of the usage or quota limits Codex treats as final.
    pub fn error_body(self) -> serde_json::Value {
        serde_json::json!({"error": {
            "message": self.message(),
            "type": "client_rate_limit",
            "code": "client_rate_limited",
        }})
    }

    /// The same answer on a Responses WebSocket.
    pub fn websocket_event(self) -> serde_json::Value {
        serde_json::json!({
            "type": "error",
            "status": 429,
            "error": {
                "type": "client_rate_limit",
                "code": "client_rate_limited",
                "message": self.message(),
            },
            "headers": {"retry-after": self.retry_after_seconds().to_string()},
        })
    }
}

/// One running response; dropping it frees the slot.
pub struct Permit {
    limits: Arc<ClientLimits>,
    key: String,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut keys = self.limits.lock();
        if let Some(entry) = keys.get_mut(&self.key) {
            entry.in_flight = entry.in_flight.saturating_sub(1);
        }
    }
}

impl ClientLimits {
    pub fn new(max_in_flight: usize, max_per_minute: usize) -> Self {
        Self {
            max_in_flight,
            max_per_minute,
            keys: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, KeyUse>> {
        self.keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Counts a request in the key's minute, if there is room.
    fn start(&self, entry: &mut KeyUse, now: Instant) -> Result<(), Over> {
        while entry
            .started
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= WINDOW)
        {
            entry.started.pop_front();
        }
        if entry.started.len() >= self.max_per_minute {
            let oldest = entry.started.front().copied().unwrap_or(now);
            let retry_after = WINDOW.saturating_sub(now.saturating_duration_since(oldest));
            return Err(Over::PerMinute { retry_after });
        }
        entry.started.push_back(now);
        Ok(())
    }

    /// A request that runs a response: counted in the minute and held in
    /// flight until the permit drops.
    pub fn admit(self: &Arc<Self>, key: &str, now: Instant) -> Result<Permit, Over> {
        let mut keys = self.lock();
        let entry = keys.entry(key.to_string()).or_default();
        if entry.in_flight >= self.max_in_flight {
            return Err(Over::InFlight);
        }
        self.start(entry, now)?;
        entry.in_flight += 1;
        Ok(Permit {
            limits: self.clone(),
            key: key.to_string(),
        })
    }

    /// A request that only opens something (a WebSocket handshake dials
    /// upstream): counted in the minute, not held in flight.
    pub fn count(&self, key: &str, now: Instant) -> Result<(), Over> {
        let mut keys = self.lock();
        let entry = keys.entry(key.to_string()).or_default();
        self.start(entry, now)
    }

    /// Whether a hold on this key should be logged now.
    fn log_due(&self, key: &str, now: Instant) -> bool {
        let mut keys = self.lock();
        let entry = keys.entry(key.to_string()).or_default();
        let due = entry
            .logged
            .is_none_or(|at| now.saturating_duration_since(at) >= WINDOW);
        if due {
            entry.logged = Some(now);
        }
        due
    }
}

impl super::app::App {
    /// Admits a request that runs a response, or answers the client with a
    /// 429 and returns None.
    pub fn admit_client(
        &self,
        api_key: &str,
        entry: &str,
        writer: &mut dyn super::http_io::ResponseWriter,
    ) -> Option<Permit> {
        let now = Instant::now();
        match self.client_limits.admit(api_key, now) {
            Ok(permit) => Some(permit),
            Err(over) => {
                self.hold_client(api_key, entry, over, now, writer);
                None
            }
        }
    }

    /// Counts a WebSocket handshake, or answers it with a 429.
    pub fn count_client(
        &self,
        api_key: &str,
        entry: &str,
        writer: &mut dyn super::http_io::ResponseWriter,
    ) -> bool {
        let now = Instant::now();
        match self.client_limits.count(api_key, now) {
            Ok(()) => true,
            Err(over) => {
                self.hold_client(api_key, entry, over, now, writer);
                false
            }
        }
    }

    fn hold_client(
        &self,
        api_key: &str,
        entry: &str,
        over: Over,
        now: Instant,
        writer: &mut dyn super::http_io::ResponseWriter,
    ) {
        self.log_client_held(api_key, entry, over, now);
        super::http_io::set_header(
            writer,
            "retry-after",
            &over.retry_after_seconds().to_string(),
        );
        super::http_io::write_json(
            writer,
            http::StatusCode::TOO_MANY_REQUESTS,
            &over.error_body(),
        );
    }

    pub fn log_client_held(&self, api_key: &str, entry: &str, over: Over, now: Instant) {
        if self.client_limits.log_due(api_key, now) {
            self.logger.printf(&format!(
                "client held back before upstream: entry={entry} key={} limit={} retry_after={}s",
                super::hash::api_key_fingerprint(api_key),
                over.reason(),
                over.retry_after_seconds()
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_a_key_to_its_running_responses() {
        let limits = Arc::new(ClientLimits::new(2, 100));
        let now = Instant::now();
        let first = limits.admit("a", now);
        let second = limits.admit("a", now);
        assert!(first.is_ok() && second.is_ok());
        assert_eq!(limits.admit("a", now).err(), Some(Over::InFlight));
        // Another key has its own allowance.
        assert!(limits.admit("b", now).is_ok());
        drop(first);
        assert!(limits.admit("a", now).is_ok());
    }

    #[test]
    fn holds_a_key_to_its_minute_and_says_when_it_frees() {
        let limits = Arc::new(ClientLimits::new(100, 3));
        let start = Instant::now();
        assert!(limits.count("a", start).is_ok());
        let later = start + Duration::from_secs(20);
        drop(limits.admit("a", later));
        assert!(limits.count("a", later).is_ok());
        assert_eq!(
            limits.count("a", later).err(),
            Some(Over::PerMinute {
                retry_after: Duration::from_secs(40)
            })
        );
        // A permit dropped at once still counted in the minute.
        assert!(limits.admit("a", later).is_err());
        // The oldest start leaves the window after a minute.
        assert!(limits.count("a", start + WINDOW).is_ok());
    }

    #[test]
    fn the_answer_is_one_codex_waits_on() {
        let over = Over::PerMinute {
            retry_after: Duration::from_millis(1500),
        };
        assert_eq!(over.retry_after_seconds(), 2);
        assert_eq!(Over::InFlight.retry_after_seconds(), 5);
        let event = over.websocket_event();
        assert_eq!(event["status"], 429);
        assert_eq!(event["headers"]["retry-after"], "2");
        // Not one of the 429s Codex reads as final (codex-api/src/api_bridge.rs).
        let body = over.error_body();
        for final_type in [
            "usage_limit_reached",
            "usage_not_included",
            "insufficient_quota",
        ] {
            assert_ne!(body["error"]["type"], final_type);
        }
    }
}
