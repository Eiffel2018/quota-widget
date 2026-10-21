use quota_widget::{
    accounting::{self, CostIndex},
    tokens::TokenEvent,
    usage_views::{self, GroupBy, UsageIndex},
};
fn event(id: &str, excluded: bool, cost: f64) -> TokenEvent {
    serde_json::from_value(serde_json::json!({"id":id,"timestamp":200,"model":if excluded {"codex-auto-review"}else{"gpt-6-luna"},"model_assumed":false,"session":id,"input":100,"cached":50,"uncached":50,"output":10,"reasoning":5,"cost_usd":cost,"partial_flags":if excluded {vec!["excluded_recorded_codex_auto_review"]}else{vec![]}})).unwrap()
}
#[test]
fn core_accounting_and_grouping_ignore_excluded_even_when_caller_passes_all_records() {
    let events = vec![event("normal", false, 0.1), event("review", true, 1000.)];
    assert_eq!(CostIndex::new(&events).observed(100, 300).known_usd, 0.1);
    assert_eq!(accounting::sum_cost(&events, 100, 300).0, Some(0.1));
    assert_eq!(
        UsageIndex::new(&events)
            .task_rows(&events, Some(100), 100, 300)
            .len(),
        1
    );
    assert_eq!(usage_views::task_rows(&events, Some(100), 300).len(), 1);
    let refs = events.iter().collect::<Vec<_>>();
    assert_eq!(usage_views::group(&refs, GroupBy::Task).len(), 1);
    assert_eq!(
        usage_views::group_with_excluded(&refs, GroupBy::Task, true).len(),
        2
    );
}
#[test]
fn ordinary_review_named_task_is_not_excluded() {
    let e = event("code-review-by-sol", false, 0.2);
    assert!(!e.is_excluded_auto_review());
    assert_eq!(CostIndex::new(&[e]).observed(100, 300).known_usd, 0.2);
}
