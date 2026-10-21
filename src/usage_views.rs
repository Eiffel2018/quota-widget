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
    pub reasoning_efforts: Vec<String>,
    pub unknown_effort_events: usize,
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
impl UsageGroup {
    /// A partial group can still contain priced responses. Keep that known
    /// subtotal visible; distinguish an actually priced zero from no priced data.
    pub fn display_cost_usd(&self) -> Option<f64> {
        (self.event_count > self.unknown_cost_events).then_some(
            if self.known_cost_subtotal_usd == 0.0 {
                0.0
            } else {
                self.known_cost_subtotal_usd
            },
        )
    }
    pub fn display_cost_is_partial(&self) -> bool {
        self.cost_usd.is_none() || !self.partial_flags.is_empty()
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct TaskRow {
    pub task_id: String,
    pub latest_event_index: usize,
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
    for e in events
        .iter()
        .filter(|e| !e.is_excluded_auto_review() && e.timestamp <= end)
    {
        by_task.entry(&e.session).or_default().push(e);
    }
    let mut rows: Vec<_> = by_task
        .into_iter()
        .filter_map(|(task, mut events)| {
            events.sort_by(|a, b| (a.timestamp, &a.id).cmp(&(b.timestamp, &b.id)));
            let latest = *events.last()?;
            if latest.timestamp < end.saturating_sub(2 * 60 * 60)
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
                latest_event_index: 0,
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
#[derive(Debug, Clone, Default)]
struct TaskSeries {
    indices: Vec<usize>,
    times: Vec<i64>,
    known: Vec<f64>,
    unknown: Vec<usize>,
    partial: Vec<usize>,
}
#[derive(Debug, Clone, Default)]
pub struct UsageIndex {
    tasks: BTreeMap<String, TaskSeries>,
}
impl UsageIndex {
    pub fn new(events: &[TokenEvent]) -> Self {
        let mut grouped: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, e) in events
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.is_excluded_auto_review())
        {
            grouped.entry(e.session.clone()).or_default().push(i);
        }
        let tasks = grouped
            .into_iter()
            .map(|(task, mut indices)| {
                indices.sort_by(|a, b| {
                    (events[*a].timestamp, &events[*a].id)
                        .cmp(&(events[*b].timestamp, &events[*b].id))
                });
                let mut s = TaskSeries {
                    known: vec![0.],
                    unknown: vec![0],
                    partial: vec![0],
                    ..TaskSeries::default()
                };
                for &i in &indices {
                    let e = &events[i];
                    s.times.push(e.timestamp);
                    s.known
                        .push(s.known.last().copied().unwrap_or(0.) + e.cost_usd.unwrap_or(0.));
                    s.unknown.push(
                        s.unknown.last().copied().unwrap_or(0) + usize::from(e.cost_usd.is_none()),
                    );
                    s.partial.push(
                        s.partial.last().copied().unwrap_or(0)
                            + usize::from(!e.partial_flags.is_empty()),
                    );
                }
                s.indices = indices;
                (task, s)
            })
            .collect();
        Self { tasks }
    }
    pub fn task_rows(
        &self,
        events: &[TokenEvent],
        boundary: Option<i64>,
        fallback_start: i64,
        end: i64,
    ) -> Vec<TaskRow> {
        let start = boundary.unwrap_or(fallback_start);
        let mut rows: Vec<_> = self
            .tasks
            .iter()
            .filter_map(|(task, s)| {
                let hi = s.times.partition_point(|t| *t <= end);
                if hi == 0 {
                    return None;
                }
                let latest_index = s.indices[hi - 1];
                let latest = &events[latest_index];
                if latest.timestamp < end.saturating_sub(2 * 60 * 60) && latest.timestamp <= start {
                    return None;
                }
                let lo = s.times.partition_point(|t| *t <= start).min(hi);
                Some(TaskRow {
                    task_id: task.clone(),
                    latest_event_index: latest_index,
                    latest_timestamp: latest.timestamp,
                    latest_cost_usd: latest.cost_usd,
                    interval_cost_usd: Some((s.known[hi] - s.known[lo]).max(0.)),
                    interval_events: hi - lo,
                    partial: boundary.is_none()
                        || s.unknown[hi] > s.unknown[lo]
                        || s.partial[hi] > s.partial[lo],
                })
            })
            .collect();
        rows.sort_by(|a, b| {
            (b.latest_timestamp, &b.task_id).cmp(&(a.latest_timestamp, &a.task_id))
        });
        rows
    }
}
pub fn visible_task_rows(task_count: usize, threshold: usize) -> usize {
    task_count.max(1).min(threshold.clamp(1, 50))
}
/// Logical pixels at the original 310px width; upper quota geometry stays fixed.
pub fn widget_height(task_count: usize, threshold: usize) -> f32 {
    324. + 22. * visible_task_rows(task_count, threshold) as f32
}

/// What the supplied authoritative metadata says about a task's immediate parent.
/// `Unlinked` means no relationship is known; it does not infer that this is a main task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum TaskRelationship {
    Unlinked,
    Child {
        parent_id: String,
    },
    /// A relationship was recorded without a usable parent identity.
    Orphan {
        parent_id: Option<String>,
    },
    /// This edge participates in a cycle and is not used to build the hierarchy.
    Cycle {
        parent_id: String,
    },
}

/// A presentation row, not an additional billable usage event.
/// Summaries overlap when nested; global totals must continue to use deduplicated
/// usage events (or each unique task's own cost), never the sum of display rows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum TaskTreeRow {
    SelfTask {
        task_id: String,
        /// Index into the supplied `TaskRow` slice. `None` denotes an ancestor
        /// identified by metadata whose own recent usage is unavailable.
        task_index: Option<usize>,
        depth: usize,
        relationship: TaskRelationship,
    },
    ChildrenSummary {
        parent_id: String,
        depth: usize,
        /// All known descendants, including those with metadata but no recent usage.
        /// A grandchild is counted once, regardless of the expansion state.
        descendant_count: usize,
        direct_child_count: usize,
        /// The known current-interval subtotal, excluding the parent's own usage.
        known_cost_usd: f64,
        /// At least one descendant has unavailable or incomplete interval usage.
        partial: bool,
        expanded: bool,
    },
}

#[derive(Debug)]
struct TaskTreeNode {
    id: String,
    task_index: Option<usize>,
    parent: Option<usize>,
    relationship: TaskRelationship,
    children: Vec<usize>,
    own_known_cost: f64,
    own_partial: bool,
    descendant_count: usize,
    descendant_known_cost: f64,
    descendant_partial: bool,
    latest_subtree_timestamp: Option<i64>,
}

impl TaskTreeNode {
    fn new(id: String, task_index: Option<usize>, tasks: &[TaskRow]) -> Self {
        let task = task_index.map(|index| &tasks[index]);
        // Invalid amounts are unknown, never NaN/negative contributions to totals.
        let own_cost = task
            .and_then(|row| row.interval_cost_usd)
            .filter(|cost| cost.is_finite() && *cost >= 0.);
        Self {
            id,
            task_index,
            parent: None,
            relationship: TaskRelationship::Unlinked,
            children: Vec::new(),
            own_known_cost: own_cost.unwrap_or(0.),
            own_partial: own_cost.is_none() || task.is_some_and(|row| row.partial),
            descendant_count: 0,
            descendant_known_cost: 0.,
            descendant_partial: false,
            latest_subtree_timestamp: task.map(|row| row.latest_timestamp),
        }
    }
}

/// Build the visible task tree using only recorded child-thread -> parent-thread
/// relationships. An empty `expanded` set collapses every subagent group.
///
/// The scope starts at the supplied recent/current-interval task rows, includes
/// their metadata ancestors, and includes all recorded descendants of those
/// roots. Unrelated historical metadata components are not shown. A referenced parent without a row is preserved as a metadata-only
/// task, with no invented response time or cost. Empty parent identities remain
/// separate orphans. Missing relationships remain unlinked; names, paths and
/// similar timestamps are never evidence of parentage.
///
/// Every edge in a cycle is detached and marked; other verified edges remain.
/// Each task appears at most once. Summaries are computed from descendant self
/// costs, not nested summaries, so expansion cannot change their totals.
/// Construction is O(n log n), including ordering, and uses no recursive calls.
/// Pass the returned row count to `widget_height` / `visible_task_rows` so the
/// row limit applies to actual visible rows, including group summaries.
pub fn task_tree_rows(
    tasks: &[TaskRow],
    parents: &BTreeMap<String, String>,
    expanded: &BTreeSet<String>,
) -> Vec<TaskTreeRow> {
    // TaskRow producers emit unique tasks. If a caller supplies duplicates, keep
    // the newest row instead of charging the same thread more than once.
    let mut task_indices: BTreeMap<&str, usize> = BTreeMap::new();
    for (index, task) in tasks.iter().enumerate() {
        let selected = task_indices.entry(task.task_id.as_str()).or_insert(index);
        if (task.latest_timestamp, task.latest_event_index)
            > (
                tasks[*selected].latest_timestamp,
                tasks[*selected].latest_event_index,
            )
        {
            *selected = index;
        }
    }
    let mut nodes: Vec<_> = task_indices
        .into_iter()
        .map(|(id, index)| TaskTreeNode::new(id.to_owned(), Some(index), tasks))
        .collect();
    let mut node_indices: BTreeMap<String, usize> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.clone(), index))
        .collect();

    // Reverse edges let a relevant root retain all known subagents, including
    // inactive descendants with metadata but no recent own usage. Unrelated
    // historical components are never visited.
    let mut recorded_children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (child, parent) in parents {
        if !parent.trim().is_empty() {
            recorded_children.entry(parent).or_default().push(child);
        }
    }
    // Visit each relevant identity once; the index map bounds even cyclic walks.
    let mut index = 0;
    while index < nodes.len() {
        if let Some(parent_id) = parents.get(&nodes[index].id) {
            if parent_id.trim().is_empty() {
                nodes[index].relationship = TaskRelationship::Orphan { parent_id: None };
            } else {
                let parent = if let Some(&parent) = node_indices.get(parent_id) {
                    parent
                } else {
                    let parent = nodes.len();
                    nodes.push(TaskTreeNode::new(parent_id.clone(), None, tasks));
                    node_indices.insert(parent_id.clone(), parent);
                    parent
                };
                nodes[index].parent = Some(parent);
                nodes[index].relationship = TaskRelationship::Child {
                    parent_id: parent_id.clone(),
                };
            }
        }
        if let Some(children) = recorded_children.get(nodes[index].id.as_str()) {
            for &child in children {
                if !node_indices.contains_key(child) {
                    let child_index = nodes.len();
                    nodes.push(TaskTreeNode::new(child.to_owned(), None, tasks));
                    node_indices.insert(child.to_owned(), child_index);
                }
            }
        }
        index += 1;
    }

    // A functional graph (at most one parent per task) allows linear cycle
    // detection. Detach every cycle edge instead of inventing a preferred root.
    let mut state = vec![0u8; nodes.len()];
    for start in 0..nodes.len() {
        if state[start] != 0 {
            continue;
        }
        let mut path = Vec::new();
        let mut current = Some(start);
        while let Some(node) = current {
            if state[node] != 0 {
                break;
            }
            state[node] = 1;
            path.push(node);
            current = nodes[node].parent;
        }
        if let Some(cycle_start) = current.filter(|&node| state[node] == 1) {
            let first = path
                .iter()
                .position(|&node| node == cycle_start)
                .expect("visiting nodes belong to the current parent walk");
            for &node in &path[first..] {
                let parent = nodes[node].parent.take().expect("cycle edge exists");
                nodes[node].relationship = TaskRelationship::Cycle {
                    parent_id: nodes[parent].id.clone(),
                };
            }
        }
        for node in path {
            state[node] = 2;
        }
    }

    let mut roots = Vec::new();
    for node in 0..nodes.len() {
        if let Some(parent) = nodes[node].parent {
            nodes[parent].children.push(node);
        } else {
            roots.push(node);
        }
    }

    // Fold leaves toward roots once. These aggregates do not depend on expansion.
    let mut children_remaining: Vec<_> = nodes.iter().map(|node| node.children.len()).collect();
    let mut ready: Vec<_> = children_remaining
        .iter()
        .enumerate()
        .filter_map(|(node, &count)| (count == 0).then_some(node))
        .collect();
    while let Some(node) = ready.pop() {
        if let Some(parent) = nodes[node].parent {
            let count = nodes[node].descendant_count + 1;
            let cost = nodes[node].own_known_cost + nodes[node].descendant_known_cost;
            let partial = nodes[node].own_partial || nodes[node].descendant_partial;
            let latest = nodes[node].latest_subtree_timestamp;
            let ancestor = &mut nodes[parent];
            ancestor.descendant_count += count;
            ancestor.descendant_known_cost += cost;
            ancestor.descendant_partial |= partial;
            ancestor.latest_subtree_timestamp = ancestor.latest_subtree_timestamp.max(latest);
            children_remaining[parent] -= 1;
            if children_remaining[parent] == 0 {
                ready.push(parent);
            }
        }
    }

    let order = |a: &usize, b: &usize| {
        nodes[*b]
            .latest_subtree_timestamp
            .cmp(&nodes[*a].latest_subtree_timestamp)
            .then_with(|| nodes[*a].id.cmp(&nodes[*b].id))
    };
    roots.sort_by(order);
    // Sort separate child lists to avoid aliasing the node storage used by `order`.
    let ordered_children: Vec<Vec<usize>> = nodes
        .iter()
        .map(|node| {
            let mut children = node.children.clone();
            children.sort_by(order);
            children
        })
        .collect();
    let mut stack: Vec<_> = roots.into_iter().rev().map(|node| (node, 0usize)).collect();
    let mut visible = Vec::new();
    while let Some((index, depth)) = stack.pop() {
        let node = &nodes[index];
        visible.push(TaskTreeRow::SelfTask {
            task_id: node.id.clone(),
            task_index: node.task_index,
            depth,
            relationship: node.relationship.clone(),
        });
        if !node.children.is_empty() {
            let is_expanded = expanded.contains(&node.id);
            visible.push(TaskTreeRow::ChildrenSummary {
                parent_id: node.id.clone(),
                depth: depth + 1,
                descendant_count: node.descendant_count,
                direct_child_count: node.children.len(),
                known_cost_usd: node.descendant_known_cost,
                partial: node.descendant_partial,
                expanded: is_expanded,
            });
            if is_expanded {
                stack.extend(
                    ordered_children[index]
                        .iter()
                        .rev()
                        .map(|&child| (child, depth + 1)),
                );
            }
        }
    }
    visible
}

/// Group only the supplied scope (e.g. a local day). Unknown turns stay separate.
/// Task is the logical thread/session identity, not a guessed parent task.
pub fn group(events: &[&TokenEvent], by: GroupBy) -> Vec<UsageGroup> {
    group_with_excluded(events, by, false)
}
/// Explicit audit-only grouping; never use included review rows for default accounting.
pub fn group_with_excluded(
    events: &[&TokenEvent],
    by: GroupBy,
    include_excluded: bool,
) -> Vec<UsageGroup> {
    let mut groups: BTreeMap<(String, String), Vec<&TokenEvent>> = BTreeMap::new();
    for e in events
        .iter()
        .filter(|e| include_excluded || !e.is_excluded_auto_review())
    {
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
                reasoning_efforts: rows
                    .iter()
                    .filter_map(|e| e.reasoning_effort.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                unknown_effort_events: rows.iter().filter(|e| e.reasoning_effort.is_none()).count(),
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
            reasoning_effort: None,
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
            coverage_start_timestamp_ms: None,
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
    #[test]
    fn partial_groups_show_known_subtotal_but_unpriced_is_not_zero() {
        let mut known = e("known", 10, "task", Some("turn"));
        known.cost_usd = Some(0.1141);
        known.partial_flags = vec!["mixed_schema_legacy_overlap_suppressed".into()];
        let group = group(&[&known], GroupBy::Turn).remove(0);
        assert_eq!(group.cost_usd, None);
        assert_eq!(group.unknown_cost_events, 0);
        assert_eq!(group.display_cost_usd(), Some(0.1141));
        assert!(group.display_cost_is_partial());
        let mut unknown = e("unknown", 11, "task", Some("turn"));
        unknown.cost_usd = None;
        let mixed = super::group(&[&known, &unknown], GroupBy::Turn).remove(0);
        assert_eq!(mixed.display_cost_usd(), Some(0.1141));
        assert_eq!(mixed.unknown_cost_events, 1);
        let absent = super::group(&[&unknown], GroupBy::Turn).remove(0);
        assert_eq!(absent.display_cost_usd(), None);
        known.cost_usd = Some(0.0);
        let zero = super::group(&[&known, &unknown], GroupBy::Turn).remove(0);
        assert_eq!(zero.display_cost_usd(), Some(0.0));
        assert!(zero.display_cost_is_partial());
    }
    #[test]
    fn mixed_efforts_are_metadata_not_token_or_price_multipliers() {
        let mut a = e("a", 10, "task", Some("turn"));
        a.reasoning_effort = Some("max".into());
        let mut b = e("b", 11, "task", Some("turn"));
        b.reasoning_effort = Some("high".into());
        let c = e("c", 12, "task", Some("turn"));
        let row = group(&[&a, &b, &c], GroupBy::Turn).remove(0);
        assert_eq!(row.reasoning_efforts, vec!["high", "max"]);
        assert_eq!(row.unknown_effort_events, 1);
        assert_eq!(row.output, 300);
        assert_eq!(row.reasoning, 240);
        assert!((row.known_cost_subtotal_usd - 1.3245).abs() < 1e-12);
    }
}
