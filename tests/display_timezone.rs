//! Synthetic-only regressions: no actual sessions or system timezone changes.
use chrono::{DateTime, NaiveDate, Utc};
use quota_widget::{
    display_time::{self, annotate_timestamps},
    storage,
    tokens::{Pricing, TokenStore},
};
use serde_json::{json, Value};
use std::{fs, process::Command};

fn epoch(iso: &str) -> i64 {
    DateTime::parse_from_rfc3339(iso).unwrap().timestamp()
}

fn date(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).unwrap()
}

#[test]
fn october_17_late_utc_displays_october_18_in_hong_kong() {
    let raw = epoch("2026-10-17T23:51:00Z");
    assert_eq!(display_time::time(raw), "07:51:00");
    assert_eq!(display_time::day(raw), Some(date(2026, 10, 18)));
    assert_eq!(
        display_time::datetime(raw).as_deref(),
        Some("2026-10-18 07:51:00 +08:00")
    );
    assert_eq!(
        display_time::iso(raw).as_deref(),
        Some("2026-10-18T07:51:00+08:00")
    );
    assert_eq!(display_time::timezone().local_minus_utc(), 8 * 60 * 60);
    assert_eq!(display_time::OFFSET_LABEL, "+08:00");
    assert_eq!(display_time::DISPLAY_ZONE, "Asia/Hong_Kong (UTC+08:00)");
}

#[test]
fn hong_kong_day_rolls_over_at_1600_utc() {
    let before = epoch("2026-10-17T15:59:59Z");
    let after = epoch("2026-10-17T16:00:00Z");
    assert_eq!(display_time::day(before), Some(date(2026, 10, 17)));
    assert_eq!(display_time::time(before), "23:59:59");
    assert_eq!(display_time::day(after), Some(date(2026, 10, 18)));
    assert_eq!(display_time::time(after), "00:00:00");
    assert_eq!(after - before, 1);
    let (start, end) = display_time::day_bounds(date(2026, 10, 18)).unwrap();
    assert_eq!(start, after);
    assert_eq!(end, epoch("2026-10-18T16:00:00Z"));
    assert_eq!(end - start, 86_400);
    assert_eq!(display_time::day(start - 1), Some(date(2026, 10, 17)));
    assert_eq!(display_time::day(end - 1), Some(date(2026, 10, 18)));
    assert_eq!(display_time::day(end), Some(date(2026, 10, 19)));
}

#[test]
fn fixed_timezone_ignores_host_timezone() {
    // Each child gets its own environment; parallel tests never mutate global TZ.
    if std::env::var_os("QUOTA_DISPLAY_TIME_TEST_CHILD").is_some() {
        october_17_late_utc_displays_october_18_in_hong_kong();
        hong_kong_day_rolls_over_at_1600_utc();
        assert_eq!(
            display_time::time(epoch("1941-06-15T00:00:00Z")),
            "08:00:00"
        );
        return;
    }
    for tz in [
        "UTC",
        "America/Los_Angeles",
        "Asia/Hong_Kong",
        "Pacific/Kiritimati",
    ] {
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fixed_timezone_ignores_host_timezone",
                "--nocapture",
            ])
            .env("QUOTA_DISPLAY_TIME_TEST_CHILD", "1")
            .env("TZ", tz)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "TZ={tz}: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn current_day_uses_utc_clock_plus_fixed_offset() {
    let before = display_time::day(Utc::now().timestamp()).unwrap();
    let current = display_time::now_day();
    let after = display_time::day(Utc::now().timestamp()).unwrap();
    assert!(current == before || current == after);
}

#[test]
fn invalid_or_calendar_overflow_timestamps_are_unavailable() {
    for invalid in [i64::MIN, i64::MAX, DateTime::<Utc>::MAX_UTC.timestamp()] {
        assert_eq!(display_time::time(invalid), "—");
        assert_eq!(display_time::day(invalid), None);
        assert_eq!(display_time::datetime(invalid), None);
        assert_eq!(display_time::iso(invalid), None);
    }
    assert_eq!(display_time::time(0), "08:00:00");
    assert_eq!(display_time::time(-1), "07:59:59");
    assert_eq!(display_time::day_bounds(NaiveDate::MIN), None);
}

#[test]
fn exported_nested_timestamps_preserve_epochs_and_are_idempotent() {
    let raw = epoch("2026-10-17T23:51:00Z");
    let mut export = json!({
        "timestamp": raw,
        "duration_seconds": 3600,
        "rows": [{"observed_at_unix": raw, "before_observed_at_unix": null,
            "children": [{"first_timestamp": raw - 60, "last_timestamp": raw,
                "start": raw - 60, "end": raw,
                "interval_start": raw - 60, "interval_end": raw}]}],
        "source": {"timestamp": "2026-10-18T07:51:00+08:00"},
        "payload": {"timestamp": raw, "start": raw, "nested": [{"end": raw}]},
        "invalid": [{"timestamp": i64::MAX}, {"timestamp": 1.5}],
        "timestamp_ms": raw * 1000
    });
    annotate_timestamps(&mut export);
    assert_eq!(export["timestamp"], raw);
    assert_eq!(export["timestamp_utc_unix"], raw);
    assert_eq!(export["timestamp_hk"], "2026-10-18 07:51:00 +08:00");
    assert_eq!(export["rows"][0]["observed_at_unix_utc_unix"], raw);
    assert_eq!(
        export["rows"][0]["observed_at_unix_hk"],
        export["timestamp_hk"]
    );
    assert_eq!(export["rows"][0]["before_observed_at_unix_hk"], Value::Null);
    assert!(export["rows"][0]
        .get("before_observed_at_unix_utc_unix")
        .is_some());
    let nested = &export["rows"][0]["children"][0];
    for key in [
        "first_timestamp",
        "last_timestamp",
        "start",
        "end",
        "interval_start",
        "interval_end",
    ] {
        assert_eq!(nested[format!("{key}_utc_unix")], nested[key]);
        assert!(nested[format!("{key}_hk")]
            .as_str()
            .unwrap()
            .ends_with("+08:00"));
    }
    assert_eq!(
        nested["end"].as_i64().unwrap() - nested["start"].as_i64().unwrap(),
        60
    );
    assert_eq!(export["duration_seconds"], 3600);
    assert_eq!(export["timestamp_ms"], raw * 1000);
    assert_eq!(
        export["payload"],
        json!({"timestamp": raw, "start": raw, "nested": [{"end": raw}]})
    );
    assert_eq!(
        export["source"],
        json!({"timestamp": "2026-10-18T07:51:00+08:00"})
    );
    assert_eq!(export["invalid"][0]["timestamp_utc_unix"], i64::MAX);
    assert_eq!(export["invalid"][0]["timestamp_hk"], Value::Null);
    assert_eq!(export["invalid"][1]["timestamp_utc_unix"], 1.5);
    assert_eq!(export["invalid"][1]["timestamp_hk"], Value::Null);
    let first = export.clone();
    annotate_timestamps(&mut export);
    assert_eq!(export, first);
    let serialized = serde_json::to_string(&export).unwrap();
    let restored: Value = serde_json::from_str(&serialized).unwrap();
    assert_eq!(restored, export);
}

#[test]
fn offset_input_is_parsed_once_and_raw_epochs_survive_store_restart() {
    let temporary = tempfile::tempdir().unwrap();
    let logs = temporary.path().join("synthetic-logs");
    fs::create_dir(&logs).unwrap();
    let source_time = "2026-10-18T07:51:00+08:00";
    let raw = epoch("2026-10-17T23:51:00Z");
    assert_eq!(epoch(source_time), raw);
    let row = json!({"type":"token_usage_record", "timestamp":source_time,
        "payload":{"thread_id":"synthetic-timezone-task", "session_id":"synthetic-root",
            "turn_id":"synthetic-turn", "response_id":"synthetic-response", "model":"gpt-6.1-sol",
            "usage":{"input_tokens":100, "cached_input_tokens":20, "cache_write_input_tokens":0,
                "output_tokens":10, "reasoning_output_tokens":0, "total_tokens":110}}});
    fs::write(logs.join("synthetic.jsonl"), format!("{row}\n")).unwrap();
    let pricing = Pricing::demo();
    let mut store = TokenStore::default();
    let report = store.import_dir(&logs, &pricing).unwrap();
    assert_eq!(report.malformed_lines, 0, "{:?}", report.warnings);
    assert_eq!(store.events().len(), 1);
    assert_eq!(store.events()[0].timestamp, raw);
    assert_eq!(display_time::time(store.events()[0].timestamp), "07:51:00");
    let original = store.events().to_vec();
    let state_path = temporary.path().join("tokens.json");
    store.save(&state_path).unwrap();
    let state: Value = storage::load_json(&state_path).unwrap();
    assert_eq!(state["events"][0]["timestamp"], raw);
    let mut restarted = TokenStore::load(&state_path).unwrap();
    assert_eq!(restarted.events(), original);
    let repeat = restarted.import_dir(&logs, &pricing).unwrap();
    assert_eq!(repeat.lines_read, 0);
    assert_eq!(restarted.events(), original);
    assert_eq!(
        display_time::iso(restarted.events()[0].timestamp).as_deref(),
        Some(source_time)
    );
}

#[test]
fn prediction_reset_and_task_epoch_aliases_preserve_durations() {
    let raw = epoch("2026-10-17T23:51:00Z");
    let fields = [
        "latest_timestamp",
        "observed_at",
        "last_observed_at",
        "reset_at",
        "before_reset_at",
        "earliest_at",
        "latest_at",
        "estimated_at",
    ];
    let mut row =
        json!({"age_seconds": 60, "window_duration_mins": 300, "observed_at_ms": raw * 1000});
    for key in fields {
        row[key] = json!(raw);
    }
    annotate_timestamps(&mut row);
    for key in fields {
        assert_eq!(row[key], raw);
        assert_eq!(row[format!("{key}_utc_unix")], raw);
        assert_eq!(row[format!("{key}_hk")], "2026-10-18 07:51:00 +08:00");
    }
    for key in ["age_seconds", "window_duration_mins", "observed_at_ms"] {
        assert!(row.get(format!("{key}_hk")).is_none());
        assert!(row.get(format!("{key}_utc_unix")).is_none());
    }
}

#[test]
fn configured_offset_is_scoped_and_prediction_is_an_absolute_instant() {
    let utc8 = display_time::parse_local("2030-04-02 01:00", 480).unwrap();
    let pst = display_time::parse_local("2030-04-01 09:00", -480).unwrap();
    assert_eq!(utc8, pst);
    let mut settings = quota_widget::storage::Settings::default();
    settings.reset_reminder_local = "2030-04-02 01:00".into();
    assert_eq!(settings.prediction_target(), Some(utc8));
    {
        let _zone = display_time::scope(-480);
        assert_eq!(display_time::zone_label(), "UTC-08:00");
        assert_eq!(display_time::time(utc8), "09:00:00");
        let (start, end) = display_time::day_bounds(date(2030, 4, 1)).unwrap();
        assert_eq!(end - start, 86400);
        assert!(start <= utc8 && utc8 < end);
        assert_eq!(settings.prediction_target(), Some(utc8));
        std::thread::spawn(|| assert_eq!(display_time::zone_label(), "UTC+08:00"))
            .join()
            .unwrap();
        let mut value = serde_json::json!({"timestamp":utc8});
        display_time::annotate_timestamps(&mut value);
        assert_eq!(value["timestamp"], utc8);
        assert!(value["timestamp_local"]
            .as_str()
            .unwrap()
            .ends_with("-08:00"));
        assert!(value["timestamp_hk"].as_str().unwrap().ends_with("+08:00"));
    }
    assert_eq!(display_time::zone_label(), "UTC+08:00");
    let _zone = display_time::scope(345);
    assert_eq!(display_time::zone_label(), "UTC+05:45");
}
