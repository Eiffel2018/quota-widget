//! Synthetic regressions for conservative quota baselines and rollover evidence.
//! No user session files, account data, credentials, or live quota access.
use quota_widget::{
    history::{is_confirmed_quota_change, normalized, replay},
    intervals::{completed_intervals, IntervalReason},
    model::{HistoryEvent, HistoryKind, HistoryReconciliation},
    tokens::{ImportedQuotaObservation, TokenEvent},
};
use serde_json::json;

fn observation(t: i64, remaining: i64, reset: Option<i64>) -> ImportedQuotaObservation {
    ImportedQuotaObservation {
        timestamp: t,
        timestamp_ms: Some(t * 1000),
        origin_session: Some("synthetic-session".into()),
        source_id: Some("synthetic-source".into()),
        payload: json!({"limit_id":"codex","primary":{
            "used_percent":100-remaining,"window_minutes":10080,"resets_at":reset
        }})
        .into(),
    }
}

fn token(t: i64, cost: f64) -> TokenEvent {
    TokenEvent {
        id: format!("synthetic-token-{t}"),
        timestamp: t,
        model: "synthetic".into(),
        model_assumed: false,
        reasoning_effort: None,
        session: "synthetic-session".into(),
        response_id: None,
        turn_id: None,
        source_kind: Some("token_usage_record".into()),
        input: 0,
        cached: 0,
        uncached: 0,
        output: 0,
        reasoning: 0,
        cost_usd: Some(cost),
        coverage_start_timestamp_ms: None,
        partial_flags: vec![],
    }
}

fn full_depletion(reset: Option<i64>) -> Vec<ImportedQuotaObservation> {
    (0..=100)
        .map(|n| observation(100 + n * 10, 100 - n, reset))
        .collect()
}

fn checked_rows(observations: &[ImportedQuotaObservation]) -> Vec<HistoryEvent> {
    let observations_before = serde_json::to_value(observations).unwrap();
    let raw = replay(observations);
    let raw_before = raw.clone();
    let rows = normalized(&raw);
    assert_eq!(raw, raw_before, "raw evidence must remain unchanged");
    assert_eq!(
        serde_json::to_value(observations).unwrap(),
        observations_before,
        "imported payloads must remain unchanged"
    );
    assert_eq!(normalized(&rows), rows, "normalization must be idempotent");
    let mut reversed = observations.to_vec();
    reversed.reverse();
    assert_eq!(replay(&reversed), raw, "discovery order must not matter");
    assert_eq!(normalized(&replay(&reversed)), rows);
    rows
}

fn endpoints(rows: &[HistoryEvent]) -> Vec<(&str, &str)> {
    rows.iter()
        .map(|e| (e.before.as_str(), e.after.as_str()))
        .collect()
}

#[test]
fn reused_endpoints_keep_the_raw_disputed_trajectory_without_cost_boundaries() {
    let mut observations = full_depletion(None);
    observations.extend((0..=4).map(|n| observation(2000 + n * 100, 100 - n, None)));
    let rows = checked_rows(&observations);
    let tail: Vec<_> = rows
        .iter()
        .filter(|e| e.observed_at_unix >= 2000)
        .cloned()
        .collect();
    assert_eq!(
        endpoints(&tail),
        vec![
            ("0", "100"),
            ("100", "99"),
            ("99", "98"),
            ("98", "97"),
            ("97", "96")
        ]
    );
    assert_eq!(tail[0].kind, HistoryKind::QuotaReset);
    assert!(tail[1..]
        .iter()
        .all(|e| e.kind == HistoryKind::QuotaDecrease && e.delta == "1"));
    assert!(tail.iter().all(|e| e.partial
        && e.epoch == 0
        && e.reconciliation == Some(HistoryReconciliation::UncertainRecovery)
        && !is_confirmed_quota_change(e)));
    assert!(completed_intervals(&[], &rows).iter().all(|e| e.end < 2000));
}

#[test]
fn previously_unseen_endpoints_still_preserve_a_new_partial_trajectory() {
    let rows = checked_rows(&[
        observation(100, 1, None),
        observation(110, 0, None),
        observation(200, 100, None),
        observation(210, 99, None),
        observation(220, 98, None),
    ]);
    let next = &rows[1..];
    assert_eq!(
        endpoints(next),
        vec![("0", "100"), ("100", "99"), ("99", "98")]
    );
    assert!(!is_confirmed_quota_change(&next[0]));
    assert!(next[1].partial && is_confirmed_quota_change(&next[1]));
    assert!(!next[2].partial && is_confirmed_quota_change(&next[2]));
}

#[test]
fn unchanged_zero_with_new_metadata_does_not_hide_the_actual_rollover() {
    let prefix = full_depletion(Some(1200));
    let mut control = prefix.clone();
    control.extend([
        observation(1300, 100, Some(10000)),
        observation(1310, 99, Some(10000)),
    ]);
    let control_rows = checked_rows(&control);
    let mut early_metadata = prefix;
    early_metadata.extend([
        observation(1250, 0, Some(10000)),
        observation(1300, 100, Some(10000)),
        observation(1310, 99, Some(10000)),
    ]);
    let raw = replay(&early_metadata);
    assert!(!raw.iter().any(|e| e.observed_at_unix == 1250));
    let raw_reset = raw.iter().find(|e| e.observed_at_unix == 1300).unwrap();
    assert_eq!(
        raw_reset.evidence.as_ref().unwrap().before_reset_at,
        Some(10000)
    );
    let rows = checked_rows(&early_metadata);
    let tail = &rows[rows.len() - 2..];
    assert_eq!(endpoints(tail), vec![("0", "100"), ("100", "99")]);
    assert!(tail
        .iter()
        .all(|e| is_confirmed_quota_change(e) && e.epoch == 1));
    assert_eq!(tail[0].kind, HistoryKind::QuotaReset);
    assert_eq!(tail[1].kind, HistoryKind::QuotaDecrease);
    assert_eq!(
        endpoints(tail),
        endpoints(&control_rows[control_rows.len() - 2..])
    );
    let intervals = completed_intervals(&[token(0, 0.), token(1305, 4.)], &rows);
    let after_reset = intervals.iter().find(|e| e.end == 1310).unwrap();
    assert_eq!(after_reset.start, Some(1300));
    assert_eq!(after_reset.known_usd, 4.);
}

#[test]
fn pinned_two_preserves_disputed_drops_and_keeps_long_subtotal_noncomparable() {
    let mut observations: Vec<_> = (2..=12)
        .rev()
        .enumerate()
        .map(|(n, r)| observation(100 + n as i64 * 10, r, None))
        .collect();
    observations.extend(
        (3..=11)
            .rev()
            .enumerate()
            .map(|(n, r)| observation(1000 + n as i64 * 10, r, None)),
    );
    observations.extend([
        observation(2000, 1, None),
        observation(2001, 2, None),
        observation(2100, 0, None),
    ]);
    let rows = checked_rows(&observations);
    let tail: Vec<_> = rows
        .iter()
        .filter(|e| e.observed_at_unix >= 1000)
        .cloned()
        .collect();
    assert_eq!(
        endpoints(&tail),
        vec![
            ("2", "11"),
            ("11", "10"),
            ("10", "9"),
            ("9", "8"),
            ("8", "7"),
            ("7", "6"),
            ("6", "5"),
            ("5", "4"),
            ("4", "3"),
            ("2", "1"),
            ("1", "0")
        ]
    );
    assert!(tail[..9]
        .iter()
        .all(|e| e.reconciliation.is_some() && e.partial && !is_confirmed_quota_change(e)));
    assert!(tail[1..9]
        .iter()
        .all(|e| e.kind == HistoryKind::QuotaDecrease));
    let intervals =
        completed_intervals(&[token(0, 0.), token(500, 1000.), token(1500, 937.)], &rows);
    assert!(!intervals.iter().any(|e| (1000..2000).contains(&e.end)));
    let interval = intervals.iter().find(|e| e.end == 2000).unwrap();
    assert_eq!(interval.start, Some(200));
    assert_eq!(interval.known_usd, 1937.);
    assert_eq!(interval.percentage_points, 1.);
    assert!(interval.partial && !interval.comparable && interval.cost_per_point.is_none());
    assert!(interval
        .reasons
        .contains(&IntervalReason::UncertainQuotaObservation));
}

#[test]
fn snake_case_percent_and_deadline_fields_keep_their_recorded_meaning() {
    let rows = checked_rows(&[
        observation(100, 100, Some(1000)),
        observation(110, 99, Some(1000)),
        observation(120, 98, Some(1000)),
    ]);
    assert_eq!(endpoints(&rows), vec![("100", "99"), ("99", "98")]);
    assert!(rows.iter().all(|e| e.kind == HistoryKind::QuotaDecrease));
    assert!(rows
        .iter()
        .all(|e| e.evidence.as_ref().unwrap().reset_at == Some(1000)));
}

#[test]
fn late_old_cycle_then_cached_full_snapshot_cannot_confirm_a_second_reset() {
    let mut old = observation(150, 0, Some(120));
    old.origin_session = Some("synthetic-old-session".into());
    let rows = checked_rows(&[
        observation(100, 1, Some(120)),
        observation(110, 0, Some(120)),
        observation(130, 100, Some(10000)),
        observation(140, 99, Some(10000)),
        old,
        observation(160, 100, Some(10000)),
        observation(170, 99, Some(10000)),
        observation(180, 98, Some(10000)),
    ]);
    let resets: Vec<_> = rows
        .iter()
        .filter(|e| e.kind == HistoryKind::QuotaReset && is_confirmed_quota_change(e))
        .collect();
    assert_eq!(resets.len(), 1);
    assert_eq!(resets[0].observed_at_unix, 130);
    assert!(
        !rows.iter().any(|e| e.observed_at_unix == 160),
        "canonical 99→100 is an ignored +1 echo of the accepted cycle"
    );
    assert!(rows
        .iter()
        .filter(|e| e.observed_at_unix >= 130)
        .all(|e| e.epoch == 1));
    let intervals = completed_intervals(&[token(0, 0.), token(155, 7.)], &rows);
    assert!(!intervals.iter().any(|e| e.end == 170));
    let next = intervals.iter().find(|e| e.end == 180).unwrap();
    assert_eq!(next.start, Some(140));
    assert_eq!(next.known_usd, 7.);
    assert!(!next.partial && next.comparable && next.cost_per_point.is_some());
}

#[test]
fn expired_metadata_jitter_or_missing_rollover_conditions_never_confirm_a_reset() {
    for (old, new, time) in [
        (1200, 1280, 1250),
        (1200, 1290, 1250),
        (2000, 10000, 1300),
        (1200, 1210, 1300),
    ] {
        let mut observations = full_depletion(Some(old));
        observations.extend([
            observation(time - 10, 0, Some(new)),
            observation(time, 100, Some(new)),
            observation(time + 10, 99, Some(new)),
        ]);
        let rows = checked_rows(&observations);
        let tail: Vec<_> = rows.iter().filter(|e| e.observed_at_unix >= time).collect();
        assert_eq!(tail.len(), 2);
        assert!(tail
            .iter()
            .all(|e| e.epoch == 0 && e.reconciliation.is_some() && !is_confirmed_quota_change(e)));
    }
}

#[test]
fn meaningful_rollover_at_an_equal_timeline_endpoint_still_starts_a_cycle() {
    let rows = checked_rows(&[
        observation(100, 100, Some(120)),
        observation(110, 99, Some(120)),
        // An even older cycle is ignored, leaving the accepted endpoint at 99.
        observation(130, 0, Some(20)),
        observation(140, 99, Some(10000)),
        observation(150, 98, Some(10000)),
    ]);
    assert_eq!(
        endpoints(&rows),
        vec![("100", "99"), ("0", "99"), ("99", "98")]
    );
    assert!(rows.iter().all(is_confirmed_quota_change));
    assert_eq!(rows[1].kind, HistoryKind::QuotaReset);
    assert_eq!(rows[1].epoch, 1);
    assert_eq!(rows[2].epoch, 1);
}

#[test]
fn zero_to_zero_metadata_rollover_never_emits_a_reset_or_advances_epoch() {
    let mut observations = full_depletion(Some(1200));
    observations.extend([
        observation(1250, 0, Some(10000)),
        observation(1300, 0, Some(10001)),
        observation(1400, 0, Some(10002)),
    ]);
    let rows = checked_rows(&observations);
    assert_eq!(rows.len(), 100);
    assert!(rows
        .iter()
        .all(|e| e.epoch == 0 && e.kind == HistoryKind::QuotaDecrease));
    let mut zero_event = rows.last().unwrap().clone();
    zero_event.observed_at_unix = 1250;
    zero_event.kind = HistoryKind::QuotaReset;
    zero_event.before = "0".into();
    zero_event.after = "0".into();
    zero_event.delta = "0".into();
    zero_event.evidence.as_mut().unwrap().reset_at = Some(10000);
    let mut raw = replay(&observations);
    raw.push(zero_event);
    assert_eq!(normalized(&raw), rows);
}

#[test]
fn repeated_two_three_two_echoes_never_recharge_a_percentage_point() {
    let mut observations = vec![
        observation(100, 4, Some(10000)),
        observation(110, 3, Some(10000)),
        observation(120, 2, Some(10000)),
    ];
    observations.extend((0..20).map(|n| observation(130 + n * 10, 3 - n % 2, Some(10000))));
    observations.push(observation(400, 1, Some(10000)));
    let rows = checked_rows(&observations);
    let changes: Vec<_> = rows
        .iter()
        .filter(|e| is_confirmed_quota_change(e))
        .collect();
    assert_eq!(changes.len(), 3);
    assert_eq!(
        changes
            .iter()
            .map(|e| (e.before.as_str(), e.after.as_str()))
            .collect::<Vec<_>>(),
        vec![("4", "3"), ("3", "2"), ("2", "1")]
    );
    let intervals = completed_intervals(&[token(0, 0.), token(200, 4.)], &rows);
    assert_eq!(intervals.len(), 3);
    assert_eq!(intervals[2].start, Some(120));
    assert_eq!(intervals[2].known_usd, 4.);
    assert!(
        !intervals[2].partial && intervals[2].comparable,
        "ignored same-cycle +1 observations do not poison coverage"
    );
}

#[test]
fn immediate_pair_rollover_is_used_when_the_timeline_has_no_deadline() {
    let rows = checked_rows(&[
        observation(100, 3, None),
        observation(110, 2, None),
        // Metadata-only snapshots are not raw transitions.
        observation(120, 2, Some(125)),
        observation(130, 3, Some(10000)),
        observation(140, 2, Some(10000)),
    ]);
    assert_eq!(endpoints(&rows), vec![("3", "2"), ("2", "3"), ("3", "2")]);
    assert!(rows.iter().all(is_confirmed_quota_change));
    assert_eq!(rows[1].kind, HistoryKind::QuotaReset);
    assert_eq!(rows[1].epoch, 1);
    assert_eq!(rows[2].epoch, 1);
}
