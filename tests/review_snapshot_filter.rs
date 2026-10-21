//! Synthetic-only regressions for the default usage view. No installed sessions,
//! account, credentials, or user settings are read or changed by these tests.
use quota_widget::{
    background::{Command, Event, Snapshot, Worker},
    history,
    tokens::{ModelPrice, Pricing, TokenStore},
    usage_views::{self, GroupBy, TaskRow, TaskTreeRow},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

const BASE: i64 = 1_790_000_000;
const END: i64 = BASE + 100;
const SOL: &str = "gpt-6.1-sol";
const LUNA: &str = "gpt-6-luna";
const REVIEW: &str = "codex-auto-review";

fn counts(input: u64, cached: u64, output: u64) -> Value {
    json!({"input_tokens":input,"cached_input_tokens":cached,
        "cache_write_input_tokens":0,"output_tokens":output,
        "reasoning_output_tokens":0,"total_tokens":input+output})
}
fn metadata(task: &str, parent: Option<&str>, model: &str) -> Value {
    let mut payload = json!({"id":task,"thread_name":format!("Synthetic {task}"),"model":model});
    if let Some(parent) = parent {
        payload["source"] = json!({"subagent":{"thread_spawn":{"parent_thread_id":parent}}});
    }
    json!({"type":"session_meta","timestamp":BASE,"payload":payload})
}
fn response(task: &str, turn: &str, id: &str, model: &str, offset: i64, usage: Value) -> Value {
    json!({"type":"token_usage_record","timestamp":BASE+offset,"payload":{
        "thread_id":task,"session_id":"synthetic-common-root","turn_id":turn,
        "response_id":id,"model":model,"usage":usage}})
}
fn write_rows(root: &Path, file: &str, rows: &[Value]) {
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join(file),
        rows.iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>(),
    )
    .unwrap();
}
fn write_task(root: &Path, task: &str, parent: Option<&str>, model: &str, offset: i64) {
    write_rows(
        root,
        &format!("{task}.jsonl"),
        &[
            metadata(task, parent, model),
            response(
                task,
                "turn",
                "response",
                model,
                offset,
                counts(100_000, 80_000, 1_000),
            ),
        ],
    );
}
fn priced_review() -> Pricing {
    let mut pricing = Pricing::demo();
    pricing
        .set_rate(
            REVIEW,
            ModelPrice {
                uncached_input_per_million: 1.,
                cached_input_per_million: 0.25,
                output_per_million: 2.,
            },
        )
        .unwrap();
    pricing
}
fn import(root: &Path, pricing: &Pricing) -> TokenStore {
    let mut store = TokenStore::default();
    let report = store.import_dir(root, pricing).unwrap();
    assert_eq!(report.malformed_lines, 0, "{:?}", report.warnings);
    store
}
fn task_rows(snapshot: &Snapshot) -> Vec<TaskRow> {
    snapshot
        .usage_index
        .task_rows(snapshot.events(), Some(BASE), BASE, END)
}
fn close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-10, "{actual} != {expected}");
}
fn summary(rows: &[TaskTreeRow], id: &str) -> (usize, usize, f64) {
    rows.iter()
        .find_map(|row| match row {
            TaskTreeRow::ChildrenSummary {
                parent_id,
                descendant_count,
                direct_child_count,
                known_cost_usd,
                ..
            } if parent_id == id => Some((*descendant_count, *direct_child_count, *known_cost_usd)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing summary for {id}"))
}

#[test]
fn huge_review_and_metadata_only_review_are_absent_from_seven_luna_child_costs_and_counts() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("synthetic-logs");
    write_task(&root, "parent", None, SOL, 10);
    for child in 0..7 {
        write_task(
            &root,
            &format!("luna-{child}"),
            Some("parent"),
            LUNA,
            20 + child,
        );
    }
    let huge = response(
        "review",
        "review-turn",
        "huge-review",
        REVIEW,
        90,
        counts(9_000_000_000, 0, 9_000_000_000),
    );
    write_rows(
        &root,
        "review.jsonl",
        &[metadata("review", Some("parent"), REVIEW), huge.clone()],
    );
    write_rows(&root, "review-copy.jsonl", &[huge]);
    write_rows(
        &root,
        "review-metadata.jsonl",
        &[metadata("review-metadata", Some("parent"), REVIEW)],
    );
    let store = import(&root, &priced_review());
    let original = serde_json::to_value(&store).unwrap();
    let snapshot = Snapshot::from_store(&store);
    assert_eq!(snapshot.events.len(), 8);
    assert_eq!(snapshot.excluded_events.len(), 1);
    assert_eq!(snapshot.excluded_events[0].total_tokens(), 18_000_000_000);
    assert!(snapshot.excluded_events[0].cost_usd.unwrap() > 10_000.);
    assert_eq!(
        snapshot.excluded_tasks,
        BTreeSet::from(["review".into(), "review-metadata".into()])
    );
    assert_eq!(snapshot.task_parents.len(), 7);
    let tasks = task_rows(&snapshot);
    assert_eq!(tasks.len(), 8);
    let parent = tasks.iter().find(|task| task.task_id == "parent").unwrap();
    close(parent.interval_cost_usd.unwrap(), 0.042);
    for child in tasks
        .iter()
        .filter(|task| task.task_id.starts_with("luna-"))
    {
        close(child.latest_cost_usd.unwrap(), 0.019);
        close(child.interval_cost_usd.unwrap(), 0.019);
        assert_eq!(snapshot.events[child.latest_event_index].model, LUNA);
    }
    let collapsed = usage_views::task_tree_rows(&tasks, &snapshot.task_parents, &BTreeSet::new());
    assert_eq!(collapsed.len(), 2);
    let (descendants, direct, child_cost) = summary(&collapsed, "parent");
    assert_eq!((descendants, direct), (7, 7));
    close(child_cost, 7. * 0.019);
    let expanded = usage_views::task_tree_rows(
        &tasks,
        &snapshot.task_parents,
        &BTreeSet::from(["parent".into()]),
    );
    assert_eq!(expanded.len(), 9);
    assert_eq!(summary(&expanded, "parent"), summary(&collapsed, "parent"));
    let total = snapshot.cost_index.observed(BASE, END);
    assert_eq!((total.events, total.unknown_events), (8, 0));
    close(total.known_usd, 0.042 + 7. * 0.019);
    let details = usage_views::group(
        &snapshot.events.iter().collect::<Vec<_>>(),
        GroupBy::Response,
    );
    assert_eq!(details.len(), 8);
    assert!(details
        .iter()
        .all(|group| group.models.iter().all(|model| model != REVIEW)));
    assert_eq!(store.events().len(), 9);
    assert_eq!(
        store.observation_count(),
        9,
        "duplicate archive is retained once"
    );
    assert_eq!(
        serde_json::to_value(&store).unwrap(),
        original,
        "view construction is read-only"
    );
}

#[test]
fn excluded_parent_edges_detach_survivors_and_nested_totals_count_each_task_once() {
    let temp = TempDir::new().unwrap();
    write_task(temp.path(), "parent", None, SOL, 10);
    write_task(temp.path(), "child", Some("parent"), LUNA, 20);
    write_task(temp.path(), "grandchild", Some("child"), LUNA, 30);
    write_task(temp.path(), "sibling", Some("parent"), LUNA, 40);
    write_task(temp.path(), "review-bridge", Some("parent"), REVIEW, 50);
    write_task(temp.path(), "survivor", Some("review-bridge"), LUNA, 60);
    let store = import(temp.path(), &priced_review());
    let snapshot = Snapshot::from_store(&store);
    assert!(!snapshot.task_parents.contains_key("review-bridge"));
    assert!(!snapshot.task_parents.contains_key("survivor"));
    assert_eq!(
        store.task_parents().get("survivor").map(String::as_str),
        Some("review-bridge")
    );
    let tasks = task_rows(&snapshot);
    let rows = usage_views::task_tree_rows(
        &tasks,
        &snapshot.task_parents,
        &BTreeSet::from(["parent".into(), "child".into(), "review-bridge".into()]),
    );
    assert_eq!(summary(&rows, "parent").0, 3);
    assert_eq!(summary(&rows, "parent").1, 2);
    close(summary(&rows, "parent").2, 3. * 0.019);
    assert_eq!(summary(&rows, "child").0, 1);
    close(summary(&rows, "child").2, 0.019);
    let mut seen = BTreeSet::new();
    let mut total = 0.;
    for row in &rows {
        if let TaskTreeRow::SelfTask {
            task_id,
            task_index,
            depth,
            ..
        } = row
        {
            assert!(seen.insert(task_id));
            assert_ne!(task_id, "review-bridge");
            if task_id == "survivor" {
                assert_eq!(*depth, 0);
            }
            total += tasks[task_index.unwrap()].interval_cost_usd.unwrap();
        }
    }
    assert_eq!(seen.len(), 5);
    close(total, 0.042 + 4. * 0.019);
    close(snapshot.cost_index.observed(BASE, END).known_usd, total);
}

#[test]
fn trailing_review_in_mixed_thread_does_not_replace_latest_included_response_or_turn() {
    let temp = TempDir::new().unwrap();
    let mut first = response(
        "mixed",
        "normal-turn",
        "normal-1",
        SOL,
        10,
        counts(100, 20, 10),
    );
    first["payload"]["turn_token_usage"] = counts(100, 20, 10);
    let mut second = response(
        "mixed",
        "normal-turn",
        "normal-2",
        SOL,
        20,
        counts(200, 40, 20),
    );
    second["payload"]["turn_token_usage"] = counts(300, 60, 30);
    let mut review = response(
        "mixed",
        "review-turn",
        "review-response",
        REVIEW,
        30,
        counts(9_000_000, 0, 900_000),
    );
    review["payload"]["turn_token_usage"] = counts(9_000_000, 0, 900_000);
    write_rows(
        temp.path(),
        "mixed.jsonl",
        &[metadata("mixed", None, SOL), first, second, review],
    );
    let store = import(temp.path(), &priced_review());
    let snapshot = Snapshot::from_store(&store);
    assert_eq!(
        (snapshot.events.len(), snapshot.excluded_events.len()),
        (2, 1)
    );
    assert!(!snapshot.excluded_tasks.contains("mixed"));
    let tasks = task_rows(&snapshot);
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].latest_timestamp, BASE + 20);
    assert_eq!(tasks[0].interval_events, 2);
    let event = &snapshot.events[tasks[0].latest_event_index];
    assert_eq!(event.response_id.as_deref(), Some("normal-2"));
    let turn = &snapshot.task_turn_usage["mixed"];
    assert_eq!(turn.turn_id.as_deref(), Some("normal-turn"));
    assert_eq!((turn.input, turn.output), (Some(300), Some(30)));
    assert!(turn.recorded_snapshot);
    assert_eq!(
        store.task_turn_usage()["mixed"].turn_id.as_deref(),
        Some("review-turn")
    );
    close(
        tasks[0].interval_cost_usd.unwrap(),
        snapshot
            .events
            .iter()
            .filter_map(|event| event.cost_usd)
            .sum(),
    );
}

#[test]
fn mixed_turn_cumulative_counters_do_not_leak_excluded_review_tokens_into_details() {
    let temp = TempDir::new().unwrap();
    let mut first = response(
        "mixed",
        "same-turn",
        "normal-1",
        SOL,
        10,
        counts(100, 20, 10),
    );
    first["payload"]["turn_token_usage"] = counts(100, 20, 10);
    let mut review = response(
        "mixed",
        "same-turn",
        "review-response",
        REVIEW,
        20,
        counts(1_000_000, 0, 100_000),
    );
    review["payload"]["turn_token_usage"] = counts(1_000_100, 20, 100_010);
    let mut last = response(
        "mixed",
        "same-turn",
        "normal-2",
        SOL,
        30,
        counts(200, 40, 20),
    );
    last["payload"]["turn_token_usage"] = counts(1_000_300, 60, 100_030);
    write_rows(
        temp.path(),
        "mixed.jsonl",
        &[metadata("mixed", None, SOL), first, review, last],
    );
    let store = import(temp.path(), &priced_review());
    let snapshot = Snapshot::from_store(&store);
    let turn = &snapshot.task_turn_usage["mixed"];
    assert_eq!((turn.input, turn.output), (Some(300), Some(30)));
    assert!(
        !turn.recorded_snapshot,
        "mixed turn uses the included subtotal"
    );
    assert!(turn.partial);
    assert_eq!(store.task_turn_usage()["mixed"].input, Some(1_000_300));
    let details = usage_views::group(&snapshot.events.iter().collect::<Vec<_>>(), GroupBy::Turn);
    assert_eq!(details.len(), 1);
    assert_eq!((details[0].input, details[0].output), (300, 30));
}

#[test]
fn unknown_or_similarly_named_models_and_review_task_titles_remain_visible() {
    let temp = TempDir::new().unwrap();
    write_task(temp.path(), "unpriced", None, "synthetic-future-model", 10);
    write_task(
        temp.path(),
        "near-match",
        None,
        "codex-auto-review-preview",
        20,
    );
    write_task(temp.path(), "codex-auto-review", None, SOL, 30);
    write_task(temp.path(), "actual-review", None, REVIEW, 40);
    let snapshot = Snapshot::from_store(&import(temp.path(), &Pricing::demo()));
    assert_eq!(
        (snapshot.events.len(), snapshot.excluded_events.len()),
        (3, 1)
    );
    let tasks = task_rows(&snapshot);
    assert_eq!(tasks.len(), 3);
    assert!(tasks.iter().any(|task| task.task_id == "codex-auto-review"));
    assert!(
        tasks
            .iter()
            .find(|task| task.task_id == "unpriced")
            .unwrap()
            .partial
    );
    let total = snapshot.cost_index.observed(BASE, END);
    assert_eq!((total.events, total.unknown_events), (3, 2));
    close(total.known_usd, 0.042);
    assert_eq!(snapshot.excluded_events[0].cost_usd, None);
}

#[test]
fn restart_and_repricing_preserve_audit_records_and_exclusion_independent_of_price() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("synthetic-logs");
    write_task(&root, "normal", None, SOL, 10);
    write_task(&root, "review", Some("normal"), REVIEW, 20);
    let mut store = import(&root, &Pricing::demo());
    let source_before = fs::read(root.join("review.jsonl")).unwrap();
    let path = temp.path().join("state/tokens.json");
    store.save(&path).unwrap();
    let persisted: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(persisted["events"].as_array().unwrap().len(), 2);
    assert_eq!(persisted["observations"].as_array().unwrap().len(), 2);
    store = TokenStore::load(&path).unwrap();
    let first = Snapshot::from_store(&store);
    assert_eq!((first.events.len(), first.excluded_events.len()), (1, 1));
    store.reprice(&priced_review());
    let priced = Snapshot::from_store(&store);
    assert!(priced.excluded_events[0].cost_usd.is_some());
    close(priced.cost_index.observed(BASE, END).known_usd, 0.042);
    let mut override_pricing = Pricing::demo();
    override_pricing.model_override = Some(LUNA.into());
    store.reprice(&override_pricing);
    let overridden = Snapshot::from_store(&store);
    assert_eq!(
        (overridden.events.len(), overridden.excluded_events.len()),
        (1, 1)
    );
    close(overridden.cost_index.observed(BASE, END).known_usd, 0.019);
    store.save(&path).unwrap();
    store = TokenStore::load(&path).unwrap();
    let report = store.import_dir(&root, &override_pricing).unwrap();
    assert_eq!(report.lines_read, 0);
    assert_eq!(report.events_added, 0);
    assert_eq!(store.observation_count(), 2);
    assert_eq!(store.events().len(), 2);
    let restored = Snapshot::from_store(&store);
    assert_eq!(
        (restored.events.len(), restored.excluded_events.len()),
        (1, 1)
    );
    assert_eq!(restored.excluded_tasks, BTreeSet::from(["review".into()]));
    assert_eq!(fs::read(root.join("review.jsonl")).unwrap(), source_before);
}

fn next(worker: &Worker) -> Event {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match worker.try_recv() {
            Ok(Event::IntervalsUpdated { .. }) => continue,
            Ok(event) => return event,
            Err(std::sync::mpsc::TryRecvError::Empty) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            other => panic!("background response unavailable: {other:?}"),
        }
    }
}

#[test]
fn background_filter_leaves_review_logged_quota_observations_and_history_unchanged() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("synthetic-logs");
    let mut rows = vec![metadata("review", None, REVIEW)];
    for (offset, used) in [(1, 80), (10, 81), (20, 82)] {
        rows.push(
            json!({"type":"event_msg","timestamp":BASE+offset,"payload":{
            "type":"token_count","thread_id":"review","rate_limits":{"primary":{
                "used_percent":used,"window_minutes":300,"resets_at":BASE+10_000}}}}),
        );
    }
    rows.push(response(
        "review",
        "turn",
        "review-response",
        REVIEW,
        15,
        counts(9_000_000, 0, 900_000),
    ));
    write_rows(&root, "review.jsonl", &rows);
    let store = import(&root, &priced_review());
    let expected_quota = store.quota_observations().to_vec();
    let expected_history = history::normalized(&history::replay(&expected_quota));
    assert_eq!(expected_quota.len(), 3);
    assert!(!expected_history.is_empty());
    let token_path = temp.path().join("state/tokens.json");
    let history_path = temp.path().join("state/history.json");
    let mut worker = Worker::spawn();
    worker
        .send(Command::Import {
            root,
            metadata: None,
            pricing: priced_review(),
            save_path: Some(token_path.clone()),
            history_save_path: Some(history_path.clone()),
        })
        .unwrap();
    match next(&worker) {
        Event::Updated {
            snapshot: Some(snapshot),
            normalized_history: Some(actual),
            error: None,
            ..
        } => {
            assert!(snapshot.events.is_empty());
            assert_eq!(snapshot.excluded_events.len(), 1);
            assert_eq!(*actual, expected_history);
            assert_eq!(snapshot.cost_index.observed(BASE, END).events, 0);
        }
        other => panic!("unexpected background import result: {other:?}"),
    }
    worker
        .send(Command::Load {
            token_path: token_path.clone(),
            history_path: Some(history_path),
            pricing: priced_review(),
        })
        .unwrap();
    match next(&worker) {
        Event::Loaded {
            snapshot,
            normalized_history,
            error: None,
            history_error: None,
        } => {
            assert!(snapshot.events.is_empty());
            assert_eq!(snapshot.excluded_events.len(), 1);
            assert_eq!(*normalized_history, expected_history);
        }
        other => panic!("unexpected background load result: {other:?}"),
    }
    worker.shutdown().unwrap();
    let loaded = TokenStore::load(token_path).unwrap();
    assert_eq!(loaded.quota_observations(), expected_quota);
    assert_eq!(loaded.events().len(), 1);
}
