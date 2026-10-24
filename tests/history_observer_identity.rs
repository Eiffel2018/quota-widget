//! Synthetic observer/cycle identity regressions. No private inputs are fixtures.
use quota_widget::{
    history::{is_confirmed_quota_change, merge, normalized, replay},
    model::{HistoryEvent, HistoryKind, QuotaModel},
    tokens::ImportedQuotaObservation,
};
use serde_json::json;

fn observation(ms: i64, observer: &str, remaining: i64, reset: i64) -> ImportedQuotaObservation {
    ImportedQuotaObservation {
        timestamp: ms / 1000,
        timestamp_ms: Some(ms),
        origin_session: Some(observer.into()),
        source_id: Some(format!("synthetic-{observer}")),
        payload: json!({"account_id":"synthetic-account", "limit_id":"codex", "primary": {
            "used_percent":100-remaining,"window_minutes":10080,"resets_at":reset
        }})
        .into(),
    }
}
fn checked(observations: &[ImportedQuotaObservation]) -> Vec<HistoryEvent> {
    let before = observations.to_vec();
    let raw = replay(observations);
    let raw_before = raw.clone();
    let rows = normalized(&raw);
    assert_eq!(observations, before);
    assert_eq!(raw, raw_before);
    assert_eq!(normalized(&rows), rows, "presentation is idempotent");
    let mut reverse = observations.to_vec();
    reverse.reverse();
    assert_eq!(replay(&reverse), raw, "input discovery order is irrelevant");
    rows
}
fn changes(rows: &[HistoryEvent]) -> Vec<&HistoryEvent> {
    rows.iter()
        .filter(|row| is_confirmed_quota_change(row))
        .collect()
}

#[test]
fn unseen_stale_recovery_cannot_recharge_the_starting_endpoint() {
    let rows = checked(&[
        observation(100_000, "fresh", 86, 10_000),
        observation(110_000, "stale", 87, 10_000),
        observation(120_000, "fresh", 86, 10_000),
        observation(130_000, "stale", 87, 10_000),
        observation(140_000, "fresh", 86, 10_000),
        observation(150_000, "fresh", 85, 10_000),
    ]);
    let confirmed = changes(&rows);
    assert_eq!(
        confirmed.len(),
        1,
        "86 was already observed before the stale rise"
    );
    assert_eq!((&*confirmed[0].before, &*confirmed[0].after), ("86", "85"));
    assert!(rows
        .iter()
        .filter(|row| row.kind == HistoryKind::QuotaReset)
        .all(|row| !is_confirmed_quota_change(row)));
}

#[test]
fn mirrored_observers_with_millisecond_jitter_keep_own_continuous_baseline() {
    let rows = checked(&[
        observation(100_000, "A", 90, 10_000),
        observation(100_005, "B", 90, 10_001),
        observation(110_000, "A", 89, 10_002),
        observation(110_005, "B", 89, 10_003),
        observation(120_000, "A", 87, 10_000),
        observation(120_005, "B", 87, 10_001),
    ]);
    let confirmed = changes(&rows);
    assert_eq!(confirmed.len(), 2);
    assert_eq!(
        (
            &*confirmed[1].before,
            &*confirmed[1].after,
            &*confirmed[1].delta
        ),
        ("89", "87", "2")
    );
    assert!(
        confirmed.iter().all(|row| !row.partial),
        "switching observers alone must not invalidate their own matching continuous baseline"
    );
    assert_eq!(
        confirmed[0]
            .evidence
            .as_ref()
            .unwrap()
            .before_origin_session
            .as_deref(),
        Some("A")
    );
}

#[test]
fn exact_mirrored_inputs_use_deterministic_provenance_and_do_not_add_changes() {
    let observations = vec![
        observation(100_000, "A", 90, 10_000),
        observation(110_000, "A", 89, 10_000),
        observation(120_000, "A", 88, 10_000),
    ];
    let expected = checked(&observations);
    let mut mirrored = observations.clone();
    mirrored.extend(observations.iter().cloned().map(|mut row| {
        row.source_id = Some("zz-mirrored-source".into());
        row
    }));
    assert_eq!(
        checked(&mirrored),
        expected,
        "the selected mirror must not change provenance of the next boundary"
    );
    let raw = replay(&mirrored);
    assert_eq!(merge(raw.clone(), raw.clone()), raw);
}

#[test]
fn actual_rollovers_retain_two_identical_cycles_and_multi_point_drops() {
    let rows = checked(&[
        observation(100_000, "A", 100, 125),
        observation(110_000, "A", 98, 125),
        observation(130_000, "B", 100, 250),
        observation(140_000, "B", 98, 250),
        observation(260_000, "A", 100, 10000),
        observation(270_000, "A", 98, 10000),
    ]);
    let confirmed = changes(&rows);
    assert_eq!(confirmed.len(), 5);
    assert_eq!(
        confirmed
            .iter()
            .filter(|row| row.kind == HistoryKind::QuotaDecrease)
            .count(),
        3
    );
    assert_eq!(
        confirmed
            .iter()
            .filter(|row| row.kind == HistoryKind::QuotaReset)
            .count(),
        2
    );
    assert_eq!(
        confirmed.iter().map(|row| row.epoch).collect::<Vec<_>>(),
        vec![0, 1, 1, 2, 2]
    );
}

#[test]
fn named_accounts_and_window_slots_never_share_baselines() {
    let a = observation(100_000, "same-observer", 90, 10_000);
    let mut b = observation(101_000, "same-observer", 90, 10_000);
    b.payload["account_id"] = json!("other-account");
    let mut c = observation(102_000, "same-observer", 90, 10_000);
    c.payload["secondary"] = c.payload["primary"].take();
    c.payload.as_object_mut().unwrap().remove("primary");
    let mut observations = vec![a, b, c];
    observations.extend(observations.clone().into_iter().map(|mut row| {
        row.timestamp += 10;
        row.timestamp_ms = row.timestamp_ms.map(|t| t + 10_000);
        let slot = if row.payload.get("primary").is_some() {
            "primary"
        } else {
            "secondary"
        };
        row.payload[slot]["used_percent"] = json!(11);
        row
    }));
    let rows = checked(&observations);
    assert_eq!(changes(&rows).len(), 3);
}

#[test]
fn independent_live_epochs_preserve_identical_recorded_transitions() {
    let mut model = QuotaModel::new("synthetic-live");
    for (t, r) in [(100, 100), (110, 99)] {
        model
            .ingest_json(&observation(t * 1000, "A", r, 10_000).payload, t)
            .unwrap();
    }
    let first = model.history[0].clone();
    let mut second = first.clone();
    second.observed_at_unix = 300;
    second.before_observed_at_unix = Some(290);
    second.epoch = first.epoch + 1;
    let rows = normalized(&[first.clone(), second]);
    assert_eq!(
        rows.len(),
        2,
        "explicit independent epoch cannot be discarded by equal endpoint"
    );
    assert_ne!(rows[0].epoch, rows[1].epoch);
    assert_eq!(normalized(&rows), rows);
    let mut next_run = first.clone();
    next_run.session = "synthetic-next-live-run".into();
    next_run.observed_at_unix = 400;
    next_run.before_observed_at_unix = Some(390);
    assert_eq!(normalized(&[first, next_run]).len(), 2);
}

#[test]
fn duplicate_raw_transition_representative_is_independent_of_merge_order() {
    let raw = replay(&[
        observation(100_000, "A", 90, 10_000),
        observation(110_000, "A", 89, 10_000),
    ]);
    let first = raw[0].clone();
    let mut copy = first.clone();
    copy.evidence.as_mut().unwrap().source_id = Some("another-copy".into());
    let before = normalized(&merge(vec![first.clone()], vec![copy.clone()]));
    let reverse = normalized(&merge(vec![copy], vec![first]));
    assert_eq!(before, reverse);
    assert_eq!(before.len(), 1);
}

#[test]
fn new_observer_without_a_matching_own_baseline_stays_partial() {
    let rows = checked(&[
        observation(100_000, "A", 90, 10_000),
        observation(110_000, "B", 89, 10_000),
        observation(120_000, "B", 88, 10_000),
    ]);
    assert!(rows[0].partial);
    assert!(!rows[1].partial);
}

#[test]
fn late_expired_cycle_cannot_create_spend_or_a_second_reset() {
    let rows = checked(&[
        observation(100_000, "A", 1, 120),
        observation(110_000, "A", 0, 120),
        observation(130_000, "A", 100, 10_000),
        observation(140_000, "A", 99, 10_000),
        observation(150_000, "stale", 0, 120),
        observation(160_000, "A", 100, 10_000),
        observation(170_000, "A", 99, 10_000),
        observation(180_000, "A", 97, 10_000),
    ]);
    let confirmed = changes(&rows);
    assert_eq!(confirmed.len(), 4);
    assert_eq!(
        confirmed
            .iter()
            .filter(|row| row.kind == HistoryKind::QuotaReset)
            .count(),
        1
    );
    assert_eq!((&*confirmed[3].before, &*confirmed[3].after), ("99", "97"));
}

#[test]
fn imported_rollover_and_its_new_live_epoch_are_one_observation() {
    let imported = replay(&[
        observation(100_000, "A", 1, 125),
        observation(110_000, "A", 0, 125),
        observation(130_000, "A", 100, 10000),
    ]);
    let mut model = QuotaModel::new("synthetic-live");
    for (t, r, reset) in [
        (100, 1, 125),
        (110, 0, 125),
        (135, 100, 10000),
        (145, 99, 10000),
    ] {
        model
            .ingest_json(&observation(t * 1000, "A", r, reset).payload, t)
            .unwrap();
    }
    let rows = normalized(&merge(imported, model.history));
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter()
            .filter(|row| row.kind == HistoryKind::QuotaReset)
            .count(),
        1
    );
    assert_eq!(rows[1].observed_at_unix, 135);
    assert_eq!(rows[1].session, "synthetic-live");
    assert_eq!(
        rows.iter().map(|row| row.epoch).collect::<Vec<_>>(),
        vec![0, 1, 1]
    );
    assert_eq!(normalized(&rows), rows);
}

#[test]
fn modern_zero_reset_epoch_cannot_indirectly_clear_the_next_cost_boundary() {
    let mut model = QuotaModel::new("synthetic-live");
    for (t, r) in [(100, 100), (110, 99), (130, 98)] {
        model
            .ingest_json(&observation(t * 1000, "A", r, 10_000).payload, t)
            .unwrap();
    }
    let first = model.history[0].clone();
    let mut zero = first.clone();
    zero.observed_at_unix = 120;
    zero.kind = HistoryKind::QuotaReset;
    zero.before = "99".into();
    zero.after = "99".into();
    zero.delta = "0".into();
    zero.epoch = 7;
    let mut second = model.history[1].clone();
    second.epoch = 7;
    let raw = vec![first, zero, second];
    let rows = normalized(&raw);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].epoch, rows[1].epoch);
    let intervals = quota_widget::intervals::completed_intervals(&[], &rows);
    assert_eq!(intervals[1].start, Some(110));
    assert!(intervals[1].previous_history_event_key.is_some());
    assert_eq!(raw.len(), 3);
    assert_eq!(normalized(&rows), rows);
}

#[test]
fn stale_rise_followed_by_a_new_low_charges_only_below_its_previous_anchor() {
    let rows = checked(&[
        observation(100_000, "A", 86, 10000),
        observation(110_000, "stale", 87, 10000),
        observation(120_000, "A", 85, 10000),
    ]);
    let confirmed = changes(&rows);
    assert_eq!(confirmed.len(), 1);
    assert_eq!(
        (
            &*confirmed[0].before,
            &*confirmed[0].after,
            &*confirmed[0].delta
        ),
        ("86", "85", "1")
    );
    assert!(confirmed[0].partial);
}

#[test]
fn established_new_partial_trajectory_can_revisit_old_percentage_values() {
    let rows = checked(&[
        observation(100_000, "A", 90, 10000),
        observation(110_000, "A", 89, 10000),
        observation(120_000, "A", 88, 10000),
        observation(130_000, "A", 100, 10000),
        observation(140_000, "A", 99, 10000),
        observation(150_000, "A", 90, 10000),
        observation(160_000, "A", 89, 10000),
    ]);
    let confirmed = changes(&rows);
    assert_eq!(confirmed.len(), 5);
    assert_eq!(
        confirmed
            .iter()
            .filter(|row| row.before == "90" && row.after == "89")
            .count(),
        2,
        "repeated percentage values alone are not global identities"
    );
    assert!(rows
        .iter()
        .filter(|row| row.kind == HistoryKind::QuotaReset)
        .all(|row| row.reconciliation.is_some()));
}
