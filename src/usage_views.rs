//! Views over normalized, deduplicated usage events. Never adds cumulative snapshots.
use crate::tokens::TokenEvent;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GroupBy {
    #[default]
    Response,
    Turn,
    Task,
}
#[derive(Debug, Clone, Serialize)]
pub struct UsageGroup {
    pub session: String,
    pub turn_id: Option<String>,
    pub response_id: Option<String>,
    pub source_kinds: Vec<String>,
    pub first_timestamp: i64,
    pub last_timestamp: i64,
    pub event_count: usize,
    pub response_count: usize,
    pub models: Vec<String>,
    pub model_assumed: bool,
    pub input: u64,
    pub cached: u64,
    pub uncached: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cost_usd: Option<f64>,
    pub known_cost_subtotal_usd: f64,
    pub unknown_cost_events: usize,
    pub partial_flags: Vec<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct TaskRow {
    pub task_id: String,
    pub latest_timestamp: i64,
    pub latest_cost_usd: Option<f64>,
    pub interval_cost_usd: Option<f64>,
    pub interval_events: usize,
    pub partial: bool,
}
/// A known response price can coexist with incomplete task/turn coverage.
pub fn coverage_incomplete(event: &TokenEvent) -> bool {
    event.partial_flags.iter().any(|f| {
        matches!(
            f.as_str(),
            "response_coverage_gap" | "mixed_schema_legacy_overlap_suppressed"
        )
    })
}
/// Recent tasks remain visible after a boundary; only (boundary, end] contributes.
/// No boundary means unknown interval cost, not the entire historical total.
/// The caller supplies normalized deduplicated events, never cumulative snapshots.
pub fn task_rows(events: &[TokenEvent], boundary: Option<i64>, end: i64) -> Vec<TaskRow> {
    let mut by_task: BTreeMap<&str, Vec<&TokenEvent>> = BTreeMap::new();
    for e in events.iter().filter(|e| e.timestamp <= end) {
        by_task.entry(&e.session).or_default().push(e);
    }
    let mut rows: Vec<_> = by_task
        .into_iter()
        .filter_map(|(task, mut events)| {
            events.sort_by(|a, b| (a.timestamp, &a.id).cmp(&(b.timestamp, &b.id)));
            let latest = *events.last()?;
            if latest.timestamp < end.saturating_sub(86400)
                && boundary.is_none_or(|b| latest.timestamp <= b)
            {
                return None;
            }
            let selected: Vec<_> = events
                .iter()
                .filter(|e| boundary.is_some_and(|b| e.timestamp > b))
                .collect();
            let unknown = selected
                .iter()
                .any(|e| e.cost_usd.is_none() || coverage_incomplete(e));
            Some(TaskRow {
                task_id: task.into(),
                latest_timestamp: latest.timestamp,
                latest_cost_usd: latest.cost_usd,
                interval_cost_usd: boundary.and_then(|_| {
                    (!unknown).then(|| selected.iter().filter_map(|e| e.cost_usd).sum())
                }),
                interval_events: selected.len(),
                partial: boundary.is_none()
                    || unknown
                    || selected.iter().any(|e| !e.partial_flags.is_empty()),
            })
        })
        .collect();
    rows.sort_by(|a, b| (b.latest_timestamp, &b.task_id).cmp(&(a.latest_timestamp, &a.task_id)));
    rows
}
/// Group only the supplied scope (e.g. a local day). Unknown turns stay separate.
/// Task is the logical thread/session identity, not a guessed parent task.
pub fn group(events: &[&TokenEvent], by: GroupBy) -> Vec<UsageGroup> {
    let mut groups: BTreeMap<(String, String), Vec<&TokenEvent>> = BTreeMap::new();
    for e in events {
        let key = match by {
            GroupBy::Response => format!("event:{}", e.id),
            GroupBy::Turn => e
                .turn_id
                .as_ref()
                .map(|t| format!("turn:{t}"))
                .unwrap_or_else(|| format!("unknown:{}", e.id)),
            GroupBy::Task => String::new(),
        };
        groups.entry((e.session.clone(), key)).or_default().push(e);
    }
    let mut out: Vec<_> = groups
        .into_values()
        .map(|rows| {
            let first = rows[0];
            let unknown = rows.iter().filter(|e| e.cost_usd.is_none()).count();
            let known: f64 = rows.iter().filter_map(|e| e.cost_usd).sum();
            let mut flags: BTreeSet<String> = rows
                .iter()
                .flat_map(|e| e.partial_flags.iter().cloned())
                .collect();
            if by == GroupBy::Turn && first.turn_id.is_none() {
                flags.insert("turn_id_missing_not_inferred".into());
            }
            let models: BTreeSet<_> = rows.iter().map(|e| e.model.clone()).collect();
            let sources: BTreeSet<_> = rows
                .iter()
                .map(|e| {
                    e.source_kind
                        .clone()
                        .unwrap_or_else(|| "legacy_unknown".into())
                })
                .collect();
            let sum =
                |f: fn(&TokenEvent) -> u64| rows.iter().fold(0u64, |a, e| a.saturating_add(f(e)));
            UsageGroup {
                session: first.session.clone(),
                turn_id: (by != GroupBy::Task)
                    .then(|| first.turn_id.clone())
                    .flatten(),
                response_id: (by == GroupBy::Response)
                    .then(|| first.response_id.clone())
                    .flatten(),
                source_kinds: sources.into_iter().collect(),
                first_timestamp: rows.iter().map(|e| e.timestamp).min().unwrap_or_default(),
                last_timestamp: rows.iter().map(|e| e.timestamp).max().unwrap_or_default(),
                event_count: rows.len(),
                response_count: rows.iter().filter(|e| e.response_id.is_some()).count(),
                models: models.into_iter().collect(),
                model_assumed: rows.iter().any(|e| e.model_assumed),
                input: sum(|e| e.input),
                cached: sum(|e| e.cached),
                uncached: sum(|e| e.uncached),
                output: sum(|e| e.output),
                reasoning: sum(|e| e.reasoning),
                cost_usd: (unknown == 0
                    && (by == GroupBy::Response || !rows.iter().any(|e| coverage_incomplete(e))))
                .then_some(known),
                known_cost_subtotal_usd: known,
                unknown_cost_events: unknown,
                partial_flags: flags.into_iter().collect(),
            }
        })
        .collect();
    out.sort_by(|a, b| {
        (a.last_timestamp, &a.session, &a.turn_id, &a.response_id).cmp(&(
            b.last_timestamp,
            &b.session,
            &b.turn_id,
            &b.response_id,
        ))
    });
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    fn e(id: &str, time: i64, session: &str, turn: Option<&str>) -> TokenEvent {
        TokenEvent {
            id: id.into(),
            timestamp: time,
            model: "gpt-6.1-sol".into(),
            model_assumed: false,
            session: session.into(),
            response_id: Some(id.into()),
            turn_id: turn.map(str::to_owned),
            source_kind: Some("token_usage_record".into()),
            input: 300_000,
            cached: 200_000,
            uncached: 100_000,
            output: 100,
            reasoning: 80,
            cost_usd: Some(0.4415),
            partial_flags: vec![],
        }
    }
    #[test]
    fn tasks_keep_distinct_latest_events_and_only_current_interval_cost() {
        let events = vec![
            e("a", 10, "task-a", Some("a")),
            e("b", 20, "task-b", Some("b")),
            e("c", 30, "task-a", Some("a")),
            e("d", 40, "task-b", Some("b")),
        ];
        let rows = task_rows(&events, Some(20), 50);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].task_id, "task-b");
        assert_eq!(rows[0].latest_timestamp, 40);
        assert_eq!(rows[0].latest_cost_usd, Some(0.4415));
        assert_eq!(rows[0].interval_cost_usd, Some(0.4415));
        assert_eq!(rows[1].task_id, "task-a");
        assert_eq!(rows[1].interval_events, 1);
        assert_eq!(rows[1].interval_cost_usd, Some(0.4415));
        let reset = task_rows(&events, Some(40), 50);
        assert!(reset
            .iter()
            .all(|r| r.interval_cost_usd == Some(0.) && r.interval_events == 0));
        assert_eq!(reset[0].latest_timestamp, 40);
        assert!(task_rows(&events, None, 50)
            .iter()
            .all(|r| r.interval_cost_usd.is_none()));
    }
    #[test]
    fn task_interval_unknown_is_not_zero_and_old_inactive_tasks_are_hidden() {
        let mut events = vec![
            e("old", 1, "old-task", None),
            e("a", 90_000, "active", Some("a")),
        ];
        events[1].cost_usd = None;
        let rows = task_rows(&events, Some(89_000), 90_001);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].partial);
        assert_eq!(rows[0].interval_cost_usd, None);
        // A long quota interval still includes older tasks whose usage belongs to it.
        assert_eq!(task_rows(&events, Some(0), 90_001).len(), 2);
    }
    #[test]
    fn turn_and_task_use_exact_ids_without_rebilling_aggregate_input() {
        let events = [
            e("a", 1, "task-a", Some("turn-1")),
            e("b", 2, "task-a", Some("turn-1")),
            e("c", 3, "task-a", Some("turn-2")),
            e("d", 4, "task-b", Some("turn-1")),
        ];
        let refs = events.iter().collect::<Vec<_>>();
        let turns = group(&refs, GroupBy::Turn);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].input, 600_000);
        assert_eq!(turns[0].cost_usd, Some(0.883));
        let tasks = group(&refs, GroupBy::Task);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].event_count, 3);
        assert_eq!(tasks[0].input, 900_000);
        assert_eq!(tasks[0].reasoning, 240);
    }
    #[test]
    fn unknown_turns_stay_separate_and_unknown_costs_stay_unknown() {
        let mut events = [e("a", 1, "task", None), e("b", 2, "task", None)];
        events[1].cost_usd = None;
        let refs = events.iter().collect::<Vec<_>>();
        assert_eq!(group(&refs, GroupBy::Turn).len(), 2);
        let tasks = group(&refs, GroupBy::Task);
        assert_eq!(tasks[0].cost_usd, None);
        assert_eq!(tasks[0].known_cost_subtotal_usd, 0.4415);
        assert_eq!(tasks[0].unknown_cost_events, 1);
    }
    #[test]
    fn known_response_price_survives_incomplete_task_coverage() {
        let mut event = e("a", 10, "task", Some("turn"));
        event.partial_flags.push("response_coverage_gap".into());
        let refs = vec![&event];
        assert_eq!(group(&refs, GroupBy::Response)[0].cost_usd, Some(0.4415));
        assert_eq!(group(&refs, GroupBy::Task)[0].cost_usd, None);
        let rows = task_rows(&[event], Some(0), 20);
        assert_eq!(rows[0].latest_cost_usd, Some(0.4415));
        assert_eq!(rows[0].interval_cost_usd, None);
    }
}
