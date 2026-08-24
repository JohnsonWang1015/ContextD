//! Timestamp helpers. All persisted timestamps are UTC RFC3339 strings so the
//! database stays human-readable and portable.

use chrono::{DateTime, Utc};

/// Current UTC time.
pub fn now() -> DateTime<Utc> {
    Utc::now()
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
    fn corrupt_timestamp_does_not_panic() {
        assert_eq!(from_storage("not-a-date").timestamp(), 0);
    }
}
