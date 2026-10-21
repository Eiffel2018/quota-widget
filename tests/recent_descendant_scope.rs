//! Synthetic regression coverage for hiding inactive historical descendants.
//! Presentation scope must not prune ledger events or authoritative parent edges.
use quota_widget::{
    accounting::{self, CostIndex},
    tokens::TokenEvent,
    usage_views::{self, TaskRelationship, TaskRow, TaskTreeRow, UsageIndex},
};
use std::collections::{BTreeMap, BTreeSet};

const END: i64 = 1_000_000;

fn event(task: &str, timestamp: i64, cost_usd: Option<f64>) -> TokenEvent {
    TokenEvent {
        id: format!("{task}:{timestamp}"),
        timestamp,
        model: "synthetic".into(),
        model_assumed: false,
        reasoning_effort: None,
        session: task.into(),
        response_id: None,
        turn_id: None,
        source_kind: Some("token_usage_record".into()),
        input: 1,
        cached: 0,
        uncached: 1,
        output: 1,
        reasoning: 0,
        cost_usd,
        coverage_start_timestamp_ms: None,
        partial_flags: vec![],
    }
}

fn parents(edges: &[(&str, &str)]) -> BTreeMap<String, String> {
    edges
        .iter()
        .map(|(child, parent)| ((*child).into(), (*parent).into()))
        .collect()
}

fn expanded(map: &BTreeMap<String, String>) -> BTreeSet<String> {
    map.keys().chain(map.values()).cloned().collect()
}

fn self_ids(rows: &[TaskTreeRow]) -> BTreeSet<&str> {
    rows.iter()
        .filter_map(|row| match row {
            TaskTreeRow::SelfTask { task_id, .. } => Some(task_id.as_str()),
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

fn reducers(events: &[TokenEvent], boundary: Option<i64>, end: i64) -> [Vec<TaskRow>; 2] {
    [
        UsageIndex::new(events).task_rows(events, boundary, end - 60, end),
        usage_views::task_rows(events, boundary, end),
    ]
}

#[test]
fn historical_286_sibling_and_descendant_edges_do_not_reenter_the_visible_scope() {
    let boundary = END - 60;
    let mut events = vec![
        event("main", END - 1, Some(10.)),
        event("active", END - 2, Some(2.)),
    ];
    let mut map = parents(&[("active", "main")]);
    for i in 0..143 {
        let sibling = format!("stale-sibling-{i}");
        let descendant = format!("stale-descendant-{i}");
        events.push(event(&sibling, END - 30_000 - i, Some(1.)));
        events.push(event(&descendant, END - 40_000 - i, Some(1.)));
        map.insert(sibling.clone(), "main".into());
        map.insert(
            descendant,
            if i % 2 == 0 { "active".into() } else { sibling },
        );
    }
    let original_events = events.clone();
    let original_parents = map.clone();
    let full_history = CostIndex::new(&events).observed(0, END);
    let interval = accounting::sum_cost(&events, boundary, END);
    assert_eq!(events.len(), 288);
    assert_eq!(map.len(), 287);
    assert_eq!(full_history.events, 288);
    assert_eq!(full_history.known_usd, 298.);
    assert_eq!(interval.0, Some(12.));
    assert_eq!(interval.2, 2);

    for tasks in reducers(&events, Some(boundary), END) {
        assert_eq!(tasks.len(), 2);
        let closed = usage_views::task_tree_rows(&tasks, &map, &BTreeSet::new());
        assert_eq!(closed.len(), 2);
        assert_eq!(summary(&closed, "main"), (1, 1, 2., false, false));

        // Even expansion IDs persisted from the old UI cannot resurrect stale rows.
        let open = usage_views::task_tree_rows(&tasks, &map, &expanded(&map));
        assert_eq!(open.len(), 3);
        assert_eq!(self_ids(&open), BTreeSet::from(["main", "active"]));
        assert_eq!(summary(&open, "main"), (1, 1, 2., false, true));
        assert_eq!(usage_views::visible_task_rows(open.len(), 50), 3);
        assert_eq!(usage_views::widget_height(open.len(), 50), 390.);
    }

    assert_eq!(events, original_events, "hiding rows must not delete usage");
    assert_eq!(
        map, original_parents,
        "historical parent edges are retained"
    );
    assert_eq!(CostIndex::new(&events).observed(0, END), full_history);
    assert_eq!(accounting::sum_cost(&events, boundary, END), interval);
}

#[test]
fn old_current_interval_usage_survives_including_zero_and_unpriced_events() {
    let boundary = END - 30_000;
    let events = vec![
        event("main", END, Some(100.)),
        event("old-priced", END - 20_000, Some(7.)),
        event("old-zero", END - 18_000, Some(0.)),
        event("old-unpriced", END - 16_000, None),
        event("at-boundary", boundary, Some(400.)),
        event("future", END + 1, Some(800.)),
    ];
    let map = parents(&[
        ("old-priced", "main"),
        ("old-zero", "main"),
        ("old-unpriced", "main"),
        ("at-boundary", "main"),
        ("future", "main"),
        ("metadata-only-stale", "main"),
    ]);
    for tasks in reducers(&events, Some(boundary), END) {
        assert_eq!(tasks.len(), 4);
        for id in ["old-priced", "old-zero", "old-unpriced"] {
            let row = tasks.iter().find(|row| row.task_id == id).unwrap();
            assert!(row.latest_timestamp < END - 7200);
            assert_eq!(row.interval_events, 1);
        }
        assert!(
            tasks
                .iter()
                .find(|row| row.task_id == "old-unpriced")
                .unwrap()
                .partial
        );
        let tree = usage_views::task_tree_rows(&tasks, &map, &expanded(&map));
        assert_eq!(
            self_ids(&tree),
            BTreeSet::from(["main", "old-priced", "old-zero", "old-unpriced"])
        );
        assert_eq!(summary(&tree, "main"), (3, 3, 7., true, true));
    }
    let observed = CostIndex::new(&events).observed(boundary, END);
    assert_eq!(observed.events, 4);
    assert_eq!(observed.unknown_events, 1);
    assert_eq!(observed.known_usd, 107.);
    assert_eq!(accounting::sum_cost(&events, boundary, END).0, None);
}

#[test]
fn required_metadata_ancestors_remain_without_their_inactive_siblings() {
    let events = vec![
        event("leaf", END, Some(5.)),
        event("middle", END - 40_000, Some(900.)),
        event("root", END - 50_000, Some(800.)),
    ];
    let map = parents(&[
        ("leaf", "middle"),
        ("middle", "root"),
        ("stale-sibling", "root"),
        ("stale-cousin", "middle"),
    ]);
    for tasks in reducers(&events, Some(END - 60), END) {
        assert_eq!(tasks.len(), 1);
        let tree = usage_views::task_tree_rows(&tasks, &map, &expanded(&map));
        assert_eq!(self_ids(&tree), BTreeSet::from(["root", "middle", "leaf"]));
        assert_eq!(summary(&tree, "root"), (2, 1, 5., true, true));
        assert_eq!(summary(&tree, "middle"), (1, 1, 5., false, true));
        for (id, expected_depth) in [("root", 0), ("middle", 1), ("leaf", 2)] {
            assert!(tree.iter().any(|row| matches!(
                row,
                TaskTreeRow::SelfTask { task_id, task_index, depth, .. }
                    if task_id == id && *depth == expected_depth
                        && task_index.is_some() == (id == "leaf")
            )));
        }
    }
    assert_eq!(CostIndex::new(&events).observed(0, END).known_usd, 1705.);
}

#[test]
fn nested_counts_and_costs_are_unique_and_independent_of_expansion() {
    let events = vec![
        event("main", END - 4, Some(100.)),
        event("branch", END - 3, Some(10.)),
        event("leaf", END - 1, Some(2.)),
        event("sibling", END - 2, Some(3.)),
        event("stale", END - 50_000, Some(900.)),
    ];
    let map = parents(&[
        ("branch", "main"),
        ("leaf", "branch"),
        ("sibling", "main"),
        ("stale", "branch"),
        ("stale-grandchild", "stale"),
    ]);
    for mut tasks in reducers(&events, Some(END - 60), END) {
        let mut older_duplicate = tasks
            .iter()
            .find(|row| row.task_id == "branch")
            .unwrap()
            .clone();
        older_duplicate.latest_timestamp -= 1;
        older_duplicate.interval_cost_usd = Some(999.);
        tasks.push(older_duplicate);

        let closed = usage_views::task_tree_rows(&tasks, &map, &BTreeSet::new());
        let root_open = usage_views::task_tree_rows(&tasks, &map, &BTreeSet::from(["main".into()]));
        let all_open = usage_views::task_tree_rows(&tasks, &map, &expanded(&map));
        assert_eq!(summary(&closed, "main"), (3, 2, 15., false, false));
        assert_eq!(summary(&root_open, "main"), (3, 2, 15., false, true));
        assert_eq!(summary(&all_open, "main"), (3, 2, 15., false, true));
        assert_eq!(summary(&root_open, "branch"), (1, 1, 2., false, false));
        assert_eq!(summary(&all_open, "branch"), (1, 1, 2., false, true));
        assert_eq!(
            self_ids(&all_open),
            BTreeSet::from(["main", "branch", "leaf", "sibling"])
        );

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
                    assert_eq!(tasks[*index].task_id, *task_id);
                    tasks[*index].interval_cost_usd
                }
                _ => None,
            })
            .sum();
        assert_eq!(
            own_total, 115.,
            "display summaries are not additional costs"
        );
    }
    assert_eq!(
        CostIndex::new(&events).observed(END - 60, END).known_usd,
        115.
    );
}

#[test]
fn cycle_ancestors_are_safe_and_unrelated_or_stale_metadata_stays_hidden() {
    let events = vec![
        event("leaf", END, Some(3.)),
        event("orphan", END - 1, Some(2.)),
        event("unlinked", END - 2, Some(1.)),
    ];
    let map = parents(&[
        ("leaf", "cycle-a"),
        ("cycle-a", "cycle-b"),
        ("cycle-b", "cycle-a"),
        ("stale-cycle-child", "cycle-a"),
        ("stale-cycle-grandchild", "stale-cycle-child"),
        ("orphan", "  "),
        ("unrelated-a", "unrelated-b"),
        ("unrelated-b", "unrelated-a"),
        ("unrelated-child", "unrelated-root"),
    ]);
    for tasks in reducers(&events, Some(END - 60), END) {
        let tree = usage_views::task_tree_rows(&tasks, &map, &expanded(&map));
        let expected = BTreeSet::from(["cycle-a", "cycle-b", "leaf", "orphan", "unlinked"]);
        assert_eq!(self_ids(&tree), expected);
        assert_eq!(
            tree.len(),
            6,
            "each task and the one valid summary appear once"
        );
        assert_eq!(summary(&tree, "cycle-a"), (1, 1, 3., false, true));
        for row in &tree {
            if let TaskTreeRow::SelfTask {
                task_id,
                task_index,
                depth,
                relationship,
            } = row
            {
                match task_id.as_str() {
                    "cycle-a" | "cycle-b" => {
                        assert_eq!(*depth, 0);
                        assert!(task_index.is_none());
                        assert!(matches!(relationship, TaskRelationship::Cycle { .. }));
                    }
                    "leaf" => assert_eq!(
                        relationship,
                        &TaskRelationship::Child {
                            parent_id: "cycle-a".into()
                        }
                    ),
                    "orphan" => {
                        assert_eq!(relationship, &TaskRelationship::Orphan { parent_id: None })
                    }
                    "unlinked" => assert_eq!(relationship, &TaskRelationship::Unlinked),
                    _ => unreachable!(),
                }
            }
        }
    }
    assert!(usage_views::task_tree_rows(&[], &map, &expanded(&map)).is_empty());
}

#[test]
fn expiry_and_exclusive_interval_boundary_remove_descendants_without_reimport() {
    let events = vec![
        event("main", END, Some(10.)),
        event("at-two-hours", END - 7200, Some(2.)),
        event("old-interval", END - 8000, Some(3.)),
        event("stale", END - 30_000, Some(99.)),
    ];
    let map = parents(&[
        ("at-two-hours", "main"),
        ("old-interval", "main"),
        ("stale", "main"),
    ]);
    // Reuse the same index: wall-clock expiry must not depend on a new import.
    let index = UsageIndex::new(&events);
    let cases: &[(i64, i64, &[&str], f64)] = &[
        (END - 60, END, &["main", "at-two-hours"], 0.),
        (END - 60, END + 1, &["main"], 0.),
        (
            END - 10_000,
            END,
            &["main", "at-two-hours", "old-interval"],
            5.,
        ),
        (END - 8000, END, &["main", "at-two-hours"], 2.),
        (END - 7200, END, &["main", "at-two-hours"], 0.),
        (END - 7200, END + 1, &["main"], 0.),
    ];
    for &(boundary, end, expected, descendant_cost) in cases {
        for tasks in [
            index.task_rows(&events, Some(boundary), boundary, end),
            usage_views::task_rows(&events, Some(boundary), end),
        ] {
            let tree = usage_views::task_tree_rows(&tasks, &map, &expanded(&map));
            assert_eq!(
                self_ids(&tree),
                expected.iter().copied().collect(),
                "boundary={boundary}, end={end}"
            );
            if expected.len() == 1 {
                assert_eq!(
                    tree.len(),
                    1,
                    "expired descendants cannot leave an empty summary"
                );
            } else {
                let count = expected.len() - 1;
                assert_eq!(
                    summary(&tree, "main"),
                    (count, count, descendant_cost, false, true)
                );
            }
        }
    }
    assert_eq!(events.len(), 4);
    assert_eq!(map.len(), 3);
    assert_eq!(CostIndex::new(&events).observed(0, END).known_usd, 114.);
}

#[test]
fn unknown_interval_boundary_keeps_recent_grace_without_reviving_old_descendants() {
    let events = vec![
        event("main", END, Some(10.)),
        event("at-two-hours", END - 7200, Some(2.)),
        event("just-expired", END - 7201, Some(300.)),
    ];
    let map = parents(&[
        ("at-two-hours", "main"),
        ("just-expired", "main"),
        ("metadata-only-stale", "main"),
    ]);
    for tasks in reducers(&events, None, END) {
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().all(|row| row.partial));
        let tree = usage_views::task_tree_rows(&tasks, &map, &expanded(&map));
        assert_eq!(self_ids(&tree), BTreeSet::from(["main", "at-two-hours"]));
        assert_eq!(summary(&tree, "main"), (1, 1, 0., true, true));
    }
    assert_eq!(CostIndex::new(&events).observed(0, END).known_usd, 312.);
}
