//! Indexed merging of local historical observations and live deductions.
use crate::model::{HistoryEvent, HistoryKind, QuotaModel};
use crate::tokens::ImportedQuotaObservation;
use std::collections::{BTreeMap, BTreeSet};
type Window = (String, String);
type Transition = (String, String, u8, String, String);
fn kind(k: HistoryKind) -> u8 {
    match k {
        HistoryKind::QuotaDecrease => 0,
        HistoryKind::CreditDecrease => 1,
        HistoryKind::QuotaReset => 2,
        HistoryKind::CreditTopUp => 3,
    }
}
fn transition(e: &HistoryEvent) -> Transition {
    (
        e.source_key.clone(),
        e.window_key
            .clone()
            .unwrap_or_else(|| e.window_label.clone()),
        kind(e.kind),
        e.before.clone(),
        e.after.clone(),
    )
}
fn key(e: &HistoryEvent) -> String {
    serde_json::to_string(&(e.observed_at_unix, transition(e), e.epoch)).unwrap_or_default()
}
pub fn replay(observations: &[ImportedQuotaObservation]) -> Vec<HistoryEvent> {
    let mut ordered: Vec<_> = observations.iter().collect();
    ordered.sort_by_key(|o| o.timestamp);
    let mut model = QuotaModel::new("imported-local-observations");
    for o in ordered {
        let _ = model.ingest_json(&o.payload, o.timestamp);
    }
    model.history
}
pub fn merge(mut existing: Vec<HistoryEvent>, incoming: Vec<HistoryEvent>) -> Vec<HistoryEvent> {
    let mut keys: BTreeSet<_> = existing.iter().map(key).collect();
    let mut index: BTreeMap<Transition, BTreeSet<i64>> = BTreeMap::new();
    let mut resets: BTreeMap<Window, BTreeSet<i64>> = BTreeMap::new();
    let mut changes: BTreeMap<Window, BTreeSet<i64>> = BTreeMap::new();
    for e in existing.iter().chain(incoming.iter()) {
        if e.delta.parse::<f64>().is_ok_and(|d| d > 0.) {
            changes
                .entry((
                    e.source_key.clone(),
                    e.window_key
                        .clone()
                        .unwrap_or_else(|| e.window_label.clone()),
                ))
                .or_default()
                .insert(e.observed_at_unix);
        }
        if matches!(e.kind, HistoryKind::QuotaReset | HistoryKind::CreditTopUp)
            && e.before != e.after
        {
            resets
                .entry((
                    e.source_key.clone(),
                    e.window_key
                        .clone()
                        .unwrap_or_else(|| e.window_label.clone()),
                ))
                .or_default()
                .insert(e.observed_at_unix);
        }
    }
    for e in &existing {
        index
            .entry(transition(e))
            .or_default()
            .insert(e.observed_at_unix);
    }
    for event in incoming {
        let k = key(&event);
        if keys.contains(&k) {
            continue;
        }
        let t = event.observed_at_unix;
        let trans = transition(&event);
        let ws = (
            event.source_key.clone(),
            event
                .window_key
                .clone()
                .unwrap_or_else(|| event.window_label.clone()),
        );
        let near = index.get(&trans).is_some_and(|times| {
            times
                .range(t.saturating_sub(90)..=t.saturating_add(90))
                .any(|&other| {
                    !(if matches!(
                        event.kind,
                        HistoryKind::QuotaReset | HistoryKind::CreditTopUp
                    ) {
                        changes.get(&ws)
                    } else {
                        resets.get(&ws)
                    })
                    .is_some_and(|r| {
                        other != t
                            && r.range((
                                std::ops::Bound::Excluded(other.min(t)),
                                std::ops::Bound::Excluded(other.max(t)),
                            ))
                            .next()
                            .is_some()
                    })
                })
        });
        if !near {
            keys.insert(k);
            index.entry(trans).or_default().insert(t);
            existing.push(event);
        }
    }
    existing.sort_by_key(|e| e.observed_at_unix);
    existing
}
#[cfg(test)]
mod tests {
    use super::*;
    fn e(t: i64, k: HistoryKind, b: &str, a: &str) -> HistoryEvent {
        HistoryEvent {
            session: "s".into(),
            observed_at_unix: t,
            source_key: "codex".into(),
            epoch: 0,
            kind: k,
            window_label: "week".into(),
            before: b.into(),
            after: a.into(),
            delta: "1".into(),
            partial: false,
            before_observed_at_unix: None,
            window_key: Some("primary".into()),
        }
    }
    #[test]
    fn duplicate_reset_can_merge_but_repeated_true_reset_is_retained() {
        let reset = e(10, HistoryKind::QuotaReset, "99", "100");
        let copy = e(15, HistoryKind::QuotaReset, "99", "100");
        assert_eq!(merge(vec![reset.clone()], vec![copy]).len(), 1);
        let drop = e(20, HistoryKind::QuotaDecrease, "100", "99");
        let refill = e(30, HistoryKind::QuotaReset, "99", "100");
        assert_eq!(merge(vec![reset, drop], vec![refill]).len(), 3);
    }
    #[test]
    fn same_transition_after_reset_is_not_erased() {
        let first = e(10, HistoryKind::QuotaDecrease, "100", "99");
        let reset = e(20, HistoryKind::QuotaReset, "99", "100");
        let second = e(30, HistoryKind::QuotaDecrease, "100", "99");
        assert_eq!(merge(vec![first, reset], vec![second]).len(), 3);
    }
    #[test]
    fn log_and_live_duplicate_are_merged() {
        let first = e(10, HistoryKind::QuotaDecrease, "100", "99");
        let second = e(20, HistoryKind::QuotaDecrease, "100", "99");
        assert_eq!(merge(vec![first], vec![second]).len(), 1);
    }
}
