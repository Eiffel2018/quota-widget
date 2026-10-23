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
        .filter(|e| !e.is_excluded_auto_review() && e.timestamp > start && e.timestamp <= end)
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
/// A cost interval ends at an actual, confirmed quota change.
/// Use `history_costs` for a complete history view rather than one call per row.
pub fn event_cost(
    events: &[TokenEvent],
    event: &crate::model::HistoryEvent,
    all: &[crate::model::HistoryEvent],
) -> (Option<f64>, bool) {
    event_cost_indexed(&CostIndex::new(events), event, all)
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

/// Known recorded cost is always numeric. Unknown/missing coverage is separate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ObservedCost {
    pub known_usd: f64,
    pub unknown_events: usize,
    pub events: usize,
    pub partial: bool,
}
/// Immutable prefix index, built on the worker when token data changes.
#[derive(Debug, Clone, Default)]
pub struct CostIndex {
    times: Vec<i64>,
    known: Vec<f64>,
    unknown: Vec<usize>,
    partial: Vec<usize>,
    coverage: Vec<usize>,
}
impl CostIndex {
    pub fn new(events: &[TokenEvent]) -> Self {
        let mut rows: Vec<_> = events
            .iter()
            .filter(|e| !e.is_excluded_auto_review())
            .collect();
        if !rows.windows(2).all(|w| w[0].timestamp <= w[1].timestamp) {
            rows.sort_by_key(|e| e.timestamp);
        }
        let mut index = Self {
            known: vec![0.],
            unknown: vec![0],
            partial: vec![0],
            coverage: vec![0],
            ..Self::default()
        };
        for e in rows {
            index.times.push(e.timestamp);
            index
                .known
                .push(index.known.last().copied().unwrap_or_default() + e.cost_usd.unwrap_or(0.));
            index.unknown.push(
                index.unknown.last().copied().unwrap_or_default()
                    + usize::from(e.cost_usd.is_none()),
            );
            index.partial.push(
                index.partial.last().copied().unwrap_or_default()
                    + usize::from(!e.partial_flags.is_empty()),
            );
            index.coverage.push(
                index.coverage.last().copied().unwrap_or_default()
                    + usize::from(crate::usage_views::coverage_incomplete(e)),
            );
        }
        index
    }
    pub fn observed(&self, start: i64, end: i64) -> ObservedCost {
        if self.times.is_empty() || end < start {
            return ObservedCost {
                partial: true,
                ..ObservedCost::default()
            };
        }
        let lo = self.times.partition_point(|t| *t <= start);
        let hi = self.times.partition_point(|t| *t <= end);
        let unknown = self.unknown[hi] - self.unknown[lo];
        ObservedCost {
            known_usd: (self.known[hi] - self.known[lo]).max(0.),
            unknown_events: unknown,
            events: hi - lo,
            partial: unknown > 0
                || self.partial[hi] > self.partial[lo]
                || self.times.first().is_none_or(|t| *t > start),
        }
    }
    pub fn strict(&self, start: i64, end: i64) -> (Option<f64>, bool, usize) {
        let observed = self.observed(start, end);
        if self.times.is_empty() || end < start {
            return (None, true, 0);
        }
        let lo = self.times.partition_point(|t| *t <= start);
        let hi = self.times.partition_point(|t| *t <= end);
        let unknown = observed.events == 0
            || observed.unknown_events > 0
            || self.coverage[hi] > self.coverage[lo];
        (
            (!unknown).then_some(observed.known_usd),
            observed.partial || unknown,
            observed.events,
        )
    }
}
fn confirmed_boundary(event: &crate::model::HistoryEvent) -> bool {
    crate::history::is_confirmed_quota_change(event)
        && event
            .delta
            .parse::<f64>()
            .is_ok_and(|d| d.is_finite() && d > 0.)
}
fn account(event: &crate::model::HistoryEvent) -> Option<&str> {
    event
        .evidence
        .as_ref()
        .and_then(|evidence| evidence.account_id.as_deref())
}
fn interval_cost(
    index: &CostIndex,
    previous: &crate::model::HistoryEvent,
    event: &crate::model::HistoryEvent,
) -> (Option<f64>, bool) {
    if previous.session != event.session
        || (previous.epoch != event.epoch && previous.kind != crate::model::HistoryKind::QuotaReset)
    {
        return (None, true);
    }
    let (cost, partial, _) = index.strict(previous.observed_at_unix, event.observed_at_unix);
    (cost, partial || event.partial)
}

/// Account/source/window-scoped accounting for one event without rescanning tokens.
/// This is a convenience API; `history_costs` avoids rescanning history per row.
pub fn event_cost_indexed(
    index: &CostIndex,
    event: &crate::model::HistoryEvent,
    all: &[crate::model::HistoryEvent],
) -> (Option<f64>, bool) {
    if !confirmed_boundary(event) {
        return (None, true);
    }
    let mut at_event = 0;
    let mut previous: Option<&crate::model::HistoryEvent> = None;
    let mut previous_count = 0;
    for candidate in all {
        if candidate.observed_at_unix > event.observed_at_unix
            || candidate.source_key != event.source_key
            || account(candidate) != account(event)
            || !match (&candidate.window_key, &event.window_key) {
                (Some(a), Some(b)) => a == b,
                _ => candidate.window_label == event.window_label,
            }
            || !confirmed_boundary(candidate)
        {
            continue;
        }
        if candidate.observed_at_unix == event.observed_at_unix {
            at_event += 1;
        } else if previous.is_none_or(|old| candidate.observed_at_unix > old.observed_at_unix) {
            previous = Some(candidate);
            previous_count = 1;
        } else if previous.is_some_and(|old| candidate.observed_at_unix == old.observed_at_unix) {
            previous_count += 1;
        }
    }
    if at_event != 1 || previous_count != 1 {
        return (None, true);
    }
    previous.map_or((None, true), |previous| {
        interval_cost(index, previous, event)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum WindowIdentity<'a> {
    Key(&'a str),
    LegacyLabel(&'a str),
}
type StreamIdentity<'a> = (Option<&'a str>, &'a str, WindowIdentity<'a>);

/// Compute an entire history's costs in one pass per account/source/window stream.
/// The result is aligned with `all`, even if its input is not chronologically sorted.
/// Unconfirmed changes never establish boundaries. Ambiguous same-second changes
/// remain unknown and cannot establish a defensible predecessor for the next row.
/// Legacy label-only rows bridge to a keyed window only when that label identifies
/// exactly one keyed window for the same account and source.
pub fn history_costs(
    index: &CostIndex,
    all: &[crate::model::HistoryEvent],
) -> Vec<(Option<f64>, bool)> {
    use std::collections::{BTreeMap, BTreeSet};
    type LegacyAlias<'a> = (Option<&'a str>, &'a str, &'a str);
    let mut aliases: BTreeMap<LegacyAlias<'_>, BTreeSet<&str>> = BTreeMap::new();
    let confirmed: Vec<_> = all
        .iter()
        .enumerate()
        .filter(|(_, event)| confirmed_boundary(event))
        .collect();
    for (_, event) in &confirmed {
        if let Some(key) = event.window_key.as_deref() {
            aliases
                .entry((account(event), &event.source_key, &event.window_label))
                .or_default()
                .insert(key);
        }
    }
    let mut streams: BTreeMap<StreamIdentity<'_>, Vec<usize>> = BTreeMap::new();
    for (position, event) in confirmed {
        let identity = if let Some(key) = event.window_key.as_deref() {
            WindowIdentity::Key(key)
        } else {
            let candidates = aliases.get(&(account(event), &event.source_key, &event.window_label));
            match candidates
                .filter(|keys| keys.len() == 1)
                .and_then(|keys| keys.first())
            {
                Some(key) => WindowIdentity::Key(key),
                None => WindowIdentity::LegacyLabel(&event.window_label),
            }
        };
        streams
            .entry((account(event), &event.source_key, identity))
            .or_default()
            .push(position);
    }
    let mut result = vec![(None, true); all.len()];
    for mut rows in streams.into_values() {
        rows.sort_by_key(|position| all[*position].observed_at_unix);
        let mut previous = None;
        let mut at = 0;
        while at < rows.len() {
            let mut end = at + 1;
            while end < rows.len()
                && all[rows[end]].observed_at_unix == all[rows[at]].observed_at_unix
            {
                end += 1;
            }
            if end == at + 1 {
                let current = rows[at];
                if let Some(previous) = previous {
                    result[current] = interval_cost(index, &all[previous], &all[current]);
                }
                previous = Some(current);
            } else {
                previous = None;
            }
            at = end;
        }
    }
    result
}
