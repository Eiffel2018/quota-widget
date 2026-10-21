//! All subprocesses use isolated synthetic homes and explicitly selected logs.
use quota_widget::{
    storage::DataLock,
    token_db::TokenDb,
    tokens::{Pricing, TokenStore},
};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

fn cli(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_quota-widget"))
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CODEX_HOME", home.join("unused-synthetic-codex"))
        .env("XDG_CONFIG_HOME", home.join("unused-synthetic-config"))
        .env("LOCALAPPDATA", home.join("unused-synthetic-local"))
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap()
}
fn success(home: &Path, args: &[&str]) -> Value {
    let out = cli(home, args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
fn append(root: &Path, id: &str, time: i64) {
    fs::create_dir_all(root).unwrap();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("synthetic.jsonl"))
        .unwrap();
    writeln!(file,"{}",json!({"timestamp":time,"type":"token_usage_record","payload":{"thread_id":"synthetic-thread","turn_id":"synthetic-turn","response_id":id,"model":"gpt-6.1-sol","usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":10,"reasoning_output_tokens":2}}})).unwrap();
}
#[test]
fn cli_uses_active_database_and_lossless_export_keeps_post_migration_events() {
    let home = TempDir::new().unwrap();
    let logs = home.path().join("logs");
    let state = home.path().join("state/tokens.json");
    append(&logs, "first", 1_700_000_001);
    let mut old = TokenStore::default();
    old.import_dir(&logs, &Pricing::demo()).unwrap();
    old.save(&state).unwrap();
    let original = fs::read(&state).unwrap();
    let logs_s = logs.to_str().unwrap();
    let state_s = state.to_str().unwrap();
    let first = success(
        home.path(),
        &["import", "--sessions", logs_s, "--state", state_s],
    );
    assert_eq!(first["events"].as_array().unwrap().len(), 1);
    assert_eq!(
        fs::read(&state).unwrap(),
        original,
        "legacy source is preserved"
    );
    append(&logs, "second", 1_700_000_002);
    let second = success(
        home.path(),
        &["import", "--sessions", logs_s, "--state", state_s],
    );
    assert_eq!(second["events"].as_array().unwrap().len(), 2);
    fs::remove_file(logs.join("synthetic.jsonl")).unwrap();
    let inspected = success(
        home.path(),
        &["inspect", "--sessions", logs_s, "--state", state_s],
    );
    assert_eq!(
        inspected["events"], second["events"],
        "retained SQLite evidence must survive source removal"
    );
    let export = home.path().join("current-rollback.json");
    success(
        home.path(),
        &[
            "export",
            "--state",
            state_s,
            "--output",
            export.to_str().unwrap(),
        ],
    );
    assert_eq!(
        TokenStore::load(&export).unwrap().events().len(),
        2,
        "rollback must include post-migration data"
    );
    assert_eq!(fs::read(&state).unwrap(), original);
}
#[test]
fn unreadable_authoritative_database_never_falls_back_or_overwrites_either_store() {
    let home = TempDir::new().unwrap();
    let logs = home.path().join("logs");
    let state = home.path().join("state/tokens.json");
    append(&logs, "first", 1_700_000_001);
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    TokenStore::default().save(&state).unwrap();
    let before = fs::read(&state).unwrap();
    let database = TokenDb::database_path(&state);
    let corrupt = b"synthetic invalid SQLite data";
    fs::write(&database, corrupt).unwrap();
    for mode in ["inspect", "import"] {
        let result = cli(
            home.path(),
            &[
                mode,
                "--sessions",
                logs.to_str().unwrap(),
                "--state",
                state.to_str().unwrap(),
            ],
        );
        assert!(!result.status.success());
        assert_eq!(fs::read(&state).unwrap(), before);
        assert_eq!(fs::read(&database).unwrap(), corrupt);
    }
}
#[test]
fn shared_gui_writer_lock_blocks_cli_import_and_export_refuses_overwrite() {
    let home = TempDir::new().unwrap();
    let logs = home.path().join("logs");
    let state = home.path().join("state/tokens.json");
    append(&logs, "first", 1_700_000_001);
    let lock = DataLock::acquire(state.parent().unwrap()).unwrap();
    let result = cli(
        home.path(),
        &[
            "import",
            "--sessions",
            logs.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
        ],
    );
    assert!(!result.status.success());
    assert!(!TokenDb::database_path(&state).exists());
    drop(lock);
    success(
        home.path(),
        &[
            "import",
            "--sessions",
            logs.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
        ],
    );
    let output = home.path().join("keep.json");
    fs::write(&output, "keep this output").unwrap();
    let result = cli(
        home.path(),
        &[
            "export",
            "--state",
            state.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ],
    );
    assert!(!result.status.success());
    assert_eq!(fs::read_to_string(output).unwrap(), "keep this output");
}
