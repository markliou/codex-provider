//! When the background quota poll asks upstream about each account.
//!
//! The poll once asked about every account in one burst,
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
//! `ACTIVE_WINDOW`). An idle account is asked on a sparse cadence with no
//! beat: each gap is drawn anew from a truncated exponential between
//! `IDLE_MIN_INTERVAL` and `IDLE_MAX_INTERVAL` (`IDLE_MEAN_EXTRA` above the
//! minimum on average, about an hour and a half between reads), the spacing of
//! arrivals that happen at random, not a timer's. A gap is never a fixed or
//! clipped value, so no interval repeats often enough to read as a timer. It is
//! also asked a random `WAKE_DELAY_MIN` to `WAKE_DELAY_MAX` after its longest
//! window resets (`wake_at`), if that comes first, and is due at the in-use
//! cadence again as soon as a request lands on it.
//!
//! History, so neither extreme comes back: the pool once asked idle accounts
//! every 20 to 30 minutes around the clock, then (2026-10-27, after account
//! bans) not at all between resets, on the assumption that only the owner's
//! own Codex changes an idle account's quota. That assumption is wrong: the
//! owner's accounts serve more than one pool, and what another pool spends was
//! invisible here until a request was refused (SPEC.md 9.9). The owner chose
//! (2026-11-09) to read idle accounts again, sparsely and at random. A native
//! Codex reads usage every 60 seconds while it is open (tui
//! app/rate_limit_refresh.rs `poll_deadline`, chatwidget/rate_limits.rs
//! `rate_limit_refresh_interval`). Only when a poll is sent is the pool's own;
//! what is sent stays the native Codex request.
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
/// An idle account's gap between reads: never shorter than the minimum or
/// longer than the maximum, `IDLE_MEAN_EXTRA` above the minimum on average
/// before truncation (about 66 minutes after it).
pub const IDLE_MIN_INTERVAL: Duration = Duration::from_secs(20 * 60);
pub const IDLE_MAX_INTERVAL: Duration = Duration::from_secs(4 * 3600);
pub const IDLE_MEAN_EXTRA: Duration = Duration::from_secs(70 * 60);
/// How long after an idle account's reset it is asked again.
pub const WAKE_DELAY_MIN: Duration = Duration::from_secs(60);
pub const WAKE_DELAY_MAX: Duration = Duration::from_secs(10 * 60);
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
    /// When an idle account is asked again: just after its reset.
    wake: Option<Instant>,
    /// Brings the next poll forward to decide a waiting prime (Go
    /// codex_prime.go, quota prime grace).
    by: Option<Instant>,
}

impl Plan {
    fn next_at(&self, active: bool) -> Instant {
        let next = match self.last {
            None => self.first,
            Some(last) if active => last + self.active_gap,
            Some(last) => {
                let idle = last + self.idle_gap;
                self.wake.map_or(idle, |wake| wake.min(idle))
            }
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

    /// The idle gap: `IDLE_MIN_INTERVAL` plus an exponential with mean
    /// `IDLE_MEAN_EXTRA`, truncated at `IDLE_MAX_INTERVAL` by inverting its
    /// distribution rather than clipping, so no gap is ever the bound
    /// itself.
    fn idle_gap(&mut self) -> Duration {
        let range = (IDLE_MAX_INTERVAL - IDLE_MIN_INTERVAL).as_secs_f64();
        let mean = IDLE_MEAN_EXTRA.as_secs_f64();
        let kept = 1.0 - (-range / mean).exp();
        let u = (self.random)().clamp(0.0, 1.0 - f64::EPSILON);
        let extra = -mean * (1.0 - u * kept).ln();
        IDLE_MIN_INTERVAL + Duration::from_secs_f64(extra.clamp(0.0, range))
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
                    wake: None,
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
            idle_gap: self.idle_gap(),
            wake: None,
            by: None,
        };
        self.plans.insert(account_id.to_string(), plan);
    }

    /// Asks the account again, while idle, a random moment after `reset`.
    pub fn wake_at(&mut self, account_id: &str, reset: Instant) {
        let delay = self.between(WAKE_DELAY_MIN, WAKE_DELAY_MAX);
        if let Some(plan) = self.plans.get_mut(account_id) {
            plan.wake = Some(reset + delay);
        }
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

    // Go TestQuotaPollScheduleAsksIdleAccountsLess, changed twice: an idle
    // account is read sparsely, at random gaps, and also just after its
    // reset when that comes first.
    #[test]
    fn reads_an_idle_account_sparsely_and_after_its_reset() {
        let mut schedule = QuotaPollSchedule::new(replayable(3));
        let ids = vec!["idle".to_string(), "busy".to_string()];
        let start = Instant::now();
        let reset = start + Duration::from_secs(3 * 3600);
        let mut polls: HashMap<String, Vec<Instant>> = HashMap::new();
        let mut now = start;
        while now < start + Duration::from_secs(48 * 3600) {
            if let Some(id) = schedule.due(now, &ids, |id| id == "busy") {
                polls.entry(id.clone()).or_default().push(now);
                now += Duration::from_secs(1);
                schedule.polled(&id, now);
                if id == "idle" && now < reset {
                    schedule.wake_at(&id, reset);
                }
            }
            now += schedule.pause();
        }
        let idle = &polls["idle"];
        assert!(idle[0] - start <= START_SPREAD + PAUSE_MAX * 2);
        let slack = (PAUSE_MAX + Duration::from_secs(1)) * 2;
        assert!(
            idle.iter()
                .any(|at| *at >= reset + WAKE_DELAY_MIN && *at <= reset + WAKE_DELAY_MAX + slack),
            "idle account not asked after its reset"
        );
        // Between reads: never closer than the minimum, never further than
        // the maximum, and no gap repeated.
        let mut gaps = BTreeSet::new();
        for pair in idle.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(gap <= IDLE_MAX_INTERVAL + slack, "idle gap {gap:?}");
            if pair[0] > reset + WAKE_DELAY_MAX + slack || pair[1] < reset {
                assert!(gap >= IDLE_MIN_INTERVAL, "idle gap {gap:?}");
            }
            gaps.insert(gap.as_secs());
        }
        assert_eq!(gaps.len(), idle.len() - 1, "idle reads on a beat");
        // About an hour and a half apart over two days: sparse, not off.
        assert!(
            (18..=50).contains(&idle.len()),
            "idle account read {} times in two days",
            idle.len()
        );
        assert!(polls["busy"].len() >= 400);
        // Use resumes: the idle account, last asked over six minutes ago, is due.
        let last = *idle.last().unwrap();
        let resumed = last + MAX_INTERVAL + Duration::from_secs(2);
        assert_eq!(
            schedule
                .due(resumed, &["idle".to_string()], |_| true)
                .as_deref(),
            Some("idle")
        );
    }

    // The idle gap is drawn inside its bounds, reaches neither bound as a
    // clipped value, and averages near the minimum plus its mean.
    #[test]
    fn idle_gap_is_a_truncated_exponential() {
        let mut low = QuotaPollSchedule::new(|| 0.0);
        assert_eq!(low.idle_gap(), IDLE_MIN_INTERVAL);
        let mut high = QuotaPollSchedule::new(|| 1.0);
        assert!(high.idle_gap() <= IDLE_MAX_INTERVAL);
        let mut schedule = QuotaPollSchedule::new(replayable(7));
        let draws: Vec<Duration> = (0..5000).map(|_| schedule.idle_gap()).collect();
        assert!(
            draws
                .iter()
                .all(|gap| *gap >= IDLE_MIN_INTERVAL && *gap < IDLE_MAX_INTERVAL)
        );
        let mean = draws.iter().sum::<Duration>() / 5000;
        assert!(
            mean > Duration::from_secs(75 * 60) && mean < Duration::from_secs(100 * 60),
            "mean idle gap {mean:?}"
        );
        let distinct: BTreeSet<u128> = draws.iter().map(Duration::as_millis).collect();
        assert!(distinct.len() > 4900, "idle gaps repeat");
    }

    // An idle account with no known reset is asked on the idle cadence.
    #[test]
    fn idle_account_without_a_reset_waits_its_idle_gap() {
        let mut schedule = QuotaPollSchedule::new(|| 0.0);
        let now = Instant::now();
        let a = vec!["a".to_string()];
        schedule.due(now, &a, |_| false);
        schedule.polled("a", now);
        assert_eq!(
            schedule.due(now + IDLE_MIN_INTERVAL - Duration::from_secs(1), &a, |_| {
                false
            }),
            None
        );
        assert_eq!(
            schedule
                .due(now + IDLE_MIN_INTERVAL, &a, |_| false)
                .as_deref(),
            Some("a")
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
