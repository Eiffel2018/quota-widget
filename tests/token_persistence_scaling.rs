//! Synthetic-only persistence and mixed-schema scaling regressions.
use quota_widget::tokens::{Pricing, TokenStore};
use serde_json::{json, Value};
use std::{fs, io::Write, path::Path};
use tempfile::TempDir;

fn counts(input: u64) -> Value {
    json!({"input_tokens":input,"cached_input_tokens":40,
        "output_tokens":10,"reasoning_output_tokens":2})
}
fn response(thread: usize, review: bool) -> Value {
    json!({"timestamp":1_791_158_400i64 + thread as i64,
        "type":"token_usage_record","payload":{
        "thread_id":format!("synthetic-task-{thread}"),"turn_id":"synthetic-turn",
        "response_id":format!("synthetic-response-{thread}"),
        "model":if review {"codex-auto-review"} else {"gpt-6.1-sol"},
        "reasoning_effort":"xhigh","usage":counts(100)}})
}
fn anonymous(ordinal: usize) -> Value {
    json!({"timestamp":1_791_159_000i64 + ordinal as i64,
        "type":"event_msg","payload":{"type":"token_count",
        "info":{"last_token_usage":counts(100)}}})
}
fn write_rows(path: &Path, rows: impl IntoIterator<Item = Value>) {
    let mut output = std::io::BufWriter::new(fs::File::create(path).unwrap());
    for row in rows {
        writeln!(output, "{row}").unwrap();
    }
    output.flush().unwrap();
}

#[test]
fn buffered_save_is_byte_identical_roundtrips_and_replaces_existing_file() {
    let dir = TempDir::new().unwrap();
    write_rows(
        &dir.path().join("modern.jsonl"),
        (0..8).map(|i| response(i, i == 7)),
    );
    let mut store = TokenStore::default();
    store.import_dir(dir.path(), &Pricing::demo()).unwrap();
    let path = dir.path().join("tokens.json");
    fs::write(&path, b"previous synthetic cache").unwrap();
    let mut expected = serde_json::to_vec(&store).unwrap();
    expected.push(b'\n');
    store.save(&path).unwrap();
    assert_eq!(fs::read(&path).unwrap(), expected);
    let restored = TokenStore::load(&path).unwrap();
    assert_eq!(restored.events(), store.events());
    assert_eq!(
        serde_json::to_vec(&restored).unwrap(),
        serde_json::to_vec(&store).unwrap()
    );
    restored.save(&path).unwrap();
    assert_eq!(fs::read(&path).unwrap(), expected);
}

#[test]
fn failed_replacement_preserves_destination_and_removes_temporary_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tokens.json");
    fs::create_dir(&path).unwrap();
    let marker = path.join("existing-content");
    fs::write(&marker, b"keep this synthetic content").unwrap();
    assert!(TokenStore::default().save(&path).is_err());
    assert_eq!(fs::read(&marker).unwrap(), b"keep this synthetic content");
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        1,
        "failed rename must remove its temporary file"
    );

    let blocked_parent = dir.path().join("existing-file");
    fs::write(&blocked_parent, b"untouched original").unwrap();
    assert!(TokenStore::default()
        .save(blocked_parent.join("tokens.json"))
        .is_err());
    assert_eq!(fs::read(blocked_parent).unwrap(), b"untouched original");
}

#[test]
fn anonymous_legacy_marks_every_response_without_changing_dedup_counts_or_costs() {
    let dir = TempDir::new().unwrap();
    write_rows(
        &dir.path().join("modern.jsonl"),
        [
            response(0, false),
            response(1, false),
            response(2, true),
            response(0, false),
        ],
    );
    let named = json!({"timestamp":1_791_158_410i64,"type":"token_count","payload":{
        "thread_id":"synthetic-legacy-only-task","model":"gpt-6.1-sol",
        "request_id":"synthetic-legacy-request","info":{"last_token_usage":counts(200)}}});
    write_rows(&dir.path().join("named.jsonl"), [named]);
    let pricing = Pricing::demo();
    let mut store = TokenStore::default();
    let report = store.import_dir(dir.path(), &pricing).unwrap();
    assert_eq!(report.duplicate_observations, 1);
    assert_eq!(store.events().len(), 4);
    let mut expected = store.events().to_vec();
    for event in &mut expected {
        if event.source_kind.as_deref() == Some("token_usage_record") {
            event
                .partial_flags
                .push("mixed_schema_legacy_overlap_suppressed".into());
        }
    }
    write_rows(&dir.path().join("anonymous.jsonl"), (0..25).map(anonymous));
    let report = store.import_dir(dir.path(), &pricing).unwrap();
    assert_eq!(report.events_added, 0);
    // The review flag is inserted after overlap reconciliation, so compare the
    // flag set independently while keeping every other event field exact.
    let mut actual = store.events().to_vec();
    for event in actual.iter_mut().chain(expected.iter_mut()) {
        event.partial_flags.sort();
    }
    assert_eq!(actual, expected);
    assert_eq!(store.summary(None, None, &pricing).input, 400);
    assert_eq!(store.summary(None, None, &pricing).cost_usd, None);
    assert_eq!(
        store
            .events()
            .iter()
            .filter(|e| e.is_excluded_auto_review())
            .count(),
        1
    );
    let state = dir.path().join("tokens.json");
    store.save(&state).unwrap();
    let mut restored = TokenStore::load(&state).unwrap();
    let report = restored.import_dir(dir.path(), &pricing).unwrap();
    assert_eq!(report.lines_read, 0);
    assert_eq!(report.events_added, 0);
    assert_eq!(restored.events(), store.events());
}

#[test]
#[ignore = "substantial synthetic anonymous/modern mixed-schema benchmark; run in release mode"]
fn anonymous_legacy_scaling_benchmark() {
    const THREADS: usize = 3245;
    const ANONYMOUS: usize = 100_000;
    let dir = TempDir::new().unwrap();
    write_rows(
        &dir.path().join("modern.jsonl"),
        (0..THREADS).map(|i| response(i, false)),
    );
    write_rows(
        &dir.path().join("anonymous.jsonl"),
        (0..ANONYMOUS).map(anonymous),
    );
    let mut store = TokenStore::default();
    let started = std::time::Instant::now();
    store.import_dir(dir.path(), &Pricing::demo()).unwrap();
    let elapsed = started.elapsed();
    assert_eq!(store.events().len(), THREADS);
    assert!(store.events().iter().all(|e| e
        .partial_flags
        .iter()
        .any(|f| f == "mixed_schema_legacy_overlap_suppressed")));
    println!("anonymous_legacy_scaling anonymous={ANONYMOUS} threads={THREADS} observations={} elapsed_seconds={:.6} avoided_cross_product={}",
        store.observation_count(), elapsed.as_secs_f64(), ANONYMOUS * THREADS);
}
