//! End-to-end migration regressions using only synthetic temporary state.
//! No installed session logs, accounts, or real user data are accessed.
use quota_widget::{
    background::{Command, Event, Worker},
    model::HistoryEvent,
    storage::{self, Settings},
    tokens::{Pricing, TokenStore},
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const LEGACY_MINIFIED: &str = r#"{"version":1,"events":[{"session":3,"observed_at_unix":1790000020,"source_key":"codex","epoch":0,"kind":"QuotaDecrease","window_label":"weekly","before":"90","after":"89","delta":"1"}]}"#;

#[derive(Debug, Deserialize)]
struct HistoryFile {
    version: u32,
    events: Vec<HistoryEvent>,
}

struct Fixture {
    _temp: tempfile::TempDir,
    state: PathBuf,
    logs: PathBuf,
}

impl Fixture {
    fn new(history: impl AsRef<[u8]>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("synthetic-state");
        let logs = temp.path().join("synthetic-logs");
        fs::create_dir(&state).unwrap();
        fs::create_dir(&logs).unwrap();
        fs::write(state.join("history.json"), history).unwrap();
        Self {
            _temp: temp,
            state,
            logs,
        }
    }

    fn history_path(&self) -> PathBuf {
        self.state.join("history.json")
    }

    fn load(&self, worker: &Worker) {
        worker
            .send(Command::Load {
                token_path: self.state.join("tokens.json"),
                history_path: Some(self.history_path()),
                pricing: Pricing::default(),
            })
            .unwrap();
    }

    fn state_bytes(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fs::read_dir(&self.state)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                assert!(entry.file_type().unwrap().is_file());
                (
                    PathBuf::from(entry.file_name()),
                    fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }

    fn backups(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        self.state_bytes()
            .into_iter()
            .filter(|(name, _)| {
                let name = name.to_str().unwrap();
                name.starts_with("history.json.pre-0.3.4-") && name.ends_with(".bak")
            })
            .collect()
    }
}

fn next(worker: &Worker) -> Event {
    let deadline = Instant::now() + Duration::from_secs(15);
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

fn loaded(worker: &Worker) -> Arc<Vec<HistoryEvent>> {
    match next(worker) {
        Event::Loaded {
            normalized_history,
            history_error: None,
            error: None,
            ..
        } => normalized_history,
        other => panic!("expected successful history load: {other:?}"),
    }
}

fn row(session: Value, index: i64) -> Value {
    json!({
        "session": session,
        "observed_at_unix": 1_790_000_020 + index * 30,
        "source_key": "codex",
        "epoch": 0,
        "kind": "QuotaDecrease",
        "window_label": "weekly",
        "before": (90 - index).to_string(),
        "after": (89 - index).to_string(),
        "delta": "1"
    })
}

fn event(session: &str, index: i64) -> HistoryEvent {
    serde_json::from_value(row(json!(session), index)).unwrap()
}

fn merge(worker: &Worker, events: Vec<HistoryEvent>, save_path: Option<&Path>) {
    worker
        .send(Command::MergeHistory {
            events,
            save_path: save_path.map(Path::to_path_buf),
        })
        .unwrap();
    assert!(matches!(next(worker), Event::HistoryUpdated { .. }));
}

#[test]
fn exact_minified_version_one_numeric_session_loads_without_any_write() {
    // Keep this literal byte-for-byte: its numeric session is the old shape
    // that failed deserialization before the compatibility fix.
    assert!(LEGACY_MINIFIED.starts_with(r#"{"version":1,"events":[{"session":3,"#));
    let fixture = Fixture::new(LEGACY_MINIFIED);
    let original = fixture.state_bytes();
    let mut worker = Worker::spawn();
    fixture.load(&worker);
    let history = loaded(&worker);
    assert_eq!(history.len(), 1);
    assert_eq!(history[0], event("3", 0));
    assert!(!history[0].partial);
    assert_eq!(history[0].before_observed_at_unix, None);
    assert_eq!(history[0].window_key, None);
    assert_eq!(history[0].evidence, None);
    assert_eq!(history[0].reconciliation, None);
    worker.shutdown().unwrap();
    assert_eq!(fixture.state_bytes(), original, "Load must be read-only");
    assert!(fixture.backups().is_empty());
}

#[test]
fn existing_string_sessions_load_in_both_supported_versions() {
    for version in [1, 2] {
        // Leading zeroes and Unicode in string IDs must not be interpreted as
        // numeric counters or otherwise normalized away.
        let sessions = ["003", "synthetic-工作階段", "", "-3", "3.0"];
        let rows: Vec<_> = sessions
            .iter()
            .enumerate()
            .map(|(index, session)| row(json!(session), index as i64))
            .collect();
        let fixture = Fixture::new(json!({"version":version,"events":rows}).to_string());
        let original = fixture.state_bytes();
        let mut worker = Worker::spawn();
        fixture.load(&worker);
        let history = loaded(&worker);
        assert_eq!(
            history
                .iter()
                .map(|event| event.session.as_str())
                .collect::<Vec<_>>(),
            sessions
        );
        worker.shutdown().unwrap();
        assert_eq!(fixture.state_bytes(), original);
    }
}

#[test]
fn mixed_session_types_preserve_zero_large_integer_and_u64_max_exactly() {
    // Serialize integer JSON tokens directly; no intermediate f64 is involved.
    for version in [1, 2] {
        let ids = [
            json!(0_u64),
            json!(3_u64),
            json!("003"),
            json!(9_007_199_254_740_993_u64),
            json!(u64::MAX),
        ];
        let rows: Vec<_> = ids
            .into_iter()
            .enumerate()
            .map(|(index, session)| row(session, index as i64))
            .collect();
        let original = json!({"version":version,"events":rows}).to_string();
        assert!(original.contains("18446744073709551615"));
        assert!(original.contains("9007199254740993"));
        let fixture = Fixture::new(&original);
        let mut worker = Worker::spawn();
        fixture.load(&worker);
        let history = loaded(&worker);
        assert_eq!(
            history
                .iter()
                .map(|event| event.session.as_str())
                .collect::<Vec<_>>(),
            ["0", "3", "003", "9007199254740993", "18446744073709551615"]
        );
        worker.shutdown().unwrap();
        assert_eq!(
            fs::read(fixture.history_path()).unwrap(),
            original.as_bytes()
        );
        assert!(fixture.backups().is_empty());
    }
}

#[test]
fn unknown_envelope_and_event_fields_are_ignored_without_touching_source() {
    let mut legacy = row(json!(3), 0);
    legacy["unknown_legacy_field"] = json!({"nested":[true,null,17]});
    let fixture = Fixture::new(
        json!({"version":1,"events":[legacy],"unknown_metadata":{"format":"synthetic"}})
            .to_string(),
    );
    let original = fixture.state_bytes();
    let mut worker = Worker::spawn();
    fixture.load(&worker);
    assert_eq!(loaded(&worker).as_slice(), [event("3", 0)]);
    worker.shutdown().unwrap();
    assert_eq!(fixture.state_bytes(), original);
}

#[test]
fn malformed_session_types_fail_instead_of_coercing_history_ids() {
    for version in [1, 2] {
        for session in [
            "-1",
            "-9223372036854775808",
            "1.5",
            "3.0",
            "3e0",
            "true",
            "false",
            "null",
            "[]",
            "{}",
            "18446744073709551616",
        ] {
            let mut raw =
                LEGACY_MINIFIED.replacen("\"version\":1", &format!("\"version\":{version}"), 1);
            raw = raw.replacen("\"session\":3", &format!("\"session\":{session}"), 1);
            let fixture = Fixture::new(&raw);
            let original = fixture.state_bytes();
            let mut worker = Worker::spawn();
            fixture.load(&worker);
            match next(&worker) {
                Event::Loaded {
                    normalized_history,
                    history_error: Some(error),
                    error: None,
                    ..
                } => {
                    assert!(normalized_history.is_empty(), "accepted {session}");
                    assert!(error.starts_with("history.json:"), "{error}");
                }
                other => panic!("version {version}, invalid session {session}: {other:?}"),
            }
            merge(
                &worker,
                vec![event("synthetic-new", 1)],
                Some(&fixture.history_path()),
            );
            worker.shutdown().unwrap();
            assert_eq!(fixture.state_bytes(), original, "invalid session {session}");
        }
    }
}

#[test]
fn numeric_compatibility_does_not_coerce_other_history_string_fields() {
    for field in [
        "source_key",
        "window_label",
        "before",
        "after",
        "delta",
        "window_key",
    ] {
        let mut legacy = row(json!(3), 0);
        legacy[field] = json!(3);
        let fixture = Fixture::new(json!({"version":1,"events":[legacy]}).to_string());
        let original = fixture.state_bytes();
        let mut worker = Worker::spawn();
        fixture.load(&worker);
        match next(&worker) {
            Event::Loaded {
                history_error: Some(error),
                error: None,
                ..
            } => {
                assert!(error.starts_with("history.json:"), "{error}");
            }
            other => panic!("numeric field {field} was incorrectly accepted: {other:?}"),
        }
        worker.shutdown().unwrap();
        assert_eq!(fixture.state_bytes(), original);
    }
}

#[test]
fn authorized_duplicate_history_does_not_migrate_or_create_backup() {
    let fixture = Fixture::new(LEGACY_MINIFIED);
    let mut worker = Worker::spawn();
    fixture.load(&worker);
    let history = loaded(&worker);
    worker
        .send(Command::MergeHistory {
            events: history.as_ref().clone(),
            save_path: Some(fixture.history_path()),
        })
        .unwrap();
    assert!(matches!(
        next(&worker),
        Event::HistoryUpdated { normalized: None }
    ));
    worker.shutdown().unwrap();
    assert_eq!(
        fs::read(fixture.history_path()).unwrap(),
        LEGACY_MINIFIED.as_bytes()
    );
    assert!(fixture.backups().is_empty());
}

#[test]
fn first_authorized_changed_save_backs_up_exact_bytes_once_and_retains_all_raw_rows() {
    let mut hidden_raw = row(json!(u64::MAX), 2);
    hidden_raw["before"] = json!("10");
    hidden_raw["after"] = json!("11");
    // Inconsistent directions and zero changes stay as raw evidence, even
    // though both are excluded from the normalized presentation.
    let mut zero_raw = row(json!("legacy-zero"), 2);
    zero_raw["after"] = zero_raw["before"].clone();
    zero_raw["delta"] = json!("0");
    let original =
        format!(
        " {{\n \"version\": 1, \"events\": [{}, {}, {}, {}], \"unknown_old_metadata\": true\n}}\n",
        row(json!(3), 0), row(json!("legacy-text"), 1), hidden_raw, zero_raw
    );
    let old: HistoryFile = serde_json::from_str(&original).unwrap();
    let fixture = Fixture::new(&original);
    let existing_backup = PathBuf::from("history.json.pre-0.3.4-1.bak");
    fs::write(
        fixture.state.join(&existing_backup),
        b"preexisting synthetic backup",
    )
    .unwrap();
    let before_load = fixture.state_bytes();
    let mut worker = Worker::spawn();
    fixture.load(&worker);
    assert_eq!(loaded(&worker).len(), 2);
    assert_eq!(fixture.state_bytes(), before_load);

    let first_new = event("synthetic-new-one", 3);
    merge(&worker, vec![first_new.clone()], None);
    assert_eq!(
        fixture.state_bytes(),
        before_load,
        "read-only merge wrote state"
    );

    // Authorize persistence only after the in-memory merge. Even an empty next
    // batch must back up before committing the previously changed history.
    merge(&worker, Vec::new(), Some(&fixture.history_path()));
    let saved: HistoryFile = storage::load_json(&fixture.history_path()).unwrap();
    assert_eq!(saved.version, 2);
    let mut expected = old.events;
    expected.push(first_new);
    assert_eq!(saved.events, expected, "raw legacy rows must not be lost");
    let backups = fixture.backups();
    assert_eq!(
        backups.len(),
        2,
        "one new migration backup plus the existing backup"
    );
    assert_eq!(backups[&existing_backup], b"preexisting synthetic backup");
    let (backup_name, backup_bytes) = backups
        .iter()
        .find(|(name, _)| **name != existing_backup)
        .unwrap();
    assert_eq!(
        backup_bytes,
        original.as_bytes(),
        "backup must preserve exact bytes"
    );
    let stamp = backup_name
        .to_str()
        .unwrap()
        .strip_prefix("history.json.pre-0.3.4-")
        .unwrap()
        .strip_suffix(".bak")
        .unwrap();
    assert!(
        stamp.parse::<u128>().is_ok(),
        "backup suffix must be a timestamp"
    );
    let serialized: Value = storage::load_json(&fixture.history_path()).unwrap();
    assert!(serialized["events"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["session"].is_string()));
    assert_eq!(serialized["events"][2]["session"], "18446744073709551615");

    let second_new = event("synthetic-new-two", 4);
    merge(
        &worker,
        vec![second_new.clone()],
        Some(&fixture.history_path()),
    );
    expected.push(second_new);
    assert_eq!(
        fixture.backups(),
        backups,
        "later writes must not recreate or overwrite backups"
    );
    worker.shutdown().unwrap();

    let before_restart = fixture.state_bytes();
    let mut restarted = Worker::spawn();
    fixture.load(&restarted);
    let restored = loaded(&restarted);
    assert_eq!(
        restored.len(),
        4,
        "version-2 restart must restore the visible rows"
    );
    assert_eq!(
        fixture.state_bytes(),
        before_restart,
        "restart load wrote state"
    );
    let third_new = event("synthetic-new-three", 5);
    merge(
        &restarted,
        vec![third_new.clone()],
        Some(&fixture.history_path()),
    );
    expected.push(third_new);
    restarted.shutdown().unwrap();
    assert_eq!(
        storage::load_json::<HistoryFile>(&fixture.history_path())
            .unwrap()
            .events,
        expected
    );
    assert_eq!(
        fixture.backups(),
        backups,
        "version-2 restart must not create another legacy backup"
    );
}

#[cfg(unix)]
#[test]
fn backup_creation_failure_preserves_original_and_blocks_replacement() {
    let fixture = Fixture::new(LEGACY_MINIFIED);
    // A 240-byte source filename fits a normal Unix component limit, but its
    // generated backup name exceeds 255 bytes. This induces a real create-new
    // I/O failure without relying on permissions that a privileged test runner
    // could bypass, changing the clock, or touching any non-test directory.
    let long_path = fixture
        .state
        .join(format!("{}history.json", "x".repeat(228)));
    fs::rename(fixture.history_path(), &long_path).unwrap();
    let original = fixture.state_bytes();
    let mut worker = Worker::spawn();
    worker
        .send(Command::Load {
            token_path: fixture.state.join("tokens.json"),
            history_path: Some(long_path.clone()),
            pricing: Pricing::default(),
        })
        .unwrap();
    assert_eq!(loaded(&worker).as_slice(), [event("3", 0)]);
    worker
        .send(Command::MergeHistory {
            events: vec![event("synthetic-new", 1)],
            save_path: Some(long_path.clone()),
        })
        .unwrap();
    match next(&worker) {
        Event::SaveFailed { path, error } => {
            assert_eq!(path, long_path);
            assert!(
                error.contains("Legacy history backup failed; original preserved"),
                "{error}"
            );
        }
        other => panic!("expected backup failure before replacement: {other:?}"),
    }
    assert!(matches!(next(&worker), Event::HistoryUpdated { .. }));
    assert_eq!(fixture.state_bytes(), original);
    attempt_all_state_writes(&worker, &fixture);
    worker.shutdown().unwrap();
    assert_eq!(
        fixture.state_bytes(),
        original,
        "backup failure must leave all subsequent state writers blocked"
    );
}

fn attempt_all_state_writes(worker: &Worker, fixture: &Fixture) {
    fs::write(
        fixture.logs.join("synthetic.jsonl"),
        concat!(
            "{\"type\":\"token_usage_record\",\"timestamp\":1790000030,\"payload\":{",
            "\"thread_id\":\"synthetic-thread\",\"turn_id\":\"synthetic-turn\",",
            "\"response_id\":\"synthetic-response\",\"model\":\"synthetic-model\",",
            "\"usage\":{\"input_tokens\":100,\"cached_input_tokens\":0,",
            "\"output_tokens\":20,\"total_tokens\":120}}}\n"
        ),
    )
    .unwrap();
    worker
        .send(Command::Import {
            root: fixture.logs.clone(),
            metadata: None,
            pricing: Pricing::default(),
            save_path: Some(fixture.state.join("tokens.json")),
            history_save_path: Some(fixture.history_path()),
        })
        .unwrap();
    worker
        .send(Command::MergeHistory {
            events: vec![event("synthetic-new", 1)],
            save_path: Some(fixture.history_path()),
        })
        .unwrap();
    worker
        .send(Command::Reprice {
            pricing: Pricing::default(),
            save_path: Some(fixture.state.join("tokens.json")),
        })
        .unwrap();
    worker
        .send(Command::SaveSettings {
            path: fixture.state.join("settings.json"),
            settings: Settings {
                sessions_path: fixture.logs.to_string_lossy().into_owned(),
                ..Settings::default()
            },
        })
        .unwrap();
}

#[test]
fn malformed_current_history_is_preserved_and_blocks_all_state_writes() {
    for damaged in [
        r#"{"version":2,"events":[{"session":"unfinished""#,
        r#"{"version":2,"events":null}"#,
        r#"{"version":2,"events":[false]}"#,
        r#"{"version":999,"events":[]}"#,
    ] {
        let fixture = Fixture::new(damaged);
        TokenStore::default()
            .save(fixture.state.join("tokens.json"))
            .unwrap();
        let original = fixture.state_bytes();
        let mut worker = Worker::spawn();
        fixture.load(&worker);
        match next(&worker) {
            Event::Loaded {
                normalized_history,
                history_error: Some(error),
                error: None,
                ..
            } => {
                assert!(normalized_history.is_empty());
                assert!(error.starts_with("history.json:"), "{error}");
            }
            other => panic!("expected corrupt history error: {other:?}"),
        }
        attempt_all_state_writes(&worker, &fixture);
        worker.shutdown().unwrap();
        assert_eq!(
            fixture.state_bytes(),
            original,
            "corrupt history must block every state-file writer"
        );
    }
}

#[test]
fn malformed_tokens_report_their_filename_and_block_legacy_migration() {
    let fixture = Fixture::new(LEGACY_MINIFIED);
    fs::write(
        fixture.state.join("tokens.json"),
        b"synthetic damaged tokens",
    )
    .unwrap();
    let original = fixture.state_bytes();
    let mut worker = Worker::spawn();
    fixture.load(&worker);
    match next(&worker) {
        Event::Loaded {
            normalized_history,
            history_error: None,
            error: Some(error),
            ..
        } => {
            assert_eq!(normalized_history.as_slice(), [event("3", 0)]);
            assert!(error.starts_with("tokens.json:"), "{error}");
        }
        other => panic!("expected corrupt token error: {other:?}"),
    }
    attempt_all_state_writes(&worker, &fixture);
    worker.shutdown().unwrap();
    assert_eq!(fixture.state_bytes(), original);
}
