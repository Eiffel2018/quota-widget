//! Synthetic-only completed-interval accounting and scaling regressions.
use quota_widget::{
    history::{normalized, replay},
    intervals::{
        completed_intervals, history_event_key, IntervalIndex, IntervalReason, StartBoundaryKind,
    },
    model::{HistoryEvent, HistoryEvidence, HistoryKind, HistoryReconciliation},
    tokens::{ImportedQuotaObservation, ModelPrice, Pricing, TokenEvent},
};
use serde_json::json;

fn token(t: i64, model: &str, cost: Option<f64>) -> TokenEvent {
    TokenEvent {
        id: format!("synthetic:{model}:{t}"),
        timestamp: t,
        model: model.into(),
        model_assumed: false,
        reasoning_effort: None,
        session: "synthetic-task".into(),
        response_id: Some(format!("r{t}")),
        turn_id: Some("synthetic-turn".into()),
        source_kind: Some("token_usage_record".into()),
        input: 100,
        cached: 40,
        uncached: 60,
        output: 30,
        reasoning: 20,
        cost_usd: cost,
        coverage_start_timestamp_ms: None,
        partial_flags: Vec::new(),
    }
}
fn quota(t: i64, before: f64, after: f64) -> HistoryEvent {
    HistoryEvent {
        session: "synthetic-observer".into(),
        observed_at_unix: t,
        source_key: "codex".into(),
        epoch: 0,
        kind: if before > after {
            HistoryKind::QuotaDecrease
        } else {
            HistoryKind::QuotaReset
        },
        window_label: "week".into(),
        before: before.to_string(),
        after: after.to_string(),
        delta: (before - after).abs().to_string(),
        partial: false,
        before_observed_at_unix: Some(t - 10),
        window_key: Some("primary".into()),
        evidence: Some(HistoryEvidence {
            account_id: Some("synthetic-account".into()),
            ..Default::default()
        }),
        reconciliation: None,
    }
}
fn near(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
}

#[test]
fn actual_model_prices_reconcile_without_parent_price_or_double_reasoning() {
    let mut pricing = Pricing::default();
    pricing
        .set_rate(
            "synthetic-parent",
            ModelPrice {
                uncached_input_per_million: 10.,
                cached_input_per_million: 1.,
                output_per_million: 30.,
            },
        )
        .unwrap();
    pricing
        .set_rate(
            "synthetic-child",
            ModelPrice {
                uncached_input_per_million: 2.,
                cached_input_per_million: 0.2,
                output_per_million: 6.,
            },
        )
        .unwrap();
    let mut parent = token(15, "synthetic-parent", None);
    parent.cost_usd = pricing.cost(&parent.model, parent.uncached, parent.cached, parent.output);
    let mut child = token(20, "synthetic-child", None);
    child.session = "synthetic-child-task".into();
    child.cost_usd = pricing.cost(&child.model, child.uncached, child.cached, child.output);
    let events = vec![
        token(0, "synthetic-parent", Some(999.)),
        parent.clone(),
        child.clone(),
    ];
    let history = vec![quota(10, 100., 99.), quota(20, 99., 98.)];
    let rows = completed_intervals(&events, &history);
    let row = &rows[1];
    assert!(row.comparable);
    assert_eq!(row.start, Some(10));
    assert_eq!(row.end, 20);
    assert_eq!(row.events, 2);
    assert_eq!(row.models.len(), 2);
    assert_eq!(row.history_event_key, history_event_key(&history[1]));
    near(
        row.known_usd,
        parent.cost_usd.unwrap() + child.cost_usd.unwrap(),
    );
    near(row.known_usd, row.models.iter().map(|m| m.known_usd).sum());
    near(row.cost_per_point.unwrap(), row.known_usd);
    let child_row = row.models.iter().find(|m| m.model == child.model).unwrap();
    near(child_row.known_usd, 0.000308);
    assert_eq!(
        (
            child_row.input,
            child_row.cached,
            child_row.uncached,
            child_row.output,
            child_row.reasoning
        ),
        (100, 40, 60, 30, 20)
    );
    assert_eq!(child_row.input, child_row.cached + child_row.uncached);
}

#[test]
fn two_percentage_points_are_one_interval_with_average_cost() {
    let rows = completed_intervals(
        &[
            token(0, "a", Some(0.)),
            token(12, "a", Some(2.)),
            token(20, "b", Some(4.)),
        ],
        &[quota(10, 100., 99.), quota(20, 99., 97.)],
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].percentage_points, 2.);
    near(rows[1].known_usd, 6.);
    near(rows[1].cost_per_point.unwrap(), 3.);
}

#[test]
fn unknown_and_assumed_costs_keep_numeric_model_subtotals_but_are_not_comparable() {
    let mut assumed = token(17, "assumed", Some(3.));
    assumed.model_assumed = true;
    let rows = completed_intervals(
        &[
            token(0, "a", Some(0.)),
            token(12, "a", Some(2.)),
            token(15, "unknown", None),
            assumed,
        ],
        &[quota(10, 100., 99.), quota(20, 99., 98.)],
    );
    let row = &rows[1];
    assert_eq!(row.events, 3);
    assert_eq!(row.unknown_cost_events, 1);
    assert_eq!(row.assumed_model_events, 1);
    near(row.known_usd, 5.);
    assert!(row.partial && !row.comparable);
    assert_eq!(row.cost_per_point, None);
    assert!(row.reasons.contains(&IntervalReason::UnknownModelPrice));
    assert!(row.reasons.contains(&IntervalReason::AssumedModel));
    assert!(
        row.models
            .iter()
            .find(|m| m.model == "assumed")
            .unwrap()
            .model_assumed
    );
    let encoded = serde_json::to_string(&rows).unwrap();
    let decoded: Vec<quota_widget::intervals::CompletedInterval> =
        serde_json::from_str(&encoded).unwrap();
    assert_eq!(rows, decoded);
}

#[test]
fn exact_recorded_auto_review_is_excluded_but_regular_review_work_counts() {
    let mut excluded = token(15, "codex-auto-review", Some(1000.));
    excluded
        .partial_flags
        .push("excluded_recorded_codex_auto_review".into());
    let mut regular = token(16, "synthetic-sol", Some(7.));
    regular.session = "review-named-task".into();
    let rows = completed_intervals(
        &[token(0, "a", Some(0.)), excluded, regular],
        &[quota(10, 100., 99.), quota(20, 99., 98.)],
    );
    assert_eq!(rows[1].events, 1);
    near(rows[1].known_usd, 7.);
    assert!(rows[1].comparable);
    assert_eq!(rows[1].models[0].model, "synthetic-sol");
}

#[test]
fn startup_uses_only_a_real_recorded_baseline_and_never_claims_complete() {
    let events = [
        token(0, "a", Some(100.)),
        token(6, "a", Some(2.)),
        token(9, "a", Some(3.)),
    ];
    let mut first = quota(10, 100., 99.);
    first.before_observed_at_unix = Some(5);
    let row = completed_intervals(&events, &[first.clone()]).remove(0);
    assert_eq!(row.start, Some(5));
    assert_eq!(row.start_boundary_kind, StartBoundaryKind::ObservedBaseline);
    near(row.known_usd, 5.);
    assert_eq!(row.cost_per_point, None);
    assert!(row.reasons.contains(&IntervalReason::StartupIncomplete));
    first.before_observed_at_unix = None;
    let row = completed_intervals(&events, &[first]).remove(0);
    assert_eq!(row.start, None);
    near(row.known_usd, 0.);
    assert_eq!(row.events, 0);
    assert!(row.reasons.contains(&IntervalReason::MissingStartBoundary));
}

#[test]
fn genuine_reset_starts_a_fresh_interval_and_never_becomes_a_drop() {
    let mut reset = quota(20, 98., 100.);
    reset.epoch = 1;
    let mut after = quota(30, 100., 99.);
    after.epoch = 1;
    let rows = completed_intervals(
        &[
            token(0, "a", Some(0.)),
            token(15, "a", Some(999.)),
            token(20, "a", Some(999.)),
            token(25, "a", Some(4.)),
        ],
        &[quota(10, 99., 98.), reset.clone(), after],
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].start, Some(20));
    assert_eq!(rows[1].start_boundary_kind, StartBoundaryKind::QuotaReset);
    assert_eq!(
        rows[1].previous_history_event_key,
        Some(history_event_key(&reset))
    );
    near(rows[1].known_usd, 4.);
    assert!(rows[1].partial);
    assert!(rows[1].reasons.contains(&IntervalReason::ResetBoundary));
    assert_eq!(rows[1].cost_per_point, None);
}

#[test]
fn explicit_quota_and_token_gaps_are_flagged_but_quiet_time_is_not_a_gap() {
    let mut gap = quota(200, 99., 98.);
    gap.partial = true;
    let mut token_gap = token(250, "a", Some(7.));
    token_gap.partial_flags.push("response_coverage_gap".into());
    let rows = completed_intervals(
        &[
            token(0, "a", Some(0.)),
            token(100, "a", Some(3.)),
            token_gap,
            token(10000, "a", Some(2.)),
        ],
        &[
            quota(10, 100., 99.),
            gap,
            quota(300, 98., 97.),
            quota(20000, 97., 96.),
        ],
    );
    assert!(rows[1]
        .reasons
        .contains(&IntervalReason::QuotaObservationGap));
    assert!(rows[2].reasons.contains(&IntervalReason::TokenCoverageGap));
    assert!(rows[2]
        .reasons
        .contains(&IntervalReason::PartialTokenRecords));
    assert!(
        rows[3].comparable,
        "quiet elapsed time alone does not prove missing coverage"
    );
    near(rows[3].known_usd, 2.);
}

#[test]
fn incomplete_previous_interval_does_not_poison_next_complete_interval() {
    let mut first = quota(10, 100., 99.);
    first.partial = true;
    let rows = completed_intervals(
        &[token(0, "a", Some(0.)), token(15, "a", Some(1.))],
        &[first, quota(20, 99., 98.)],
    );
    assert!(rows[0].partial);
    assert!(rows[1].comparable);
}

#[test]
fn account_source_window_session_and_epoch_never_supply_each_others_boundary() {
    for case in 0..5 {
        let first = quota(10, 100., 99.);
        let mut other = quota(15, 99., 98.);
        match case {
            0 => other.evidence.as_mut().unwrap().account_id = Some("other-account".into()),
            1 => other.source_key = "other-source".into(),
            2 => other.window_key = Some("secondary".into()),
            3 => other.session = "other-observer".into(),
            _ => other.epoch = 1,
        }
        other.before_observed_at_unix = None;
        let third = quota(20, 99., 98.);
        let rows = completed_intervals(
            &[
                token(0, "a", Some(0.)),
                token(12, "a", Some(2.)),
                token(17, "a", Some(3.)),
            ],
            &[first, other, third],
        );
        assert_eq!(rows[1].start, None, "case {case}");
        assert!(rows[1].partial);
        assert_eq!(rows[2].start, Some(10), "case {case}");
        near(rows[2].known_usd, 5.);
        if case == 3 {
            assert!(rows[1].reasons.contains(&IntervalReason::SessionChanged));
        }
        if case == 4 {
            assert!(rows[1].reasons.contains(&IntervalReason::EpochChanged));
        }
    }
}

#[test]
fn ambiguous_same_second_changes_cannot_double_allocate_or_seed_next_interval() {
    let rows = completed_intervals(
        &[
            token(0, "a", Some(0.)),
            token(15, "a", Some(2.)),
            token(25, "a", Some(3.)),
        ],
        &[
            quota(10, 100., 99.),
            quota(20, 99., 98.),
            quota(20, 98., 97.),
            quota(30, 97., 96.),
            quota(40, 96., 95.),
        ],
    );
    assert_eq!(rows[1].start, None);
    assert_eq!(rows[2].start, None);
    assert_eq!(rows[1].known_usd + rows[2].known_usd, 0.);
    assert!(rows[1].reasons.contains(&IntervalReason::AmbiguousBoundary));
    assert!(rows[3].reasons.contains(&IntervalReason::AmbiguousBoundary));
    assert!(rows[3].partial);
}

#[test]
fn uncertain_observations_never_reset_accumulator() {
    let mut uncertain = quota(15, 99., 100.);
    uncertain.reconciliation = Some(HistoryReconciliation::UncertainRecovery);
    let rows = completed_intervals(
        &[
            token(0, "a", Some(0.)),
            token(12, "a", Some(2.)),
            token(18, "a", Some(3.)),
        ],
        &[quota(10, 100., 99.), uncertain, quota(20, 99., 98.)],
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].start, Some(10));
    near(rows[1].known_usd, 5.);
    assert!(rows[1]
        .reasons
        .contains(&IntervalReason::UncertainQuotaObservation));
}

#[test]
fn normalized_stale_replay_yields_only_actual_decrement_intervals() {
    let snapshots: Vec<_> = [(100, "A", 4.), (110, "A", 3.), (120, "A", 2.), (121, "B", 3.), (124, "A", 2.), (126, "B", 3.), (128, "A", 2.), (140, "A", 1.)]
        .into_iter().map(|(timestamp, origin, remaining)| ImportedQuotaObservation {
            timestamp, timestamp_ms: Some(timestamp * 1000), origin_session: Some(origin.into()), source_id: Some(format!("synthetic-{origin}")),
            payload: json!({"limit_id":"codex","primary":{"used_percent":100. - remaining,"window_minutes":10080,"resets_at":10000}}).into(),
        }).collect();
    let history = normalized(&replay(&snapshots));
    let events = [
        token(0, "a", Some(0.)),
        token(115, "a", Some(2.)),
        token(125, "b", Some(3.)),
        token(135, "a", Some(4.)),
    ];
    let rows = completed_intervals(&events, &history);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows.iter().map(|r| r.percentage_points).sum::<f64>(), 3.);
    assert_eq!(rows[2].start, Some(120));
    near(rows[2].known_usd, 7.);
    assert!(
        !rows[2].partial && rows[2].comparable,
        "ignored +1 echoes do not introduce a new coverage gap"
    );
    assert!(history[rows[2].history_index].reconciliation.is_none());
}

#[test]
fn unsorted_tokens_and_history_have_stable_keys_and_exact_endpoint_membership() {
    let events = [
        token(20, "a", Some(4.)),
        token(0, "a", Some(0.)),
        token(10, "a", Some(100.)),
        token(11, "b", Some(3.)),
        token(21, "a", Some(999.)),
    ];
    let history = [quota(20, 99., 98.), quota(10, 100., 99.)];
    let rows = completed_intervals(&events, &history);
    assert_eq!(rows[0].history_index, 0);
    assert_eq!(rows[0].start, Some(10));
    near(rows[0].known_usd, 7.);
    assert_eq!(rows[0].events, 2);
    assert_eq!(rows[0].history_event_key, history_event_key(&history[0]));
    let mut sorted = events.to_vec();
    sorted.sort_by_key(|e| e.timestamp);
    assert_eq!(
        IntervalIndex::new(&sorted).completed_intervals(&history),
        rows
    );
}

#[test]
fn no_tokens_and_late_start_are_unknown_coverage_not_free_complete_intervals() {
    let history = [quota(10, 100., 99.), quota(20, 99., 98.)];
    let empty = completed_intervals(&[], &history);
    assert_eq!(empty[1].known_usd, 0.);
    assert_eq!(empty[1].cost_per_point, None);
    assert!(empty[1].reasons.contains(&IntervalReason::NoTokenEvents));
    let late = completed_intervals(&[token(15, "a", Some(3.))], &history);
    assert_eq!(late[1].known_usd, 3.);
    assert!(late[1]
        .reasons
        .contains(&IntervalReason::TokenCoverageStartsLate));
}

#[test]
fn unique_legacy_window_alias_bridges_but_ambiguous_alias_does_not() {
    let mut legacy = quota(10, 100., 99.);
    legacy.window_key = None;
    let current = quota(20, 99., 98.);
    let events = [token(0, "a", Some(0.)), token(15, "a", Some(2.))];
    let rows = completed_intervals(&events, &[legacy.clone(), current.clone()]);
    assert_eq!(rows[1].start, Some(10));
    assert!(rows[1].comparable);
    let mut other = quota(12, 100., 99.);
    other.window_key = Some("secondary".into());
    let rows = completed_intervals(&events, &[legacy, other, current]);
    assert_eq!(
        rows[2].start,
        Some(10),
        "recorded baseline may coincide, but no prior change is claimed"
    );
    assert_eq!(
        rows[2].start_boundary_kind,
        StartBoundaryKind::ObservedBaseline
    );
    assert!(rows[2].partial);
}

#[test]
fn per_model_index_matches_naive_reference_across_many_distinct_ranges() {
    let events: Vec<_> = (0..1000)
        .map(|i| {
            token(
                i,
                &format!("model-{}", (i * 37) % 73),
                Some((i % 19) as f64 / 100.),
            )
        })
        .collect();
    let history: Vec<_> = (0..60)
        .map(|i| quota(i * 13, 100. - i as f64, 99. - i as f64))
        .collect();
    let index = IntervalIndex::new(&events);
    for row in index.completed_intervals(&history).into_iter().skip(1) {
        let expected: Vec<_> = events
            .iter()
            .filter(|e| e.timestamp > row.start.unwrap() && e.timestamp <= row.end)
            .collect();
        assert_eq!(row.events, expected.len());
        near(
            row.known_usd,
            expected.iter().filter_map(|e| e.cost_usd).sum(),
        );
        for model in &row.models {
            let selected: Vec<_> = expected.iter().filter(|e| e.model == model.model).collect();
            assert_eq!(model.events, selected.len());
            assert_eq!(model.input, selected.iter().map(|e| e.input).sum::<u64>());
            near(
                model.known_usd,
                selected.iter().filter_map(|e| e.cost_usd).sum(),
            );
        }
    }
}

#[test]
#[ignore = "release scaling benchmark: cargo test --release --test completed_intervals benchmark_100k_events_10k_history -- --ignored --nocapture"]
fn benchmark_100k_events_10k_history() {
    use std::time::Instant;
    let events: Vec<_> = (0..100_000)
        .map(|i| token(i, &format!("synthetic-model-{}", i % 8), Some(0.000001)))
        .collect();
    let history: Vec<_> = (0..10_000)
        .map(|i| {
            quota(
                i * 10,
                100. - i as f64 * 0.005,
                100. - (i + 1) as f64 * 0.005,
            )
        })
        .collect();
    let start = Instant::now();
    let index = IntervalIndex::new(&events);
    let indexed = start.elapsed();
    let start = Instant::now();
    let rows = index.completed_intervals(&history);
    let query = start.elapsed();
    assert_eq!(rows.len(), 10_000);
    assert_eq!(rows.iter().filter(|r| r.comparable).count(), 9_999);
    assert_eq!(rows.iter().map(|r| r.events).sum::<usize>(), 99_991);
    near(rows.iter().map(|r| r.known_usd).sum(), 0.099991);
    eprintln!("completed_intervals synthetic E=100000 H=10000: index={indexed:?} history+model rows={query:?} output_models={}", rows.iter().map(|r| r.models.len()).sum::<usize>());
}

#[test]
fn later_post_reset_interval_recovers_and_multiple_accounts_are_explicit() {
    let mut reset = quota(10, 98., 100.);
    reset.epoch = 1;
    let mut first = quota(20, 100., 99.);
    first.epoch = 1;
    let mut second = quota(30, 99., 98.);
    second.epoch = 1;
    let events = [
        token(0, "a", Some(0.)),
        token(15, "a", Some(2.)),
        token(25, "a", Some(3.)),
    ];
    let mut history = vec![reset, first, second];
    let rows = completed_intervals(&events, &history);
    assert!(rows[0].reasons.contains(&IntervalReason::ResetBoundary));
    assert!(rows[1].comparable);
    assert_eq!(rows[1].cost_per_point, Some(3.));
    let mut other = quota(25, 100., 99.);
    other.evidence.as_mut().unwrap().account_id = Some("second-synthetic-account".into());
    history.push(other);
    for row in completed_intervals(&events, &history) {
        assert!(row.reasons.contains(&IntervalReason::AmbiguousTokenAccount));
        assert!(row.partial && !row.comparable);
        assert_eq!(row.cost_per_point, None);
    }
}

#[test]
fn recorded_zero_cost_is_distinct_from_missing_cost_and_drop_comes_from_endpoints() {
    let mut current = quota(20, 99., 97.);
    current.delta = "500".into();
    let events = [token(0, "a", Some(0.)), token(15, "a", Some(0.))];
    let rows = completed_intervals(&events, &[quota(10, 100., 99.), current]);
    assert_eq!(rows[1].known_usd, 0.);
    assert_eq!(rows[1].unknown_cost_events, 0);
    assert_eq!(rows[1].percentage_points, 2.);
    assert_eq!(rows[1].cost_per_point, Some(0.));
}

#[test]
fn mismatched_quota_anchor_is_visible_without_claiming_a_comparable_ratio() {
    let events = [token(0, "a", Some(0.)), token(15, "a", Some(4.))];
    let history = [quota(10, 100., 99.), quota(20, 98., 97.)];
    let rows = completed_intervals(&events, &history);
    assert_eq!(rows[1].known_usd, 4.);
    assert!(rows[1]
        .reasons
        .contains(&IntervalReason::QuotaBoundaryMismatch));
    assert_eq!(rows[1].cost_per_point, None);
}

#[test]
fn empty_known_subtotal_is_positive_zero() {
    let h: HistoryEvent = serde_json::from_value(serde_json::json!({"session":"test","observed_at_unix":100,"source_key":"codex","epoch":0,"kind":"QuotaDecrease","window_label":"week","before":"90","after":"89","delta":"1"})).unwrap();
    let rows = completed_intervals(&[], &[h]);
    assert_eq!(rows[0].known_usd, 0.0);
    assert!(rows[0].known_usd.is_sign_positive());
}
