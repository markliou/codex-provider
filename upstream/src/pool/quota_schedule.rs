//! When the background quota poll asks upstream about each account.
//!
//! Every account is read on a native Codex's cadence (the owner's choice,
//! 2026-11-11): a native Codex TUI reads usage `rate_limit_refresh_interval`
//! after its last read was sent (tui chatwidget/rate_limits.rs), every 60
//! seconds, 30 from 75% used, 15 from 90% and 5 from 99%, taking the most
//! used of the ordinary `codex` windows; and it never sends a periodic read
//! while one is pending (tui app/rate_limit_refresh.rs `poll_deadline`). The
//! pool's accounts serve more than one pool at once, so an account this pool
//! is not using can still be spent elsewhere, and only reading it shows that
//! (SPEC.md 9.9). One exception, also the owner's: an account the quota
//! reserve or its own protection already holds out of routing is read every
//! 60 seconds, not every 15 or 5 until its reset, since nothing is sent to it
//! on what those reads would show (`App::quota_poll_interval`).
//!
//! Each account keeps its own schedule, the first round after a start is
//! spread over `START_SPREAD`, and at most one account is asked at a time,
//! with a random pause of `PAUSE_MIN` to `PAUSE_MAX` after each read before
//! the next account's. Do not go back to one ticker sweeping every account:
//! the pool once asked every account in the same second on one beat, which
//! ties the accounts to each other for anyone watching.
//!
//! History: in-use accounts were read every four to six minutes at random and
//! idle ones sparsely (every hour and a half at random, 2026-11-09; not at all
//! between resets, 2026-10-27). Both left another pool's spending unseen for
//! long enough that two pools' readings of one account drifted ten points
//! apart (2026-11-10). What is sent stays the native Codex request
//! (own_call.rs); only when is the pool's own.
//!
//! The random source is the caller's (uniform in [0, 1)), so the schedule
//! needs no dependency and tests can replay it.

use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;
use std::time::Instant;

/// A native Codex's usage read interval below 75% used, and the interval of
/// an account held out of routing.
pub const INTERVAL: Duration = Duration::from_secs(60);
/// The longest an account waits for its next read: its interval plus the
/// reads and pauses of other accounts due at the same moment.
pub const MAX_INTERVAL: Duration = Duration::from_secs(2 * 60);
pub const START_SPREAD: Duration = Duration::from_secs(2 * 60);
pub const PAUSE_MIN: Duration = Duration::from_secs(3);
pub const PAUSE_MAX: Duration = Duration::from_secs(12);
/// How often the poll looks for a due account when none was due.
pub const IDLE_TICK: Duration = Duration::from_secs(1);

/// A native Codex's interval for the most used of the `codex` windows
/// (tui chatwidget/rate_limits.rs `rate_limit_refresh_interval`, rust-v0.167.0).
pub fn codex_interval(used_percent: f64) -> Duration {
    let seconds = if used_percent >= 99.0 {
        5
    } else if used_percent >= 90.0 {
        15
    } else if used_percent >= 75.0 {
        30
    } else {
        60
    };
    Duration::from_secs(seconds)
}

#[derive(Clone, Copy)]
struct Plan {
    first: Instant,
    /// When the last read was sent, as Codex's `last_requested_at`.
    last: Option<Instant>,
    /// Brings the next poll forward to decide a waiting prime (Go
    /// codex_prime.go, quota prime grace).
    by: Option<Instant>,
}

impl Plan {
    fn next_at(&self, interval: Duration) -> Instant {
        let next = self.last.map_or(self.first, |last| last + interval);
        match self.by {
            Some(by) if by < next => by,
            _ => next,
        }
    }
}

pub struct QuotaPollSchedule<R: FnMut() -> f64> {
    plans: HashMap<String, Plan>,
    random: R,
}

impl<R: FnMut() -> f64> QuotaPollSchedule<R> {
    pub fn new(random: R) -> Self {
        Self {
            plans: HashMap::new(),
            random,
        }
    }

    fn between(&mut self, low: Duration, high: Duration) -> Duration {
        low + (high - low).mul_f64((self.random)())
    }

    /// The account whose poll is most overdue at `now`, each read `interval`
    /// after its last one was sent. An account seen for the first time gets
    /// its first poll within `START_SPREAD`; one no longer listed is
    /// forgotten.
    pub fn due(
        &mut self,
        now: Instant,
        account_ids: &[String],
        interval: impl Fn(&str) -> Duration,
    ) -> Option<String> {
        let listed: HashSet<&str> = account_ids.iter().map(String::as_str).collect();
        for id in account_ids {
            if !self.plans.contains_key(id) {
                let first = now + self.between(Duration::ZERO, START_SPREAD);
                let plan = Plan {
                    first,
                    last: None,
                    by: None,
                };
                self.plans.insert(id.clone(), plan);
            }
        }
        self.plans.retain(|id, _| listed.contains(id.as_str()));
        let mut due: Option<(&String, Instant)> = None;
        for id in account_ids {
            let at = self.plans[id].next_at(interval(id));
            if at > now {
                continue;
            }
            if due.is_none_or(|(_, due_at)| at < due_at) {
                due = Some((id, at));
            }
        }
        due.map(|(id, _)| id.clone())
    }

    /// Records that the account's read was sent at `sent`; the next one is
    /// timed from there, as Codex times it from `last_requested_at`.
    pub fn polled(&mut self, account_id: &str, sent: Instant) {
        let plan = Plan {
            first: sent,
            last: Some(sent),
            by: None,
        };
        self.plans.insert(account_id.to_string(), plan);
    }

    /// Makes the account due no later than `at`.
    pub fn poll_by(&mut self, account_id: &str, at: Instant) {
        if let Some(plan) = self.plans.get_mut(account_id) {
            plan.by = Some(at);
        }
    }

    pub fn pause(&mut self) -> Duration {
        self.between(PAUSE_MIN, PAUSE_MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // splitmix64: a fixed, replayable stream for the simulation.
    fn replayable(mut state: u64) -> impl FnMut() -> f64 {
        move || {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            (z >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// Runs the refresher's loop for `span`, each read taking `takes`.
    fn simulate(
        ids: &[String],
        interval: impl Fn(&str) -> Duration,
        span: Duration,
        takes: Duration,
    ) -> (Instant, HashMap<String, Vec<Instant>>) {
        let mut schedule = QuotaPollSchedule::new(replayable(1));
        let start = Instant::now();
        let mut polls: HashMap<String, Vec<Instant>> = HashMap::new();
        let mut last: Option<Instant> = None;
        let mut now = start;
        while now < start + span {
            if let Some(id) = schedule.due(now, ids, &interval) {
                if let Some(last) = last {
                    assert!(now - last >= PAUSE_MIN, "two polls {:?} apart", now - last);
                }
                last = Some(now);
                polls.entry(id.clone()).or_default().push(now);
                schedule.polled(&id, now);
                now += takes;
                now += schedule.pause();
            } else {
                now += IDLE_TICK;
            }
        }
        (start, polls)
    }

    // Codex's cadence: each account read every minute, timed from when the
    // last read was sent, never two accounts in one moment, and the first
    // round spread out.
    #[test]
    fn reads_every_account_every_minute_one_at_a_time() {
        let ids: Vec<String> = (0..3).map(|i| format!("acct-{i}")).collect();
        let takes = Duration::from_secs(1);
        let (start, polls) = simulate(&ids, |_| INTERVAL, Duration::from_secs(3600), takes);
        let serial = (PAUSE_MAX + takes) * ids.len() as u32;
        for id in &ids {
            let times = &polls[id];
            assert!(
                (45..=61).contains(&times.len()),
                "{id} polled {} times in an hour",
                times.len()
            );
            assert!(
                times[0] - start <= START_SPREAD + serial,
                "{id} first poll late"
            );
            for pair in times.windows(2) {
                let gap = pair[1] - pair[0];
                assert!(gap >= INTERVAL, "{id} polls {gap:?} apart");
                assert!(
                    gap <= INTERVAL + serial + IDLE_TICK,
                    "{id} polls {gap:?} apart"
                );
            }
        }
    }

    // The interval is the caller's for each account at each moment: one near
    // its limit is read more often than one with room.
    #[test]
    fn reads_an_account_near_its_limit_more_often() {
        let ids = vec!["roomy".to_string(), "near".to_string()];
        let (_, polls) = simulate(
            &ids,
            |id| {
                if id == "near" {
                    codex_interval(80.0)
                } else {
                    codex_interval(10.0)
                }
            },
            Duration::from_secs(3600),
            Duration::from_secs(1),
        );
        assert!(polls["near"].len() > polls["roomy"].len() * 3 / 2);
    }

    #[test]
    fn codex_interval_follows_codex_s_steps() {
        let seconds = |used| codex_interval(used).as_secs();
        assert_eq!(seconds(0.0), 60);
        assert_eq!(seconds(74.9), 60);
        assert_eq!(seconds(75.0), 30);
        assert_eq!(seconds(89.9), 30);
        assert_eq!(seconds(90.0), 15);
        assert_eq!(seconds(98.9), 15);
        assert_eq!(seconds(99.0), 5);
        assert_eq!(seconds(100.0), 5);
    }

    // Go TestQuotaPollScheduleFollowsTheAccountList.
    #[test]
    fn follows_the_account_list() {
        let mut schedule = QuotaPollSchedule::new(|| 0.0);
        let now = Instant::now();
        let a = vec!["a".to_string()];
        assert_eq!(schedule.due(now, &a, |_| INTERVAL).as_deref(), Some("a"));
        schedule.polled("a", now);
        assert_eq!(
            schedule.due(now + Duration::from_secs(59), &a, |_| INTERVAL),
            None
        );
        assert_eq!(
            schedule.due(now + INTERVAL, &a, |_| INTERVAL).as_deref(),
            Some("a")
        );
        assert!(
            schedule
                .due(now, &["b".to_string()], |_| INTERVAL)
                .is_some()
        );
        assert!(
            !schedule.plans.contains_key("a"),
            "a removed account kept its schedule"
        );
    }

    // Go TestQuotaPollScheduleDecidesAWaitingPrimeOnTime.
    #[test]
    fn decides_a_waiting_prime_on_time() {
        let mut schedule = QuotaPollSchedule::new(|| 0.5);
        let now = Instant::now();
        let a = vec!["a".to_string()];
        let long = |_: &str| Duration::from_secs(3600);
        schedule.due(now, &a, long);
        schedule.polled("a", now);
        let by = now + Duration::from_secs(7 * 60);
        schedule.poll_by("a", by);
        assert_eq!(schedule.due(by - Duration::from_secs(1), &a, long), None);
        assert_eq!(schedule.due(by, &a, long).as_deref(), Some("a"));
        schedule.polled("a", by);
        assert_eq!(
            schedule.due(by + Duration::from_secs(10 * 60), &a, long),
            None
        );
    }
}
