//! Presentation-only Hong Kong time. Stored timestamps remain UTC Unix seconds.
//!
//! Use an explicit fixed offset rather than the host's local timezone or an IANA
//! timezone database: the widget always displays UTC+08:00, including old dates.
use chrono::{DateTime, Duration, FixedOffset, NaiveDate, SecondsFormat, Utc};
use serde_json::Value;

pub const OFFSET_LABEL: &str = "+08:00";
pub const DISPLAY_ZONE: &str = "Asia/Hong_Kong (UTC+08:00)";
const OFFSET_SECONDS: i32 = 8 * 60 * 60;

pub fn timezone() -> FixedOffset {
    FixedOffset::east_opt(OFFSET_SECONDS).expect("UTC+08:00 is a valid fixed offset")
}

fn zoned(timestamp: i64) -> Option<DateTime<FixedOffset>> {
    let utc = DateTime::<Utc>::from_timestamp(timestamp, 0)?;
    // A valid UTC timestamp can still exceed chrono's local-calendar range once
    // the offset is applied. Check before date/format methods can panic.
    utc.naive_utc()
        .checked_add_signed(Duration::seconds(i64::from(OFFSET_SECONDS)))?;
    Some(utc.with_timezone(&timezone()))
}

/// Clock time for a UTC Unix-second timestamp, or an unavailable marker.
pub fn time(timestamp: i64) -> String {
    zoned(timestamp)
        .map(|value| value.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "—".into())
}

/// Calendar day for display and day grouping, independent of the host timezone.
pub fn day(timestamp: i64) -> Option<NaiveDate> {
    zoned(timestamp).map(|value| value.date_naive())
}

/// Full human-readable timestamp with an explicit offset for exports/tooltips.
pub fn datetime(timestamp: i64) -> Option<String> {
    zoned(timestamp).map(|value| value.format("%Y-%m-%d %H:%M:%S %:z").to_string())
}

pub fn iso(timestamp: i64) -> Option<String> {
    zoned(timestamp).map(|value| value.to_rfc3339_opts(SecondsFormat::Secs, false))
}

pub fn now_day() -> NaiveDate {
    Utc::now().with_timezone(&timezone()).date_naive()
}

/// UTC epoch bounds for a Hong Kong calendar day, with an exclusive end.
/// Offsets are applied exactly once; elapsed durations remain UTC seconds.
pub fn day_bounds(day: NaiveDate) -> Option<(i64, i64)> {
    let start = day
        .and_hms_opt(0, 0, 0)?
        .checked_sub_signed(Duration::seconds(i64::from(OFFSET_SECONDS)))?
        .and_utc()
        .timestamp();
    Some((start, start.checked_add(24 * 60 * 60)?))
}

/// Add unambiguous presentation fields to exported JSON, preserving raw epochs.
/// Only the known numeric/null timestamp fields are annotated. Input strings,
/// elapsed durations, millisecond evidence, and generated fields are untouched.
/// Calling this repeatedly produces the same result.
pub fn annotate_timestamps(value: &mut Value) {
    const FIELDS: &[&str] = &[
        "timestamp",
        "observed_at_unix",
        "before_observed_at_unix",
        "first_timestamp",
        "last_timestamp",
        "latest_timestamp",
        "observed_at",
        "last_observed_at",
        "reset_at",
        "before_reset_at",
        "earliest_at",
        "latest_at",
        "estimated_at",
        "start",
        "end",
        "interval_start",
        "interval_end",
    ];
    match value {
        Value::Array(rows) => rows.iter_mut().for_each(annotate_timestamps),
        Value::Object(object) => {
            // Walk only the existing children before adding sibling annotations.
            for (key, child) in object.iter_mut() {
                // Imported provider payloads are exact source evidence, not our
                // export schema. Do not modify or annotate any of their fields.
                if key != "payload" {
                    annotate_timestamps(child);
                }
            }
            for &key in FIELDS {
                let Some(raw) = object.get(key).filter(|v| v.is_number() || v.is_null()) else {
                    continue;
                };
                let raw = raw.clone();
                let display = raw.as_i64().and_then(datetime).map(Value::String);
                object.insert(format!("{key}_utc_unix"), raw);
                object.insert(format!("{key}_hk"), display.unwrap_or(Value::Null));
            }
        }
        _ => {}
    }
}
