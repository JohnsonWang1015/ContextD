//! Timestamp helpers. All persisted timestamps are UTC RFC3339 strings so the
//! database stays human-readable and portable.

use chrono::{DateTime, SubsecRound, Utc};

/// Current UTC time, truncated to the precision ContextD can store.
///
/// Storage keeps milliseconds. Without truncating here, a record created in
/// memory carries microseconds that are lost on the way to SQLite, so the same
/// record read back compares as *earlier* than the in-memory value it came
/// from — which silently drops rows from any window query (a session's
/// activity, `bundle --since`) that straddles the same millisecond.
pub fn now() -> DateTime<Utc> {
    Utc::now().trunc_subsecs(3)
}

/// Format for storage.
///
/// Millisecond precision matters: several memories are routinely written
/// inside the same second (an import, a refresh), and "which of these is
/// newer" decides which one survives deduplication.
pub fn to_storage(ts: &DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Parse a stored timestamp, falling back to the epoch for corrupt rows so a
/// single bad value cannot make a whole listing unreadable.
pub fn from_storage(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(DateTime::UNIX_EPOCH)
}

/// Human friendly "2 hours ago".
pub fn humanize_since(ts: &DateTime<Utc>) -> String {
    let delta = Utc::now().signed_duration_since(*ts);
    let secs = delta.num_seconds();
    if secs < 0 {
        return "in the future".into();
    }
    match secs {
        s if s < 60 => "just now".to_string(),
        s if s < 3600 => plural(s / 60, "minute"),
        s if s < 86_400 => plural(s / 3600, "hour"),
        s if s < 2_592_000 => plural(s / 86_400, "day"),
        s if s < 31_536_000 => plural(s / 2_592_000, "month"),
        s => plural(s / 31_536_000, "year"),
    }
}

fn plural(n: i64, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{n} {unit}s ago")
    }
}

/// Age in days, used by recency scoring.
pub fn age_days(ts: &DateTime<Utc>) -> f64 {
    let delta = Utc::now().signed_duration_since(*ts);
    (delta.num_seconds().max(0) as f64) / 86_400.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn roundtrip() {
        let now = now();
        let parsed = from_storage(&to_storage(&now));
        assert_eq!(parsed.timestamp(), now.timestamp());
    }

    #[test]
    fn humanize() {
        assert_eq!(humanize_since(&(Utc::now() - Duration::seconds(5))), "just now");
        assert_eq!(humanize_since(&(Utc::now() - Duration::hours(2))), "2 hours ago");
        assert_eq!(humanize_since(&(Utc::now() - Duration::hours(1))), "1 hour ago");
    }

    #[test]
    fn now_matches_what_storage_can_keep() {
        let now = now();
        assert_eq!(from_storage(&to_storage(&now)), now, "a round trip must be lossless");
        assert_eq!(now.timestamp_subsec_micros() % 1000, 0, "sub-millisecond precision is dropped");
    }

    #[test]
    fn corrupt_timestamp_does_not_panic() {
        assert_eq!(from_storage("not-a-date").timestamp(), 0);
    }
}
