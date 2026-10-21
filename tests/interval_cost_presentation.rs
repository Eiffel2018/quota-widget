//! Synthetic-only presentation tests over the canonical interval calculator.
use chrono::DateTime;
use quota_widget::{
    display_time,
    interval_views::{newest_indices, present},
    intervals::{completed_intervals, CompletedInterval, IntervalReason, StartBoundaryKind},
    model::{HistoryEvent, HistoryEvidence, HistoryKind},
    tokens::TokenEvent,
};

fn token(timestamp: i64, model: &str, cost_usd: Option<f64>) -> TokenEvent {
    TokenEvent {
        id: format!("synthetic:{model}:{timestamp}"),
        timestamp,
        model: model.into(),
        model_assumed: false,
        reasoning_effort: None,
        session: "synthetic-task".into(),
        response_id: Some(format!("response:{timestamp}")),
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

fn complete() -> CompletedInterval {
    completed_intervals(
        &[token(0, "a", Some(0.)), token(15, "a", Some(6.))],
        &[quota(10, 100., 99.), quota(20, 99., 98.)],
    )
    .remove(1)
}

fn epoch(value: &str) -> i64 {
    DateTime::parse_from_rfc3339(value).unwrap().timestamp()
}

#[test]
fn empty_history_and_empty_intervals_have_no_invented_rows() {
    let intervals = completed_intervals(&[], &[]);
    assert!(intervals.is_empty());
    assert!(newest_indices(&intervals).is_empty());
    assert!(newest_indices(&[]).is_empty());
}

#[test]
fn complete_interval_uses_canonical_subtotal_ratio_and_full_hk_bounds() {
    let interval = complete();
    assert_eq!(interval.known_usd, 6.);
    assert_eq!(interval.cost_per_point, Some(6.));
    let view = present(&interval);
    assert_eq!(view.change, "99% → 98%");
    assert_eq!(view.start, "1970-01-01 08:00:10 +08:00");
    assert_eq!(view.end, "1970-01-01 08:00:20 +08:00");
    assert_eq!(view.duration, "00:00:10");
    assert_eq!(view.total, "$6.0000");
    assert_eq!(view.per_point, "$6.0000/百分點");
    assert_eq!(view.status, "完整");
    for expected in [
        &view.start,
        &view.end,
        &view.duration,
        &view.total,
        &view.per_point,
    ] {
        assert!(view.tooltip.contains(expected));
    }
    assert!(view.tooltip.contains("記錄來源：codex"));
    assert!(view.tooltip.contains("觀察工作階段：synthetic-observer"));
    assert!(view.tooltip.lines().all(|line| line.contains('：')));
    assert!(!view.tooltip.contains("API 等價費用估算"));
    assert!(!view.tooltip.contains("並非訂閱實際帳單"));
    assert!(!view.tooltip.contains("不能當作每%價格"));
}

#[test]
fn partial_unknown_prices_keep_known_subtotal_without_a_per_point_price() {
    let intervals = completed_intervals(
        &[
            token(0, "a", Some(0.)),
            token(12, "a", Some(2.)),
            token(15, "unknown", None),
            token(20, "b", Some(4.)),
        ],
        &[quota(10, 100., 99.), quota(20, 99., 97.)],
    );
    let interval = &intervals[1];
    assert_eq!(interval.known_usd, 6.);
    assert_eq!(interval.unknown_cost_events, 1);
    assert_eq!(interval.cost_per_point, None);
    let view = present(interval);
    assert_eq!(view.change, "99% → 97%");
    assert_eq!(view.total, "$6.0000 *");
    assert_eq!(view.per_point, "未知");
    assert_eq!(view.status, "部分");
    assert!(interval
        .reasons
        .contains(&IntervalReason::UnknownModelPrice));
    assert!(!view.tooltip.contains("不能當作每%價格"));
    assert!(!view.tooltip.contains("$3.0000"));
}

#[test]
fn compact_partial_tooltip_preserves_numeric_data_and_exported_flags() {
    let mut partial = token(15, "a", Some(6.));
    partial.model_assumed = true;
    partial.partial_flags.push("response_coverage_gap".into());
    let intervals = completed_intervals(
        &[token(0, "a", Some(0.)), partial],
        &[quota(10, 100., 99.), quota(20, 99., 98.)],
    );
    let interval = &intervals[1];
    let before = serde_json::to_value(interval).unwrap();
    let view = present(interval);
    assert_eq!(view.total, "$6.0000 *");
    assert_eq!(view.per_point, "未知");
    assert_eq!(view.status, "部分");
    assert!(view.tooltip.lines().all(|line| line.contains('：')));
    for prose in [
        "不可比較",
        "限制：",
        "資料不完整",
        "不能當作",
        "並非訂閱",
        "不拆成",
    ] {
        assert!(!view.tooltip.contains(prose), "unexpected prose: {prose}");
    }
    assert_eq!(interval.models[0].events, 1);
    assert_eq!(interval.models[0].partial_events, 1);
    assert_eq!(interval.models[0].coverage_gap_events, 1);
    for reason in [
        IntervalReason::AssumedModel,
        IntervalReason::PartialTokenRecords,
        IntervalReason::TokenCoverageGap,
    ] {
        assert!(interval.reasons.contains(&reason));
    }
    assert_eq!(serde_json::to_value(interval).unwrap(), before);
}

#[test]
fn unknown_prices_and_missing_tokens_have_zero_known_subtotal_with_reasons() {
    let history = [quota(10, 100., 99.), quota(20, 99., 98.)];
    for (events, expected_reason) in [
        (
            vec![token(0, "a", Some(0.)), token(15, "unknown", None)],
            IntervalReason::UnknownModelPrice,
        ),
        (Vec::new(), IntervalReason::NoTokenEvents),
    ] {
        let intervals = completed_intervals(&events, &history);
        assert_eq!(intervals[1].known_usd, 0.);
        let view = present(&intervals[1]);
        assert_eq!(view.total, "$0.0000 *");
        assert_eq!(view.per_point, "未知");
        assert!(intervals[1].reasons.contains(&expected_reason));
    }
}

#[test]
fn recorded_zero_cost_is_complete_numeric_zero() {
    let intervals = completed_intervals(
        &[token(0, "a", Some(0.)), token(15, "a", Some(0.))],
        &[quota(10, 100., 99.), quota(20, 99., 98.)],
    );
    let view = present(&intervals[1]);
    assert_eq!(view.total, "$0.0000");
    assert_eq!(view.per_point, "$0.0000/百分點");
    assert_eq!(view.status, "完整");
    assert!(!view.tooltip.contains("不完整"));
}

#[test]
fn raw_fractional_endpoints_and_small_costs_are_not_rounded_into_other_observations() {
    let intervals = completed_intervals(
        &[token(0, "a", Some(0.)), token(15, "a", Some(0.000025))],
        &[
            quota(10, 100., 99.123456789),
            quota(20, 99.123456789, 98.123456789),
        ],
    );
    let view = present(&intervals[1]);
    assert_eq!(view.change, "99.123456789% → 98.123456789%");
    assert_eq!(view.total, "$0.000025");
    assert_eq!(view.per_point, "$0.000025/百分點");
    let mut invalid = intervals[1].clone();
    invalid.before = f64::NAN;
    invalid.after = f64::INFINITY;
    invalid.percentage_points = f64::NAN;
    let view = present(&invalid);
    assert_eq!(view.change, "未知% → 未知%");
    assert_eq!(view.per_point, "未知");
    assert!(!view.tooltip.contains("NaN"));
    assert!(!view.tooltip.contains("inf"));
}

#[test]
fn multi_point_drop_stays_one_observation_with_an_explicit_average() {
    let intervals = completed_intervals(
        &[
            token(0, "a", Some(0.)),
            token(12, "a", Some(2.)),
            token(20, "b", Some(4.)),
        ],
        &[quota(10, 100., 99.5), quota(20, 99.5, 97.)],
    );
    assert_eq!(intervals.len(), 2);
    assert_eq!(newest_indices(&intervals), [1, 0]);
    assert_eq!(intervals[1].percentage_points, 2.5);
    assert_eq!(intervals[1].cost_per_point, Some(2.4));
    let view = present(&intervals[1]);
    assert_eq!(view.change, "99.5% → 97%");
    assert_eq!(view.total, "$6.0000");
    assert_eq!(view.per_point, "平均 $2.4000/百分點");
    assert_eq!(view.status, "平均");
    assert!(view.tooltip.contains("下降：2.5 百分點"));
    assert!(!view.tooltip.contains("不拆成虛構 1% 區間"));
}

#[test]
fn reset_derived_interval_preserves_subtotal_and_rejects_comparison() {
    let intervals = completed_intervals(
        &[token(0, "a", Some(0.)), token(15, "a", Some(6.))],
        &[quota(10, 90., 100.), quota(20, 100., 98.)],
    );
    assert_eq!(intervals.len(), 1);
    assert_eq!(
        intervals[0].start_boundary_kind,
        StartBoundaryKind::QuotaReset
    );
    let view = present(&intervals[0]);
    assert_eq!(view.total, "$6.0000 *");
    assert_eq!(view.per_point, "未知");
    assert!(intervals[0]
        .reasons
        .contains(&IntervalReason::ResetBoundary));
}

#[test]
fn partial_noncomparable_and_reset_flags_never_leak_a_stale_ratio() {
    for case in 0..3 {
        let mut interval = complete();
        match case {
            0 => interval.partial = true,
            1 => interval.comparable = false,
            _ => interval.start_boundary_kind = StartBoundaryKind::QuotaReset,
        }
        assert_eq!(interval.cost_per_point, Some(6.));
        let view = present(&interval);
        assert_eq!(view.total, "$6.0000 *");
        assert_eq!(view.per_point, "未知");
        assert!(!view.tooltip.contains("$6.0000/百分點"));
    }
}

#[test]
fn invalid_or_missing_ratio_is_unavailable_and_never_recomputed() {
    for cost in [None, Some(f64::NAN), Some(f64::INFINITY), Some(-1.)] {
        let mut interval = complete();
        interval.cost_per_point = cost;
        let view = present(&interval);
        assert_eq!(view.total, "$6.0000");
        assert_eq!(view.per_point, "未知");
        assert_eq!(view.status, "未知");
        assert!(!view.tooltip.contains("$6.0000/百分點"));
        assert!(!view.tooltip.contains("NaN"));
        assert!(!view.tooltip.contains("inf"));
    }
}

#[test]
fn absent_reversed_equal_and_overflowing_boundaries_have_unknown_duration() {
    let mut missing = quota(20, 99., 98.);
    missing.before_observed_at_unix = None;
    let interval = completed_intervals(&[], &[missing]).remove(0);
    let view = present(&interval);
    assert_eq!(view.start, "未知");
    assert_eq!(view.duration, "未知");
    assert!(interval
        .reasons
        .contains(&IntervalReason::MissingStartBoundary));
    for (start, end) in [
        (20, 20),
        (21, 20),
        (i64::MIN, i64::MAX),
        (i64::MAX - 10, i64::MAX),
    ] {
        let mut interval = complete();
        interval.start = Some(start);
        interval.end = end;
        let view = present(&interval);
        assert_eq!(view.duration, "未知");
        assert_eq!(view.per_point, "未知");
        assert_eq!(view.total, "$6.0000 *");
    }
}

#[test]
fn hong_kong_day_month_year_rollovers_preserve_elapsed_seconds() {
    for (utc_start, utc_end, hk_start, hk_end) in [
        (
            "2026-10-17T15:59:59Z",
            "2026-10-17T16:00:01Z",
            "2026-10-17 23:59:59 +08:00",
            "2026-10-18 00:00:01 +08:00",
        ),
        (
            "2026-10-31T15:59:59Z",
            "2026-10-31T16:00:01Z",
            "2026-10-31 23:59:59 +08:00",
            "2026-11-01 00:00:01 +08:00",
        ),
        (
            "2026-12-31T15:59:59Z",
            "2026-12-31T16:00:01Z",
            "2026-12-31 23:59:59 +08:00",
            "2027-01-01 00:00:01 +08:00",
        ),
        (
            "2026-10-17T23:59:59Z",
            "2026-10-18T00:00:01Z",
            "2026-10-18 07:59:59 +08:00",
            "2026-10-18 08:00:01 +08:00",
        ),
    ] {
        let start = epoch(utc_start);
        let end = epoch(utc_end);
        let intervals = completed_intervals(
            &[token(start - 20, "a", Some(0.)), token(end, "a", Some(1.))],
            &[quota(start, 100., 99.), quota(end, 99., 98.)],
        );
        let view = present(&intervals[1]);
        assert_eq!(view.start, hk_start);
        assert_eq!(view.end, hk_end);
        assert_eq!(view.duration, "00:00:02");
        assert!(view.tooltip.contains(hk_start));
        assert!(view.tooltip.contains(hk_end));
        assert_eq!(intervals[1].start, Some(start));
        assert_eq!(intervals[1].end, end);
    }
    let mut interval = complete();
    interval.end = interval.start.unwrap() + 90_061;
    assert_eq!(present(&interval).duration, "1d 01:01:01");
}

#[test]
fn newest_order_is_deterministic_and_keeps_every_real_interval_and_duplicate() {
    let history: Vec<_> = (0..8)
        .map(|i| quota(10 + i * 10, 100. - i as f64, 99. - i as f64))
        .collect();
    let mut intervals = completed_intervals(&[], &history);
    intervals.reverse();
    let newest = newest_indices(&intervals);
    assert_eq!(newest.len(), 8);
    assert_eq!(newest, (0..8).collect::<Vec<_>>());
    assert_eq!(newest.iter().take(5).count(), 5);
    let mut second = intervals[0].clone();
    second.history_event_key = "z-key".into();
    intervals[0].history_event_key = "a-key".into();
    intervals.push(second.clone());
    intervals.push(second);
    let newest = newest_indices(&intervals);
    assert_eq!(newest.len(), 10);
    assert_eq!(&newest[..3], &[0, 8, 9]);
    let mut reverse = intervals.clone();
    reverse.reverse();
    let keys = |source: &[CompletedInterval]| {
        newest_indices(source)
            .iter()
            .map(|&i| source[i].history_event_key.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(keys(&intervals), keys(&reverse));
}

#[test]
fn presentation_and_ordering_leave_source_events_history_and_exports_unchanged() {
    let tokens = vec![
        token(0, "a", Some(0.)),
        token(12, "a", Some(2.)),
        token(20, "b", Some(4.)),
        token(21, "b", Some(999.)),
    ];
    let history = vec![quota(10, 100., 99.), quota(20, 99., 97.)];
    let intervals = completed_intervals(&tokens, &history);
    let before = serde_json::to_value((&tokens, &history, &intervals)).unwrap();
    assert_eq!(intervals[1].known_usd, 6.);
    assert_eq!(
        intervals[1].known_usd,
        intervals[1]
            .models
            .iter()
            .map(|model| model.known_usd)
            .sum::<f64>()
    );
    for index in newest_indices(&intervals) {
        let view = present(&intervals[index]);
        assert!(view
            .total
            .starts_with(&format!("${:.4}", intervals[index].known_usd)));
        assert_eq!(
            view.end,
            display_time::datetime(intervals[index].end).unwrap()
        );
    }
    let after = serde_json::to_value((&tokens, &history, &intervals)).unwrap();
    assert_eq!(before, after);
    assert_eq!(intervals, completed_intervals(&tokens, &history));
}
