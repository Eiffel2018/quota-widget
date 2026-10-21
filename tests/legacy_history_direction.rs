//! Synthetic-only compatibility checks for recovered numeric-run live history.
//! Real user files are never included in the source tree or test fixtures.
use quota_widget::{
    history::{is_confirmed_quota_change, normalized},
    intervals::{completed_intervals, IntervalReason},
    model::{HistoryEvent, HistoryEvidence, HistoryKind, HistoryReconciliation},
};
use serde_json::json;

fn row(
    run: u64,
    t: i64,
    source: &str,
    kind: HistoryKind,
    before: &str,
    after: &str,
) -> HistoryEvent {
    serde_json::from_value(json!({
        "session": run,
        "observed_at_unix": t,
        "source_key": source,
        "epoch": 4321,
        "kind": kind,
        "window_label": if source.ends_with(":credits") { "點數" } else { "每週額度" },
        "before": before,
        "after": after,
        "delta": "1"
    }))
    .unwrap()
}

fn checked(raw: &[HistoryEvent]) -> Vec<HistoryEvent> {
    let original = raw.to_vec();
    let serialized = serde_json::to_value(raw).unwrap();
    let rows = normalized(raw);
    assert_eq!(raw, original, "raw rows must remain unchanged");
    assert_eq!(serde_json::to_value(raw).unwrap(), serialized);
    assert_eq!(
        normalized(&rows),
        rows,
        "legacy shape must remain recognizable"
    );
    let mut reversed = raw.to_vec();
    reversed.reverse();
    assert_eq!(
        normalized(&reversed),
        rows,
        "discovery order must not matter"
    );
    rows
}

fn assert_observation_preserved(before: &HistoryEvent, after: &HistoryEvent) {
    let mut expected = before.clone();
    expected.kind = after.kind;
    expected.epoch = after.epoch;
    assert_eq!(
        after, &expected,
        "only presentation direction/cycle may change"
    );
}

#[test]
fn reset_labeled_decreases_are_visible_with_exact_recorded_endpoints() {
    let raw = vec![
        row(3, 100, "0:primary", HistoryKind::QuotaReset, "100", "99"),
        row(3, 110, "0:primary", HistoryKind::QuotaReset, "99", "98"),
        row(3, 120, "0:primary", HistoryKind::QuotaDecrease, "98", "97"),
    ];
    let rows = checked(&raw);
    assert_eq!(rows.len(), 3);
    for (original, presentation) in raw.iter().zip(&rows) {
        assert_observation_preserved(original, presentation);
        assert_eq!(presentation.kind, HistoryKind::QuotaDecrease);
        assert_eq!(presentation.epoch, 0);
        assert!(is_confirmed_quota_change(presentation));
    }
}

#[test]
fn missing_observations_never_rebase_a_legitimate_legacy_drop() {
    let raw = vec![
        row(3, 100, "0:primary", HistoryKind::QuotaDecrease, "100", "99"),
        // No observation supplies 99→98. The recorded 98→97 remains one point.
        row(3, 200, "0:primary", HistoryKind::QuotaDecrease, "98", "97"),
        row(3, 210, "0:primary", HistoryKind::QuotaReset, "95", "94"),
    ];
    let rows = checked(&raw);
    assert_eq!(rows.len(), raw.len());
    for (original, presentation) in raw.iter().zip(&rows) {
        assert_observation_preserved(original, presentation);
    }
    let intervals = completed_intervals(&[], &rows);
    assert_eq!(intervals[1].percentage_points, 1.);
    assert!(intervals[1].partial && !intervals[1].comparable);
    assert!(intervals[1]
        .reasons
        .contains(&IntervalReason::QuotaBoundaryMismatch));
}

#[test]
fn reused_anonymous_source_indices_never_merge_separate_runs() {
    let raw = vec![
        row(3, 100, "0:primary", HistoryKind::QuotaDecrease, "100", "99"),
        row(4, 110, "0:primary", HistoryKind::QuotaDecrease, "50", "49"),
        row(3, 120, "0:primary", HistoryKind::QuotaDecrease, "99", "98"),
        row(4, 130, "0:primary", HistoryKind::QuotaReset, "49", "48"),
        row(3, 140, "0:primary", HistoryKind::QuotaReset, "98", "100"),
        row(4, 150, "0:primary", HistoryKind::QuotaDecrease, "48", "47"),
    ];
    let rows = checked(&raw);
    assert_eq!(rows.len(), raw.len());
    for (original, presentation) in raw.iter().zip(&rows) {
        assert_observation_preserved(original, presentation);
        assert!(presentation.reconciliation.is_none());
    }
    assert_eq!(
        rows.iter().map(|e| e.epoch).collect::<Vec<_>>(),
        vec![0, 0, 0, 0, 1, 0]
    );
    let intervals = completed_intervals(&[], &rows);
    assert_eq!(
        intervals[1].start, None,
        "another run cannot provide a start"
    );
    assert_eq!(intervals[2].start, Some(100));
    assert_eq!(intervals[3].start, Some(110));
}

#[test]
fn separate_legacy_sources_and_slots_have_independent_cycles() {
    let raw = vec![
        row(3, 100, "0:primary", HistoryKind::QuotaReset, "0", "100"),
        row(3, 110, "1:primary", HistoryKind::QuotaReset, "100", "99"),
        row(3, 120, "0:secondary", HistoryKind::QuotaReset, "50", "49"),
        row(3, 130, "0:primary", HistoryKind::QuotaDecrease, "100", "99"),
    ];
    let rows = checked(&raw);
    assert_eq!(rows.len(), 4);
    assert_eq!(
        rows.iter().map(|e| e.epoch).collect::<Vec<_>>(),
        vec![1, 0, 0, 1]
    );
    for (original, presentation) in raw.iter().zip(&rows) {
        assert_observation_preserved(original, presentation);
    }
}

#[test]
fn zero_metadata_rows_disappear_only_in_presentation_and_never_advance_cycles() {
    let mut raw = vec![row(
        3,
        100,
        "0:primary",
        HistoryKind::QuotaDecrease,
        "100",
        "99",
    )];
    for n in 0..50 {
        let mut zero = row(
            3,
            110 + n,
            "0:primary",
            HistoryKind::QuotaReset,
            "99",
            "99.0",
        );
        zero.epoch = 10 + n as u64;
        zero.delta = "0".into();
        raw.push(zero);
    }
    raw.extend([
        row(3, 200, "0:primary", HistoryKind::QuotaReset, "99", "98"),
        row(3, 210, "0:primary", HistoryKind::QuotaReset, "98", "100"),
        row(3, 220, "0:primary", HistoryKind::QuotaReset, "100", "99"),
    ]);
    let rows = checked(&raw);
    assert_eq!(rows.len(), 4);
    assert_eq!(
        rows.iter().map(|e| e.epoch).collect::<Vec<_>>(),
        vec![0, 0, 1, 1]
    );
    assert_eq!(
        rows.iter()
            .filter(|e| e.kind == HistoryKind::QuotaReset)
            .count(),
        1
    );
    assert_eq!(raw.len(), 54);
    assert_eq!(raw[1].epoch, 10);
}

#[test]
fn actual_legacy_rises_are_classified_from_endpoints_even_with_a_decrease_label() {
    let raw = vec![
        row(3, 100, "0:primary", HistoryKind::QuotaDecrease, "0", "100"),
        row(3, 110, "0:primary", HistoryKind::QuotaDecrease, "100", "99"),
        row(3, 120, "0:primary", HistoryKind::QuotaReset, "97", "100"),
    ];
    let rows = checked(&raw);
    assert_eq!(rows[0].kind, HistoryKind::QuotaReset);
    assert_eq!(rows[2].kind, HistoryKind::QuotaReset);
    assert_eq!(
        rows.iter().map(|e| e.epoch).collect::<Vec<_>>(),
        vec![1, 1, 2]
    );
    assert!(rows.iter().all(is_confirmed_quota_change));
    for (original, presentation) in raw.iter().zip(&rows) {
        assert_observation_preserved(original, presentation);
    }
}

#[test]
fn legacy_decimal_credit_rows_preserve_precision_spelling_and_distinct_observations() {
    let mut first = row(
        3,
        100,
        "0:credits",
        HistoryKind::CreditTopUp,
        "9007199254740993.000000000000000001",
        "9007199254740993",
    );
    first.delta = "0.000000000000000001".into();
    let mut second = row(
        3,
        110,
        "0:credits",
        HistoryKind::CreditDecrease,
        "0002.10",
        "0002.00",
    );
    second.delta = "0.10".into();
    let mut repeated_endpoint = second.clone();
    repeated_endpoint.observed_at_unix = 120;
    let mut other_run = second.clone();
    other_run.session = "4".into();
    other_run.observed_at_unix = 130;
    let raw = vec![first, second, repeated_endpoint, other_run];
    let rows = checked(&raw);
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().all(|e| e.kind == HistoryKind::CreditDecrease));
    for (original, presentation) in raw.iter().zip(&rows) {
        assert_observation_preserved(original, presentation);
        assert_eq!(original.epoch, presentation.epoch);
    }
}

#[test]
fn legacy_credit_direction_uses_exact_decimal_comparison_and_skips_only_equal_values() {
    let raw = vec![
        row(
            3,
            100,
            "0:credits",
            HistoryKind::CreditDecrease,
            "2.1",
            "2.1001",
        ),
        row(3, 110, "0:credits", HistoryKind::CreditTopUp, "10", "9.999"),
        row(
            3,
            120,
            "0:credits",
            HistoryKind::CreditDecrease,
            "0.01",
            "0.001",
        ),
        row(
            3,
            130,
            "0:credits",
            HistoryKind::CreditTopUp,
            "2.00",
            "002.0",
        ),
    ];
    let rows = checked(&raw);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].kind, HistoryKind::CreditTopUp);
    assert_eq!(rows[1].kind, HistoryKind::CreditDecrease);
    assert_eq!(rows[2].kind, HistoryKind::CreditDecrease);
}

#[test]
fn legacy_compatibility_never_promotes_malformed_imported_or_modern_history() {
    let base = row(3, 100, "0:primary", HistoryKind::QuotaReset, "100", "99");
    let mut cases = Vec::new();
    for session in [
        "",
        "imported-local-observations",
        "imported-local-observations-v2",
        "run-3",
        "+3",
        "3.0",
    ] {
        let mut event = base.clone();
        event.session = session.into();
        cases.push(event);
    }
    for source in [
        "codex",
        ":primary",
        "a:primary",
        "0:primary:extra",
        "0:other",
        "0:credits",
    ] {
        let mut event = base.clone();
        event.source_key = source.into();
        cases.push(event);
    }
    let mut annotated = base.clone();
    annotated.evidence = Some(HistoryEvidence {
        imported: true,
        ..Default::default()
    });
    cases.push(annotated);
    let mut keyed = base.clone();
    keyed.window_key = Some("primary".into());
    cases.push(keyed);
    let mut anchored = base.clone();
    anchored.before_observed_at_unix = Some(90);
    cases.push(anchored);
    let mut reconciled = base;
    reconciled.reconciliation = Some(HistoryReconciliation::UncertainRecovery);
    cases.push(reconciled);
    for event in cases {
        assert!(
            normalized(std::slice::from_ref(&event)).is_empty(),
            "must not repair unsupported shape: {event:?}"
        );
    }
}

#[test]
fn legacy_rows_never_supply_or_rewrite_a_current_codex_baseline() {
    let legacy = row(3, 100, "0:primary", HistoryKind::QuotaReset, "100", "99");
    let mut current = row(3, 110, "codex", HistoryKind::QuotaDecrease, "20", "19");
    current.session = "current-live-session".into();
    current.window_key = Some("primary".into());
    current.evidence = Some(HistoryEvidence::default());
    let legacy_next = row(3, 120, "0:primary", HistoryKind::QuotaDecrease, "98", "97");
    let raw = vec![legacy, current, legacy_next];
    let rows = checked(&raw);
    assert_eq!(rows.len(), 3);
    for (original, presentation) in raw.iter().zip(&rows) {
        assert_observation_preserved(original, presentation);
        assert!(presentation.reconciliation.is_none());
    }
}

#[test]
fn adjacent_recorded_same_endpoints_remain_distinct_legacy_observations() {
    let raw = vec![
        row(3, 100, "0:primary", HistoryKind::QuotaReset, "100", "99"),
        row(3, 110, "0:primary", HistoryKind::QuotaDecrease, "100", "99"),
    ];
    let rows = checked(&raw);
    assert_eq!(rows.len(), 2);
    for (original, presentation) in raw.iter().zip(&rows) {
        assert_observation_preserved(original, presentation);
    }
}
