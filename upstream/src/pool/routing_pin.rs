//! The ChatGPT routing pin a Guardian reviewer's socket carries (SPEC.md
//! 2.3.2, routing pin).
//!
//! From Codex 0.166 a Guardian reviewer opens its Responses socket with the
//! routing cookies (`__oailb`, `__cflb`) that the reviewed thread's own socket
//! received in its handshake, so the edge can place the review beside the
//! response it reviews (core/src/client.rs `connect_websocket` with
//! `parent_routing_affinity`; http-client chatgpt_routing_affinity.rs). A
//! Codex that talks to the pool never captures those cookies, because the pool
//! is not a ChatGPT host, so the pool keeps them itself: each thread's latest
//! upstream socket on each account leaves its pin here, and a reviewer's
//! socket on the same account takes the pin of its parent thread
//! (`x-codex-parent-thread-id`, which Codex sends on a reviewer's handshake).
//! A different account's pin is never used: in Codex the reviewer and its
//! parent always share one login.
//!
//! The pin is only a hint the edge validates; an expired one adds nothing
//! (ChatGptRoutingAffinity keeps the cookies' own expiry).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use codex_http_client::ChatGptRoutingAffinity;
use http::HeaderMap;

/// How long a thread's pin is kept after its socket opened. Routing cookies
/// live far shorter; this only bounds the table.
const KEEP: Duration = Duration::from_secs(6 * 60 * 60);

/// Account id and thread id to that thread's latest socket pin (none when
/// the socket got no routing cookie) and when the socket opened.
type Pins = HashMap<(String, String), (Option<ChatGptRoutingAffinity>, Instant)>;

#[derive(Default)]
pub struct RoutingPins {
    pins: Mutex<Pins>,
}

/// Codex's own test of a reviewer request (core/src/client.rs
/// `is_guardian_reviewer`).
pub fn is_guardian_reviewer(headers: &HeaderMap) -> bool {
    headers
        .get("x-codex-guardian")
        .is_some_and(|value| value == "reviewer")
}

fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .unwrap_or_default()
        .to_string()
}

impl RoutingPins {
    fn lock(&self) -> std::sync::MutexGuard<'_, Pins> {
        self.pins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records the pin of a thread's newly opened socket. A socket without
    /// one replaces the older pin too: Codex pins the reviewer to the
    /// connection that produced the parent's latest response, never to an
    /// earlier one.
    pub fn note(
        &self,
        account_id: &str,
        handshake: &HeaderMap,
        pin: Option<ChatGptRoutingAffinity>,
        now: Instant,
    ) {
        let thread = header(handshake, "thread-id");
        if thread.is_empty() || is_guardian_reviewer(handshake) {
            return;
        }
        let mut pins = self.lock();
        pins.retain(|_, (_, at)| now.saturating_duration_since(*at) < KEEP);
        pins.insert((account_id.to_string(), thread), (pin, now));
    }

    /// The pin a reviewer's socket on this account carries, if any.
    pub fn for_reviewer(
        &self,
        account_id: &str,
        handshake: &HeaderMap,
    ) -> Option<ChatGptRoutingAffinity> {
        if !is_guardian_reviewer(handshake) {
            return None;
        }
        let parent = header(handshake, "x-codex-parent-thread-id");
        if parent.is_empty() {
            return None;
        }
        self.lock()
            .get(&(account_id.to_string(), parent))
            .and_then(|(pin, _)| pin.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use http::header::SET_COOKIE;

    fn pin(value: &str) -> ChatGptRoutingAffinity {
        let mut headers = HeaderMap::new();
        headers.insert(SET_COOKIE, HeaderValue::from_str(value).unwrap());
        ChatGptRoutingAffinity::from_response(
            &"wss://chatgpt.com/backend-api/codex/responses"
                .parse()
                .unwrap(),
            &headers,
        )
        .expect("pin")
    }

    fn handshake(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn a_reviewer_takes_its_parent_thread_s_pin_on_the_same_account_only() {
        let pins = RoutingPins::default();
        let now = Instant::now();
        let parent = pin("__oailb=parent; Path=/; Secure");
        pins.note(
            "a",
            &handshake(&[("thread-id", "t1")]),
            Some(parent.clone()),
            now,
        );
        let reviewer = handshake(&[
            ("x-codex-guardian", "reviewer"),
            ("x-codex-parent-thread-id", "t1"),
            ("thread-id", "r1"),
        ]);
        assert_eq!(pins.for_reviewer("a", &reviewer), Some(parent));
        assert_eq!(pins.for_reviewer("b", &reviewer), None);
        // Only a reviewer carries a pin, and a reviewer never leaves one.
        assert_eq!(
            pins.for_reviewer("a", &handshake(&[("x-codex-parent-thread-id", "t1")])),
            None
        );
        pins.note("a", &reviewer, Some(pin("__oailb=x; Path=/; Secure")), now);
        assert_eq!(
            pins.for_reviewer(
                "a",
                &handshake(&[
                    ("x-codex-guardian", "reviewer"),
                    ("x-codex-parent-thread-id", "r1"),
                ])
            ),
            None
        );
    }

    #[test]
    fn a_new_parent_socket_replaces_the_pin_even_without_one() {
        let pins = RoutingPins::default();
        let now = Instant::now();
        let parent = handshake(&[("thread-id", "t1")]);
        let reviewer = handshake(&[
            ("x-codex-guardian", "reviewer"),
            ("x-codex-parent-thread-id", "t1"),
        ]);
        let first = pin("__oailb=one; Path=/; Secure");
        pins.note("a", &parent, Some(first.clone()), now);
        let second = pin("__oailb=two; Path=/; Secure");
        pins.note("a", &parent, Some(second.clone()), now);
        let got = pins.for_reviewer("a", &reviewer);
        assert_eq!(got, Some(second));
        assert_ne!(got, Some(first));
        pins.note("a", &parent, None, now);
        assert_eq!(pins.for_reviewer("a", &reviewer), None);
    }
}
