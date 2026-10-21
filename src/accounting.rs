//! Align observed quota changes with local token estimates. Never treats these as a bill.
use crate::tokens::TokenEvent;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostBucket {
    pub start: i64,
    pub end: i64,
    pub percentage_points: f64,
    pub estimated_usd: Option<f64>,
    pub partial: bool,
    pub events: usize,
}
pub fn sum_cost(events: &[TokenEvent], start: i64, end: i64) -> (Option<f64>, bool, usize) {
    let selected: Vec<_> = events
        .iter()
        .filter(|e| e.timestamp > start && e.timestamp <= end)
        .collect();
    if selected.is_empty() {
        return (None, true, 0);
    }
    let partial = selected.iter().any(|e| !e.partial_flags.is_empty())
        || events.first().map(|e| e.timestamp > start).unwrap_or(true);
    if selected
        .iter()
        .any(|e| e.cost_usd.is_none() || crate::usage_views::coverage_incomplete(e))
    {
        (None, true, selected.len())
    } else {
        (
            Some(selected.iter().filter_map(|e| e.cost_usd).sum()),
            partial,
            selected.len(),
        )
    }
}
pub fn bucket(events: &[TokenEvent], start: i64, end: i64, percentage_points: f64) -> CostBucket {
    let (cost, partial, count) = sum_cost(events, start, end);
    CostBucket {
        start,
        end,
        percentage_points,
        estimated_usd: cost,
        partial,
        events: count,
    }
}
pub fn weighted_per_point(buckets: &[CostBucket]) -> Option<f64> {
    let valid: Vec<_> = buckets
        .iter()
        .filter(|b| !b.partial && b.percentage_points > 0. && b.estimated_usd.is_some())
        .collect();
    let points: f64 = valid.iter().map(|b| b.percentage_points).sum();
    (points > 0.).then(|| valid.iter().filter_map(|b| b.estimated_usd).sum::<f64>() / points)
}
/// A cost interval ends at an actual quota change; reset establishes a new boundary.
/// Ambiguous same-second changes never each receive the entire interval's cost.
pub fn event_cost(
    events: &[TokenEvent],
    event: &crate::model::HistoryEvent,
    all: &[crate::model::HistoryEvent],
) -> (Option<f64>, bool) {
    use crate::model::HistoryKind::{QuotaDecrease, QuotaReset};
    let relevant = |e: &&crate::model::HistoryEvent| {
        e.source_key == event.source_key
            && match (&e.window_key, &event.window_key) {
                (Some(a), Some(b)) => a == b,
                _ => e.window_label == event.window_label,
            }
            && matches!(e.kind, QuotaDecrease | QuotaReset)
            && e.delta.parse::<f64>().is_ok_and(|d| d > 0.0)
            && e.before.parse::<f64>().ok() != e.after.parse::<f64>().ok()
    };
    if all
        .iter()
        .filter(relevant)
        .filter(|e| e.observed_at_unix == event.observed_at_unix)
        .count()
        != 1
    {
        return (None, true);
    }
    let boundary = all
        .iter()
        .filter(relevant)
        .filter(|e| e.observed_at_unix < event.observed_at_unix)
        .max_by_key(|e| e.observed_at_unix);
    let Some(previous) = boundary else {
        return (None, true);
    };
    if previous.session != event.session
        || (previous.epoch != event.epoch && previous.kind != QuotaReset)
    {
        return (None, true);
    }
    let (cost, partial, _) = sum_cost(events, previous.observed_at_unix, event.observed_at_unix);
    (cost, partial || event.partial)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn weighted_not_average_of_ratios() {
        let a = CostBucket {
            start: 0,
            end: 1,
            percentage_points: 1.,
            estimated_usd: Some(1.),
            partial: false,
            events: 1,
        };
        let b = CostBucket {
            start: 1,
            end: 2,
            percentage_points: 3.,
            estimated_usd: Some(9.),
            partial: false,
            events: 1,
        };
        assert_eq!(weighted_per_point(&[a, b]), Some(2.5));
    }
    #[test]
    fn missing_cost_is_not_zero() {
        let a = CostBucket {
            start: 0,
            end: 1,
            percentage_points: 1.,
            estimated_usd: None,
            partial: true,
            events: 1,
        };
        assert_eq!(weighted_per_point(&[a]), None);
    }
}
