//! A shared historical estimate, separate from the observed token ledger.
//! Integer quota observations and response completion times do not define a
//! dollar price per individual decrement. Pool contiguous observations instead.
use crate::intervals::{CompletedInterval, IntervalReason, StartBoundaryKind};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

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
    value["quota_estimate_method"] = serde_json::json!("contiguous_ratio_of_sums_with_carry_v2");
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

fn same_scope(a: &CompletedInterval, b: &CompletedInterval) -> bool {
    a.session == b.session
        && a.account_id == b.account_id
        && a.source_key == b.source_key
        && a.window_key == b.window_key
        && a.window_label == b.window_label
        && a.epoch == b.epoch
}

/// All rows in one uninterrupted chain share the same ratio of sums. Never
/// average individual ratios, combine quota windows, or bridge missing records.
/// At least three intervals spanning five points are required. Coverage-only
/// partials may contribute a clearly marked lower-bound estimate; other defects
/// break the chain. Input accounting is never modified.
pub fn shared_estimates(rows: &[CompletedInterval]) -> Vec<Option<QuotaEstimate>> {
    let mut result = vec![None; rows.len()];
    let mut order: Vec<_> = (0..rows.len()).collect();
    order.sort_by_key(|&i| (rows[i].end, &rows[i].history_event_key));
    let mut counts = BTreeMap::new();
    for row in rows {
        *counts.entry(&row.history_event_key).or_insert(0usize) += 1;
    }
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut group_by_key: BTreeMap<&str, usize> = BTreeMap::new();
    let mut used_predecessors = BTreeSet::new();
    for &i in &order {
        let row = &rows[i];
        if !eligible(row) || counts[&row.history_event_key] != 1 {
            continue;
        }
        let previous = row.previous_history_event_key.as_deref();
        let group = previous
            .and_then(|key| group_by_key.get(key).copied())
            .filter(|&g| {
                let prior = &rows[*groups[g].last().unwrap()];
                same_scope(prior, row)
                    && row.start == Some(prior.end)
                    && (prior.after - row.before).abs() < 1e-6
                    && !used_predecessors.contains(previous.unwrap())
            });
        let g = group.unwrap_or_else(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        if group.is_some() {
            used_predecessors.insert(previous.unwrap());
        }
        groups[g].push(i);
        group_by_key.insert(&row.history_event_key, g);
    }
    for group in groups {
        let points: f64 = group.iter().map(|&i| rows[i].percentage_points).sum();
        let known_usd: f64 = group.iter().map(|&i| rows[i].known_usd).sum();
        if group.len() < 3 || points < 5.0 || !points.is_finite() || !known_usd.is_finite() {
            continue;
        }
        let estimate = QuotaEstimate {
            usd_per_point: known_usd / points,
            known_usd,
            percentage_points: points,
            intervals: group.len(),
            start: rows[group[0]].start.unwrap(),
            end: rows[*group.last().unwrap()].end,
            lower_bound: group
                .iter()
                .any(|&i| rows[i].partial || !rows[i].reasons.is_empty() || !rows[i].comparable),
        };
        for i in group {
            result[i] = Some(estimate.clone());
        }
    }
    // A new observer or a temporary token-import gap must not erase a usable
    // calibration. Carry it forward only along matching quota endpoints for a
    // known account/window, never across a reset, changed epoch or stale gap.
    // These rows do NOT contribute zero/missing token costs to calibration.
    let mut latest: BTreeMap<_, (usize, Option<QuotaEstimate>)> = BTreeMap::new();
    for i in order {
        let row = &rows[i];
        let Some(account) = row.account_id.as_deref() else {
            continue;
        };
        let key = (account, &row.source_key, &row.window_key, &row.window_label);
        if result[i].is_none() && counts[&row.history_event_key] == 1 {
            if let Some((prior_index, Some(estimate))) = latest.get(&key) {
                let prior = &rows[*prior_index];
                let continuous = row
                    .start
                    .is_some_and(|start| start >= prior.end && start < row.end)
                    && row.end > prior.end
                    && estimate.end <= prior.end
                    && row.end - estimate.end <= 86400
                    && ((row.before - prior.after).abs() < 1e-6
                        || (row.session != prior.session && row.before < prior.after))
                    && row.before.is_finite()
                    && row.after.is_finite()
                    && (0.0..=100.0).contains(&row.after)
                    && row.percentage_points.is_finite()
                    && row.percentage_points > 0.0
                    && ((row.before - row.after) - row.percentage_points).abs() < 1e-6
                    && (row.session != prior.session || row.epoch == prior.epoch)
                    && row.start_boundary_kind != StartBoundaryKind::QuotaReset
                    && !row.reasons.iter().any(|reason| {
                        matches!(
                            reason,
                            IntervalReason::ResetBoundary
                                | IntervalReason::EpochChanged
                                | IntervalReason::UncertainQuotaObservation
                                | IntervalReason::AmbiguousBoundary
                                | IntervalReason::QuotaBoundaryMismatch
                                | IntervalReason::InvalidBoundaryTime
                                | IntervalReason::AmbiguousTokenAccount
                        )
                    });
                if continuous {
                    result[i] = Some(estimate.clone());
                }
            }
        }
        latest.insert(key, (i, result[i].clone()));
    }
    result
}
