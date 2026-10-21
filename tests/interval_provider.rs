//! Synthetic-only equivalence of bounded database-style queries and the owned
//! interval index. No session logs, real accounts, or installed state are read.
use quota_widget::{
    intervals::{
        completed_intervals, completed_intervals_with_provider, model_usage, CompletedInterval,
        IntervalReason, StartBoundaryKind,
    },
    model::{HistoryEvent, HistoryEvidence, HistoryKind, HistoryReconciliation},
    tokens::TokenEvent,
};
use std::io;

fn token(timestamp: i64, model: &str, cost_usd: Option<f64>) -> TokenEvent {
    TokenEvent {
        id: format!("synthetic:{model}:{timestamp}"),
        timestamp,
        model: model.into(),
        model_assumed: false,
        reasoning_effort: None,
        session: "synthetic-task".into(),
        response_id: Some(format!("r{timestamp}")),
        turn_id: Some("synthetic-turn".into()),
        source_kind: Some("token_usage_record".into()),
        input: 100,
        cached: 40,
        uncached: 60,
        output: 30,
        reasoning: 20,
        cost_usd,
        coverage_start_timestamp_ms: None,
        partial_flags: Vec::new(),
    }
}

fn quota(timestamp: i64, before: f64, after: f64) -> HistoryEvent {
    HistoryEvent {
        session: "synthetic-observer".into(),
        observed_at_unix: timestamp,
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
        before_observed_at_unix: Some(timestamp - 10),
        window_key: Some("primary".into()),
        evidence: Some(HistoryEvidence {
            account_id: Some("synthetic-account".into()),
            ..Default::default()
        }),
        reconciliation: None,
    }
}

fn from_provider(events: &[TokenEvent], history: &[HistoryEvent]) -> Vec<CompletedInterval> {
    let earliest = events
        .iter()
        .filter(|event| !event.is_excluded_auto_review())
        .map(|event| event.timestamp)
        .min();
    let mut ordered: Vec<_> = events.iter().collect();
    ordered.sort_by(|a, b| (a.timestamp, &a.id).cmp(&(b.timestamp, &b.id)));
    completed_intervals_with_provider(history, earliest, |start, end| {
        assert!(start < end, "only bounded nonempty time ranges are queried");
        Ok(model_usage(ordered.iter().copied().filter(|event| {
            event.timestamp > start && event.timestamp <= end
        })))
    })
    .unwrap()
}

fn equivalent(events: &[TokenEvent], history: &[HistoryEvent]) -> Vec<CompletedInterval> {
    let actual = from_provider(events, history);
    assert_eq!(actual, completed_intervals(events, history));
    actual
}

#[test]
fn empty_history_and_empty_or_late_tokens_keep_coverage_reasons() {
    assert!(completed_intervals_with_provider(&[], None, |_, _| {
        panic!("empty history must never query tokens")
    })
    .unwrap()
    .is_empty());
    let history = [quota(10, 100., 99.), quota(20, 99., 98.)];
    let empty = equivalent(&[], &history);
    assert!(empty[1].reasons.contains(&IntervalReason::NoTokenEvents));
    assert!(empty[1]
        .reasons
        .contains(&IntervalReason::TokenCoverageStartsLate));
    assert!(empty[1].known_usd.is_sign_positive());
    let late = equivalent(&[token(15, "a", Some(2.))], &history);
    assert_eq!(late[1].known_usd, 2.);
    assert!(late[1]
        .reasons
        .contains(&IntervalReason::TokenCoverageStartsLate));
}

#[test]
fn multimodel_ranges_preserve_exact_endpoints_sorting_and_exclusions() {
    let mut excluded = token(-50, "codex-auto-review", Some(999.));
    excluded
        .partial_flags
        .push("excluded_recorded_codex_auto_review".into());
    let mut also_excluded = excluded.clone();
    also_excluded.timestamp = 15;
    also_excluded.id = "synthetic-excluded-in-range".into();
    let events = [
        token(20, "z", Some(4.)),
        token(10, "z", Some(100.)),
        token(11, "a", Some(3.)),
        token(21, "z", Some(999.)),
        excluded,
        also_excluded,
    ];
    let history = [quota(20, 99., 97.), quota(10, 100., 99.)];
    let rows = equivalent(&events, &history);
    assert_eq!(rows[0].events, 2);
    assert_eq!(rows[0].known_usd, 7.);
    assert_eq!(rows[0].cost_per_point, Some(3.5));
    assert_eq!(rows[0].models[0].model, "a");
    assert_eq!(rows[0].models[1].model, "z");
    assert!(rows[1]
        .reasons
        .contains(&IntervalReason::TokenCoverageStartsLate));
}

#[test]
fn partial_unknown_assumed_and_quota_gap_reasons_match_the_owned_index() {
    let mut partial = token(15, "a", Some(2.));
    partial.model_assumed = true;
    partial.partial_flags.push("response_coverage_gap".into());
    let mut end = quota(20, 99., 98.);
    end.partial = true;
    let mut uncertain = quota(17, 99., 100.);
    uncertain.reconciliation = Some(HistoryReconciliation::UncertainRecovery);
    let rows = equivalent(
        &[token(0, "a", Some(0.)), partial, token(18, "unknown", None)],
        &[quota(10, 100., 99.), uncertain, end],
    );
    assert_eq!(rows[1].events, 2);
    assert_eq!(rows[1].known_usd, 2.);
    for reason in [
        IntervalReason::UncertainQuotaObservation,
        IntervalReason::QuotaObservationGap,
        IntervalReason::UnknownModelPrice,
        IntervalReason::AssumedModel,
        IntervalReason::PartialTokenRecords,
        IntervalReason::TokenCoverageGap,
    ] {
        assert!(rows[1].reasons.contains(&reason), "missing {reason:?}");
    }
    assert!(rows[1].partial && !rows[1].comparable);
    assert_eq!(rows[1].cost_per_point, None);
}

#[test]
fn reset_multiwindow_and_session_epoch_boundaries_keep_their_scope() {
    let events: Vec<_> = (0..=80)
        .map(|timestamp| token(timestamp, "a", Some(0.125)))
        .collect();
    let mut history = vec![quota(10, 100., 99.), quota(20, 99., 98.)];
    let mut reset = quota(30, 98., 100.);
    reset.epoch = 1;
    let mut first = quota(40, 100., 99.);
    first.epoch = 1;
    let mut second = quota(50, 99., 98.);
    second.epoch = 1;
    history.extend([reset, first, second]);
    let mut new_session = quota(60, 98., 97.);
    new_session.epoch = 1;
    new_session.session = "second-observer".into();
    history.push(new_session);
    let mut new_epoch = quota(70, 97., 96.);
    new_epoch.epoch = 2;
    new_epoch.session = "second-observer".into();
    history.push(new_epoch);
    let alternate_window: Vec<_> = history
        .iter()
        .cloned()
        .map(|mut event| {
            event.window_key = Some("secondary".into());
            event.window_label = "five-hour".into();
            event
        })
        .collect();
    history.extend(alternate_window);
    let rows = equivalent(&events, &history);
    assert_eq!(rows.len(), 12);
    for window in [&rows[..6], &rows[6..]] {
        assert_eq!(window[2].start, Some(30));
        assert_eq!(window[2].start_boundary_kind, StartBoundaryKind::QuotaReset);
        assert!(window[2].reasons.contains(&IntervalReason::ResetBoundary));
        assert!(window[3].comparable);
        assert!(window[4].reasons.contains(&IntervalReason::SessionChanged));
        assert!(window[5].reasons.contains(&IntervalReason::EpochChanged));
    }
    let mut other = quota(80, 100., 99.);
    other.evidence.as_mut().unwrap().account_id = Some("second-account".into());
    history.push(other);
    assert!(equivalent(&events, &history).iter().all(|interval| {
        interval
            .reasons
            .contains(&IntervalReason::AmbiguousTokenAccount)
            && interval.partial
            && !interval.comparable
            && interval.cost_per_point.is_none()
    }));
}

#[test]
fn unusable_boundaries_never_query_or_allocate_tokens() {
    let mut missing = quota(10, 100., 99.);
    missing.before_observed_at_unix = None;
    let mut invalid = quota(40, 100., 99.);
    invalid.session = "different-session".into();
    invalid.before_observed_at_unix = Some(50);
    let history = [missing, quota(20, 99., 98.), quota(20, 98., 97.), invalid];
    let actual = completed_intervals_with_provider(&history, Some(0), |_, _| {
        panic!("ambiguous, unavailable, and reversed boundaries have no token query")
    })
    .unwrap();
    assert_eq!(
        actual,
        completed_intervals(&[token(0, "a", Some(0.))], &history)
    );
    assert!(actual[0]
        .reasons
        .contains(&IntervalReason::MissingStartBoundary));
    assert!(actual[1]
        .reasons
        .contains(&IntervalReason::AmbiguousBoundary));
    assert!(actual[2]
        .reasons
        .contains(&IntervalReason::AmbiguousBoundary));
    assert!(actual[3]
        .reasons
        .contains(&IntervalReason::InvalidBoundaryTime));
    assert!(actual.iter().all(|interval| interval.events == 0));
}

#[test]
fn errors_propagate_without_an_empty_or_partial_success() {
    let history = [
        quota(10, 100., 99.),
        quota(20, 99., 98.),
        quota(30, 98., 97.),
    ];
    let mut queried = Vec::new();
    let error = completed_intervals_with_provider(&history, Some(0), |start, end| {
        queried.push((start, end));
        if end == 20 {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "synthetic DB failure",
            ))
        } else {
            Ok(Vec::new())
        }
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "synthetic DB failure");
    assert_eq!(queried, [(0, 10), (10, 20)]);
}

#[test]
fn aggregation_saturates_counters_and_does_not_price_reasoning_twice() {
    let mut first = token(1, "a", Some(3.));
    first.input = u64::MAX;
    first.output = u64::MAX;
    first.reasoning = u64::MAX;
    let second = first.clone();
    let events = [
        first,
        second,
        token(2, "b", Some(f64::NAN)),
        token(3, "b", Some(f64::INFINITY)),
        token(4, "b", Some(-1.)),
        token(5, "b", None),
        token(6, "b", Some(0.)),
    ];
    let models = model_usage(events.iter());
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].input, u64::MAX);
    assert_eq!(models[0].output, u64::MAX);
    assert_eq!(models[0].reasoning, u64::MAX);
    assert_eq!(models[0].known_usd, 6.);
    assert_eq!(models[0].events, 2);
    assert_eq!(models[1].known_usd, 0.);
    assert_eq!(models[1].events, 5);
    assert_eq!(models[1].unknown_cost_events, 4);
}

#[test]
fn many_partial_multimodel_ranges_match_with_unsorted_history_and_tokens() {
    let mut events: Vec<_> = (0..2_000)
        .map(|timestamp| {
            let mut event = token(
                timestamp,
                &format!("model-{}", (timestamp * 37) % 73),
                (timestamp % 17 != 0).then_some((timestamp % 19) as f64 * 0.125),
            );
            event.model_assumed = timestamp % 31 == 0;
            if timestamp % 47 == 0 {
                event.partial_flags.push("response_coverage_gap".into());
            }
            event
        })
        .collect();
    events.reverse();
    let mut history: Vec<_> = (0..100)
        .map(|index| quota(index * 13, 100. - index as f64, 99. - index as f64))
        .collect();
    history.reverse();
    let rows = equivalent(&events, &history);
    assert_eq!(rows.len(), 100);
    assert!(rows.iter().any(|interval| interval.models.len() > 10));
}
