//! Explicit synthetic-only SQLite benchmarks. Never discovers a Codex home.
use quota_widget::{
    token_db::{fingerprint, TokenDb},
    tokens::{Pricing, TokenStore},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Instant,
};
fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.
}
fn timed<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let started = Instant::now();
    let value = f();
    (value, ms(started))
}
fn event_hash(store: &TokenStore) -> String {
    let mut hash = Sha256::new();
    for event in store.events() {
        hash.update(serde_json::to_vec(event).unwrap());
        hash.update(b"\n");
    }
    format!("{:x}", hash.finalize())
}
fn db_event_hash(db: &TokenDb) -> String {
    let mut hash = Sha256::new();
    db.for_each_event(None, None, |event| {
        hash.update(serde_json::to_vec(&event).unwrap());
        hash.update(b"\n");
        Ok(true)
    })
    .unwrap();
    format!("{:x}", hash.finalize())
}
fn proc_io() -> Value {
    let text = fs::read_to_string("/proc/self/io").unwrap_or_default();
    let mut result = serde_json::Map::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(':') {
            result.insert(key.into(), json!(value.trim().parse::<u64>().unwrap_or(0)));
        }
    }
    Value::Object(result)
}
fn rss() -> Value {
    let text = fs::read_to_string("/proc/self/status").unwrap_or_default();
    json!(text
        .lines()
        .filter(|line| line.starts_with("VmHWM:") || line.starts_with("VmRSS:"))
        .collect::<Vec<_>>())
}
fn save(root: &Path, mode: &str, mut result: Value) {
    result["mode"] = json!(mode);
    result["memory"] = rss();
    fs::write(
        root.join(format!("{mode}.json")),
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .unwrap();
    println!("{result}");
}
#[test]
#[ignore = "explicit 312K synthetic database benchmark; select mode and destination"]
fn synthetic_sqlite_312k() {
    let source = Path::new("/workspace/shared/quota-perf-synthetic-312k/tokens.json");
    let corpus: Value =
        serde_json::from_slice(&fs::read(source.parent().unwrap().join("corpus.json")).unwrap())
            .unwrap();
    assert_eq!(corpus["records"], 312611);
    let root = PathBuf::from(
        std::env::var("QUOTA_DB_BENCH_ROOT").expect("explicit disposable destination required"),
    );
    assert!(root.starts_with("/workspace/shared/quota-sqlite-prototype"));
    fs::create_dir_all(&root).unwrap();
    let legacy = root.join("tokens.json");
    let mode = std::env::var("QUOTA_DB_BENCH_MODE").unwrap();
    match mode.as_str() {
        "reference" => {
            let before = fingerprint(source).unwrap();
            let (store, load_ms) = timed(|| TokenStore::load(source).unwrap());
            let (summary, summary_ms) = timed(|| store.summary(None, None, &Pricing::demo()));
            let sha = event_hash(&store);
            save(
                &root,
                &mode,
                json!({"source_fingerprint":before,"source_bytes":fs::metadata(source).unwrap().len(),"load_ms":load_ms,"summary_ms":summary_ms,"events":store.events().len(),"excluded":store.events().iter().filter(|e|e.is_excluded_auto_review()).count(),"summary":summary,"event_sha256":sha}),
            );
        }
        "migrate" => {
            let before = fingerprint(source).unwrap();
            assert!(!legacy.exists(), "refusing existing benchmark JSON copy");
            fs::hard_link(source, &legacy).unwrap();
            assert_eq!(fingerprint(&legacy).unwrap().sha256, before.sha256);
            let start = Instant::now();
            let mut last = Instant::now();
            let db = TokenDb::open_or_migrate(&legacy, true, &mut |progress| {
                if last.elapsed().as_millis() > 500 {
                    println!(
                        "phase={} {}/{} elapsed_ms={:.1}",
                        progress.phase,
                        progress.done,
                        progress.total,
                        ms(start)
                    );
                    last = Instant::now();
                }
            })
            .unwrap()
            .unwrap();
            let elapsed = ms(start);
            db.checkpoint().unwrap();
            let reference: Value =
                serde_json::from_slice(&fs::read(root.join("reference.json")).unwrap()).unwrap();
            let counts = db.counts().unwrap();
            let summary = db.query_summary(None, None, &Pricing::demo()).unwrap();
            assert_eq!(
                serde_json::to_value(&summary).unwrap(),
                reference["summary"]
            );
            assert_eq!(counts.included_events, 250088);
            assert_eq!(counts.excluded_events, 62523);
            let event_sha = db_event_hash(&db);
            assert_eq!(event_sha, reference["event_sha256"].as_str().unwrap());
            assert_eq!(fingerprint(source).unwrap(), before);
            assert_eq!(fingerprint(&legacy).unwrap().sha256, before.sha256);
            save(
                &root,
                &mode,
                json!({"migration_ms":elapsed,"database_bytes":fs::metadata(db.path()).unwrap().len(),"counts":counts,"summary":summary,"all_events_sha256":event_sha,"exact_oracle_match":true,"source_unchanged":true}),
            );
        }
        "reads" => {
            let (db, open_ms) =
                timed(|| TokenDb::open_readonly(&TokenDb::database_path(&legacy)).unwrap());
            let (_, header_ms) = timed(|| db.header().unwrap());
            let mut result = json!({"open_ms":open_ms,"header_ms":header_ms});
            let (_, counts_ms) = timed(|| db.counts().unwrap());
            let (_, days_ms) = timed(|| db.days().unwrap());
            let (_, models_ms) = timed(|| db.models().unwrap());
            let (latest, latest_ms) = timed(|| db.latest_per_thread(false).unwrap());
            let (_, page_ms) =
                timed(|| db.query_events_filtered(None, None, false, 0, 100).unwrap());
            let (_, summary_ms) = timed(|| db.query_summary(None, None, &Pricing::demo()).unwrap());
            let day = db.days().unwrap()[0];
            let (_, daily_ms) = timed(|| {
                db.query_summary(Some(day), Some(day + 86400), &Pricing::demo())
                    .unwrap()
            });
            let (interval, interval_ms) = timed(|| {
                db.query_summary(Some(1791159120), Some(1791160560), &Pricing::demo())
                    .unwrap()
            });
            result["middle_interval_ms"] = json!(interval_ms);
            result["middle_interval_summary"] = serde_json::to_value(interval).unwrap();
            result["counts_ms"] = json!(counts_ms);
            result["days_ms"] = json!(days_ms);
            result["models_ms"] = json!(models_ms);
            result["latest_threads_ms"] = json!(latest_ms);
            result["latest_threads"] = json!(latest.len());
            result["latest_100_ms"] = json!(page_ms);
            result["full_summary_ms"] = json!(summary_ms);
            result["day_summary_ms"] = json!(daily_ms);
            save(&root, &mode, result);
        }
        "append" => {
            let tag = std::env::var("QUOTA_DB_BENCH_TAG").unwrap_or_else(|_| "v2".into());
            assert!(tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
            let append_copy = root.join(format!("append-copy-{tag}"));
            fs::create_dir(&append_copy).unwrap();
            let append_database = append_copy.join("tokens.sqlite3");
            let baseline = root.join("append-baseline.sqlite3");
            let baseline = if baseline.exists() {
                baseline
            } else {
                TokenDb::database_path(&legacy)
            };
            fs::copy(baseline, &append_database).unwrap();
            let mut db = TokenDb::open(&append_database, true).unwrap();
            let header = db.header().unwrap();
            let old_revision = header.revision;
            let mut store = TokenStore::from_header(header);
            let root_sessions = root.join(format!("append-synthetic-source-{tag}"));
            fs::create_dir_all(&root_sessions).unwrap();
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(root_sessions.join("single-new-response.jsonl"))
                .unwrap();
            writeln!(file,"{}",json!({"timestamp":1791161281,"type":"session_meta","payload":{"id":"synthetic-thread-00001"}})).unwrap();
            writeln!(file,"{}",json!({"timestamp":1791161281,"type":"turn_context","payload":{"turn_id":"sqlite-append-turn","model":"gpt-6.1-sol"}})).unwrap();
            writeln!(file,"{}",json!({"timestamp":1791161281,"type":"event_msg","payload":{"type":"token_usage_record","thread_id":"synthetic-thread-00001","turn_id":"sqlite-append-turn","response_id":"sqlite-append-response","usage":{"input_tokens":10000,"cached_input_tokens":8000,"output_tokens":400,"reasoning_output_tokens":100}}})).unwrap();
            file.sync_all().unwrap();
            drop(file);
            let start = Instant::now();
            let (report, import_ms) = timed(|| {
                store
                    .import_dir_hydrated(
                        &root_sessions,
                        None,
                        &Pricing::demo(),
                        &mut |id| db.load_thread(id),
                        &mut |_| {},
                    )
                    .unwrap()
            });
            let (delta, export_ms) = timed(|| store.dirty_export());
            let before_io = proc_io();
            let wal = PathBuf::from(format!("{}-wal", db.path().display()));
            let before_wal = fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
            let (stats, commit_ms) = timed(|| db.commit(&delta, old_revision).unwrap());
            let after_io = proc_io();
            let after_wal = fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
            store.mark_committed();
            let total = ms(start);
            save(
                &root,
                &mode,
                json!({"import_ms":import_ms,"export_ms":export_ms,"commit_ms":commit_ms,"total_ms":total,"wal_growth_bytes":after_wal.saturating_sub(before_wal),"io_before":before_io,"io_after":after_io,"report":report,"commit":stats,"counts":db.counts().unwrap()}),
            );
            assert_eq!(report.events_added, 1);
            assert_eq!(stats.threads_written, 1);
            assert_eq!(stats.event_changes.len(), 1);
            assert_eq!(db.counts().unwrap().included_events, 250089);
        }
        _ => panic!("unknown mode"),
    }
}
