//! Synthetic-only regression and substantial-history measurements. This never
//! discovers a real Codex home or reads a user's sessions.
use quota_widget::tokens::{Pricing, TokenStore};
use serde_json::json;
use std::{
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    time::Instant,
};
use tempfile::TempDir;

fn response(thread: usize, ordinal: usize) -> String {
    json!({"timestamp": 1_791_158_400i64 + ordinal as i64,
        "type":"event_msg","payload":{"type":"token_usage_record",
        "thread_id":format!("synthetic-thread-{thread}"),
        "turn_id":format!("turn-{ordinal}"),"response_id":format!("response-{ordinal}"),
        "usage":{"input_tokens":10000,"cached_input_tokens":8000,"output_tokens":400,"reasoning_output_tokens":100}}}).to_string()
}

fn memory_kib() -> Option<u64> {
    fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmHWM:")
                .and_then(|s| s.split_whitespace().next()?.parse().ok())
        })
}

#[test]
#[ignore = "substantial synthetic history benchmark; run explicitly in release mode"]
fn synthetic_history_benchmark() {
    const FILES: usize = 200;
    const RECORDS_PER_FILE: usize = 250;
    let home = TempDir::new().unwrap();
    let root = home.path().join("synthetic-sessions");
    fs::create_dir_all(&root).unwrap();
    let mut bytes = 0u64;
    for thread in 0..FILES {
        let path = root.join(format!("{thread:05}.jsonl"));
        let mut writer = BufWriter::new(File::create(&path).unwrap());
        writeln!(writer,"{}",json!({"timestamp":1_791_158_399i64,"type":"session_meta","payload":{"id":format!("synthetic-thread-{thread}")}})).unwrap();
        for ordinal in 0..RECORDS_PER_FILE {
            writeln!(writer,"{}",json!({"timestamp":1_791_158_400i64 + ordinal as i64,"type":"turn_context","payload":{"turn_id":format!("turn-{ordinal}"),"model":"gpt-6.1-sol"}})).unwrap();
            writeln!(writer, "{}", response(thread, ordinal)).unwrap();
        }
        writer.flush().unwrap();
        bytes += fs::metadata(path).unwrap().len();
    }
    let pricing = Pricing::official_standard();
    let mut store = TokenStore::default();
    let started = Instant::now();
    let first = store.import_dir(&root, &pricing).unwrap();
    let first_ms = started.elapsed().as_secs_f64() * 1000.;
    assert_eq!(store.events().len(), FILES * RECORDS_PER_FILE);
    let mut unchanged_ms = Vec::new();
    for _ in 0..10 {
        let started = Instant::now();
        let report = store.import_dir(&root, &pricing).unwrap();
        unchanged_ms.push(started.elapsed().as_secs_f64() * 1000.);
        assert_eq!(report.lines_read, 0);
        assert_eq!(report.events_added, 0);
    }
    unchanged_ms.sort_by(f64::total_cmp);
    let mut writer = OpenOptions::new()
        .append(true)
        .open(root.join("00000.jsonl"))
        .unwrap();
    writeln!(writer, "{}", response(0, RECORDS_PER_FILE)).unwrap();
    let started = Instant::now();
    let appended = store.import_dir(&root, &pricing).unwrap();
    let append_ms = started.elapsed().as_secs_f64() * 1000.;
    assert_eq!(appended.lines_read, 1);
    assert_eq!(appended.events_added, 1);
    assert_eq!(store.events().len(), FILES * RECORDS_PER_FILE + 1);
    let started = Instant::now();
    store.reprice(&pricing);
    let reprice_ms = started.elapsed().as_secs_f64() * 1000.;
    println!("synthetic_history_benchmark files={FILES} events={} initial_lines={} bytes={bytes} initial_ms={first_ms:.3} unchanged_median_ms={:.3} unchanged_max_ms={:.3} one_append_ms={append_ms:.3} same_pricing_ms={reprice_ms:.3} peak_process_rss_kib={:?}",FILES*RECORDS_PER_FILE,first.lines_read,unchanged_ms[5],unchanged_ms[9],memory_kib());
}

fn write_rows(path: &std::path::Path, rows: &[serde_json::Value]) {
    let mut output = File::create(path).unwrap();
    for row in rows {
        writeln!(output, "{row}").unwrap();
    }
}
fn quota_row(thread: &str, millis: &str) -> serde_json::Value {
    json!({"timestamp":millis,"type":"event_msg","payload":{"type":"token_count","thread_id":thread,"rate_limits":{"primary":{"used_percent":21,"resets_at":1_791_200_000}}}})
}

#[test]
fn idle_poll_and_identical_reprice_preserve_revision_and_skip_file_content() {
    let home = TempDir::new().unwrap();
    fs::write(
        home.path().join("one.jsonl"),
        format!("{}\n", response(1, 1)),
    )
    .unwrap();
    let pricing = Pricing::official_standard();
    let mut store = TokenStore::default();
    store.import_dir(home.path(), &pricing).unwrap();
    let revision = store.revision();
    let report = store.import_dir(home.path(), &pricing).unwrap();
    assert_eq!(report.files_scanned, 1);
    assert_eq!(report.files_unchanged, 1);
    assert_eq!(report.lines_read, 0);
    assert_eq!(report.events_added, 0);
    assert_eq!(store.revision(), revision);
    store.reprice(&pricing);
    assert_eq!(store.revision(), revision);
    let state = home.path().join("state.json");
    store.save(&state).unwrap();
    let mut restarted = TokenStore::load(&state).unwrap();
    assert_eq!(
        restarted
            .import_dir(home.path(), &pricing)
            .unwrap()
            .files_unchanged,
        1
    );
    assert_eq!(restarted.revision(), revision);
}

#[test]
fn quota_provenance_keeps_distinct_origins_but_dedupes_same_origin_copy() {
    let home = TempDir::new().unwrap();
    let time = "2026-10-17T22:00:00.123Z";
    write_rows(&home.path().join("a.jsonl"), &[quota_row("a", time)]);
    write_rows(&home.path().join("copy.jsonl"), &[quota_row("a", time)]);
    write_rows(&home.path().join("b.jsonl"), &[quota_row("b", time)]);
    let mut store = TokenStore::default();
    store.import_dir(home.path(), &Pricing::default()).unwrap();
    assert_eq!(store.quota_observations().len(), 2);
    for quota in store.quota_observations() {
        assert!(quota.origin_session.is_some());
        assert_eq!(quota.timestamp_ms.unwrap().rem_euclid(1000), 123);
        assert_eq!(quota.source_id.as_ref().unwrap().len(), 16);
        assert_eq!(quota.payload["primary"]["used_percent"], 21);
    }
}

#[test]
fn recorded_turn_counter_missing_fields_are_unknown_and_fallback_is_partial() {
    let home = TempDir::new().unwrap();
    let mut first: serde_json::Value = serde_json::from_str(&response(1, 1)).unwrap();
    first["payload"]["turn_token_usage"] = json!({"input_tokens":12345});
    write_rows(&home.path().join("a.jsonl"), &[first]);
    let mut store = TokenStore::default();
    store
        .import_dir(home.path(), &Pricing::official_standard())
        .unwrap();
    let usage = &store.task_turn_usage()["synthetic-thread-1"];
    assert!(usage.recorded_snapshot);
    assert!(usage.partial);
    assert_eq!(usage.input, Some(12345));
    assert_eq!(usage.output, None);
    let mut output = OpenOptions::new()
        .append(true)
        .open(home.path().join("a.jsonl"))
        .unwrap();
    writeln!(output, "{}", response(1, 2)).unwrap();
    store
        .import_dir(home.path(), &Pricing::official_standard())
        .unwrap();
    let usage = &store.task_turn_usage()["synthetic-thread-1"];
    assert_eq!(usage.turn_id.as_deref(), Some("turn-2"));
    assert!(!usage.recorded_snapshot);
    assert!(usage.partial);
    assert_eq!(usage.input, Some(10000));
    assert_eq!(usage.output, Some(400));
}

#[test]
fn missing_cached_is_unpriced_but_missing_reasoning_keeps_inclusive_output_price() {
    let home = TempDir::new().unwrap();
    let mut missing_cached: serde_json::Value = serde_json::from_str(&response(1, 1)).unwrap();
    missing_cached["payload"]["usage"]
        .as_object_mut()
        .unwrap()
        .remove("cached_input_tokens");
    let mut missing_reasoning: serde_json::Value = serde_json::from_str(&response(1, 2)).unwrap();
    missing_reasoning["payload"]["usage"]
        .as_object_mut()
        .unwrap()
        .remove("reasoning_output_tokens");
    write_rows(
        &home.path().join("a.jsonl"),
        &[missing_cached, missing_reasoning],
    );
    let mut store = TokenStore::default();
    store
        .import_dir(home.path(), &Pricing::official_standard())
        .unwrap();
    assert!(store.events()[0]
        .partial_flags
        .iter()
        .any(|f| f == "cached_count_unavailable"));
    assert_eq!(store.events()[0].cost_usd, None);
    assert!(store.events()[1]
        .partial_flags
        .iter()
        .any(|f| f == "reasoning_count_unavailable"));
    assert!(store.events()[1].cost_usd.is_some());
}

#[test]
fn v2_replay_enriches_raw_counter_availability_and_quota_origin_without_duplication() {
    let home = TempDir::new().unwrap();
    let mut response: serde_json::Value = serde_json::from_str(&response(1, 1)).unwrap();
    response["payload"]["usage"]
        .as_object_mut()
        .unwrap()
        .remove("cached_input_tokens");
    write_rows(
        &home.path().join("a.jsonl"),
        &[
            response,
            quota_row("synthetic-thread-1", "2026-10-17T22:00:00.123Z"),
        ],
    );
    let pricing = Pricing::official_standard();
    let mut store = TokenStore::default();
    store.import_dir(home.path(), &pricing).unwrap();
    let mut old = serde_json::to_value(&store).unwrap();
    old["version"] = json!(2);
    for row in old["observations"].as_array_mut().unwrap() {
        row["flags"] = json!([]);
        row.as_object_mut().unwrap().remove("raw_turn_counts");
    }
    for row in old["quota"].as_array_mut().unwrap() {
        for key in ["origin_session", "source_id", "timestamp_ms"] {
            row.as_object_mut().unwrap().remove(key);
        }
    }
    let state = home.path().join("state.json");
    fs::write(&state, old.to_string()).unwrap();
    let mut store = TokenStore::load(&state).unwrap();
    store.import_dir(home.path(), &pricing).unwrap();
    assert_eq!(store.events().len(), 1);
    assert_eq!(store.quota_observations().len(), 1);
    assert_eq!(
        store.quota_observations()[0].origin_session.as_deref(),
        Some("synthetic-thread-1")
    );
    assert!(store.events()[0]
        .partial_flags
        .iter()
        .any(|f| f == "cached_count_unavailable"));
    assert!(!store.events()[0]
        .partial_flags
        .iter()
        .any(|f| f == "reasoning_count_unavailable"));
    assert_eq!(store.events()[0].cost_usd, None);
}

#[cfg(unix)]
#[test]
fn restored_mtime_same_length_rewrite_is_detected_by_change_time() {
    let home = TempDir::new().unwrap();
    let path = home.path().join("one.jsonl");
    fs::write(&path, format!("{}\n", response(1, 1))).unwrap();
    let mut store = TokenStore::default();
    store.import_dir(home.path(), &Pricing::default()).unwrap();
    let original_time = fs::metadata(&path).unwrap().modified().unwrap();
    // Same-length replacement with its mtime restored is still a rewrite.
    fs::write(&path, format!("{}\n", response(1, 2))).unwrap();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(original_time)
        .unwrap();
    let report = store.import_dir(home.path(), &Pricing::default()).unwrap();
    assert_eq!(report.source_resets, 1);
    assert_eq!(store.events().len(), 2);
}

#[test]
fn actual_parent_metadata_persists_without_root_session_or_fork_inference() {
    let home = TempDir::new().unwrap();
    write_rows(
        &home.path().join("parents.jsonl"),
        &[
            json!({"timestamp":100,"type":"session_meta","payload":{"id":"child-nested","session_id":"root","thread_name":"Actual child title","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent","depth":2}}}}}),
            json!({"timestamp":101,"type":"session_meta","payload":{"id":"child-direct","session_id":"root","parent_thread_id":"parent"}}),
            json!({"timestamp":102,"type":"event_msg","payload":{"type":"session_configured","thread_id":"child-configured","session_id":"root","parent_thread_id":"parent","thread_name":"Configured child title"}}),
            json!({"timestamp":103,"type":"session_meta","payload":{"id":"unrelated","session_id":"root","forked_from_id":"parent","agent_path":"/root/child","source":"cli"}}),
            json!({"timestamp":104,"type":"response_item","payload":{"type":"message","thread_id":"unrelated","parent_thread_id":"parent","content":[{"text":"I am a child of parent"}]}}),
            json!({"timestamp":105,"type":"session_meta","payload":{"id":"unsupported-wire","source":{"subagent":{"spawn":{"parent_thread_id":"parent"}}}}}),
        ],
    );
    let mut store = TokenStore::default();
    store.import_dir(home.path(), &Pricing::default()).unwrap();
    let parents = store.task_parents();
    assert_eq!(parents.len(), 3);
    for child in ["child-nested", "child-direct", "child-configured"] {
        assert_eq!(parents.get(child).map(String::as_str), Some("parent"));
    }
    assert_eq!(store.task_name("child-nested"), Some("Actual child title"));
    assert_eq!(
        store.task_name("child-configured"),
        Some("Configured child title")
    );
    let revision = store.revision();
    let state = home.path().join("state.json");
    store.save(&state).unwrap();
    let mut store = TokenStore::load(&state).unwrap();
    assert_eq!(store.task_parents(), parents);
    assert_eq!(
        store
            .import_dir(home.path(), &Pricing::default())
            .unwrap()
            .lines_read,
        0
    );
    assert_eq!(store.revision(), revision);
}

#[test]
fn conflicting_parents_remain_unknown_until_newer_authoritative_metadata() {
    let home = TempDir::new().unwrap();
    let path = home.path().join("parents.jsonl");
    write_rows(
        &path,
        &[
            json!({"timestamp":100,"type":"session_meta","payload":{"id":"child","parent_thread_id":"parent-a"}}),
            json!({"timestamp":100,"type":"session_meta","payload":{"id":"child","parent_thread_id":"parent-b"}}),
            json!({"timestamp":101,"type":"session_meta","payload":{"id":"conflicting-fields","parent_thread_id":"parent-a","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-b"}}}}}),
        ],
    );
    let mut store = TokenStore::default();
    let report = store.import_dir(home.path(), &Pricing::default()).unwrap();
    assert!(store.task_parents().is_empty());
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("Conflicting recorded parent metadata")));
    let revision = store.revision();
    let mut output = OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(output,"{}", json!({"timestamp":102,"type":"event_msg","payload":{"type":"session_configured","thread_id":"child","parent_thread_id":"parent-b"}})).unwrap();
    store.import_dir(home.path(), &Pricing::default()).unwrap();
    assert_eq!(
        store.task_parents().get("child").map(String::as_str),
        Some("parent-b")
    );
    assert!(store.revision() > revision);
    assert!(store.events().is_empty());
}

#[test]
fn recorded_parent_cycles_are_preserved_as_evidence_for_safe_graph_projection() {
    let home = TempDir::new().unwrap();
    write_rows(
        &home.path().join("parents.jsonl"),
        &[
            json!({"timestamp":100,"type":"session_meta","payload":{"id":"a","parent_thread_id":"b"}}),
            json!({"timestamp":100,"type":"session_meta","payload":{"id":"b","parent_thread_id":"a"}}),
            json!({"timestamp":100,"type":"session_meta","payload":{"id":"self","parent_thread_id":"self"}}),
        ],
    );
    let mut store = TokenStore::default();
    store.import_dir(home.path(), &Pricing::default()).unwrap();
    assert_eq!(store.task_parents().len(), 3);
    assert_eq!(store.task_parents()["a"], "b");
    assert_eq!(store.task_parents()["self"], "self");
}
