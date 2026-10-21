//! Synthetic-only differential tests for the database thread boundary.
use quota_widget::tokens::{Pricing, StoreHeader, ThreadShard, TokenEvent, TokenStore};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};
use tempfile::TempDir;

fn counts(n: u64) -> Value {
    json!({"input_tokens":n,"cached_input_tokens":n/4,"output_tokens":n/10,"reasoning_output_tokens":n/20})
}
fn response(thread: &str, id: &str, time: i64, n: u64) -> Value {
    json!({"timestamp":time,"type":"token_usage_record","payload":{
        "thread_id":thread,"turn_id":"turn-1","response_id":id,"model":"gpt-6.1-sol","usage":counts(n)}})
}
fn legacy(time: i64, total: u64, last: u64) -> Value {
    json!({"timestamp":time,"type":"event_msg","payload":{"type":"token_count","info":{
        "total_token_usage":counts(total),"last_token_usage":counts(last)}}})
}
fn append(path: &Path, rows: &[Value]) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for row in rows {
        writeln!(file, "{row}").unwrap();
    }
}
fn merged(header: &StoreHeader, shards: &BTreeMap<String, ThreadShard>) -> Vec<TokenEvent> {
    let mut rows: Vec<_> = shards
        .values()
        .flat_map(|s| s.events.clone())
        .filter_map(|mut row| header.apply_event_policy(&mut row).then_some(row))
        .collect();
    rows.sort_by(|a, b| (a.timestamp, &a.id).cmp(&(b.timestamp, &b.id)));
    rows
}
fn roundtrip(
    root: &Path,
    full: &mut TokenStore,
    lazy: &mut TokenStore,
    db: &mut BTreeMap<String, ThreadShard>,
) -> (StoreHeader, Vec<String>) {
    let pricing = Pricing::demo();
    full.import_dir(root, &pricing).unwrap();
    let mut loaded = Vec::new();
    let report = lazy
        .import_dir_hydrated(
            root,
            None,
            &pricing,
            &mut |id| {
                loaded.push(id.to_owned());
                Ok(db.get(id).cloned())
            },
            &mut |_| {},
        )
        .unwrap();
    assert!(
        report
            .warnings
            .iter()
            .all(|w| !w.starts_with("Could not read")),
        "{:?}",
        report.warnings
    );
    let out = lazy.dirty_export();
    let retry = lazy.dirty_export();
    assert_eq!(
        serde_json::to_value(&out).unwrap(),
        serde_json::to_value(&retry).unwrap(),
        "export must survive failed commit"
    );
    for shard in out.threads {
        db.insert(shard.thread_id.clone(), shard);
    }
    assert_eq!(
        merged(&out.header, db),
        full.events(),
        "incremental differs from full normalizer"
    );
    let full_value = serde_json::to_value(&*full).unwrap();
    for field in ["observations", "contexts", "effort_contexts"] {
        let mut expected: Vec<_> = full_value[field]
            .as_array()
            .unwrap()
            .iter()
            .map(Value::to_string)
            .collect();
        let mut actual: Vec<_> = db
            .values()
            .flat_map(|shard| {
                serde_json::to_value(shard).unwrap()[field]
                    .as_array()
                    .unwrap()
                    .clone()
            })
            .map(|value| value.to_string())
            .collect();
        expected.sort();
        actual.sort();
        assert_eq!(
            actual, expected,
            "raw {field} evidence differs from full normalizer"
        );
    }
    lazy.mark_committed();
    assert_eq!(
        lazy.observation_count(),
        0,
        "committed raw evidence should be evicted"
    );
    (out.header, loaded)
}

#[test]
fn only_changed_threads_hydrate_and_global_anonymous_policy_matches_full() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let mut full = TokenStore::default();
    let mut lazy = TokenStore::from_header(full.header());
    let mut db = BTreeMap::new();
    append(
        &root.join("a.jsonl"),
        &[response("a", "r-a", 1_700_000_001, 100)],
    );
    append(
        &root.join("b.jsonl"),
        &[response("b", "r-b", 1_700_000_002, 200)],
    );
    let (_, loaded) = roundtrip(root, &mut full, &mut lazy, &mut db);
    assert_eq!(loaded, vec!["a", "b"]);
    let (_, loaded) = roundtrip(root, &mut full, &mut lazy, &mut db);
    assert!(loaded.is_empty());
    append(
        &root.join("a.jsonl"),
        &[response("a", "r-a", 1_700_000_003, 140)],
    );
    let (_, loaded) = roundtrip(root, &mut full, &mut lazy, &mut db);
    assert_eq!(loaded, vec!["a"]);
    append(
        &root.join("anonymous.jsonl"),
        &[legacy(1_700_000_004, 100, 100)],
    );
    let (header, loaded) = roundtrip(root, &mut full, &mut lazy, &mut db);
    assert_eq!(loaded.len(), 1);
    assert!(!header.anonymous_threads.is_empty());
    assert!(merged(&header, &db).iter().all(|e| e
        .partial_flags
        .iter()
        .any(|f| f == "mixed_schema_legacy_overlap_suppressed")));
    lazy = TokenStore::from_header(
        serde_json::from_slice(&serde_json::to_vec(&header).unwrap()).unwrap(),
    );
    append(
        &root.join("b.jsonl"),
        &[response("b", "r-new", 1_700_000_005, 300)],
    );
    let (_, loaded) = roundtrip(root, &mut full, &mut lazy, &mut db);
    assert_eq!(loaded, vec!["b"]);
}

#[test]
fn mixed_schema_late_legacy_model_effort_and_rotation_match_full() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let path = root.join("mixed.jsonl");
    let mut full = TokenStore::default();
    let mut lazy = TokenStore::from_header(full.header());
    let mut db = BTreeMap::new();
    append(
        &path,
        &[
            json!({"timestamp":1_700_000_000i64,"type":"session_meta","payload":{"id":"thread","model":"gpt-6.1-sol"}}),
            json!({"timestamp":1_700_000_000i64,"type":"turn_context","payload":{"turn_id":"turn-1","model":"gpt-6.1-sol","effort":"high"}}),
            legacy(1_700_000_001, 100, 100),
        ],
    );
    roundtrip(root, &mut full, &mut lazy, &mut db);
    let mut modern = response("thread", "r-1", 1_700_000_001, 100);
    modern["payload"]["thread_token_usage"] = counts(100);
    append(&path, &[modern]);
    roundtrip(root, &mut full, &mut lazy, &mut db);
    append(
        &path,
        &[
            legacy(1_700_000_003, 100, 100),
            legacy(1_700_000_004, 200, 100),
        ],
    );
    roundtrip(root, &mut full, &mut lazy, &mut db);
    let mut update = response("thread", "r-1", 1_700_000_005, 120);
    update["payload"].as_object_mut().unwrap().remove("model");
    append(
        &path,
        &[
            json!({"timestamp":1_700_000_002i64,"type":"turn_context","payload":{"turn_id":"turn-2","model":"codex-auto-review","effort":"xhigh"}}),
            update,
        ],
    );
    roundtrip(root, &mut full, &mut lazy, &mut db);
    fs::write(&path,format!("{}\n{}\n",json!({"timestamp":1_700_000_000i64,"type":"session_meta","payload":{"id":"thread","model":"gpt-6.1-sol"}}),legacy(1_700_000_006,300,100))).unwrap();
    roundtrip(root, &mut full, &mut lazy, &mut db);
}

#[test]
fn migration_shards_preserve_metadata_only_and_event_identity_order() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    append(
        &root.join("a.jsonl"),
        &[
            response("a", "same:opaque", 1_700_000_000, 100),
            response("b", "same:opaque", 1_700_000_000, 200),
            json!({"timestamp":1_700_000_000i64,"type":"session_meta","payload":{"id":"metadata-only","parent_thread_id":"parent-only","model":"codex-auto-review"}}),
        ],
    );
    let mut full = TokenStore::default();
    full.import_dir(root, &Pricing::demo()).unwrap();
    let header = full.header();
    let db: BTreeMap<_, _> = full
        .thread_shards()
        .into_iter()
        .map(|s| (s.thread_id.clone(), s))
        .collect();
    assert!(db.contains_key("metadata-only") && db.contains_key("parent-only"));
    assert_eq!(merged(&header, &db), full.events());
    let mut lazy = TokenStore::from_header(header);
    let mut db = db;
    append(
        &root.join("a.jsonl"),
        &[response("a", "earlier", 1_699_999_999, 80)],
    );
    roundtrip(root, &mut full, &mut lazy, &mut db);
}

#[test]
fn deterministic_randomized_append_update_and_late_records_match_oracle() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let mut full = TokenStore::default();
    let mut lazy = TokenStore::from_header(full.header());
    let mut db = BTreeMap::new();
    let mut seed = 0x12345678u64;
    for step in 0..60 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let thread = format!("thread-{}", (seed >> 16) % 7);
        let response_id = format!("response-{}", (seed >> 24) % 13);
        let time = 1_700_000_000 + (seed % 25) as i64;
        let n = 100 + ((seed >> 32) % 300);
        let mut row = response(&thread, &response_id, time, n);
        if step % 7 == 0 {
            row["payload"]["model"] = json!("codex-auto-review");
        }
        if step % 5 == 0 {
            row["payload"].as_object_mut().unwrap().remove("model");
        }
        if step % 3 == 0 {
            row["payload"]["reasoning_effort"] = json!("high");
        }
        append(&root.join(format!("{thread}.jsonl")), &[row]);
        let (header, _) = roundtrip(root, &mut full, &mut lazy, &mut db);
        if step % 4 == 0 {
            lazy = TokenStore::from_header(header);
        }
    }
}

#[test]
fn damaged_shard_is_rejected_before_any_partial_hydration() {
    let dir = TempDir::new().unwrap();
    append(
        &dir.path().join("a.jsonl"),
        &[response("a", "r", 1_700_000_001, 100)],
    );
    let mut full = TokenStore::default();
    full.import_dir(dir.path(), &Pricing::demo()).unwrap();
    let mut value = serde_json::to_value(full.thread_shards().remove(0)).unwrap();
    let rows = value["observations"].as_array_mut().unwrap();
    rows.push(rows[0].clone());
    let mut lazy = TokenStore::from_header(full.header());
    assert!(lazy
        .hydrate_thread(serde_json::from_value(value).unwrap())
        .is_err());
    assert_eq!(lazy.observation_count(), 0);
    assert!(lazy.events().is_empty());
    assert!(lazy.dirty_export().threads.is_empty());
}

#[test]
fn consuming_migration_and_compatibility_export_preserve_all_accounting() {
    let dir = TempDir::new().unwrap();
    append(
        &dir.path().join("a.jsonl"),
        &[
            response("a", "r-a", 1_700_000_003, 100),
            response("z", "r-z", 1_700_000_001, 200),
        ],
    );
    let mut full = TokenStore::default();
    full.import_dir(dir.path(), &Pricing::demo()).unwrap();
    let expected = full.events().to_vec();
    let header = full.header();
    let revision = header.revision;
    let shards = full.into_thread_shards();
    let mut export = TokenStore::from_header(header);
    for shard in shards {
        export.hydrate_thread(shard).unwrap();
    }
    export.finish_hydration_for_export();
    assert_eq!(export.events(), expected);
    assert_eq!(export.revision(), revision);
    let output = dir.path().join("compatibility.json");
    export.save(&output).unwrap();
    assert_eq!(TokenStore::load(output).unwrap().events(), expected);
}

#[test]
fn randomized_legacy_cumulative_and_same_millisecond_context_replay_match_oracle() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let mut full = TokenStore::default();
    let mut lazy = TokenStore::from_header(full.header());
    let mut db = BTreeMap::new();
    for i in 0..4 {
        append(
            &root.join(format!("thread-{i}.jsonl")),
            &[
                json!({"timestamp":1_700_000_000i64,"type":"session_meta","payload":{"id":format!("thread-{i}"),"model":"gpt-6.1-sol"}}),
            ],
        );
    }
    let mut seed = 0x987654321u64;
    for step in 0..40 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let i = (seed >> 16) % 4;
        let time = 1_700_000_001 + (seed % 10) as i64;
        let row = legacy(time, 100 + ((seed >> 24) % 10) * 100, 100);
        let mut rows = vec![row];
        if step % 4 == 0 {
            rows.insert(0,json!({"timestamp":time,"type":"turn_context","payload":{"turn_id":format!("turn-{step}"),"model":if step%8==0 {"codex-auto-review"} else {"gpt-6.1-sol"},"effort":"xhigh"}}));
        }
        append(&root.join(format!("thread-{i}.jsonl")), &rows);
        let (header, _) = roundtrip(root, &mut full, &mut lazy, &mut db);
        if step % 3 == 0 {
            lazy = TokenStore::from_header(header);
        }
    }
}

fn turn_context(thread: &str, turn: &str, time: i64) -> Value {
    json!({"timestamp":time,"type":"turn_context","payload":{
        "thread_id":thread,"turn_id":turn,"model":"gpt-6.1-sol"}})
}
fn complete_response(thread: &str, turn: &str, id: &str, time: i64, n: u64) -> Value {
    let mut value = response(thread, id, time, n);
    value["payload"]["turn_id"] = json!(turn);
    value["payload"]["turn_token_usage"] = counts(n);
    value
}
fn overlap_warning(event: &TokenEvent) -> bool {
    event
        .partial_flags
        .iter()
        .any(|flag| flag == "mixed_schema_legacy_overlap_suppressed")
}

#[test]
fn known_thread_ambiguity_does_not_poison_a_proven_later_turn() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("thread.jsonl");
    append(
        &path,
        &[
            json!({"timestamp":1,"type":"session_meta","payload":{"id":"thread"}}),
            legacy(2, 300, 100),
            response("thread", "old", 3, 100),
            turn_context("thread", "later", 4),
            complete_response("thread", "later", "new", 5, 200),
        ],
    );
    let mut full = TokenStore::default();
    let mut lazy = TokenStore::from_header(full.header());
    let mut db = BTreeMap::new();
    roundtrip(dir.path(), &mut full, &mut lazy, &mut db);
    let old = full
        .events()
        .iter()
        .find(|e| e.response_id.as_deref() == Some("old"))
        .unwrap();
    let new = full
        .events()
        .iter()
        .find(|e| e.response_id.as_deref() == Some("new"))
        .unwrap();
    assert!(
        overlap_warning(old),
        "affected historical coverage remains unknown"
    );
    assert!(!overlap_warning(new));
    assert_eq!(new.coverage_start_timestamp_ms, Some(4000));
    assert!(full
        .summary(None, Some(4), &Pricing::demo())
        .cost_usd
        .is_none());
    assert!(full
        .summary(Some(4), None, &Pricing::demo())
        .cost_usd
        .is_some());
}

#[test]
fn anonymous_bounds_update_without_loading_unaffected_response_shards() {
    let dir = TempDir::new().unwrap();
    append(&dir.path().join("anonymous.jsonl"), &[legacy(1, 100, 100)]);
    append(
        &dir.path().join("thread.jsonl"),
        &[
            response("thread", "old", 2, 100),
            turn_context("thread", "later", 3),
            complete_response("thread", "later", "new", 4, 200),
        ],
    );
    let mut full = TokenStore::default();
    let mut lazy = TokenStore::from_header(full.header());
    let mut db = BTreeMap::new();
    let (header, _) = roundtrip(dir.path(), &mut full, &mut lazy, &mut db);
    assert!(overlap_warning(&full.events()[0]));
    assert!(!overlap_warning(&full.events()[1]));
    assert_eq!(full.events()[1].coverage_start_timestamp_ms, Some(3000));
    lazy = TokenStore::from_header(header.clone());
    // Equal to the recorded turn start is NOT proof of independence. This only
    // changes the anonymous shard/header; previously stored events are overlaid.
    append(&dir.path().join("anonymous.jsonl"), &[legacy(3, 200, 100)]);
    let (next, loaded) = roundtrip(dir.path(), &mut full, &mut lazy, &mut db);
    assert!(!header.same_event_policy(&next));
    assert_eq!(loaded.len(), 1);
    assert!(loaded.iter().all(|id| id != "thread"));
    assert!(full.events().iter().all(overlap_warning));
    // An older header without bounds must remain conservative.
    let mut missing_bound = header;
    missing_bound.anonymous_bounds_ms.clear();
    assert!(merged(&missing_bound, &db).iter().all(overlap_warning));
}

#[test]
fn missing_conflicting_or_non_independent_turn_evidence_never_clears_overlap() {
    for case in [
        "missing-start",
        "repeated-context",
        "missing-cached",
        "missing-reasoning",
        "later-response",
        "snapshot-conflict",
        "snapshot-reset",
    ] {
        let dir = TempDir::new().unwrap();
        append(&dir.path().join("anonymous.jsonl"), &[legacy(2, 100, 100)]);
        let mut rows = Vec::new();
        if case == "repeated-context" {
            rows.push(turn_context("thread", "later", 1));
        }
        if case != "missing-start" {
            rows.push(turn_context("thread", "later", 3));
        }
        let mut record = complete_response("thread", "later", "new", 5, 200);
        if case == "missing-cached" {
            record["payload"]["turn_token_usage"]
                .as_object_mut()
                .unwrap()
                .remove("cached_input_tokens");
        }
        if case == "missing-reasoning" {
            record["payload"]["turn_token_usage"]
                .as_object_mut()
                .unwrap()
                .remove("reasoning_output_tokens");
        }
        if case == "snapshot-conflict" {
            let mut conflict = record.clone();
            conflict["payload"]["turn_token_usage"] = counts(100);
            rows.push(conflict);
        }
        if case == "snapshot-reset" {
            let mut before = record.clone();
            before["timestamp"] = json!(4);
            before["payload"]["turn_token_usage"] = counts(300);
            rows.push(before);
        }
        rows.push(record);
        if case == "later-response" {
            let mut update = response("thread", "new", 6, 200);
            update["payload"]["turn_id"] = json!("later");
            rows.push(update);
        }
        append(&dir.path().join("thread.jsonl"), &rows);
        let mut full = TokenStore::default();
        let mut lazy = TokenStore::from_header(full.header());
        let mut db = BTreeMap::new();
        roundtrip(dir.path(), &mut full, &mut lazy, &mut db);
        assert_eq!(full.events().len(), 1);
        assert!(overlap_warning(&full.events()[0]), "{case}");
        if case != "repeated-context" {
            assert_eq!(full.events()[0].coverage_start_timestamp_ms, None, "{case}");
        } else {
            assert_eq!(full.events()[0].coverage_start_timestamp_ms, Some(1000));
        }
    }
}

#[test]
fn old_cached_snapshot_missing_counter_presence_cannot_certify_a_turn() {
    let dir = TempDir::new().unwrap();
    let logs = dir.path().join("logs");
    fs::create_dir(&logs).unwrap();
    append(&logs.join("anonymous.jsonl"), &[legacy(1, 100, 100)]);
    append(
        &logs.join("thread.jsonl"),
        &[
            turn_context("thread", "later", 3),
            complete_response("thread", "later", "new", 4, 200),
        ],
    );
    let mut full = TokenStore::default();
    full.import_dir(&logs, &Pricing::demo()).unwrap();
    assert!(!overlap_warning(&full.events()[0]));
    let mut old = serde_json::to_value(full).unwrap();
    old["normalization_version"] = json!(2);
    for row in old["observations"].as_array_mut().unwrap() {
        if let Some(raw) = row["raw_turn_counts"].as_object_mut() {
            raw.remove("cached");
            raw.remove("reasoning");
        }
    }
    let saved = dir.path().join("old.json");
    fs::write(&saved, serde_json::to_vec(&old).unwrap()).unwrap();
    let mut loaded = TokenStore::load(saved).unwrap();
    assert!(overlap_warning(&loaded.events()[0]));
    assert_eq!(loaded.events()[0].coverage_start_timestamp_ms, None);
    let original_observations = loaded.observation_count();
    let report = loaded.import_dir(&logs, &Pricing::demo()).unwrap();
    assert!(
        report.lines_read > 0,
        "normalization upgrade must replay original source evidence"
    );
    assert!(report.duplicate_observations > 0);
    assert_eq!(loaded.observation_count(), original_observations);
    assert!(!overlap_warning(&loaded.events()[0]));
    assert_eq!(loaded.events()[0].coverage_start_timestamp_ms, Some(3000));
}

#[test]
fn exact_response_conflict_stays_unknown_even_when_recorded_start_is_later() {
    let dir = TempDir::new().unwrap();
    let mut conflicting_legacy = legacy(1, 100, 100);
    conflicting_legacy["payload"]["thread_id"] = json!("thread");
    conflicting_legacy["payload"]["response_id"] = json!("new");
    conflicting_legacy["payload"]["turn_id"] = json!("later");
    append(
        &dir.path().join("thread.jsonl"),
        &[
            conflicting_legacy,
            turn_context("thread", "later", 3),
            complete_response("thread", "later", "new", 4, 200),
        ],
    );
    let mut full = TokenStore::default();
    let mut lazy = TokenStore::from_header(full.header());
    let mut db = BTreeMap::new();
    roundtrip(dir.path(), &mut full, &mut lazy, &mut db);
    assert_eq!(full.events().len(), 1);
    assert_eq!(full.events()[0].coverage_start_timestamp_ms, Some(3000));
    assert!(
        overlap_warning(&full.events()[0]),
        "explicit conflicting IDs override temporal proof"
    );
}

#[test]
fn multiple_complete_responses_certify_only_exact_non_overflowing_turn_sum() {
    for overflow in [false, true] {
        let dir = TempDir::new().unwrap();
        append(&dir.path().join("anonymous.jsonl"), &[legacy(1, 100, 100)]);
        let n = if overflow { u64::MAX } else { 100 };
        let mut first = complete_response("thread", "later", "first", 4, n);
        let mut second = complete_response("thread", "later", "second", 5, n);
        let sum = if overflow { u64::MAX } else { 200 };
        first["payload"]["turn_token_usage"] = counts(n);
        second["payload"]["turn_token_usage"] = counts(sum);
        append(
            &dir.path().join("thread.jsonl"),
            &[turn_context("thread", "later", 3), first, second],
        );
        let mut full = TokenStore::default();
        let mut lazy = TokenStore::from_header(full.header());
        let mut db = BTreeMap::new();
        roundtrip(dir.path(), &mut full, &mut lazy, &mut db);
        assert_eq!(full.events().len(), 2);
        for event in full.events() {
            assert_eq!(
                event.coverage_start_timestamp_ms,
                (!overflow).then_some(3000)
            );
            assert_eq!(overlap_warning(event), overflow);
        }
    }
}

#[test]
fn append_during_hydration_is_deferred_to_the_next_finite_import() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("thread.jsonl");
    append(&path, &[response("thread", "first", 1, 100)]);
    let mut lazy = TokenStore::from_header(TokenStore::default().header());
    let first = lazy
        .import_dir_hydrated(
            dir.path(),
            None,
            &Pricing::demo(),
            &mut |_| {
                // The source file is already opened and its high-water length pinned.
                append(&path, &[response("thread", "second", 2, 200)]);
                Ok(None)
            },
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(first.lines_read, 1);
    let export = lazy.dirty_export();
    assert_eq!(export.threads[0].events.len(), 1);
    let db: BTreeMap<_, _> = export
        .threads
        .into_iter()
        .map(|shard| (shard.thread_id.clone(), shard))
        .collect();
    let mut resumed = TokenStore::from_header(export.header);
    let second = resumed
        .import_dir_hydrated(
            dir.path(),
            None,
            &Pricing::demo(),
            &mut |id| Ok(db.get(id).cloned()),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(second.lines_read, 1);
    assert_eq!(resumed.dirty_export().threads[0].events.len(), 2);
}

#[test]
fn appending_unsnapshotted_response_keeps_previously_proven_overlap_unchanged() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("thread.jsonl");
    let mut covered = response("thread", "covered", 2, 100);
    covered["payload"]["thread_token_usage"] = counts(100);
    append(
        &path,
        &[
            json!({"timestamp":0,"type":"session_meta","payload":{"id":"thread"}}),
            legacy(1, 100, 100),
            covered,
        ],
    );
    let mut full = TokenStore::default();
    let mut lazy = TokenStore::from_header(full.header());
    let mut db = BTreeMap::new();
    roundtrip(dir.path(), &mut full, &mut lazy, &mut db);
    let previous = full.events().to_vec();
    assert_eq!(previous.len(), 1);
    assert!(!overlap_warning(&previous[0]));
    append(&path, &[response("thread", "unsnapshotted", 3, 100)]);
    roundtrip(dir.path(), &mut full, &mut lazy, &mut db);
    assert_eq!(full.events().len(), 2);
    assert_eq!(
        full.events()[0],
        previous[0],
        "a new incomplete snapshot cannot destroy an already proven overlap"
    );
    assert!(full.events().iter().all(|event| !overlap_warning(event)));
    // An uncovered residual is still ambiguous when any response lacks a
    // cumulative axis. It must not be added as if it were proven disjoint.
    append(&path, &[legacy(4, 300, 200)]);
    roundtrip(dir.path(), &mut full, &mut lazy, &mut db);
    assert_eq!(full.events().len(), 2);
    assert_eq!(
        full.events().iter().map(|event| event.input).sum::<u64>(),
        200
    );
    assert!(full.events().iter().all(overlap_warning));
}

#[test]
fn malformed_diagnostics_are_bounded_actionable_and_never_echo_content() {
    const SECRET: &str = "PRIVATE-CONVERSATION-CONTENT-MUST-NOT-APPEAR";
    let dir = TempDir::new().unwrap();
    let first = dir.path().join("a-broken.jsonl");
    let second = dir.path().join("b-broken.jsonl");
    let oversized = SECRET.repeat((4 * 1024 * 1024) / SECRET.len() + 1) + "\n";
    let malformed = format!("{{\"message\":\"{SECRET}\",\"broken\": }}\n");
    let mut file = fs::File::create(&first).unwrap();
    file.write_all(oversized.as_bytes()).unwrap();
    for _ in 0..2 {
        file.write_all(malformed.as_bytes()).unwrap();
    }
    drop(file);
    fs::write(&second, malformed.repeat(5)).unwrap();
    append(&second, &[response("thread", "valid", 1, 100)]);
    let mut store = TokenStore::default();
    let report = store.import_dir(dir.path(), &Pricing::demo()).unwrap();
    assert_eq!(report.lines_read, 9);
    assert_eq!(report.malformed_lines, 8);
    assert_eq!(
        store.events().len(),
        1,
        "valid rows following errors still import"
    );
    assert_eq!(
        report.warnings.len(),
        5,
        "one shared sample cap across source files"
    );
    assert!(report.warnings[0].contains(&format!("{} at byte 0: oversized", first.display())));
    assert!(report.warnings[1].contains(&format!(
        "at byte {}: expected value at line 1 column",
        oversized.len()
    )));
    assert!(report.warnings[3].contains(&format!("{} at byte 0: expected value", second.display())));
    assert!(!serde_json::to_string(&report).unwrap().contains(SECRET));
    assert!(!serde_json::to_string(&store).unwrap().contains(SECRET));
    assert!(store
        .import_dir(dir.path(), &Pricing::demo())
        .unwrap()
        .warnings
        .is_empty());
    // A new import receives a fresh sample budget, while the persisted cursor
    // prevents old malformed rows from being counted repeatedly.
    OpenOptions::new()
        .append(true)
        .open(&second)
        .unwrap()
        .write_all(malformed.repeat(7).as_bytes())
        .unwrap();
    let next = store.import_dir(dir.path(), &Pricing::demo()).unwrap();
    assert_eq!(next.lines_read, 7);
    assert_eq!(next.malformed_lines, 7);
    assert_eq!(next.warnings.len(), 5);
    assert!(!serde_json::to_string(&next).unwrap().contains(SECRET));
}
