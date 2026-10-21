//! Synthetic replay regressions: no user session logs, credentials, or real account access.
use quota_widget::{
    history::{is_confirmed_quota_change, merge, normalized, replay},
    model::{HistoryEvent, HistoryKind, HistoryReconciliation, QuotaModel},
    tokens::ImportedQuotaObservation,
};
use serde_json::{json, Value};

fn observation(ms: i64, origin: &str, remaining: f64, reset: i64) -> ImportedQuotaObservation {
    ImportedQuotaObservation {
        timestamp: ms / 1000,
        timestamp_ms: Some(ms),
        origin_session: Some(origin.into()),
        source_id: Some(format!("synthetic-source-{origin}")),
        payload: json!({"limit_id":"codex","primary":{
            "used_percent":100.0 - remaining,"window_minutes":10080,"resets_at":reset
        }}),
    }
}
fn confirmed(rows: &[HistoryEvent]) -> Vec<&HistoryEvent> {
    rows.iter()
        .filter(|e| is_confirmed_quota_change(e))
        .collect()
}
fn live(t: i64, remaining: f64, reset: i64) -> Value {
    let _ = t;
    json!({"rateLimits":{"limitId":"codex","primary":{
        "usedPercent":100.0-remaining,"windowDurationMins":10080,"resetsAt":reset
    }}})
}

#[test]
fn screenshot_style_concurrent_stale_replay_has_only_distinct_real_decreases() {
    let values = [
        (100, "A", 4.),
        (109, "A", 3.),
        (120, "A", 2.),
        (121, "B", 3.),
        (124, "A", 2.),
        (126, "B", 3.),
        (128, "A", 2.),
        (182, "B", 3.),
        (183, "A", 2.),
        (225, "B", 3.),
        (300, "A", 1.),
        (301, "B", 2.),
        (310, "A", 1.),
    ];
    let observations: Vec<_> = values
        .into_iter()
        .map(|(t, s, r)| observation(t * 1000, s, r, 10_000))
        .collect();
    let observations_before = serde_json::to_value(&observations).unwrap();
    let raw = replay(&observations);
    let raw_before = raw.clone();
    let rows = normalized(&raw);
    let changes = confirmed(&rows);
    assert_eq!(changes.len(), 3);
    assert_eq!(
        changes
            .iter()
            .map(|e| (&*e.before, &*e.after))
            .collect::<Vec<_>>(),
        vec![("4", "3"), ("3", "2"), ("2", "1")]
    );
    assert!(changes
        .iter()
        .all(|e| e.kind == HistoryKind::QuotaDecrease && e.epoch == 0));
    assert_eq!(
        rows.iter()
            .filter(|e| e.reconciliation == Some(HistoryReconciliation::UncertainRecovery))
            .count(),
        0,
        "same-cycle +1 echoes are omitted by the explicit user policy"
    );
    assert_eq!(raw, raw_before);
    assert_eq!(
        serde_json::to_value(observations).unwrap(),
        observations_before
    );
    assert_eq!(normalized(&rows), rows, "normalization is idempotent");
}

#[test]
fn imported_two_to_three_reset_is_preserved_when_deadline_really_rolls() {
    let raw = replay(&[
        observation(100_000, "A", 3., 125),
        observation(120_000, "A", 2., 125),
        observation(130_000, "B", 3., 10_000),
        observation(140_000, "B", 2., 10_000),
    ]);
    let rows = normalized(&raw);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].kind, HistoryKind::QuotaReset);
    assert_eq!((&*rows[1].before, &*rows[1].after), ("2", "3"));
    assert!(rows.iter().all(is_confirmed_quota_change));
    assert_eq!(
        rows.iter().map(|e| e.epoch).collect::<Vec<_>>(),
        vec![0, 1, 1]
    );
}

#[test]
fn direct_live_larger_refills_do_not_require_a_new_reset_deadline() {
    let mut model = QuotaModel::new("synthetic-live");
    for (t, r) in [(100, 3.), (120, 2.), (130, 4.), (140, 3.)] {
        model.ingest_json(&live(t, r, 10_000), t).unwrap();
    }
    let rows = normalized(&model.history);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].kind, HistoryKind::QuotaReset);
    assert!(rows.iter().all(is_confirmed_quota_change));
}

#[test]
fn genuine_reset_then_stale_previous_cycle_does_not_spend_new_window() {
    let raw = replay(&[
        observation(100_000, "A", 1., 120),
        observation(110_000, "A", 0., 120),
        observation(130_000, "A", 100., 10_000),
        observation(131_000, "B", 0., 120),
        observation(132_000, "A", 100., 10_000),
        observation(150_000, "A", 99., 10_000),
    ]);
    let rows = normalized(&raw);
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter()
            .map(|e| (&*e.before, &*e.after))
            .collect::<Vec<_>>(),
        vec![("1", "0"), ("0", "100"), ("100", "99")]
    );
    assert!(rows.iter().all(is_confirmed_quota_change));
}

#[test]
fn zero_metadata_jitter_never_creates_events_or_cycles() {
    let observations: Vec<_> = (0..5_000)
        .map(|n| {
            observation(
                (100 + n) * 1000,
                if n % 2 == 0 { "A" } else { "B" },
                0.,
                10_000 + (n % 7),
            )
        })
        .collect();
    assert!(replay(&observations).is_empty());
}

#[test]
fn metadata_jitter_does_not_certify_a_small_historical_recovery() {
    let rows = normalized(&replay(&[
        observation(100_000, "A", 3., 10_000),
        observation(110_000, "A", 2., 10_030),
        observation(120_000, "B", 3., 9_998),
        observation(130_000, "A", 2., 10_100),
    ]));
    assert_eq!(confirmed(&rows).len(), 1);
    assert_eq!(rows.len(), 1, "unproven +1 rebound is omitted");
}

#[test]
fn live_history_reconciliation_prefers_live_but_keeps_both_raw_records() {
    let imported = replay(&[
        observation(100_000, "A", 3., 10_000),
        observation(120_000, "A", 2., 10_000),
    ]);
    let mut model = QuotaModel::new("synthetic-live");
    model.ingest_json(&live(105, 3., 10_000), 105).unwrap();
    model.ingest_json(&live(125, 2., 10_000), 125).unwrap();
    let raw = merge(imported.clone(), model.history.clone());
    assert_eq!(raw.len(), 2);
    let rows = normalized(&raw);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session, "synthetic-live");
    assert_eq!(rows[0].observed_at_unix, 125);
    assert!(
        rows[0].partial,
        "different observation times do not become precise"
    );
    assert_eq!(normalized(&merge(model.history, imported)), rows);
}

#[test]
fn separate_windows_with_equal_duration_do_not_share_progress_or_uncertainty() {
    let mut second = observation(115_000, "B", 50., 10_000);
    let window = second.payload["primary"].take();
    second.payload.as_object_mut().unwrap().remove("primary");
    second.payload["secondary"] = window;
    let mut second_drop = second.clone();
    second_drop.timestamp = 125;
    second_drop.timestamp_ms = Some(125_000);
    second_drop.payload["secondary"]["used_percent"] = json!(51.);
    let rows = normalized(&replay(&[
        observation(100_000, "A", 3., 10_000),
        second,
        observation(120_000, "A", 2., 10_000),
        second_drop,
        observation(130_000, "B", 3., 10_000),
    ]));
    assert_eq!(confirmed(&rows).len(), 2);
    assert_eq!(
        rows.iter()
            .filter(|e| e.window_key.as_deref() == Some("secondary"))
            .count(),
        1
    );
}

#[test]
fn observation_milliseconds_are_ordered_independent_of_input_discovery() {
    let a = observation(100_100, "A", 3., 10_000);
    let b = observation(100_200, "A", 2., 10_000);
    let forward = normalized(&replay(&[a.clone(), b.clone()]));
    let reverse = normalized(&replay(&[b, a]));
    assert_eq!(forward, reverse);
    assert_eq!(confirmed(&forward).len(), 1);
    assert_eq!(
        forward[0].evidence.as_ref().unwrap().observed_at_ms,
        Some(100_200)
    );
}

#[test]
fn identical_timestamp_conflicting_origins_do_not_invent_an_order() {
    let a = observation(100_100, "A", 3., 10_000);
    let b = observation(100_100, "B", 2., 10_000);
    let forward = normalized(&replay(&[a.clone(), b.clone()]));
    assert_eq!(forward, normalized(&replay(&[b, a])));
    assert!(confirmed(&forward).is_empty());
    assert_eq!(
        forward[0].reconciliation,
        Some(HistoryReconciliation::ConflictingObservation)
    );
}

#[test]
fn previously_unannotated_imported_history_is_not_declared_real_replenishment() {
    let mut raw = replay(&[
        observation(100_000, "A", 3., 10_000),
        observation(110_000, "A", 2., 10_000),
        observation(120_000, "B", 3., 10_000),
        observation(130_000, "A", 2., 10_000),
    ]);
    for e in &mut raw {
        e.evidence = None;
        e.session = "imported-local-observations".into();
        e.epoch += 2340;
    }
    let before = raw.clone();
    let rows = normalized(&raw);
    assert_eq!(confirmed(&rows).len(), 1);
    assert!(rows.iter().all(|e| e.epoch == 0));
    assert_eq!(raw, before);
}

#[test]
fn tiny_fractional_changes_are_not_rounded_into_equal_percent_events() {
    let rows = normalized(&replay(&[
        observation(100_000, "A", 2.1, 10_000),
        observation(110_000, "A", 2.05, 10_000),
    ]));
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (&*rows[0].before, &*rows[0].after, &*rows[0].delta),
        ("2.1", "2.05", "0.05")
    );
}

#[test]
fn exact_credit_drop_after_true_topup_is_not_deduplicated_away() {
    let mut model = QuotaModel::new("synthetic-live");
    for (t, balance) in [
        (100, "9007199254740993.000000000000000001"),
        (110, "9007199254740993"),
        (120, "9007199254740993.000000000000000001"),
        (130, "9007199254740993"),
    ] {
        model
            .ingest_json(&json!({"rateLimits":{"credits":{"balance":balance}}}), t)
            .unwrap();
    }
    let rows = normalized(&model.history);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].delta, "0.000000000000000001");
}

#[test]
fn new_historical_recovery_without_reset_metadata_is_uncertain_but_keeps_new_drops() {
    let mut observations = vec![
        observation(100_000, "A", 2., 10_000),
        observation(110_000, "A", 100., 10_000),
        observation(120_000, "A", 99., 10_000),
        observation(130_000, "A", 98., 10_000),
    ];
    for o in &mut observations {
        o.payload["primary"]
            .as_object_mut()
            .unwrap()
            .remove("resets_at");
    }
    let rows = normalized(&replay(&observations));
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0].reconciliation,
        Some(HistoryReconciliation::UncertainRecovery)
    );
    assert_eq!(confirmed(&rows).len(), 2);
    assert!(rows[0].partial && rows[1].partial);
    assert!(
        !rows[2].partial,
        "a fresh accepted boundary restores later coverage"
    );
}

#[test]
fn conflicting_cached_and_live_drop_cannot_disguise_itself_as_recovery() {
    let imported = replay(&[
        observation(100_000, "A", 4., 10_000),
        observation(110_000, "A", 2., 10_000),
    ]);
    let mut live_model = QuotaModel::new("synthetic-live");
    for (t, r) in [(100, 4.), (120, 3.), (130, 2.)] {
        live_model.ingest_json(&live(t, r, 10_000), t).unwrap();
    }
    let rows = normalized(&merge(imported, live_model.history));
    assert!(rows.iter().all(|e| e.kind == HistoryKind::QuotaDecrease));
    let changes = confirmed(&rows);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].observed_at_unix, 130);
    assert_eq!((&*changes[0].before, &*changes[0].after), ("3", "2"));
    assert_eq!(normalized(&rows), rows);
}
