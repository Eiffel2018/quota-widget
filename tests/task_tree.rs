use quota_widget::usage_views::{
    task_tree_rows, visible_task_rows, widget_height, TaskRelationship, TaskRow, TaskTreeRow,
};
use std::collections::{BTreeMap, BTreeSet};

fn task(id: &str, timestamp: i64, cost: Option<f64>) -> TaskRow {
    TaskRow {
        task_id: id.into(),
        latest_event_index: timestamp.max(0) as usize,
        latest_timestamp: timestamp,
        latest_cost_usd: cost.map(|value| value / 10.),
        interval_cost_usd: cost,
        interval_events: 1,
        partial: cost.is_none(),
    }
}
fn parents(edges: &[(&str, &str)]) -> BTreeMap<String, String> {
    edges
        .iter()
        .map(|(child, parent)| ((*child).into(), (*parent).into()))
        .collect()
}
fn expanded(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|id| (*id).into()).collect()
}
fn self_ids(rows: &[TaskTreeRow]) -> Vec<(&str, usize)> {
    rows.iter()
        .filter_map(|row| match row {
            TaskTreeRow::SelfTask { task_id, depth, .. } => Some((task_id.as_str(), *depth)),
            _ => None,
        })
        .collect()
}
fn summary(rows: &[TaskTreeRow], parent: &str) -> (usize, usize, f64, bool, bool) {
    rows.iter()
        .find_map(|row| match row {
            TaskTreeRow::ChildrenSummary {
                parent_id,
                descendant_count,
                direct_child_count,
                known_cost_usd,
                partial,
                expanded,
                ..
            } if parent_id == parent => Some((
                *descendant_count,
                *direct_child_count,
                *known_cost_usd,
                *partial,
                *expanded,
            )),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing summary for {parent}"))
}

#[test]
fn two_parents_default_collapsed_and_independent_expansion() {
    let tasks = vec![
        task("parent-a", 10, Some(10.)),
        task("parent-b", 20, Some(20.)),
        task("child-a1", 40, Some(1.)),
        task("child-a2", 30, Some(2.)),
        task("child-b", 50, Some(3.)),
    ];
    let map = parents(&[
        ("child-a1", "parent-a"),
        ("child-a2", "parent-a"),
        ("child-b", "parent-b"),
    ]);
    let collapsed = task_tree_rows(&tasks, &map, &BTreeSet::new());
    assert_eq!(self_ids(&collapsed), [("parent-b", 0), ("parent-a", 0)]);
    assert_eq!(collapsed.len(), 4);
    assert!(
        matches!(&collapsed[1], TaskTreeRow::ChildrenSummary { parent_id, depth: 1, .. } if parent_id == "parent-b")
    );
    assert!(
        matches!(&collapsed[3], TaskTreeRow::ChildrenSummary { parent_id, depth: 1, .. } if parent_id == "parent-a")
    );
    assert_eq!(summary(&collapsed, "parent-a"), (2, 2, 3., false, false));
    assert_eq!(summary(&collapsed, "parent-b"), (1, 1, 3., false, false));
    let open = task_tree_rows(&tasks, &map, &expanded(&["parent-a"]));
    assert_eq!(
        self_ids(&open),
        [
            ("parent-b", 0),
            ("parent-a", 0),
            ("child-a1", 1),
            ("child-a2", 1)
        ]
    );
    assert_eq!(summary(&open, "parent-a"), (2, 2, 3., false, true));
    assert_eq!(summary(&open, "parent-b"), (1, 1, 3., false, false));
    assert_eq!(task_tree_rows(&tasks, &map, &BTreeSet::new()), collapsed);
}

#[test]
fn nested_descendants_count_once_and_keep_self_costs_separate() {
    let tasks = vec![
        task("main", 10, Some(100.)),
        task("child", 20, Some(10.)),
        task("sibling", 15, Some(3.)),
        task("grandchild", 30, Some(2.)),
    ];
    let map = parents(&[
        ("child", "main"),
        ("sibling", "main"),
        ("grandchild", "child"),
    ]);
    let root_open = task_tree_rows(&tasks, &map, &expanded(&["main"]));
    assert_eq!(
        self_ids(&root_open),
        [("main", 0), ("child", 1), ("sibling", 1)]
    );
    assert_eq!(summary(&root_open, "main"), (3, 2, 15., false, true));
    assert_eq!(summary(&root_open, "child"), (1, 1, 2., false, false));
    let all_open = task_tree_rows(&tasks, &map, &expanded(&["main", "child"]));
    assert_eq!(
        self_ids(&all_open),
        [("main", 0), ("child", 1), ("grandchild", 2), ("sibling", 1)]
    );
    assert_eq!(summary(&all_open, "main"), (3, 2, 15., false, true));
    let mut seen = BTreeSet::new();
    let own_total: f64 = all_open
        .iter()
        .filter_map(|row| match row {
            TaskTreeRow::SelfTask {
                task_id,
                task_index: Some(index),
                ..
            } => {
                assert!(seen.insert(task_id));
                assert_eq!(&tasks[*index].task_id, task_id);
                tasks[*index].interval_cost_usd
            }
            _ => None,
        })
        .sum();
    assert_eq!(own_total, 115.); // Summaries never add an extra billable cost.
    assert_eq!(tasks[0].interval_cost_usd, Some(100.));
    assert_eq!(tasks[1].latest_cost_usd, Some(1.));
}

#[test]
fn metadata_only_parent_keeps_active_child_and_all_known_inactive_descendants() {
    let tasks = vec![task("active-child", 20, Some(2.))];
    let map = parents(&[
        ("active-child", "parent"),
        ("inactive-child", "parent"),
        ("inactive-grandchild", "inactive-child"),
        ("irrelevant-child", "irrelevant-root"),
    ]);
    let collapsed = task_tree_rows(&tasks, &map, &BTreeSet::new());
    assert_eq!(self_ids(&collapsed), [("parent", 0)]);
    assert!(matches!(
        &collapsed[0],
        TaskTreeRow::SelfTask {
            task_index: None,
            ..
        }
    ));
    assert_eq!(summary(&collapsed, "parent"), (3, 2, 2., true, false));
    let open = task_tree_rows(&tasks, &map, &expanded(&["parent", "inactive-child"]));
    assert_eq!(
        self_ids(&open),
        [
            ("parent", 0),
            ("active-child", 1),
            ("inactive-child", 1),
            ("inactive-grandchild", 2)
        ]
    );
    assert_eq!(summary(&open, "inactive-child"), (1, 1, 0., true, true));
    assert!(!self_ids(&open)
        .iter()
        .any(|(id, _)| id.starts_with("irrelevant")));
}

#[test]
fn metadata_only_connecting_ancestor_is_unknown_not_zero() {
    let tasks = vec![task("main", 1, Some(7.)), task("leaf", 10, Some(5.))];
    let map = parents(&[("leaf", "middle"), ("middle", "main")]);
    let rows = task_tree_rows(&tasks, &map, &expanded(&["main", "middle"]));
    assert_eq!(self_ids(&rows), [("main", 0), ("middle", 1), ("leaf", 2)]);
    assert_eq!(summary(&rows, "main"), (2, 1, 5., true, true));
    assert_eq!(summary(&rows, "middle"), (1, 1, 5., false, true));
    assert!(matches!(&rows[2], TaskTreeRow::SelfTask { task_index: None,
        relationship: TaskRelationship::Child { parent_id }, .. } if parent_id == "main"));
}

#[test]
fn orphan_and_unknown_relationships_stay_separate_without_inferred_hierarchy() {
    let tasks = vec![
        task("orphan", 10, Some(1.)),
        task("unknown", 10, Some(2.)),
        task("looks-like-unknown-child", 10, Some(3.)),
    ];
    let rows = task_tree_rows(
        &tasks,
        &parents(&[("orphan", "  ")]),
        &expanded(&["unknown"]),
    );
    assert_eq!(rows.len(), 3);
    for row in rows {
        match row {
            TaskTreeRow::SelfTask {
                task_id,
                depth,
                relationship,
                ..
            } => {
                assert_eq!(depth, 0);
                if task_id == "orphan" {
                    assert_eq!(relationship, TaskRelationship::Orphan { parent_id: None });
                } else {
                    assert_eq!(relationship, TaskRelationship::Unlinked);
                }
            }
            _ => panic!("unknown metadata must not create a child summary"),
        }
    }
}

#[test]
fn cycle_edges_are_detached_and_verified_incoming_children_survive() {
    let tasks = vec![
        task("a", 10, Some(1.)),
        task("b", 20, Some(2.)),
        task("leaf", 30, Some(3.)),
        task("self-cycle", 40, Some(4.)),
    ];
    let map = parents(&[
        ("a", "b"),
        ("b", "a"),
        ("leaf", "a"),
        ("self-cycle", "self-cycle"),
    ]);
    let rows = task_tree_rows(&tasks, &map, &expanded(&["a", "b", "self-cycle"]));
    assert_eq!(
        self_ids(&rows),
        [("self-cycle", 0), ("a", 0), ("leaf", 1), ("b", 0)]
    );
    assert_eq!(summary(&rows, "a"), (1, 1, 3., false, true));
    let mut seen = BTreeSet::new();
    for row in rows {
        if let TaskTreeRow::SelfTask {
            task_id,
            relationship,
            ..
        } = row
        {
            assert!(seen.insert(task_id.clone()));
            if task_id != "leaf" {
                assert!(matches!(relationship, TaskRelationship::Cycle { .. }));
            } else {
                assert_eq!(
                    relationship,
                    TaskRelationship::Child {
                        parent_id: "a".into()
                    }
                );
            }
        }
    }
    assert_eq!(seen.len(), 4);
}

#[test]
fn partial_known_subtotals_do_not_disappear_or_borrow_parent_costs() {
    let mut partial = task("partial", 20, Some(4.));
    partial.partial = true;
    let tasks = vec![
        task("main", 10, Some(100.)),
        partial,
        task("unknown", 30, None),
        task("complete", 40, Some(2.)),
    ];
    let map = parents(&[
        ("partial", "main"),
        ("unknown", "main"),
        ("complete", "main"),
    ]);
    let rows = task_tree_rows(&tasks, &map, &BTreeSet::new());
    assert_eq!(summary(&rows, "main"), (3, 3, 6., true, false));
}

#[test]
fn duplicate_input_task_is_not_billed_twice() {
    let tasks = vec![
        task("main", 1, Some(10.)),
        task("child", 2, Some(100.)),
        task("child", 3, Some(2.)),
    ];
    let rows = task_tree_rows(&tasks, &parents(&[("child", "main")]), &expanded(&["main"]));
    assert_eq!(self_ids(&rows), [("main", 0), ("child", 1)]);
    assert_eq!(summary(&rows, "main"), (1, 1, 2., false, true));
    assert!(matches!(
        &rows[2],
        TaskTreeRow::SelfTask {
            task_index: Some(2),
            ..
        }
    ));
}

#[test]
fn height_threshold_counts_summaries_and_expanded_children() {
    let mut tasks = vec![task("main", 1, Some(10.))];
    let mut map = BTreeMap::new();
    for i in 0..12 {
        let id = format!("child-{i}");
        tasks.push(task(&id, i + 2, Some(1.)));
        map.insert(id, "main".to_owned());
    }
    let collapsed = task_tree_rows(&tasks, &map, &BTreeSet::new());
    assert_eq!(collapsed.len(), 2);
    assert_eq!(widget_height(collapsed.len(), 10), 368.);
    let open = task_tree_rows(&tasks, &map, &expanded(&["main"]));
    assert_eq!(open.len(), 14);
    assert_eq!(visible_task_rows(open.len(), 10), 10);
    assert_eq!(widget_height(open.len(), 10), 544.);
}

#[test]
fn empty_scope_does_not_show_unrelated_historical_metadata() {
    let map = parents(&[("old-child", "old-parent")]);
    assert!(task_tree_rows(&[], &map, &BTreeSet::new()).is_empty());
}

#[test]
fn deep_metadata_chain_uses_iterative_traversal() {
    let count = 10_000;
    let tasks = vec![task(&format!("node-{}", count - 1), 1, Some(1.))];
    let map: BTreeMap<_, _> = (1..count)
        .map(|i| (format!("node-{i}"), format!("node-{}", i - 1)))
        .collect();
    let open: BTreeSet<_> = (0..count).map(|i| format!("node-{i}")).collect();
    let rows = task_tree_rows(&tasks, &map, &open);
    assert_eq!(rows.len(), 2 * count - 1);
    assert_eq!(summary(&rows, "node-0"), (count - 1, 1, 1., true, true));
    assert_eq!(
        self_ids(&rows).last(),
        Some(&(format!("node-{}", count - 1).as_str(), count - 1))
    );
}
