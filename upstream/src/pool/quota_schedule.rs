//! When the background quota poll asks upstream about each account.
//!
//! A port of cmd/codex-pool/quota_schedule.go; keep the two in step until the
//! Go poller is retired. The poll once asked about every account in one burst,
//! within the same second, every five minutes exactly. From one address,
//! several accounts swept together on a fixed beat is what an automated client
//! looks like, and it ties the accounts to each other for anyone watching. Each
//! account has its own schedule instead: its next poll lands a random 4 to 6
//! minutes after its last one finished, the first round after a start is spread
//! over `START_SPREAD`, and at most one account is asked at a time, with a
//! random pause before the next one due. Do not go back to a shared ticker or a
//! sweep: that brings the burst back.
//!
//! That cadence is for an account in use (a request went to it within
//! `ACTIVE_WINDOW`). An idle account is asked every 20 to 30 minutes, and is
//! due at the in-use cadence again as soon as a request lands on it. A native
//! Codex reads usage only while it is open (every 60 seconds, more often near
//! exhaustion, timed from the end of the last read: tui
//! app/rate_limit_refresh.rs `poll_deadline`, chatwidget/rate_limits.rs
//! `rate_limit_refresh_interval`) and not at all while nobody uses it; polling
//! idle accounts at the in-use rate around the clock was most of the pool's
//! own load on upstream. Only when a poll is sent is the pool's own; what is
//! sent stays the native Codex request.
//!
//! The random source is the caller's (uniform in [0, 1)), so the schedule
//! needs no dependency and tests can replay it.

use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;
use std::time::Instant;

/// Mean spacing of one account's polls while it is in use (Go
/// quotaRefreshInterval).
pub const INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const JITTER: Duration = Duration::from_secs(60);
pub const MAX_INTERVAL: Duration = INTERVAL.saturating_add(JITTER);
pub const IDLE_MIN_INTERVAL: Duration = Duration::from_secs(20 * 60);
pub const IDLE_MAX_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// How recently a request must have gone to an account for it to be in use.
pub const ACTIVE_WINDOW: Duration = Duration::from_secs(30 * 60);
pub const START_SPREAD: Duration = Duration::from_secs(2 * 60);
pub const PAUSE_MIN: Duration = Duration::from_secs(3);
pub const PAUSE_MAX: Duration = Duration::from_secs(12);

/// One account's schedule. Both gaps are drawn when a poll finishes, so which
/// one applies can change with the account's use without redrawing it.
#[derive(Clone, Copy)]
struct Plan {
    first: Instant,
    last: Option<Instant>,
    active_gap: Duration,
    idle_gap: Duration,
    /// Brings the next poll forward to decide a waiting prime (Go
    /// codex_prime.go, quota prime grace).
    by: Option<Instant>,
}

impl Plan {
    fn next_at(&self, active: bool) -> Instant {
        let next = match self.last {
            None => self.first,
            Some(last) if active => last + self.active_gap,
            Some(last) => last + self.idle_gap,
        };
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

    /// The account whose poll is most overdue at `now`; `active` reports
    /// whether an account is in use. An account seen for the first time gets
    /// its first poll within `START_SPREAD`; one no longer listed is forgotten.
    pub fn due(
        &mut self,
        now: Instant,
        account_ids: &[String],
        active: impl Fn(&str) -> bool,
    ) -> Option<String> {
        let listed: HashSet<&str> = account_ids.iter().map(String::as_str).collect();
        for id in account_ids {
            if !self.plans.contains_key(id) {
                let first = now + self.between(Duration::ZERO, START_SPREAD);
                let plan = Plan {
                    first,
                    last: None,
                    active_gap: Duration::ZERO,
                    idle_gap: Duration::ZERO,
                    by: None,
                };
                self.plans.insert(id.clone(), plan);
            }
        }
        self.plans.retain(|id, _| listed.contains(id.as_str()));
        let mut due: Option<(&String, Instant)> = None;
        for id in account_ids {
            let at = self.plans[id].next_at(active(id));
            if at > now {
                continue;
            }
            if due.is_none_or(|(_, due_at)| at < due_at) {
                due = Some((id, at));
            }
        }
        due.map(|(id, _)| id.clone())
    }

    /// Schedules the account's next poll from when this one finished.
    pub fn polled(&mut self, account_id: &str, finished: Instant) {
        let plan = Plan {
            first: finished,
            last: Some(finished),
            active_gap: self.between(INTERVAL - JITTER, MAX_INTERVAL),
            idle_gap: self.between(IDLE_MIN_INTERVAL, IDLE_MAX_INTERVAL),
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
    use std::collections::BTreeSet;

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

    // Go TestQuotaPollScheduleHasNoFixedBeat.
    #[test]
    fn has_no_fixed_beat() {
        let mut schedule = QuotaPollSchedule::new(replayable(1));
        let ids: Vec<String> = (0..10).map(|i| format!("acct-{i}")).collect();
        let start = Instant::now();
        let poll_takes = Duration::from_secs(1);
        let mut polls: HashMap<String, Vec<Instant>> = HashMap::new();
        let mut last: Option<Instant> = None;
        let mut now = start;
        while now < start + Duration::from_secs(3 * 3600) {
            if let Some(id) = schedule.due(now, &ids, |_| true) {
                if let Some(last) = last {
                    assert!(now - last >= PAUSE_MIN, "two polls {:?} apart", now - last);
                }
                last = Some(now);
                polls.entry(id.clone()).or_default().push(now);
                now += poll_takes;
                schedule.polled(&id, now);
            }
            now += schedule.pause();
        }
        let serial = (PAUSE_MAX + poll_takes) * ids.len() as u32;
        assert_eq!(polls.len(), ids.len());
        for id in &ids {
            let times = &polls[id];
            assert!(times.len() >= 25, "{id} polled {} times", times.len());
            assert!(
                times[0] - start <= START_SPREAD + serial,
                "{id} first poll late"
            );
            let low = INTERVAL - JITTER + poll_takes;
            let high = MAX_INTERVAL + poll_takes + serial;
            let mut gaps = BTreeSet::new();
            for pair in times.windows(2) {
                let gap = pair[1] - pair[0];
                assert!(gap >= low && gap <= high, "{id} polls {gap:?} apart");
                gaps.insert(gap.as_secs());
            }
            assert!(gaps.len() >= times.len() / 2, "{id} polls on a beat");
        }
    }

    // Go TestQuotaPollScheduleFollowsTheAccountList.
    #[test]
    fn follows_the_account_list() {
        let mut schedule = QuotaPollSchedule::new(|| 0.0);
        let now = Instant::now();
        let a = vec!["a".to_string()];
        assert_eq!(schedule.due(now, &a, |_| true).as_deref(), Some("a"));
        schedule.polled("a", now);
        assert_eq!(
            schedule.due(now + Duration::from_secs(60), &a, |_| true),
            None
        );
        assert!(schedule.due(now, &["b".to_string()], |_| true).is_some());
        assert!(
            !schedule.plans.contains_key("a"),
            "a removed account kept its schedule"
        );
    }

    // Go TestQuotaPollScheduleAsksIdleAccountsLess.
    #[test]
    fn asks_idle_accounts_less() {
        let mut schedule = QuotaPollSchedule::new(replayable(3));
        let ids = vec!["idle".to_string(), "busy".to_string()];
        let start = Instant::now();
        let mut polls: HashMap<String, Vec<Instant>> = HashMap::new();
        let mut now = start;
        while now < start + Duration::from_secs(4 * 3600) {
            if let Some(id) = schedule.due(now, &ids, |id| id == "busy") {
                polls.entry(id.clone()).or_default().push(now);
                now += Duration::from_secs(1);
                schedule.polled(&id, now);
            }
            now += schedule.pause();
        }
        let slack = (PAUSE_MAX + Duration::from_secs(1)) * 2;
        for pair in polls["idle"].windows(2) {
            let gap = pair[1] - pair[0];
            assert!(
                gap >= IDLE_MIN_INTERVAL && gap <= IDLE_MAX_INTERVAL + slack,
                "idle account polled {gap:?} apart"
            );
        }
        assert!(polls["busy"].len() >= 3 * polls["idle"].len());
        // Use resumes: the idle account, last asked over six minutes ago, is due.
        let last = *polls["idle"].last().unwrap();
        let resumed = last + MAX_INTERVAL + Duration::from_secs(2);
        assert_eq!(
            schedule
                .due(resumed, &["idle".to_string()], |_| true)
                .as_deref(),
            Some("idle")
        );
    }

    // Go TestQuotaPollScheduleDecidesAWaitingPrimeOnTime.
    #[test]
    fn decides_a_waiting_prime_on_time() {
        let mut schedule = QuotaPollSchedule::new(|| 0.5);
        let now = Instant::now();
        let a = vec!["a".to_string()];
        schedule.due(now, &a, |_| false);
        schedule.polled("a", now);
        let by = now + Duration::from_secs(7 * 60);
        schedule.poll_by("a", by);
        assert_eq!(
            schedule.due(by - Duration::from_secs(1), &a, |_| false),
            None
        );
        assert_eq!(schedule.due(by, &a, |_| false).as_deref(), Some("a"));
        schedule.polled("a", by);
        assert_eq!(
            schedule.due(by + Duration::from_secs(10 * 60), &a, |_| false),
            None
        );
    }
}
