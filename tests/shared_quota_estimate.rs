use quota_widget::{
    interval_views::present_with_estimate,
    intervals::{CompletedInterval, IntervalReason, StartBoundaryKind},
    quota_estimate::{annotate_export, interval_estimates},
};

fn interval(index: usize, points: f64, dollars: f64) -> CompletedInterval {
    let before = 90. - index as f64;
    CompletedInterval {
        history_index: index,
        history_event_key: format!("synthetic-{index}"),
        previous_history_event_key: Some(format!("synthetic-{}", index.saturating_sub(1))),
        session: "synthetic-observer".into(),
        account_id: Some("synthetic-account".into()),
        source_key: "codex".into(),
        window_key: Some("primary".into()),
        window_label: "week".into(),
        epoch: 1,
        start: Some(index as i64 * 60),
        end: (index as i64 + 1) * 60,
        start_boundary_kind: StartBoundaryKind::QuotaDecrease,
        before,
        after: before - points,
        percentage_points: points,
        known_usd: dollars,
        unknown_cost_events: 0,
        events: 1,
        assumed_model_events: 0,
        partial: false,
        comparable: true,
        cost_per_point: Some(dollars / points),
        reasons: vec![],
        models: vec![],
    }
}

#[test]
fn each_interval_keeps_its_own_cost_per_point_and_preserves_ledger() {
    let rows = vec![
        interval(0, 1., 12.),
        interval(1, 1., 17.),
        interval(2, 1., 13.),
        interval(3, 1., 14.),
        interval(4, 1., 14.),
    ];
    let original = serde_json::to_value(&rows).unwrap();
    let estimates = interval_estimates(&rows);
    for (row, estimate) in rows.iter().zip(&estimates) {
        let e = estimate.as_ref().unwrap();
        assert_eq!(e.usd_per_point, row.known_usd);
        assert_eq!(e.total(row), row.known_usd);
        assert_eq!(e.intervals, 1);
        let view = present_with_estimate(row, Some(e));
        assert!(view.total.contains(&format!("{:.4}", row.known_usd)));
        assert!(view.per_point.contains(&format!("{:.4}", row.known_usd)));
        assert!(view.status.contains("本段估算"));
        let mut export = serde_json::json!({"interval_known_usd": row.known_usd});
        annotate_export(&mut export, row, Some(e));
        assert_eq!(export["quota_estimated_usd"], row.known_usd);
        assert_eq!(export["interval_known_usd"], row.known_usd);
        assert_eq!(
            export["quota_estimate_method"],
            "interval_token_cost_per_quota_point_v1"
        );
    }
    assert_eq!(serde_json::to_value(&rows).unwrap(), original);
}

#[test]
fn multipoint_decrease_divides_only_its_own_token_cost() {
    let row = interval(0, 3., 30.);
    let estimate = interval_estimates(std::slice::from_ref(&row))[0]
        .clone()
        .unwrap();
    assert_eq!(estimate.usd_per_point, 10.);
    assert_eq!(estimate.total(&row), 30.);
}

#[test]
fn coverage_only_partial_is_a_lower_bound_from_its_own_subtotal() {
    let mut row = interval(0, 1., 12.);
    row.partial = true;
    row.comparable = false;
    row.cost_per_point = None;
    row.reasons = vec![
        IntervalReason::PartialTokenRecords,
        IntervalReason::TokenCoverageGap,
    ];
    let estimate = interval_estimates(&[row.clone()])[0].clone().unwrap();
    assert!(estimate.lower_bound);
    assert_eq!(estimate.usd_per_point, 12.);
    let view = present_with_estimate(&row, Some(&estimate));
    assert!(view.total.contains('≥'));
    assert!(view.status.contains("下限"));
    assert!(view.observed_total.ends_with(" *"));
}

#[test]
fn unknown_prices_assumptions_missing_events_and_bad_boundaries_stay_unknown() {
    let mut row = interval(0, 1., 12.);
    row.unknown_cost_events = 1;
    assert!(interval_estimates(&[row.clone()])[0].is_none());
    row.unknown_cost_events = 0;
    row.assumed_model_events = 1;
    assert!(interval_estimates(&[row.clone()])[0].is_none());
    row.assumed_model_events = 0;
    row.events = 0;
    assert!(interval_estimates(&[row.clone()])[0].is_none());
    row.events = 1;
    row.start_boundary_kind = StartBoundaryKind::ObservedBaseline;
    assert!(interval_estimates(&[row])[0].is_none());
}

#[test]
fn zero_known_cost_is_numeric_but_invalid_cost_is_unknown() {
    let mut row = interval(0, 1., 0.);
    assert_eq!(
        interval_estimates(&[row.clone()])[0]
            .as_ref()
            .unwrap()
            .usd_per_point,
        0.
    );
    row.known_usd = f64::NAN;
    assert!(interval_estimates(&[row])[0].is_none());
}
