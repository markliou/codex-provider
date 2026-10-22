//! Go's `time.Time` as the pool's state files carry it.
//!
//! `runtime.json` and `config.json` are written by Go's `time.Time.MarshalJSON`
//! (RFC 3339 with up to nine fractional digits, trailing zeros dropped, `Z`
//! for UTC, the value's own offset otherwise) and the zero time is written
//! too (`0001-01-01T00:00:00Z`), since `omitempty` never omits a struct. A
//! Timestamp keeps the parsed instant with its offset, so routing can compare
//! times and the file is written back the way Go writes it, offset and
//! nanoseconds included.

use chrono::DateTime;
use chrono::FixedOffset;
use chrono::TimeDelta;
use chrono::TimeZone;
use chrono::Timelike;
use chrono::Utc;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Timestamp(pub DateTime<FixedOffset>);

fn zero() -> DateTime<FixedOffset> {
    // 0001-01-01T00:00:00Z is inside chrono's range; the fallback is never used.
    Utc.with_ymd_and_hms(1, 1, 1, 0, 0, 0)
        .single()
        .unwrap_or_default()
        .fixed_offset()
}

impl Timestamp {
    /// Go's time.Now().UTC().
    pub fn now() -> Self {
        Self(Utc::now().fixed_offset())
    }

    pub fn zero() -> Self {
        Self(zero())
    }

    /// Go's IsZero: the zero instant, whatever offset it was written with.
    pub fn is_zero(&self) -> bool {
        self.0 == zero()
    }

    pub fn parse(value: &str) -> Result<Self, chrono::ParseError> {
        DateTime::parse_from_rfc3339(value).map(Self)
    }

    pub fn from_unix(seconds: i64) -> Self {
        Self(
            DateTime::from_timestamp(seconds, 0)
                .unwrap_or_default()
                .fixed_offset(),
        )
    }

    pub fn unix(&self) -> i64 {
        self.0.timestamp()
    }

    pub fn unix_millis(&self) -> i64 {
        self.0.timestamp_millis()
    }

    /// The same instant in UTC (Go's `.UTC()`).
    pub fn utc(&self) -> Self {
        Self(self.0.with_timezone(&Utc).fixed_offset())
    }

    pub fn add(&self, delta: TimeDelta) -> Self {
        Self(self.0.checked_add_signed(delta).unwrap_or(self.0))
    }

    /// Go's `t.Sub(u)`.
    pub fn sub(&self, other: &Self) -> TimeDelta {
        self.0.signed_duration_since(other.0)
    }

    pub fn before(&self, other: &Self) -> bool {
        self.0 < other.0
    }

    pub fn after(&self, other: &Self) -> bool {
        self.0 > other.0
    }

    /// Go's `t.Truncate(d)` for a positive duration, measured from the zero
    /// time as Go does (for whole minutes and hours that is the same as from
    /// the Unix epoch).
    pub fn truncate(&self, delta: TimeDelta) -> Self {
        let step = delta.num_nanoseconds().unwrap_or(0);
        if step <= 0 {
            return *self;
        }
        let utc = self.0.with_timezone(&Utc);
        let nanos = utc.timestamp_nanos_opt().unwrap_or(0);
        let truncated = nanos - nanos.rem_euclid(step);
        Self(DateTime::from_timestamp_nanos(truncated).with_timezone(self.0.offset()))
    }
}

impl Default for Timestamp {
    fn default() -> Self {
        Self::zero()
    }
}

/// Go's RFC3339Nano layout: "2006-01-02T15:04:05.999999999Z07:00".
impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.format("%Y-%m-%dT%H:%M:%S"))?;
        let nanos = self.0.nanosecond() % 1_000_000_000;
        if nanos != 0 {
            let digits = format!("{nanos:09}");
            write!(f, ".{}", digits.trim_end_matches('0'))?;
        }
        let offset = self.0.offset().local_minus_utc();
        if offset == 0 {
            write!(f, "Z")
        } else {
            let sign = if offset < 0 { '-' } else { '+' };
            let minutes = offset.abs() / 60;
            write!(f, "{sign}{:02}:{:02}", minutes / 60, minutes % 60)
        }
    }
}

impl Serialize for Timestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// Go's time.Duration constructors, as signed deltas.
pub fn seconds(value: i64) -> TimeDelta {
    TimeDelta::seconds(value)
}

pub fn minutes(value: i64) -> TimeDelta {
    TimeDelta::minutes(value)
}

pub fn hours(value: i64) -> TimeDelta {
    TimeDelta::hours(value)
}

pub fn millis(value: i64) -> TimeDelta {
    TimeDelta::milliseconds(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn text(value: &str) -> String {
        Timestamp::parse(value)
            .unwrap_or_else(|error| panic!("{value}: {error}"))
            .to_string()
    }

    #[test]
    fn writes_what_go_writes() {
        assert_eq!(Timestamp::zero().to_string(), "0001-01-01T00:00:00Z");
        assert_eq!(text("2026-10-22T06:00:20Z"), "2026-10-22T06:00:20Z");
        assert_eq!(
            text("2026-10-22T06:00:20.123456789+08:00"),
            "2026-10-22T06:00:20.123456789+08:00"
        );
        assert_eq!(
            text("2026-10-22T06:00:20.120000000-05:30"),
            "2026-10-22T06:00:20.12-05:30"
        );
        assert_eq!(text("2026-10-22T06:00:20+00:00"), "2026-10-22T06:00:20Z");
    }

    #[test]
    fn compares_instants_and_knows_zero() {
        let a = Timestamp::parse("2026-10-22T08:00:00+08:00").unwrap_or_default();
        let b = Timestamp::parse("2026-10-22T00:00:00Z").unwrap_or_default();
        assert_eq!(a, b);
        assert!(Timestamp::parse("0001-01-01T00:00:00Z").is_ok_and(|t| t.is_zero()));
        assert!(!Timestamp::now().is_zero());
        assert_eq!(b.add(minutes(90)).sub(&b), minutes(90));
        assert_eq!(
            Timestamp::parse("2026-10-22T06:37:12.5Z")
                .unwrap_or_default()
                .truncate(minutes(30))
                .to_string(),
            "2026-10-22T06:30:00Z"
        );
        assert_eq!(Timestamp::from_unix(b.unix()), b);
    }
}
