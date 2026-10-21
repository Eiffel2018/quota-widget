//! End-to-end checks use only generated logs and isolated temporary homes.
//! No installed CLI, real account, credentials, or real session directory is used.
use quota_widget::{
    accounting::{self, CostBucket},
    storage::{self, DataLock},
    tokens::TokenEvent,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

fn command(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_quota-widget"));
    cmd.current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CODEX_HOME", home.join("synthetic-codex-home"))
        .env("XDG_CONFIG_HOME", home.join("synthetic-config"))
        .env("LOCALAPPDATA", home.join("synthetic-local-data"))
        .env("PATH", home.join("empty-executable-search-path"))
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("XDG_RUNTIME_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}
fn output(cmd: &mut Command) -> Output {
    let mut child = cmd.spawn().expect("start the built test binary");
    let started = Instant::now();
    loop {
        if child.try_wait().expect("query test process").is_some() {
            return child.wait_with_output().expect("collect test output");
        }
        if started.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let result = child
                .wait_with_output()
                .expect("reap timed-out test process");
            panic!(
                "CLI exceeded 10s: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}
fn json_output(cmd: &mut Command) -> Value {
    let result = output(cmd);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).expect("machine interface emits only JSON")
}
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, current: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(current).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                result.insert(path.strip_prefix(root).unwrap().to_owned(), Vec::new());
                visit(root, &path, result);
            } else {
                result.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}
fn sessions(root: &Path) -> PathBuf {
    let path = root.join("synthetic-logs");
    fs::create_dir_all(path.join("archive/nested")).unwrap();
    fs::write(
        path.join("main.jsonl"),
        include_str!("../fixtures/token_mixed_models.jsonl"),
    )
    .unwrap();
    fs::write(
        path.join("archive/nested/copy.JSONL"),
        include_str!("../fixtures/token_duplicate_archive.jsonl"),
    )
    .unwrap();
    path
}
fn event(timestamp: i64, cost: Option<f64>) -> TokenEvent {
    TokenEvent {
        id: format!("synthetic-{timestamp}"),
        timestamp,
        model: "synthetic-test-model".into(),
        model_assumed: false,
        reasoning_effort: None,
        session: "synthetic-test-session".into(),
        response_id: None,
        turn_id: None,
        source_kind: None,
        input: 10,
        cached: 2,
        uncached: 8,
        output: 3,
        reasoning: 1,
        cost_usd: cost,
        coverage_start_timestamp_ms: None,
        partial_flags: Vec::new(),
    }
}

#[test]
fn inspect_reads_all_nested_files_without_creating_state_or_lock() {
    let home = TempDir::new().unwrap();
    let logs = sessions(home.path());
    let before = snapshot(home.path());
    let absent_state = home.path().join("not-created/state.json");
    let parsed = json_output(
        command(home.path())
            .arg("inspect")
            .arg("--sessions")
            .arg(&logs)
            .arg("--state")
            .arg(&absent_state),
    );
    assert_eq!(parsed["schema_version"], 1);
    assert_eq!(parsed["mode"], "inspect");
    assert_eq!(parsed["files_scanned"], 2);
    let events = parsed["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events
            .iter()
            .map(|e| e["input"].as_u64().unwrap())
            .sum::<u64>(),
        160
    );
    assert!(events.iter().all(|e| e["cost_usd"].is_null()));
    assert_eq!(snapshot(home.path()), before);
}

#[test]
fn import_restart_is_incremental_and_inspect_does_not_rewrite_saved_state() {
    let home = TempDir::new().unwrap();
    let logs = sessions(home.path());
    let state = home.path().join("state/tokens.json");
    let first = json_output(
        command(home.path())
            .arg("import")
            .arg("--sessions")
            .arg(&logs)
            .arg("--state")
            .arg(&state),
    );
    let second = json_output(
        command(home.path())
            .arg("import")
            .arg("--sessions")
            .arg(&logs)
            .arg("--state")
            .arg(&state),
    );
    assert_eq!(second["lines_read"], 0);
    assert_eq!(first["events"], second["events"]);
    assert_eq!(first["quota_observations"], second["quota_observations"]);
    assert_eq!(first["events"][0]["timestamp"], 1_790_845_260i64);
    let before = snapshot(home.path());
    let inspected = json_output(
        command(home.path())
            .arg("inspect")
            .arg("--sessions")
            .arg(&logs)
            .arg("--state")
            .arg(&state),
    );
    assert_eq!(second["events"], inspected["events"]);
    assert_eq!(snapshot(home.path()), before);
}

#[test]
fn cli_counts_equal_distinct_requests_once_and_does_not_persist_conversation_text() {
    let home = TempDir::new().unwrap();
    let logs = home.path().join("synthetic-logs");
    fs::create_dir(&logs).unwrap();
    let counts = json!({"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1});
    let mut rows = vec![
        json!({"timestamp":"2026-09-01T00:00:00Z","type":"session_meta","payload":{"id":"synthetic-requests"}}),
        json!({"timestamp":"2026-09-01T00:00:00Z","type":"response_item","payload":{"content":"SYNTHETIC_CONVERSATION_MUST_NOT_BE_STORED"}}),
    ];
    for (time, id) in [(1, "a"), (1, "b"), (2, "a")] {
        rows.push(
            json!({"timestamp":format!("2026-09-01T00:00:{time:02}Z"),"type":"event_msg",
            "payload":{"type":"token_count","request_id":id,"info":{"last_token_usage":counts}}}),
        );
    }
    fs::write(
        logs.join("requests.jsonl"),
        rows.iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
    let pricing = home.path().join("synthetic-prices.json");
    fs::write(&pricing, json!({"rates":{"gpt-6.1-sol":{"uncached_input_per_million":2.0,"cached_input_per_million":0.5,"output_per_million":6.0}}}).to_string()).unwrap();
    let state = home.path().join("state/tokens.json");
    let result = json_output(
        command(home.path())
            .arg("import")
            .arg("--sessions")
            .arg(&logs)
            .arg("--state")
            .arg(&state)
            .arg("--pricing")
            .arg(&pricing),
    );
    let events = result["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    for row in events {
        assert_eq!(row["input"], 10);
        assert_eq!(row["cached"], 2);
        assert_eq!(row["uncached"], 8);
        assert_eq!(row["output"], 3);
        assert_eq!(row["reasoning"], 1);
        assert_eq!(row["model"], "gpt-6.1-sol");
        assert_eq!(row["model_assumed"], true);
        assert!((row["cost_usd"].as_f64().unwrap() - 0.000035).abs() < 1e-12);
    }
    let persisted = quota_widget::token_db::TokenDb::open_readonly(
        &quota_widget::token_db::TokenDb::database_path(&state),
    )
    .unwrap()
    .export_store()
    .unwrap();
    for contents in [
        result.to_string(),
        serde_json::to_string(&persisted).unwrap(),
    ] {
        assert!(!contents.contains("SYNTHETIC_CONVERSATION_MUST_NOT_BE_STORED"));
    }
}

#[test]
fn corrupt_or_future_version_state_is_never_overwritten_by_cli_import() {
    let home = TempDir::new().unwrap();
    let logs = sessions(home.path());
    let state = home.path().join("state/tokens.json");
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    for original in ["{invalid json", "{\"version\":999}"] {
        fs::write(&state, original).unwrap();
        let inputs = snapshot(&logs);
        let result = output(
            command(home.path())
                .arg("import")
                .arg("--sessions")
                .arg(&logs)
                .arg("--state")
                .arg(&state),
        );
        assert!(!result.status.success());
        assert_eq!(fs::read_to_string(&state).unwrap(), original);
        assert_eq!(snapshot(&logs), inputs);
    }
}

#[test]
fn invalid_arguments_fail_before_opening_gui_or_touching_persistent_data() {
    let home = TempDir::new().unwrap();
    let logs = sessions(home.path());
    let logs_text = logs.to_str().unwrap();
    let cases: Vec<Vec<&str>> = vec![
        vec!["inspec"],
        vec!["--no-setting"],
        vec!["--data-dir"],
        vec!["--data-dir", "--demo"],
        vec!["inspect"],
        vec!["inspect", "--sessions"],
        vec!["import", "--sessions", logs_text],
        vec!["inspect", "--sessions", logs_text, "--unknown"],
    ];
    let before = snapshot(home.path());
    for args in cases {
        let result = output(command(home.path()).args(&args));
        assert_eq!(result.status.code(), Some(1), "args: {args:?}");
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(
            error.contains("Unknown argument")
                || error.contains("requires a path")
                || error.contains("path is required")
                || error.contains("PATH is required"),
            "expected argument validation, not GUI/platform failure; args={args:?}, stderr={error}"
        );
        assert_eq!(snapshot(home.path()), before, "args: {args:?}");
    }
}

#[test]
fn existing_writer_blocks_import_but_allows_read_only_inspection() {
    let home = TempDir::new().unwrap();
    let logs = sessions(home.path());
    let directory = home.path().join("state");
    let state = directory.join("tokens.json");
    let lock = DataLock::acquire(&directory).unwrap();
    let rejected = output(
        command(home.path())
            .arg("import")
            .arg("--sessions")
            .arg(&logs)
            .arg("--state")
            .arg(&state),
    );
    assert_eq!(rejected.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("Another Quota Widget"));
    assert!(!state.exists());
    // Windows byte-range locks also block snapshot's read of writer.lock.
    // Keep the lock held for the CLI checks, and snapshot only outside its life.
    drop(lock);
    let before = snapshot(home.path());
    let lock = DataLock::acquire(&directory).unwrap();
    let inspected = json_output(
        command(home.path())
            .arg("inspect")
            .arg("--sessions")
            .arg(&logs),
    );
    assert_eq!(inspected["events"].as_array().unwrap().len(), 2);
    drop(lock);
    assert_eq!(snapshot(home.path()), before);
}

// Invoked as an isolated child by the crash-release test. Without the explicit
// synthetic helper environment this is a no-op ordinary test.
#[test]
fn integration_lock_holder_child() {
    let Some(directory) = std::env::var_os("QUOTA_WIDGET_TEST_LOCK_DIRECTORY") else {
        return;
    };
    let directory = PathBuf::from(directory);
    let _lock = DataLock::acquire(&directory).unwrap();
    fs::write(directory.join("ready"), b"synthetic lock acquired").unwrap();
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}

#[test]
fn operating_system_releases_writer_lock_after_process_is_killed() {
    let home = TempDir::new().unwrap();
    let directory = home.path().join("state");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("integration_lock_holder_child")
        .arg("--nocapture")
        .env("QUOTA_WIDGET_TEST_LOCK_DIRECTORY", &directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !directory.join("ready").exists() && started.elapsed() < Duration::from_secs(5) {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let ready = directory.join("ready").exists();
    let blocked = ready && DataLock::acquire(&directory).is_err();
    let _ = child.kill();
    child.wait().unwrap();
    assert!(ready, "helper must acquire its synthetic lock");
    assert!(blocked, "another process must be excluded");
    assert!(
        DataLock::acquire(&directory).is_ok(),
        "a dead process must not strand the lock"
    );
}

#[test]
fn atomic_json_replacement_leaves_complete_state_and_no_temporary_files() {
    let home = TempDir::new().unwrap();
    let path = home.path().join("state.json");
    storage::save_json(&path, &json!({"generation":1,"value":"older"})).unwrap();
    let next = json!({"generation":2,"value":"replacement"});
    storage::save_json(&path, &next).unwrap();
    assert_eq!(storage::load_json::<Value>(&path).unwrap(), next);
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 1);
}

#[test]
fn accounting_has_disjoint_buckets_and_keeps_cost_after_zero_percent() {
    let events = [
        event(10, Some(100.0)),
        event(11, Some(1.0)),
        event(20, Some(2.0)),
        event(21, Some(4.0)),
    ];
    let first = accounting::bucket(&events, 10, 20, 2.0);
    let after_zero = accounting::bucket(&events, 20, 21, 0.0);
    assert_eq!(first.estimated_usd, Some(3.0));
    assert_eq!(first.events, 2);
    assert!(!first.partial);
    assert_eq!(after_zero.estimated_usd, Some(4.0));
    assert_eq!(after_zero.events, 1);
    assert_eq!(
        accounting::weighted_per_point(&[first, after_zero]),
        Some(1.5)
    );
}

#[test]
fn accounting_empty_or_unpriced_intervals_are_unknown_instead_of_free() {
    assert_eq!(accounting::sum_cost(&[], 10, 20), (None, true, 0));
    assert_eq!(
        accounting::sum_cost(&[event(1, Some(4.0))], 10, 20),
        (None, true, 0)
    );
    let events = [event(1, Some(4.0)), event(12, Some(3.0)), event(13, None)];
    assert_eq!(accounting::sum_cost(&events, 10, 20), (None, true, 2));
}

#[test]
fn weighted_per_point_uses_total_cost_over_total_points_and_excludes_unknowns() {
    let bucket = |points, cost, partial| CostBucket {
        start: 0,
        end: 1,
        percentage_points: points,
        estimated_usd: cost,
        partial,
        events: 1,
    };
    assert_eq!(
        accounting::weighted_per_point(&[
            bucket(1.0, Some(2.0), false),
            bucket(3.0, Some(12.0), false),
            bucket(100.0, Some(9999.0), true),
            bucket(100.0, None, false),
            bucket(0.0, Some(1000.0), false),
        ]),
        Some(3.5)
    );
    assert_eq!(
        accounting::weighted_per_point(&[bucket(0.0, Some(1.0), false)]),
        None
    );
}

fn quota_change(
    timestamp: i64,
    source: &str,
    key: &str,
    label: &str,
) -> quota_widget::model::HistoryEvent {
    serde_json::from_value(json!({
        "session":"synthetic-widget-observation", "observed_at_unix":timestamp,
        "source_key":source, "window_key":key, "window_label":label, "epoch":0,
        "kind":"QuotaDecrease", "before":"50", "after":"49", "delta":"1",
        "partial":false
    }))
    .unwrap()
}

#[test]
fn event_cost_keeps_other_sources_and_windows_out_of_its_boundary() {
    let events = [
        event(1, Some(99.0)),
        event(15, Some(2.0)),
        event(20, Some(3.0)),
    ];
    let previous = quota_change(10, "codex", "secondary", "1 週");
    let current = quota_change(20, "codex", "secondary", "1 週");
    let different_source = quota_change(19, "other", "secondary", "1 週");
    let different_window = quota_change(19, "codex", "primary", "5 小時");
    let same_time_other_window = quota_change(20, "codex", "primary", "5 小時");
    let all = [
        previous,
        different_source,
        different_window,
        same_time_other_window,
        current.clone(),
    ];
    assert_eq!(
        accounting::event_cost(&events, &current, &all),
        (Some(5.0), false)
    );
}

#[test]
fn event_cost_same_second_changes_are_unknown_rather_than_double_counted() {
    let events = [
        event(1, Some(99.0)),
        event(15, Some(2.0)),
        event(20, Some(3.0)),
    ];
    let first = quota_change(20, "codex", "secondary", "1 週");
    let mut second = first.clone();
    second.before = "49".into();
    second.after = "48".into();
    let all = [
        quota_change(10, "codex", "secondary", "1 週"),
        first.clone(),
        second.clone(),
    ];
    assert_eq!(accounting::event_cost(&events, &first, &all), (None, true));
    assert_eq!(accounting::event_cost(&events, &second, &all), (None, true));
}

#[test]
fn event_cost_uses_real_reset_and_marks_partial_observations() {
    let events = [
        event(1, Some(99.0)),
        event(15, Some(2.0)),
        event(20, Some(3.0)),
    ];
    let mut reset = quota_change(18, "codex", "secondary", "1 週");
    reset.kind = quota_widget::model::HistoryKind::QuotaReset;
    reset.before = "0".into();
    reset.after = "100".into();
    reset.delta = "100".into();
    let mut current = quota_change(20, "codex", "secondary", "1 週");
    current.partial = true;
    let all = [
        quota_change(10, "codex", "secondary", "1 週"),
        reset,
        current.clone(),
    ];
    assert_eq!(
        accounting::event_cost(&events, &current, &all),
        (Some(3.0), true)
    );
    assert_eq!(
        accounting::event_cost(&events, &current, &[current.clone()]),
        (None, true)
    );
}

#[test]
fn event_cost_distinguishes_window_keys_even_when_duration_labels_match() {
    let events = [
        event(1, Some(99.0)),
        event(15, Some(2.0)),
        event(20, Some(3.0)),
    ];
    let current = quota_change(20, "codex", "secondary", "1 週");
    let all = [
        quota_change(10, "codex", "secondary", "1 週"),
        quota_change(20, "codex", "primary", "1 週"),
        current.clone(),
    ];
    assert_eq!(
        accounting::event_cost(&events, &current, &all),
        (Some(5.0), false)
    );
}

#[test]
fn event_cost_ignores_legacy_zero_delta_even_if_decimal_spelling_changes() {
    let events = [
        event(1, Some(99.0)),
        event(15, Some(2.0)),
        event(20, Some(3.0)),
    ];
    let current = quota_change(20, "codex", "secondary", "1 週");
    let mut unchanged = quota_change(19, "codex", "secondary", "1 週");
    unchanged.kind = quota_widget::model::HistoryKind::QuotaReset;
    unchanged.before = "0.0".into();
    unchanged.after = "0".into();
    unchanged.delta = "0".into();
    let all = [
        quota_change(10, "codex", "secondary", "1 週"),
        unchanged,
        current.clone(),
    ];
    assert_eq!(
        accounting::event_cost(&events, &current, &all),
        (Some(5.0), false)
    );
}

#[test]
fn integration_atomic_writer_child() {
    let Some(directory) = std::env::var_os("QUOTA_WIDGET_TEST_ATOMIC_DIRECTORY") else {
        return;
    };
    let directory = PathBuf::from(directory);
    let _lock = DataLock::acquire(&directory).unwrap();
    let payload = "synthetic-persistence-payload-".repeat(150_000);
    fs::write(directory.join("writer-ready"), b"synthetic writer ready").unwrap();
    for generation in 1u64..=u64::MAX {
        storage::save_json(
            &directory.join("state.json"),
            &json!({
                "generation":generation, "payload":payload
            }),
        )
        .unwrap();
    }
}

#[test]
fn killing_writer_during_replacements_never_leaves_partial_committed_json() {
    let home = TempDir::new().unwrap();
    let path = home.path().join("state.json");
    storage::save_json(&path, &json!({"generation":0,"payload":"original"})).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("integration_atomic_writer_child")
        .arg("--nocapture")
        .env("QUOTA_WIDGET_TEST_ATOMIC_DIRECTORY", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !home.path().join("writer-ready").exists() && started.elapsed() < Duration::from_secs(5) {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let ready = home.path().join("writer-ready").exists();
    if ready {
        thread::sleep(Duration::from_millis(30));
    }
    let _ = child.kill();
    child.wait().unwrap();
    assert!(ready, "helper must begin its synthetic write loop");
    let committed: Value =
        storage::load_json(&path).expect("committed path is old or complete new JSON");
    let generation = committed["generation"].as_u64().unwrap();
    let payload = committed["payload"].as_str().unwrap();
    if generation == 0 {
        assert_eq!(payload, "original");
    } else {
        assert_eq!(
            payload.len(),
            "synthetic-persistence-payload-".len() * 150_000
        );
    }
    assert!(DataLock::acquire(home.path()).is_ok());
}

#[test]
fn history_merge_preserves_distinct_window_keys_with_equal_labels_and_values() {
    let secondary = quota_change(20, "codex", "secondary", "1 週");
    let primary = quota_change(20, "codex", "primary", "1 週");
    let merged = quota_widget::history::merge(vec![secondary.clone()], vec![primary.clone()]);
    assert_eq!(merged.len(), 2, "same-second windows remain distinct");
    let mut later_primary = primary;
    later_primary.observed_at_unix = 30;
    let merged = quota_widget::history::merge(vec![secondary], vec![later_primary]);
    assert_eq!(
        merged.len(),
        2,
        "near-time dedup must also use window identity"
    );
}

#[test]
fn history_merge_reset_in_another_window_does_not_split_this_windows_duplicate() {
    let first = quota_change(10, "codex", "secondary", "1 週");
    let copy = quota_change(20, "codex", "secondary", "1 週");
    let mut unrelated_reset = quota_change(15, "codex", "primary", "1 週");
    unrelated_reset.kind = quota_widget::model::HistoryKind::QuotaReset;
    unrelated_reset.before = "0".into();
    unrelated_reset.after = "100".into();
    unrelated_reset.delta = "100".into();
    let raw = quota_widget::history::merge(vec![first, unrelated_reset], vec![copy]);
    assert_eq!(raw.len(), 3, "raw observations remain available");
    assert_eq!(quota_widget::history::normalized(&raw).len(), 2);
}

#[test]
fn history_merge_retains_real_repeated_transition_after_this_windows_reset() {
    let first = quota_change(10, "codex", "secondary", "1 週");
    let second = quota_change(30, "codex", "secondary", "1 週");
    let mut reset = quota_change(20, "codex", "secondary", "1 週");
    reset.kind = quota_widget::model::HistoryKind::QuotaReset;
    reset.before = "49".into();
    reset.after = "50".into();
    let merged = quota_widget::history::merge(vec![first, reset], vec![second]);
    assert_eq!(merged.len(), 3);
}

#[test]
fn history_merge_does_not_silently_map_legacy_source_to_current_account() {
    let mut legacy = quota_change(10, "0:primary", "primary", "1 週");
    legacy.window_key = None;
    let current = quota_change(10, "codex", "primary", "1 週");
    let merged = quota_widget::history::merge(vec![legacy], vec![current]);
    assert_eq!(merged.len(), 2);
}

#[test]
fn historical_replay_retains_real_observations_older_than_thirty_days() {
    use quota_widget::tokens::ImportedQuotaObservation;
    let base = 1_700_000_000i64;
    let observation = |timestamp, used| ImportedQuotaObservation {
        timestamp,
        origin_session: None,
        source_id: None,
        timestamp_ms: None,
        payload: json!({"limit_id":"codex","primary":{
            "used_percent":used,"window_minutes":10080,"resets_at":base + 100 * 86400
        }}),
    };
    let history = quota_widget::history::replay(&[
        observation(base, 10),
        observation(base + 60, 11),
        observation(base + 35 * 86400, 12),
        observation(base + 35 * 86400 + 60, 13),
    ]);
    assert!(history.iter().any(|e| e.observed_at_unix == base + 60));
    assert!(history
        .iter()
        .any(|e| e.observed_at_unix == base + 35 * 86400 && e.partial));
    assert!(history
        .iter()
        .all(|e| e.kind == quota_widget::model::HistoryKind::QuotaDecrease));
}

#[test]
fn verified_price_presets_apply_per_request_long_context_boundary() {
    use quota_widget::tokens::Pricing;
    let p = Pricing::official_standard();
    let at = p.cost("gpt-6.1-sol", 272_000, 0, 100).unwrap();
    assert!((at - 0.545).abs() < 1e-12);
    let beyond = p.cost("gpt-6.1-sol", 172_001, 100_000, 100).unwrap();
    assert!((beyond - (172001.0 * 4.0 + 100000.0 * 0.20 + 100.0 * 15.0) / 1e6).abs() < 1e-12);
    let fast = Pricing::official_fast()
        .cost("gpt-6.1-sol", 172_001, 100_000, 100)
        .unwrap();
    assert!((fast - beyond * 2.0).abs() < 1e-12);
    let luna = p.cost("gpt-6-luna", 272_001, 0, 100).unwrap();
    assert!((luna - (272001.0 * 0.20 + 100.0 * 0.75) / 1e6).abs() < 1e-12);
    assert!(Pricing::default().cost("gpt-6.1-sol", 1, 0, 1).is_none());
    assert!(
        quota_widget::storage::Settings::default()
            .pricing
            .apply_long_context
    );
}

#[test]
fn quota_interval_total_is_unknown_when_response_coverage_is_incomplete() {
    let mut e = event(10, Some(0.5));
    e.partial_flags.push("response_coverage_gap".into());
    let result = quota_widget::accounting::sum_cost(&[e], 0, 20);
    assert_eq!(result, (None, true, 1));
}

#[test]
fn response_schema_cli_and_task_intervals_keep_child_tasks_separate() {
    use quota_widget::{tokens::TokenEvent, usage_views};
    let home = TempDir::new().unwrap();
    let logs = home.path().join("synthetic-responses");
    fs::create_dir(&logs).unwrap();
    fs::write(
        logs.join("responses.jsonl"),
        include_str!("../fixtures/new_schema_responses.jsonl"),
    )
    .unwrap();
    let prices = home.path().join("pricing.json");
    fs::write(&prices, include_str!("../fixtures/pricing.json")).unwrap();
    let parsed = json_output(
        command(home.path())
            .arg("inspect")
            .arg("--sessions")
            .arg(&logs)
            .arg("--pricing")
            .arg(&prices),
    );
    let events: Vec<TokenEvent> = serde_json::from_value(parsed["events"].clone()).unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events.iter().map(|e| e.input).sum::<u64>(), 350);
    assert_eq!(parsed["turn_summaries"].as_array().unwrap().len(), 2);
    assert_eq!(parsed["task_summaries"].as_array().unwrap().len(), 2);
    assert!(events.iter().all(|e| e.session != "root-session"));
    let first = events[0].timestamp;
    let last = events.last().unwrap().timestamp;
    let rows = usage_views::task_rows(&events, Some(first), last + 1);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].task_id, "task-B");
    let a = rows.iter().find(|r| r.task_id == "task-A").unwrap();
    assert_eq!(a.interval_events, 1);
    assert!((a.interval_cost_usd.unwrap() - 0.000410).abs() < 1e-12);
    assert_eq!(a.latest_cost_usd, a.interval_cost_usd);
    assert!(usage_views::task_rows(&events, Some(last), last + 1)
        .iter()
        .all(|r| r.interval_cost_usd == Some(0.)));
}
