//! Per-interval quota estimates, separate from the raw token ledger.
//! Each estimate uses only priced token cost observed within that interval.
//! Coverage-only partials remain explicitly marked as lower bounds.
use crate::intervals::{CompletedInterval, IntervalReason, StartBoundaryKind};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuotaEstimate {
    pub usd_per_point: f64,
    pub known_usd: f64,
    pub percentage_points: f64,
    pub intervals: usize,
    pub start: i64,
    pub end: i64,
    pub lower_bound: bool,
}

impl QuotaEstimate {
    pub fn total(&self, interval: &CompletedInterval) -> f64 {
        self.usd_per_point * interval.percentage_points
    }
}

/// Add explicitly named estimate fields without overwriting raw ledger fields.
pub fn annotate_export(
    value: &mut serde_json::Value,
    interval: &CompletedInterval,
    estimate: Option<&QuotaEstimate>,
) {
    value["quota_estimated_usd"] = serde_json::json!(estimate.map(|e| e.total(interval)));
    value["quota_usd_per_point"] = serde_json::json!(estimate.map(|e| e.usd_per_point));
    value["quota_estimate"] = serde_json::json!(estimate);
    value["quota_estimate_method"] = serde_json::json!("interval_token_cost_per_quota_point_v1");
}

fn eligible(row: &CompletedInterval) -> bool {
    row.start.is_some_and(|start| start < row.end)
        && row.start_boundary_kind == StartBoundaryKind::QuotaDecrease
        && row.before.is_finite()
        && row.after.is_finite()
        && (0.0..=100.0).contains(&row.before)
        && (0.0..=100.0).contains(&row.after)
        && row.percentage_points.is_finite()
        && row.percentage_points > 0.0
        && ((row.before - row.after) - row.percentage_points).abs() < 1e-6
        && row.known_usd.is_finite()
        && row.known_usd >= 0.0
        && row.events > 0
        && row.unknown_cost_events == 0
        && row.assumed_model_events == 0
        && ((!row.partial && row.comparable) || !row.reasons.is_empty())
        && row.reasons.iter().all(|reason| {
            matches!(
                reason,
                IntervalReason::PartialTokenRecords | IntervalReason::TokenCoverageGap
            )
        })
}

/// Estimate each decrease independently as its priced token subtotal divided
/// by the observed percentage-point change. This preserves real variation
/// between intervals instead of assigning every row a pooled historical mean.
/// Unknown prices, assumed models, missing token events and bad boundaries do
/// not produce an estimate. Coverage-only partials use the known subtotal and
/// are marked as lower bounds. Raw accounting is never modified.
pub fn interval_estimates(rows: &[CompletedInterval]) -> Vec<Option<QuotaEstimate>> {
    rows.iter()
        .map(|row| {
            if !eligible(row) {
                return None;
            }
            let Some(start) = row.start else {
                return None;
            };
            let estimate = QuotaEstimate {
                usd_per_point: row.known_usd / row.percentage_points,
                known_usd: row.known_usd,
                percentage_points: row.percentage_points,
                intervals: 1,
                start,
                end: row.end,
                lower_bound: row.partial || !row.comparable || !row.reasons.is_empty(),
            };
            Some(estimate)
        })
        .collect()
}
