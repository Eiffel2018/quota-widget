//! Worker-only completed-interval persistence. Every input is synthetic; no
//! installed session logs, accounts, or real user data directories are accessed.
use quota_widget::{
    background::{Command, Event, Worker},
    intervals::CompletedInterval,
    model::HistoryEvent,
    storage,
    tokens::{ModelPrice, Pricing, TokenStore},
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const BASE: i64 = 1_790_000_000;

#[derive(Deserialize)]
struct IntervalFile {
    version: u32,
    intervals: Vec<CompletedInterval>,
}

fn pricing() -> Pricing {
    let mut pricing = Pricing::default();
    for (name, input, cached, output) in [
        ("synthetic-model-a", 2., 0.5, 10.),
        ("synthetic-model-b", 1., 0.1, 5.),
        ("codex-auto-review", 20., 5., 100.),
    ] {
        pricing
            .set_rate(
                name,
                ModelPrice {
                    uncached_input_per_million: input,
                    cached_input_per_million: cached,
                    output_per_million: output,
                },
            )
            .unwrap();
    }
    pricing
}

fn logs(root: &Path) {
    fs::create_dir_all(root).unwrap();
    let rows: Vec<_> = [
        ("a", "synthetic-model-a", 12, 200_000, 50_000, 10_000),
        ("b", "synthetic-model-b", 18, 300_000, 100_000, 20_000),
        ("review", "codex-auto-review", 19, 9_000_000, 0, 9_000_000),
    ]
    .into_iter()
    .map(|(id, model, offset, input, cached, output)| {
        json!({"type":"token_usage_record","timestamp":BASE + offset,"payload":{
            "thread_id":format!("synthetic-{id}"),"turn_id":"turn",
            "response_id":format!("response-{id}"),"model":model,
            "usage":{"input_tokens":input,"cached_input_tokens":cached,
                "output_tokens":output,"reasoning_output_tokens":100,
                "total_tokens":input+output}}})
    })
    .collect();
    fs::write(
        root.join("synthetic.jsonl"),
        rows.iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>(),
    )
    .unwrap();
}

fn history_event() -> HistoryEvent {
    serde_json::from_value(json!({
        "session":"synthetic-live", "observed_at_unix":BASE+20,
        "source_key":"codex", "epoch":0, "kind":"QuotaDecrease",
        "window_label":"week", "window_key":"primary",
        "before":"90", "after":"89", "delta":"1", "partial":false,
        "before_observed_at_unix":BASE+10
    }))
    .unwrap()
}

fn next(worker: &Worker) -> Event {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match worker.try_recv() {
            Ok(event) => return event,
            Err(std::sync::mpsc::TryRecvError::Empty) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            other => panic!("background response unavailable: {other:?}"),
        }
    }
}

fn interval_update(worker: &Worker) -> Arc<Vec<CompletedInterval>> {
    match next(worker) {
        Event::IntervalsUpdated { intervals } => intervals,
        other => panic!("expected interval update, got {other:?}"),
    }
}

fn import(root: &Path, token_path: Option<&Path>, history_path: Option<&Path>) -> Command {
    Command::Import {
        root: root.to_path_buf(),
        metadata: None,
        pricing: pricing(),
        save_path: token_path.map(Path::to_path_buf),
        history_save_path: history_path.map(Path::to_path_buf),
    }
}

fn populate(worker: &Worker, root: &Path, state: &Path) -> Arc<Vec<CompletedInterval>> {
    worker
        .send(import(root, Some(&state.join("tokens.json")), None))
        .unwrap();
    assert!(matches!(next(worker), Event::Updated { error: None, .. }));
    assert!(interval_update(worker).is_empty());
    worker
        .send(Command::MergeHistory {
            events: vec![history_event()],
            save_path: Some(state.join("history.json")),
        })
        .unwrap();
    assert!(matches!(
        next(worker),
        Event::HistoryUpdated {
            normalized: Some(_)
        }
    ));
    interval_update(worker)
}

fn close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
}

#[test]
fn worker_persists_model_totals_and_reprices_without_rewriting_raw_history() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("logs");
    let state = temp.path().join("state");
    logs(&root);
    let mut worker = Worker::spawn();
    let intervals = populate(&worker, &root, &state);
    assert_eq!(intervals.len(), 1);
    let interval = &intervals[0];
    assert_eq!(interval.models.len(), 2, "automatic review is excluded");
    assert_eq!(interval.events, 2);
    close(interval.known_usd, 0.735);
    let a = &interval.models[0];
    assert_eq!(a.model, "synthetic-model-a");
    assert_eq!(
        (a.input, a.cached, a.uncached, a.output, a.reasoning),
        (200_000, 50_000, 150_000, 10_000, 100)
    );
    close(a.known_usd, 0.425);
    let saved: IntervalFile = storage::load_json(&state.join("quota-intervals.json")).unwrap();
    assert_eq!(saved.version, 1);
    assert_eq!(saved.intervals, *intervals);
    let history_before = fs::read(state.join("history.json")).unwrap();
    let source_before = fs::read(root.join("synthetic.jsonl")).unwrap();
    let mut changed = pricing();
    changed
        .rates
        .get_mut("synthetic-model-a")
        .unwrap()
        .output_per_million = 20.;
    worker
        .send(Command::Reprice {
            pricing: changed,
            save_path: Some(state.join("tokens.json")),
        })
        .unwrap();
    assert!(matches!(
        next(&worker),
        Event::Updated {
            snapshot: Some(_),
            error: None,
            ..
        }
    ));
    let repriced = interval_update(&worker);
    close(repriced[0].known_usd, 0.835);
    let saved: IntervalFile = storage::load_json(&state.join("quota-intervals.json")).unwrap();
    assert_eq!(saved.intervals, *repriced);
    assert_eq!(
        fs::read(state.join("history.json")).unwrap(),
        history_before
    );
    assert_eq!(
        fs::read(root.join("synthetic.jsonl")).unwrap(),
        source_before
    );
    assert_eq!(
        TokenStore::load(state.join("tokens.json"))
            .unwrap()
            .events()
            .len(),
        3
    );
    assert_eq!(
        Arc::strong_count(&intervals),
        2,
        "worker retains retired GUI data"
    );
    worker.shutdown().unwrap();
}

#[test]
fn duplicate_history_identical_pricing_and_idle_import_never_republish_or_rewrite() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("logs");
    let state = temp.path().join("state");
    logs(&root);
    let mut worker = Worker::spawn();
    let initial = populate(&worker, &root, &state);
    fs::remove_file(state.join("quota-intervals.json")).unwrap();
    worker
        .send(Command::MergeHistory {
            events: vec![history_event()],
            save_path: Some(state.join("history.json")),
        })
        .unwrap();
    worker
        .send(Command::Reprice {
            pricing: pricing(),
            save_path: Some(state.join("tokens.json")),
        })
        .unwrap();
    worker
        .send(import(
            &root,
            Some(&state.join("tokens.json")),
            Some(&state.join("history.json")),
        ))
        .unwrap();
    worker.shutdown().unwrap();
    let events: Vec<_> = std::iter::from_fn(|| worker.try_recv().ok()).collect();
    assert_eq!(
        events.len(),
        3,
        "no interval publication on unchanged inputs"
    );
    assert!(matches!(
        events[0],
        Event::HistoryUpdated { normalized: None }
    ));
    assert!(events[1..].iter().all(|event| matches!(
        event,
        Event::Updated {
            snapshot: None,
            normalized_history: None,
            error: None,
            ..
        }
    )));
    assert!(
        !state.join("quota-intervals.json").exists(),
        "idle poll rewrote derived snapshot"
    );
    close(initial[0].known_usd, 0.735);
}

#[test]
fn restart_rebuilds_from_raw_inputs_and_ignores_corrupt_derived_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("logs");
    let state = temp.path().join("state");
    logs(&root);
    let expected = {
        let mut worker = Worker::spawn();
        let expected = populate(&worker, &root, &state);
        worker.shutdown().unwrap();
        expected
    };
    fs::write(
        state.join("quota-intervals.json"),
        "discardable invalid derived cache",
    )
    .unwrap();
    let mut worker = Worker::spawn();
    worker
        .send(Command::Load {
            token_path: state.join("tokens.json"),
            history_path: Some(state.join("history.json")),
            pricing: pricing(),
        })
        .unwrap();
    assert!(matches!(
        next(&worker),
        Event::Loaded {
            error: None,
            history_error: None,
            ..
        }
    ));
    let rebuilt = interval_update(&worker);
    assert_eq!(*rebuilt, *expected);
    assert_eq!(
        fs::read_to_string(state.join("quota-intervals.json")).unwrap(),
        "discardable invalid derived cache",
        "Load alone cannot write"
    );
    worker
        .send(Command::Reprice {
            pricing: pricing(),
            save_path: Some(state.join("tokens.json")),
        })
        .unwrap();
    worker.shutdown().unwrap();
    let saved: IntervalFile = storage::load_json(&state.join("quota-intervals.json")).unwrap();
    assert_eq!(saved.intervals, *expected);
    assert!(matches!(
        next(&worker),
        Event::Updated { snapshot: None, .. }
    ));
    assert!(worker.try_recv().is_err());
}

#[test]
fn read_only_changes_publish_intervals_without_creating_state_files() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("logs");
    let state = temp.path().join("state");
    logs(&root);
    let mut worker = Worker::spawn();
    worker
        .send(Command::Load {
            token_path: state.join("tokens.json"),
            history_path: Some(state.join("history.json")),
            pricing: pricing(),
        })
        .unwrap();
    worker.send(import(&root, None, None)).unwrap();
    worker
        .send(Command::MergeHistory {
            events: vec![history_event()],
            save_path: None,
        })
        .unwrap();
    worker.shutdown().unwrap();
    let updates: Vec<_> = std::iter::from_fn(|| worker.try_recv().ok())
        .filter_map(|event| match event {
            Event::IntervalsUpdated { intervals } => Some(intervals),
            Event::SaveFailed { error, .. } => panic!("read-only attempted save: {error}"),
            _ => None,
        })
        .collect();
    assert_eq!(updates.len(), 2, "empty load then completed interval");
    close(updates[1][0].known_usd, 0.735);
    assert!(!state.exists());
}

#[test]
fn corrupt_reload_clears_stale_intervals_and_blocks_all_derived_writes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("logs");
    let state = temp.path().join("state");
    logs(&root);
    let mut worker = Worker::spawn();
    let _ = populate(&worker, &root, &state);
    let bad = temp.path().join("corrupt");
    fs::create_dir(&bad).unwrap();
    fs::write(bad.join("tokens.json"), "preserve damaged raw evidence").unwrap();
    worker
        .send(Command::Load {
            token_path: bad.join("tokens.json"),
            history_path: None,
            pricing: pricing(),
        })
        .unwrap();
    match next(&worker) {
        Event::Loaded {
            snapshot,
            error: Some(_),
            ..
        } => assert!(snapshot.events.is_empty()),
        other => panic!("expected corrupt load: {other:?}"),
    }
    assert!(
        interval_update(&worker).is_empty(),
        "old intervals survived a reload"
    );
    worker
        .send(import(
            &root,
            Some(&bad.join("tokens.json")),
            Some(&bad.join("history.json")),
        ))
        .unwrap();
    worker
        .send(Command::MergeHistory {
            events: vec![history_event()],
            save_path: Some(bad.join("history.json")),
        })
        .unwrap();
    worker.shutdown().unwrap();
    assert_eq!(
        fs::read_to_string(bad.join("tokens.json")).unwrap(),
        "preserve damaged raw evidence"
    );
    assert!(!bad.join("history.json").exists());
    assert!(!bad.join("quota-intervals.json").exists());
}

#[test]
fn corrupt_raw_history_blocks_derived_persistence_even_with_authorized_token_path() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("logs");
    logs(&root);
    let history_path = temp.path().join("history.json");
    let original = json!({"version":999,"events":[]});
    fs::write(&history_path, original.to_string()).unwrap();
    let mut worker = Worker::spawn();
    worker
        .send(Command::Load {
            token_path: temp.path().join("tokens.json"),
            history_path: Some(history_path.clone()),
            pricing: pricing(),
        })
        .unwrap();
    worker
        .send(import(
            &root,
            Some(&temp.path().join("tokens.json")),
            Some(&history_path),
        ))
        .unwrap();
    worker
        .send(Command::MergeHistory {
            events: vec![history_event()],
            save_path: Some(history_path.clone()),
        })
        .unwrap();
    worker.shutdown().unwrap();
    assert_eq!(
        storage::load_json::<Value>(&history_path).unwrap(),
        original
    );
    assert!(!temp.path().join("quota-intervals.json").exists());
    assert!(!temp.path().join("tokens.json").exists());
}
