//! Operator-tunable numbers that Go keeps in package variables
//! (quotaExhaustionFloorPercent, premiumSeatWeight, the capacity retry
//! schedule and the pre-commit hold) and retunes once at startup from the
//! environment. Here they are one value the app owns and hands to the code
//! that needs them, so tests can set them without touching each other.
//!
//! An unset variable keeps the default; an invalid one is a startup error
//! rather than a silent fallback (applyQuotaFloorEnv, applyCapacityRetryEnv,
//! applyCapacityWeightEnv).

use super::plan;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tuning {
    /// The remaining share at or below which a quota window counts as
    /// consumed. It is not zero because upstream no longer stops at zero: once
    /// an included allowance runs out it bills the overage against the
    /// account's flexible credits automatically, so the last sliver of a window
    /// buys nothing and the request after it spends money the operator did not
    /// choose to spend. Reserving a floor is the only way to decline that from
    /// this side. Zero restores spending a window to the last token.
    pub quota_floor_percent: f64,
    pub premium_seat_weight: f64,
    /// The capacity retry schedule doubles from the first delay up to the cap
    /// and stops after the limit: 1s, 2s, 4s, 8s, 16s, then 20s for the
    /// remaining rounds at the defaults. Only a pool with no identity able to
    /// serve ever pays it.
    pub capacity_retry_limit: u32,
    pub capacity_retry_backoff: Duration,
    pub capacity_retry_max_wait: Duration,
    /// How long the pre-commit window may keep a streaming client waiting
    /// before headers and keepalives go out (Go streamingPrecommitMaxHold).
    pub precommit_max_hold: Duration,
    /// The first same-account overload wait; each retry doubles it. Not read
    /// from the environment: Go keeps it a variable only so tests can shorten
    /// it, as do these two.
    pub overload_retry_backoff: Duration,
    /// The first same-account transport retry wait; each retry doubles it.
    pub transport_retry_backoff: Duration,
}

/// How many times an overload is retried on the same account (after 1s, 2s
/// and 4s at the default backoff) before the pool moves the lineage to another
/// account.
pub const OVERLOAD_SAME_ACCOUNT_RETRY_LIMIT: u32 = 3;

/// How many times a request that could not reach upstream at all (a
/// connection that failed or dropped before an answer) is retried on the same
/// account before the pool moves the lineage. A failed connection says nothing
/// against the account, and moving at once is costly under strict binding: the
/// whole lineage leaves its account for good and pays a full prefill. Once, two
/// connection failures right after a restart moved a session off the
/// expiring-credit accounts it had been placed on and left it on Pro Lite. The
/// owner's rule is to keep trying before switching; this is that rule for
/// connections, as OVERLOAD_SAME_ACCOUNT_RETRY_LIMIT is for overload.
pub const TRANSPORT_SAME_ACCOUNT_RETRY_LIMIT: u32 = 2;

impl Default for Tuning {
    fn default() -> Self {
        Self {
            quota_floor_percent: 1.0,
            premium_seat_weight: plan::DEFAULT_PREMIUM_SEAT_WEIGHT,
            capacity_retry_limit: 8,
            capacity_retry_backoff: Duration::from_secs(1),
            capacity_retry_max_wait: Duration::from_secs(20),
            precommit_max_hold: Duration::from_secs(1),
            overload_retry_backoff: Duration::from_secs(1),
            transport_retry_backoff: Duration::from_millis(500),
        }
    }
}

impl Tuning {
    /// Go capacityRetryBackoff: doubles each round up to the cap, so an early
    /// blip costs a second while a sustained refusal is not retried in a tight
    /// loop. A round below one is the first.
    pub fn capacity_retry_backoff_for(&self, round: u32) -> Duration {
        let mut delay = self.capacity_retry_backoff;
        for _ in 1..round.max(1) {
            if delay >= self.capacity_retry_max_wait {
                break;
            }
            delay = delay.saturating_mul(2);
        }
        delay.min(self.capacity_retry_max_wait)
    }

    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let mut tuning = Self::default();
        let read = |name: &str| env(name).map(|value| value.trim().to_string());
        if let Some(raw) = read("CODEX_POOL_QUOTA_FLOOR_PERCENT").filter(|raw| !raw.is_empty()) {
            match raw.parse::<f64>() {
                Ok(value) if (0.0..=20.0).contains(&value) => tuning.quota_floor_percent = value,
                _ => {
                    return Err(
                        "CODEX_POOL_QUOTA_FLOOR_PERCENT must be a number between 0 and 20"
                            .to_string(),
                    );
                }
            }
        }
        tuning.premium_seat_weight = plan::premium_seat_weight_from_env(
            read("CODEX_POOL_PREMIUM_SEAT_MULTIPLIER").as_deref(),
        )?;
        if let Some(raw) = read("CODEX_POOL_CAPACITY_RETRY_LIMIT").filter(|raw| !raw.is_empty()) {
            tuning.capacity_retry_limit = raw.parse::<u32>().map_err(|_| {
                "CODEX_POOL_CAPACITY_RETRY_LIMIT must be zero or a positive integer".to_string()
            })?;
        }
        for (name, target) in [
            (
                "CODEX_POOL_CAPACITY_RETRY_BACKOFF_MS",
                &mut tuning.capacity_retry_backoff,
            ),
            (
                "CODEX_POOL_CAPACITY_RETRY_MAX_WAIT_MS",
                &mut tuning.capacity_retry_max_wait,
            ),
            (
                "CODEX_POOL_PRECOMMIT_MAX_HOLD_MS",
                &mut tuning.precommit_max_hold,
            ),
        ] {
            let Some(raw) = read(name).filter(|raw| !raw.is_empty()) else {
                continue;
            };
            // Go rejects anything that overflows a nanosecond Duration.
            match raw.parse::<i64>() {
                Ok(value) if value > 0 && value <= i64::MAX / 1_000_000 => {
                    *target = Duration::from_millis(value.unsigned_abs());
                }
                _ => {
                    return Err(format!(
                        "{name} must be a positive integer number of milliseconds"
                    ));
                }
            }
        }
        // A cap below the first delay would make the schedule shrink instead
        // of escalate, so hold the cap at or above where the schedule starts.
        if tuning.capacity_retry_max_wait < tuning.capacity_retry_backoff {
            tuning.capacity_retry_max_wait = tuning.capacity_retry_backoff;
        }
        Ok(tuning)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    // Go TestQuotaFloorIsEnvTunable.
    #[test]
    fn quota_floor_is_env_tunable() {
        let floor = |value: &str| {
            Tuning::from_env(env(&[("CODEX_POOL_QUOTA_FLOOR_PERCENT", value)]))
                .map(|t| t.quota_floor_percent)
        };
        assert_eq!(Tuning::default().quota_floor_percent, 1.0);
        assert_eq!(floor("5"), Ok(5.0));
        // Zero is a legitimate choice: it restores spending a window to the
        // last token.
        assert_eq!(floor("0"), Ok(0.0));
        for invalid in ["-1", "21", "abc", "100"] {
            assert!(floor(invalid).is_err(), "{invalid} was accepted as a floor");
        }
    }

    // Go TestCapacityRetryBackoffEscalatesToACap.
    #[test]
    fn capacity_retry_backoff_escalates_to_a_cap() {
        let tuning = Tuning {
            capacity_retry_backoff: Duration::from_secs(1),
            capacity_retry_max_wait: Duration::from_secs(15),
            ..Tuning::default()
        };
        for (round, want) in [1, 2, 4, 8, 15].into_iter().enumerate() {
            let round = u32::try_from(round).unwrap_or_default() + 1;
            assert_eq!(
                tuning.capacity_retry_backoff_for(round),
                Duration::from_secs(want)
            );
        }
        // Past the budget the cap holds, and a nonsense round is the first.
        assert_eq!(tuning.capacity_retry_backoff_for(0), Duration::from_secs(1));
        assert_eq!(
            tuning.capacity_retry_backoff_for(99),
            Duration::from_secs(15)
        );
    }

    // Go TestCapacityRetryScheduleIsEnvTunable.
    #[test]
    fn capacity_retry_schedule_is_env_tunable() {
        let base = [
            ("CODEX_POOL_CAPACITY_RETRY_LIMIT", "3"),
            ("CODEX_POOL_CAPACITY_RETRY_BACKOFF_MS", "500"),
            ("CODEX_POOL_CAPACITY_RETRY_MAX_WAIT_MS", "2000"),
        ];
        let tuning = Tuning::from_env(env(&base)).unwrap_or_default();
        assert_eq!(tuning.capacity_retry_limit, 3);
        assert_eq!(tuning.capacity_retry_backoff, Duration::from_millis(500));
        assert_eq!(tuning.capacity_retry_max_wait, Duration::from_secs(2));
        assert_eq!(tuning.capacity_retry_backoff_for(3), Duration::from_secs(2));
        // A cap below the first delay would make the schedule shrink.
        let mut low = base;
        low[2].1 = "100";
        let tuning = Tuning::from_env(env(&low)).unwrap_or_default();
        assert_eq!(
            tuning.capacity_retry_max_wait,
            tuning.capacity_retry_backoff
        );
        // An invalid value is an error, never a silent fallback.
        for (name, bad) in [
            ("CODEX_POOL_CAPACITY_RETRY_MAX_WAIT_MS", "0"),
            ("CODEX_POOL_CAPACITY_RETRY_LIMIT", "-1"),
            // Too large to convert: it would overflow into a negative duration.
            (
                "CODEX_POOL_CAPACITY_RETRY_BACKOFF_MS",
                "9223372036854775807",
            ),
            ("CODEX_POOL_PRECOMMIT_MAX_HOLD_MS", "soon"),
        ] {
            let mut pairs = base.to_vec();
            pairs.push((name, bad));
            pairs.retain(|(n, v)| *n != name || *v == bad);
            assert!(Tuning::from_env(env(&pairs)).is_err(), "{name}={bad}");
        }
        assert!(Tuning::from_env(env(&[("CODEX_POOL_PREMIUM_SEAT_MULTIPLIER", "0")])).is_err());
        assert_eq!(
            Tuning::from_env(env(&[("CODEX_POOL_PREMIUM_SEAT_MULTIPLIER", "4")]))
                .map(|t| t.premium_seat_weight),
            Ok(4.0)
        );
    }
}
