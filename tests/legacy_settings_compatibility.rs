//! Synthetic regressions for the pre-0.3.4 nullable bucket setting.
//! These tests never read installed settings, session logs, or user evidence.
use quota_widget::{
    background::{Command, Event, Worker},
    storage::{self, Settings},
    tokens::{ModelPrice, Pricing},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

const LEGACY_SETTINGS: &str = "{\n  \"dark\": true,\n  \"on_top\": true,\n  \"codex_path\": \"\",\n  \"position\": [123.5, 456.25],\n  \"bucket\": null,\n  \"window\": \"weekly\"\n}\n";

fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fs::read_dir(dir)
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

fn backups(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    files(dir)
        .into_iter()
        .filter(|(name, _)| {
            let name = name.to_str().unwrap();
            name.starts_with("settings.json.pre-0.3.4-") && name.ends_with(".bak")
        })
        .collect()
}

fn worker_save(path: &Path, settings: Settings) -> Vec<Event> {
    let mut worker = Worker::spawn();
    worker
        .send(Command::SaveSettings {
            path: path.to_owned(),
            settings,
        })
        .unwrap();
    worker.shutdown().unwrap();
    std::iter::from_fn(|| worker.try_recv().ok()).collect()
}

fn assert_save_error(events: Vec<Event>, expected_path: &Path, message: &str) {
    assert_eq!(events.len(), 1, "expected one save error: {events:?}");
    match &events[0] {
        Event::SaveFailed { path, error } => {
            assert_eq!(path, expected_path);
            assert!(error.contains(message), "{error}");
        }
        other => panic!("expected guarded settings save failure: {other:?}"),
    }
}

fn synthetic_settings(dir: &Path) -> Settings {
    let mut pricing = Pricing {
        assumed_model: "synthetic-default".into(),
        model_override: Some("synthetic-custom".into()),
        tier_label: "synthetic custom prices".into(),
        ..Pricing::default()
    };
    pricing.rates.insert(
        "synthetic-custom".into(),
        ModelPrice {
            uncached_input_per_million: 1.25,
            cached_input_per_million: 0.125,
            output_per_million: 4.5,
        },
    );
    Settings {
        dark: false,
        on_top: false,
        codex_path: "synthetic-codex".into(),
        position: Some([-45.5, 180.25]),
        size: [420., 512.],
        window: "primary".into(),
        history_days: 75,
        hide_unchanged: false,
        task_visible_rows: 17,
        sessions_path: dir.join("synthetic-logs").to_string_lossy().into_owned(),
        import_enabled: false,
        pricing,
        ..Settings::default()
    }
}

#[test]
fn old_null_selection_loads_all_legacy_fields_without_writing_or_backing_up() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    fs::write(&path, LEGACY_SETTINGS).unwrap();
    let original = files(dir.path());
    for _ in 0..2 {
        let loaded: Settings = storage::load_json(&path).unwrap();
        assert!(loaded.dark);
        assert!(loaded.on_top);
        assert_eq!(loaded.codex_path, "");
        assert_eq!(loaded.position, Some([123.5, 456.25]));
        assert_eq!(loaded.bucket, "codex");
        assert_eq!(loaded.window, "weekly");
        assert_eq!(loaded.size, Settings::default().size);
        assert_eq!(loaded.pricing, Pricing::official_standard());
    }
    assert_eq!(files(dir.path()), original);
    assert!(backups(dir.path()).is_empty());
}

#[test]
fn nullable_bucket_preserves_every_existing_preference_and_custom_price() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let expected = synthetic_settings(dir.path());
    let mut legacy = serde_json::to_value(&expected).unwrap();
    legacy["bucket"] = Value::Null;
    let original = serde_json::to_vec_pretty(&legacy).unwrap();
    fs::write(&path, &original).unwrap();
    let loaded: Settings = storage::load_json(&path).unwrap();
    assert_eq!(
        serde_json::to_value(&loaded).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(backups(dir.path()).is_empty());
}

#[test]
fn missing_bucket_uses_default_and_valid_strings_are_not_normalized() {
    for bucket in [
        None,
        Some("codex"),
        Some(""),
        Some("003"),
        Some("null"),
        Some(" custom / 額度 "),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut raw = json!({"dark":false,"position":[12.5,25.0]});
        if let Some(bucket) = bucket {
            raw["bucket"] = json!(bucket);
        }
        fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
        let loaded: Settings = storage::load_json(&path).unwrap();
        assert_eq!(loaded.bucket, bucket.unwrap_or("codex"));
        assert!(!loaded.dark);
        assert_eq!(loaded.position, Some([12.5, 25.0]));
        storage::save_settings(&path, &loaded).unwrap();
        assert!(backups(dir.path()).is_empty());
        assert_eq!(
            storage::load_json::<Settings>(&path).unwrap().bucket,
            loaded.bucket
        );
    }
}

#[test]
fn unrelated_bucket_types_are_rejected_and_cannot_be_overwritten() {
    for bucket in [
        json!(0),
        json!(1.5),
        json!(true),
        json!(false),
        json!([]),
        json!({}),
        json!(["codex"]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, json!({"bucket":bucket,"dark":false}).to_string()).unwrap();
        let original = files(dir.path());
        assert!(
            storage::load_json::<Settings>(&path).is_err(),
            "accepted {bucket}"
        );
        assert_save_error(
            worker_save(&path, synthetic_settings(dir.path())),
            &path,
            "Invalid existing settings; original preserved",
        );
        assert_eq!(files(dir.path()), original);
    }
}

#[test]
fn compatibility_does_not_coerce_nulls_or_wrong_types_in_other_fields() {
    for (field, invalid) in [
        ("dark", Value::Null),
        ("on_top", json!("true")),
        ("codex_path", Value::Null),
        ("position", json!("1,2")),
        ("size", Value::Null),
        ("window", Value::Null),
        ("history_days", json!("30")),
        ("hide_unchanged", Value::Null),
        ("task_visible_rows", json!([])),
        ("sessions_path", Value::Null),
        ("import_enabled", Value::Null),
        ("pricing", Value::Null),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut raw = json!({"bucket":null});
        raw[field] = invalid;
        fs::write(&path, raw.to_string()).unwrap();
        let original = files(dir.path());
        assert!(
            storage::load_json::<Settings>(&path).is_err(),
            "coerced {field}"
        );
        assert!(storage::save_settings(&path, &synthetic_settings(dir.path())).is_err());
        assert_eq!(
            files(dir.path()),
            original,
            "invalid {field} caused a write or backup"
        );
    }
}

#[test]
fn first_authorized_save_keeps_exact_backup_and_later_saves_and_restart_do_not_add_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let existing_name = PathBuf::from("settings.json.pre-0.3.4-1.bak");
    fs::write(&path, LEGACY_SETTINGS).unwrap();
    fs::write(dir.path().join(&existing_name), b"synthetic earlier backup").unwrap();
    let mut loaded: Settings = storage::load_json(&path).unwrap();
    loaded.dark = false;
    assert!(worker_save(&path, loaded.clone()).is_empty());
    let saved: Value = storage::load_json(&path).unwrap();
    assert_eq!(saved["bucket"], "codex");
    assert_eq!(saved, serde_json::to_value(&loaded).unwrap());
    let after_first = backups(dir.path());
    assert_eq!(after_first.len(), 2);
    assert_eq!(after_first[&existing_name], b"synthetic earlier backup");
    let (new_name, exact_bytes) = after_first
        .iter()
        .find(|(name, _)| **name != existing_name)
        .unwrap();
    assert_eq!(exact_bytes, LEGACY_SETTINGS.as_bytes());
    let stamp = new_name
        .to_str()
        .unwrap()
        .strip_prefix("settings.json.pre-0.3.4-")
        .unwrap()
        .strip_suffix(".bak")
        .unwrap();
    assert!(stamp.parse::<u128>().is_ok());

    // Each helper call starts a fresh worker, covering restart as well as edits.
    assert!(worker_save(&path, loaded.clone()).is_empty());
    loaded.on_top = false;
    assert!(worker_save(&path, loaded.clone()).is_empty());
    assert_eq!(backups(dir.path()), after_first);
    assert_eq!(
        serde_json::to_value(storage::load_json::<Settings>(&path).unwrap()).unwrap(),
        serde_json::to_value(&loaded).unwrap()
    );
}

#[test]
fn malformed_and_duplicate_field_settings_are_preserved_without_any_backup() {
    for damaged in [
        "",
        "{",
        "null",
        r#"{"bucket":null,"dark":"false"}"#,
        r#"{"bucket":null,"bucket":"codex"}"#,
        r#"{"bucket":null,"pricing":{"rates":null}}"#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, damaged).unwrap();
        let original = files(dir.path());
        assert!(storage::load_json::<Settings>(&path).is_err());
        assert_save_error(
            worker_save(&path, synthetic_settings(dir.path())),
            &path,
            "Invalid existing settings; original preserved",
        );
        assert_eq!(files(dir.path()), original);
    }
}

#[test]
fn non_object_settings_cannot_be_replaced_even_if_serde_accepts_a_struct_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    fs::write(&path, b"[]").unwrap();
    let original = files(dir.path());
    assert_save_error(
        worker_save(&path, synthetic_settings(dir.path())),
        &path,
        "Invalid existing settings; original preserved",
    );
    assert_eq!(files(dir.path()), original);
}

#[test]
fn unreadable_oversized_existing_settings_cannot_be_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let oversized = 256 * 1024 * 1024 + 1;
    // A sparse temporary file tests the size guard without allocating its body.
    let file = fs::File::create(&path).unwrap();
    #[cfg(windows)]
    {
        use std::{ffi::c_void, os::windows::io::AsRawHandle, ptr::null_mut};
        #[link(name = "kernel32")]
        extern "system" {
            fn DeviceIoControl(
                handle: *mut c_void,
                code: u32,
                input: *mut c_void,
                input_size: u32,
                output: *mut c_void,
                output_size: u32,
                returned: *mut u32,
                overlapped: *mut c_void,
            ) -> i32;
        }
        // Windows set_len alone allocates disk space. Mark the NTFS fixture
        // sparse first so this metadata-only guard also fits on a busy RAM disk.
        let mut returned = 0;
        assert_ne!(
            unsafe {
                DeviceIoControl(
                    file.as_raw_handle(),
                    0x000900c4,
                    null_mut(),
                    0,
                    null_mut(),
                    0,
                    &mut returned,
                    null_mut(),
                )
            },
            0,
            "{}",
            io::Error::last_os_error()
        );
    }
    file.set_len(oversized).unwrap();
    drop(file);
    let error = storage::save_settings(&path, &synthetic_settings(dir.path())).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::metadata(&path).unwrap().len(), oversized);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn every_replacement_revalidates_existing_settings_after_an_earlier_success() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let mut settings = synthetic_settings(dir.path());
    storage::save_settings(&path, &settings).unwrap();
    fs::write(&path, b"synthetic damaged settings").unwrap();
    let original = files(dir.path());
    settings.dark = !settings.dark;
    let error = storage::save_settings(&path, &settings).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(files(dir.path()), original);
}

#[test]
fn absent_settings_are_created_only_by_authorized_save_without_legacy_backup() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("synthetic-state");
    let path = nested.join("settings.json");
    assert_eq!(
        storage::load_json::<Settings>(&path).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert!(!nested.exists());
    assert!(worker_save(&path, synthetic_settings(dir.path())).is_empty());
    assert!(storage::load_json::<Settings>(&path).is_ok());
    assert!(backups(&nested).is_empty());
}

#[cfg(unix)]
#[test]
fn backup_creation_failure_leaves_existing_settings_and_other_backups_untouched() {
    let dir = tempfile::tempdir().unwrap();
    // Source fits the 255-byte component limit; the unique backup does not.
    let path = dir.path().join(format!("{}settings.json", "x".repeat(227)));
    fs::write(&path, LEGACY_SETTINGS).unwrap();
    fs::write(
        dir.path().join("settings.json.pre-0.3.4-1.bak"),
        b"synthetic prior backup",
    )
    .unwrap();
    let original = files(dir.path());
    for _ in 0..2 {
        assert_save_error(
            worker_save(&path, synthetic_settings(dir.path())),
            &path,
            "Legacy settings backup failed; original preserved",
        );
        assert_eq!(files(dir.path()), original);
    }
}

#[test]
fn replacement_failure_keeps_original_and_completed_byte_exact_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    fs::write(&path, LEGACY_SETTINGS).unwrap();
    let temporary_path = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::create_dir(&temporary_path).unwrap();
    let settings: Settings = storage::load_json(&path).unwrap();
    assert!(storage::save_settings(&path, &settings).is_err());
    assert_eq!(fs::read(&path).unwrap(), LEGACY_SETTINGS.as_bytes());
    // Remove only our synthetic obstruction so the directory can be inspected.
    fs::remove_dir(&temporary_path).unwrap();
    let first_backups = backups(dir.path());
    assert_eq!(first_backups.len(), 1);
    assert_eq!(
        first_backups.values().next().unwrap(),
        LEGACY_SETTINGS.as_bytes()
    );
    storage::save_settings(&path, &settings).unwrap();
    let all_backups = backups(dir.path());
    assert_eq!(all_backups.len(), 2);
    for (name, bytes) in first_backups {
        assert_eq!(
            all_backups[&name], bytes,
            "retry overwrote an earlier backup"
        );
    }
}
