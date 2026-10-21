//! Synthetic recovery regressions. Never opens an installed Codex home.
use quota_widget::{
    background::Snapshot,
    tokens::{Pricing, TokenStore},
};
use serde_json::{json, Value};
use std::fs;
use tempfile::TempDir;

fn populated() -> (TempDir, TokenStore, Pricing) {
    let dir = TempDir::new().unwrap();
    let rows: Vec<Value> = [
        ("normal", "gpt-6-luna", 100),
        ("normal", "gpt-6.1-sol", 101),
        ("review", "codex-auto-review", 102),
    ]
    .into_iter()
    .map(|(task, model, t)| {
        json!({
        "timestamp":t,"type":"token_usage_record","payload":{
        "thread_id":task,"turn_id":"turn","response_id":format!("response-{t}"),
        "model":model,"usage":{"input_tokens":10000,"cached_input_tokens":8000,
        "output_tokens":400,"reasoning_output_tokens":100}}})
    })
    .collect();
    fs::write(
        dir.path().join("synthetic.jsonl"),
        rows.iter().map(|v| format!("{v}\n")).collect::<String>(),
    )
    .unwrap();
    let pricing = Pricing::official_standard();
    let mut store = TokenStore::default();
    store.import_dir(dir.path(), &pricing).unwrap();
    assert_eq!(store.events().len(), 3);
    (dir, store, pricing)
}

#[test]
fn empty_derived_cache_recovers_from_retained_observations_without_source_replay() {
    let (dir, store, pricing) = populated();
    let original_events = serde_json::to_value(store.events()).unwrap();
    let mut payload = serde_json::to_value(&store).unwrap();
    payload["events"] = json!([]);
    let state = dir.path().join("tokens.json");
    let bytes = serde_json::to_vec(&payload).unwrap();
    fs::write(&state, &bytes).unwrap();
    let mut repaired = TokenStore::load(&state).unwrap();
    assert_eq!(
        serde_json::to_value(repaired.events()).unwrap(),
        original_events
    );
    let report = repaired.import_dir(dir.path(), &pricing).unwrap();
    assert_eq!(report.lines_read, 0);
    assert_eq!(report.files_unchanged, 1);
    assert_eq!(report.events_added, 0);
    let snapshot = Snapshot::from_store(&repaired);
    assert_eq!(snapshot.events.len(), 2);
    assert_eq!(snapshot.excluded_events.len(), 1);
    assert_eq!(snapshot.events[0].model, "gpt-6-luna");
    assert_eq!(snapshot.events[1].model, "gpt-6.1-sol");
    assert_eq!(
        fs::read(&state).unwrap(),
        bytes,
        "load never overwrites original"
    );
    let repaired_json = serde_json::to_value(&repaired).unwrap();
    for field in ["observations", "sources", "contexts", "quota"] {
        assert_eq!(repaired_json[field], payload[field], "preserve {field}");
    }
    repaired.save(dir.path().join("repaired.json")).unwrap();
    let restarted = TokenStore::load(dir.path().join("repaired.json")).unwrap();
    assert_eq!(restarted.revision(), repaired.revision());
    assert_eq!(
        serde_json::to_value(restarted.events()).unwrap(),
        original_events
    );
}

#[test]
fn healthy_cache_load_remains_an_unchanged_fast_path() {
    let (dir, store, pricing) = populated();
    let state = dir.path().join("tokens.json");
    store.save(&state).unwrap();
    let mut restarted = TokenStore::load(&state).unwrap();
    restarted.reprice(&pricing);
    assert_eq!(restarted.revision(), store.revision());
    assert_eq!(
        serde_json::to_value(restarted.events()).unwrap(),
        serde_json::to_value(store.events()).unwrap()
    );
}

#[test]
fn legacy_events_without_raw_observations_are_not_discarded() {
    let (dir, store, _) = populated();
    let mut payload = serde_json::to_value(&store).unwrap();
    payload["observations"] = json!([]);
    let state = dir.path().join("tokens.json");
    fs::write(&state, payload.to_string()).unwrap();
    let loaded = TokenStore::load(&state).unwrap();
    assert_eq!(
        serde_json::to_value(loaded.events()).unwrap(),
        payload["events"]
    );
}

#[cfg(unix)]
#[test]
fn explicitly_selected_symlink_root_reports_error_instead_of_false_empty_success() {
    let (dir, _, pricing) = populated();
    let links = TempDir::new().unwrap();
    let link = links.path().join("sessions-link");
    std::os::unix::fs::symlink(dir.path(), &link).unwrap();
    let error = TokenStore::default()
        .import_dir(&link, &pricing)
        .unwrap_err();
    assert!(error.to_string().contains("symbolic link"));
}
