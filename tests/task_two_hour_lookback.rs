//! Synthetic regression for the user's requested 24h -> 2h live-list change.
//! Accounting is retained; stale metadata does not undo the live-list cutoff.
use quota_widget::{
    tokens::TokenEvent,
    usage_views::{self, TaskTreeRow, UsageIndex},
};
use std::collections::{BTreeMap, BTreeSet};
fn event(task: &str, time: i64, cost: Option<f64>) -> TokenEvent {
    TokenEvent {
        id: task.into(),
        timestamp: time,
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
        cost_usd: cost,
        coverage_start_timestamp_ms: None,
        partial_flags: vec![],
    }
}
#[test]
fn two_hour_cutoff_is_inclusive_and_hides_older_noncontributing_tasks() {
    let end = 100_000;
    let boundary = end - 60;
    let events = vec![
        event("active", end - 5, Some(1.)),
        event("exactly-two-hours", end - 7200, Some(2.)),
        event("over-two-hours", end - 7201, Some(3.)),
        event("yesterday", end - 40000, Some(4.)),
    ];
    let expected = BTreeSet::from(["active", "exactly-two-hours"]);
    let indexed = UsageIndex::new(&events).task_rows(&events, Some(boundary), end - 20, end);
    let reference = usage_views::task_rows(&events, Some(boundary), end);
    for rows in [&indexed, &reference] {
        assert_eq!(
            rows.iter()
                .map(|r| r.task_id.as_str())
                .collect::<BTreeSet<_>>(),
            expected
        );
        assert_eq!(
            rows.iter()
                .find(|r| r.task_id == "exactly-two-hours")
                .unwrap()
                .interval_events,
            0
        );
    }
    assert_eq!(events.len(), 4, "historical ledger is not pruned");
}
#[test]
fn current_interval_contributors_remain_even_when_older_than_two_hours() {
    let end = 100_000;
    let events = vec![
        event("old-priced", end - 20000, Some(4.)),
        event("old-unpriced", end - 15000, None),
        event("old-zero", end - 10000, Some(0.)),
    ];
    for rows in [
        UsageIndex::new(&events).task_rows(&events, Some(end - 30000), end, end),
        usage_views::task_rows(&events, Some(end - 30000), end),
    ] {
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.interval_events == 1));
    }
}
#[test]
fn missing_boundary_uses_same_two_hour_grace_without_importing_lifetime_totals() {
    let end = 100_000;
    let events = vec![
        event("recent", end - 7199, Some(8.)),
        event("stale", end - 7201, Some(9.)),
    ];
    let rows = UsageIndex::new(&events).task_rows(&events, None, end - 5, end);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].task_id, "recent");
    assert_eq!(rows[0].interval_cost_usd, Some(0.));
    assert!(rows[0].partial);
    assert_eq!(usage_views::task_rows(&events, None, end).len(), 1);
}
#[test]
fn stale_recorded_descendants_do_not_bypass_the_live_list_cutoff() {
    let events = vec![event("parent", 100_000, Some(1.))];
    let tasks = UsageIndex::new(&events).task_rows(&events, Some(99_999), 99_999, 100_000);
    let parents = BTreeMap::from([("old-child".into(), "parent".into())]);
    let rows = usage_views::task_tree_rows(&tasks, &parents, &BTreeSet::from(["parent".into()]));
    assert!(!rows
        .iter()
        .any(|r| matches!(r,TaskTreeRow::SelfTask{task_id,..} if task_id=="old-child")));
    assert_eq!(parents.len(), 1, "historical relationship is retained");
}
