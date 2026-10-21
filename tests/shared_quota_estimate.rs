use quota_widget::{
    interval_views::present_with_estimate,
    intervals::{CompletedInterval, IntervalReason, StartBoundaryKind},
    quota_estimate::{annotate_export, shared_estimates},
};

fn chain(samples: &[(f64, f64)]) -> Vec<CompletedInterval> {
    let mut remaining = 90.;
    samples
        .iter()
        .enumerate()
        .map(|(i, &(points, dollars))| {
            let before = remaining;
            remaining -= points;
            CompletedInterval {
                history_index: i,
                history_event_key: format!("synthetic-{i}"),
                previous_history_event_key: Some(if i == 0 {
                    "anchor".into()
                } else {
                    format!("synthetic-{}", i - 1)
                }),
                session: "synthetic-observer".into(),
                account_id: Some("synthetic-account".into()),
                source_key: "codex".into(),
                window_key: Some("primary".into()),
                window_label: "week".into(),
                epoch: 1,
                start: Some(i as i64 * 60),
                end: (i as i64 + 1) * 60,
                start_boundary_kind: StartBoundaryKind::QuotaDecrease,
                before,
                after: remaining,
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
        })
        .collect()
}

#[test]
fn variable_dollars_per_drop_produce_one_shared_value_and_preserve_ledger() {
    let rows = chain(&[(1., 12.), (1., 17.), (1., 13.), (1., 14.), (1., 14.)]);
    let original = serde_json::to_value(&rows).unwrap();
    let estimates = shared_estimates(&rows);
    for (row, estimate) in rows.iter().zip(&estimates) {
        let e = estimate.as_ref().unwrap();
        assert_eq!(e.usd_per_point, 14.);
        assert_eq!(e.total(row), 14.);
        assert_eq!(e.intervals, 5);
        let view = present_with_estimate(row, Some(e));
        assert_eq!(view.total.trim(), "≈ $14.0000");
        assert_eq!(view.per_point.trim(), "≈ $14.0000/百分點");
        assert!(view
            .observed_total
            .contains(&format!("{:.4}", row.known_usd)));
        let mut export = serde_json::json!({"interval_known_usd": row.known_usd});
        annotate_export(&mut export, row, Some(e));
        assert_eq!(export["quota_estimated_usd"], 14.);
        assert_eq!(export["interval_known_usd"], row.known_usd);
        assert_eq!(export["quota_estimate"]["lower_bound"], false);
    }
    assert_eq!(serde_json::to_value(&rows).unwrap(), original);
}

#[test]
fn multipoint_drop_uses_ratio_of_sums_and_scales_total() {
    let rows = chain(&[(1., 8.), (1., 12.), (3., 30.)]);
    let estimates = shared_estimates(&rows);
    assert!(estimates
        .iter()
        .all(|e| e.as_ref().unwrap().usd_per_point == 10.));
    assert_eq!(estimates[2].as_ref().unwrap().total(&rows[2]), 30.);
}

#[test]
fn coverage_only_partials_supply_a_marked_lower_bound() {
    let mut rows = chain(&[(1., 12.); 5]);
    for row in &mut rows {
        row.partial = true;
        row.comparable = false;
        row.cost_per_point = None;
        row.reasons = vec![
            IntervalReason::PartialTokenRecords,
            IntervalReason::TokenCoverageGap,
        ];
    }
    let estimates = shared_estimates(&rows);
    let e = estimates[0].as_ref().unwrap();
    assert!(e.lower_bound);
    assert_eq!(e.usd_per_point, 12.);
    let view = present_with_estimate(&rows[0], Some(e));
    assert!(view.total.contains('≥'));
    assert!(view.status.contains("下限"));
    assert!(view.observed_total.ends_with(" *"));
}

#[test]
fn bad_boundaries_unknown_prices_and_assumptions_break_calibration() {
    for reason in [
        IntervalReason::StartupIncomplete,
        IntervalReason::QuotaObservationGap,
        IntervalReason::QuotaBoundaryMismatch,
        IntervalReason::UnknownModelPrice,
        IntervalReason::AssumedModel,
        IntervalReason::ResetBoundary,
        IntervalReason::AmbiguousTokenAccount,
    ] {
        let mut rows = chain(&[(1., 12.); 9]);
        rows[4].reasons = vec![reason];
        rows[4].partial = true;
        assert!(
            shared_estimates(&rows).iter().all(Option::is_none),
            "{reason:?}"
        );
    }
    for unknown in [true, false] {
        let mut rows = chain(&[(1., 12.); 5]);
        if unknown {
            rows[2].unknown_cost_events = 1;
        } else {
            rows[2].assumed_model_events = 1;
        }
        assert!(shared_estimates(&rows).iter().all(Option::is_none));
    }
}

#[test]
fn account_window_session_epoch_and_missing_link_never_mix() {
    for change in 0..7 {
        let mut rows = chain(&[(1., 12.); 8]);
        for row in &mut rows[4..] {
            match change {
                0 => row.account_id = Some("other".into()),
                1 => row.window_key = Some("secondary".into()),
                2 => row.window_label = "other".into(),
                3 => row.source_key = "other".into(),
                4 => row.session = "other".into(),
                5 => row.epoch = 2,
                _ => (),
            }
        }
        if change == 6 {
            rows[4].previous_history_event_key = Some("missing".into());
        }
        assert!(
            shared_estimates(&rows).iter().all(Option::is_none),
            "change {change}"
        );
    }
}

#[test]
fn ordering_does_not_change_estimate_and_duplicate_keys_do_not_double_count() {
    let mut rows = chain(&[(1., 12.); 5]);
    rows.reverse();
    assert!(shared_estimates(&rows).iter().all(Option::is_some));
    rows.push(rows[2].clone());
    assert!(shared_estimates(&rows).iter().all(Option::is_none));
}

#[test]
fn insufficient_or_invalid_data_is_unknown_and_known_zero_is_retained() {
    let short = chain(&[(1., 12.); 4]);
    assert!(shared_estimates(&short).iter().all(Option::is_none));
    assert_eq!(present_with_estimate(&short[0], None).total, "未知");
    let zero = chain(&[(1., 0.); 5]);
    assert_eq!(
        shared_estimates(&zero)[0].as_ref().unwrap().usd_per_point,
        0.
    );
    for invalid in [f64::NAN, f64::INFINITY, -1.] {
        let mut rows = chain(&[(1., 12.); 5]);
        rows[2].known_usd = invalid;
        assert!(shared_estimates(&rows).iter().all(Option::is_none));
    }
}
