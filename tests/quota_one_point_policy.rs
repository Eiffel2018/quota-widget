//! Explicit user policy; synthetic inputs only, raw records stay auditable.
use quota_widget::{
    history::{is_confirmed_quota_change, normalized, replay},
    intervals::completed_intervals,
    model::{HistoryKind, QuotaModel},
    tokens::ImportedQuotaObservation,
};
use serde_json::{json, Value};
fn payload(remaining: f64, reset: i64) -> Value {
    json!({"account_id":"synthetic-account","limit_id":"codex","primary":{
    "used_percent":100.0-remaining,"window_minutes":10080,"resets_at":reset}})
}
fn observation(t: i64, r: f64, reset: i64) -> ImportedQuotaObservation {
    ImportedQuotaObservation {
        timestamp: t,
        timestamp_ms: Some(t * 1000),
        origin_session: Some("synthetic-observer".into()),
        source_id: Some("synthetic-source".into()),
        payload: payload(r, reset),
    }
}
fn checked(observations: &[ImportedQuotaObservation]) -> Vec<quota_widget::model::HistoryEvent> {
    let preserved = observations.to_vec();
    let raw = replay(observations);
    let raw_before = raw.clone();
    let rows = normalized(&raw);
    assert_eq!(observations, preserved);
    assert_eq!(raw, raw_before);
    assert_eq!(normalized(&rows), rows);
    rows
}
#[test]
fn imported_repeated_rebounds_are_omitted_and_keep_the_cost_anchor() {
    let mut input = vec![observation(100, 87., 10000), observation(110, 86., 10000)];
    for n in 0..20 {
        input.push(observation(
            120 + n * 10,
            if n % 2 == 0 { 87. } else { 86. },
            10000,
        ));
    }
    input.extend([observation(330, 85., 10000), observation(340, 84., 10000)]);
    let rows = checked(&input);
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|e| e.kind == HistoryKind::QuotaDecrease));
    assert_eq!(
        rows.iter()
            .map(|e| (&*e.before, &*e.after))
            .collect::<Vec<_>>(),
        vec![("87", "86"), ("86", "85"), ("85", "84")]
    );
    assert!(rows.iter().all(|e| e.epoch == 0));
    let intervals = completed_intervals(&[], &rows);
    assert_eq!(intervals[1].start, Some(110));
    assert_eq!(intervals[1].percentage_points, 1.);
}
#[test]
fn live_rebounds_preserve_canonical_display_prediction_and_raw_evidence() {
    let mut model = QuotaModel::new("synthetic-live");
    for (t, r) in [(100, 88.), (110, 87.), (120, 86.)] {
        model.ingest_json(&payload(r, 10000), t).unwrap();
    }
    let before = model.view("codex", "primary", 120).unwrap();
    assert_eq!(before.prediction.observed_drops, 2);
    for (t, r) in [(130, 87.), (140, 86.), (150, 87.), (160, 86.)] {
        let input = payload(r, 10000);
        let original = input.clone();
        model.ingest_json(&input, t).unwrap();
        assert_eq!(input, original);
        let view = model.view("codex", "primary", t).unwrap();
        assert_eq!(view.remaining_percent, Some(86.));
        assert_eq!(view.epoch, before.epoch);
        assert_eq!(view.prediction.observed_drops, 2);
    }
    assert_eq!(
        model
            .history
            .iter()
            .filter(|e| e.kind == HistoryKind::QuotaReset)
            .count(),
        2,
        "raw ignored observations remain available"
    );
    assert!(model
        .history
        .iter()
        .filter(|e| e.kind == HistoryKind::QuotaReset)
        .all(|e| !is_confirmed_quota_change(e)));
    for (t, r) in [(170, 85.), (180, 84.)] {
        model.ingest_json(&payload(r, 10000), t).unwrap();
    }
    let rows = normalized(&model.history);
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().all(|e| e.kind == HistoryKind::QuotaDecrease));
    let view = model.view("codex", "primary", 180).unwrap();
    assert_eq!(view.remaining_percent, Some(84.));
    assert_eq!(view.prediction.observed_drops, 4);
    assert_eq!(completed_intervals(&[], &rows)[2].start, Some(120));
}
#[test]
fn deadline_confirmed_one_point_rollover_is_kept_in_live_and_imported_paths() {
    let observations = vec![
        observation(100, 87., 125),
        observation(110, 86., 125),
        observation(130, 87., 10000),
        observation(140, 86., 10000),
    ];
    let rows = checked(&observations);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].kind, HistoryKind::QuotaReset);
    assert_eq!(rows[1].delta, "1");
    let mut model = QuotaModel::new("synthetic-live");
    for o in observations {
        model.ingest_json(&o.payload, o.timestamp).unwrap();
    }
    let rows = normalized(&model.history);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].kind, HistoryKind::QuotaReset);
    assert_eq!(rows[2].epoch, 1);
}
#[test]
fn larger_and_fractional_recharges_are_not_fuzzily_suppressed() {
    for rise in [0.5, 2., 14.] {
        let mut model = QuotaModel::new("synthetic-live");
        model.ingest_json(&payload(86., 10000), 100).unwrap();
        model.ingest_json(&payload(86. + rise, 10000), 110).unwrap();
        assert_eq!(
            model
                .view("codex", "primary", 110)
                .unwrap()
                .remaining_percent,
            Some(86. + rise)
        );
        assert_eq!(normalized(&model.history)[0].kind, HistoryKind::QuotaReset);
    }
}
#[test]
fn ignored_point_does_not_hide_a_new_two_point_recharge() {
    let mut model = QuotaModel::new("synthetic-live");
    for (t, r) in [(100, 86.), (110, 87.), (120, 88.)] {
        model.ingest_json(&payload(r, 10000), t).unwrap();
    }
    let rows = normalized(&model.history);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (&*rows[0].before, &*rows[0].after, &*rows[0].delta),
        ("86", "88", "2")
    );
}
#[test]
fn raw_legacy_same_cycle_rebound_is_omitted_without_mutating_history() {
    let raw:Vec<quota_widget::model::HistoryEvent>=[(100,"87","86","QuotaDecrease"),(110,"86","87","QuotaReset"),(120,"87","86","QuotaDecrease"),(130,"86","85","QuotaDecrease")]
        .into_iter().map(|(t,b,a,k)|serde_json::from_value(json!({"session":3,"observed_at_unix":t,"source_key":"0:primary","epoch":9,"kind":k,"window_label":"week","before":b,"after":a,"delta":"1"})).unwrap()).collect();
    let before = raw.clone();
    let rows = normalized(&raw);
    assert_eq!(raw, before);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].before, "86");
    assert_eq!(rows[1].after, "85");
    assert_eq!(rows[0].epoch, rows[1].epoch);
    assert_eq!(normalized(&rows), rows);
}
#[test]
fn accounts_sources_and_windows_keep_separate_canonical_baselines() {
    let mut model = QuotaModel::new("synthetic-live");
    let mut initial = payload(86., 10000);
    initial["secondary"] = json!({"used_percent":50.,"window_minutes":60,"resets_at":10000});
    model.ingest_json(&initial, 100).unwrap();
    let mut next = payload(87., 10000);
    next["secondary"] = json!({"used_percent":51.,"window_minutes":60,"resets_at":10000});
    model.ingest_json(&next, 110).unwrap();
    assert_eq!(
        model
            .view("codex", "primary", 110)
            .unwrap()
            .remaining_percent,
        Some(86.)
    );
    assert_eq!(
        model
            .view("codex", "secondary", 110)
            .unwrap()
            .remaining_percent,
        Some(49.)
    );
    let mut other = payload(87., 10000);
    other["limit_id"] = json!("other-source");
    model.ingest_json(&other, 120).unwrap();
    assert_eq!(
        model
            .view("other-source", "primary", 120)
            .unwrap()
            .remaining_percent,
        Some(87.)
    );
    let mut account = payload(87., 10000);
    account["account_id"] = json!("other-account");
    model.ingest_json(&account, 130).unwrap();
    assert_eq!(
        model
            .view("codex", "primary", 130)
            .unwrap()
            .remaining_percent,
        Some(87.)
    );
}

#[test]
fn early_unchanged_deadline_metadata_does_not_hide_a_true_one_point_rollover() {
    let mut model = QuotaModel::new("synthetic-live");
    for (t, r, reset) in [
        (100, 87., 125),
        (110, 86., 125),
        (126, 86., 10000),
        (130, 87., 10000),
        (140, 86., 10000),
    ] {
        model.ingest_json(&payload(r, reset), t).unwrap();
    }
    let rows = normalized(&model.history);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].kind, HistoryKind::QuotaReset);
    assert_eq!(rows[1].delta, "1");
    assert!(rows.iter().all(is_confirmed_quota_change));
    assert_eq!(model.view("codex", "primary", 140).unwrap().epoch, 1);
}
