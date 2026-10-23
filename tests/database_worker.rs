use quota_widget::{
    background::{Command, Event, Snapshot, Worker},
    db_view::ViewRequest,
    token_db::TokenDb,
    tokens::{Pricing, TokenStore},
    usage_views::GroupBy,
};
use serde_json::json;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
fn append(root: &Path, id: &str, thread: &str, t: i64, model: &str) {
    fs::create_dir_all(root).unwrap();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("synthetic.jsonl"))
        .unwrap();
    writeln!(file,"{}",json!({"timestamp":t,"type":"token_usage_record","payload":{"thread_id":thread,"turn_id":"turn","response_id":id,"model":model,"usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":10,"reasoning_output_tokens":2}}})).unwrap();
}
fn next(worker: &Worker, predicate: impl Fn(&Event) -> bool) -> Event {
    let start = Instant::now();
    loop {
        if let Ok(event) = worker.try_recv() {
            if predicate(&event) {
                return event;
            }
        }
        assert!(start.elapsed() < Duration::from_secs(15), "worker timeout");
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn load(worker: &Worker, path: &Path, pricing: &Pricing) -> Arc<Snapshot> {
    worker
        .send(Command::Load {
            token_path: path.to_owned(),
            history_path: None,
            pricing: pricing.clone(),
        })
        .unwrap();
    match next(worker, |e| matches!(e, Event::Loaded { .. })) {
        Event::Loaded {
            snapshot,
            error: None,
            ..
        } => snapshot,
        other => panic!("{other:?}"),
    }
}
fn view(worker: &Worker, t: i64, page: usize) -> Arc<Snapshot> {
    worker
        .send(Command::View(ViewRequest {
            day_start: t - 1000,
            day_end: t + 1000,
            group: GroupBy::Response,
            excluded: false,
            page,
            page_size: 500,
            boundary: Some(t - 1000),
            fallback_start: t - 1000,
            now: t + 1000,
        }))
        .unwrap();
    match next(worker, |e| matches!(e, Event::Updated { .. })) {
        Event::Updated {
            snapshot: Some(s),
            error: None,
            ..
        } => s,
        other => panic!("{other:?}"),
    }
}
#[test]
fn database_worker_migration_restart_append_and_paging_reconcile() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("logs");
    let path = temp.path().join("tokens.json");
    let t = 1_700_000_000;
    for i in 0..601 {
        append(
            &root,
            &format!("r{i}"),
            if i % 2 == 0 { "a" } else { "b" },
            t + i,
            if i % 2 == 0 {
                "gpt-6-luna"
            } else {
                "gpt-6.1-sol"
            },
        );
    }
    append(&root, "review", "review", t + 602, "codex-auto-review");
    let pricing = Pricing::demo();
    let mut full = TokenStore::default();
    full.import_dir(&root, &pricing).unwrap();
    full.save(&path).unwrap();
    drop(TokenDb::open_or_migrate(&path, true, &mut |_| {}).unwrap());
    let original = fs::read(&path).unwrap();
    let mut worker = Worker::spawn_database_with_progress(true);
    load(&worker, &path, &pricing);
    let first = view(&worker, t, 0);
    let db = first.database.as_ref().unwrap();
    assert_eq!(db.total_included, 601);
    assert_eq!(db.total_excluded, 1);
    assert_eq!(db.daily.rows.len(), 500);
    assert_eq!(db.daily.group_count, 601);
    assert_eq!(db.daily.summary.event_count, 601);
    assert_eq!(first.events.len(), 2);
    assert_eq!(db.main_tasks.len(), 2);
    assert_eq!(
        db.main_tasks
            .iter()
            .map(|r| r.interval_events)
            .sum::<usize>(),
        601
    );
    let expected = full
        .summary(Some(t - 1000), Some(t + 1000), &pricing)
        .priced_cost_usd;
    assert!((db.main_cost.known_usd - expected).abs() < 1e-10);
    let page2 = view(&worker, t, 1);
    assert_eq!(page2.database.as_ref().unwrap().daily.rows.len(), 101);
    append(&root, "new", "b", t + 603, "gpt-6-luna");
    worker
        .send(Command::Import {
            root: root.clone(),
            metadata: None,
            pricing: pricing.clone(),
            save_path: Some(path.clone()),
            history_save_path: None,
        })
        .unwrap();
    let updated = next(&worker, |e| {
        matches!(
            e,
            Event::Updated {
                report: Some(_),
                ..
            }
        )
    });
    match updated {
        Event::Updated {
            snapshot: Some(s),
            error: None,
            ..
        } => assert_eq!(s.database.as_ref().unwrap().total_included, 602),
        other => panic!("{other:?}"),
    };
    worker.shutdown().unwrap();
    assert_eq!(
        fs::read(&path).unwrap(),
        original,
        "original legacy JSON must remain byte-identical"
    );
    let mut reopened = Worker::spawn_database_with_progress(false);
    let cached = load(&reopened, &path, &pricing);
    assert_eq!(cached.database.as_ref().unwrap().total_included, 602);
    reopened.shutdown().unwrap();
    assert_eq!(
        TokenDb::open_readonly(&path.with_extension("sqlite3"))
            .unwrap()
            .query_events_all(None, None, true)
            .unwrap()
            .len(),
        603
    );
}
#[test]
fn database_worker_prices_known_partial_subtotal_and_reprice_persist() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("logs");
    let path = temp.path().join("tokens.json");
    let t = 1_700_000_000;
    append(&root, "priced", "a", t, "gpt-6-luna");
    append(&root, "unknown", "a", t + 1, "unpriced-model");
    let pricing = Pricing::demo();
    let mut full = TokenStore::default();
    full.import_dir(&root, &pricing).unwrap();
    full.save(&path).unwrap();
    drop(TokenDb::open_or_migrate(&path, true, &mut |_| {}).unwrap());
    let mut worker = Worker::spawn_database_with_progress(true);
    load(&worker, &path, &pricing);
    let before = view(&worker, t, 0);
    let db = before.database.as_ref().unwrap();
    assert!(db.main_cost.known_usd > 0.);
    assert!(db.main_cost.partial);
    assert_eq!(db.main_cost.unknown_events, 1);
    assert_eq!(db.daily.summary.unpriced_events, 1);
    let mut changed = pricing.clone();
    changed
        .rates
        .get_mut("gpt-6-luna")
        .unwrap()
        .uncached_input_per_million *= 2.;
    worker
        .send(Command::Reprice {
            pricing: changed.clone(),
            save_path: Some(path.clone()),
        })
        .unwrap();
    match next(&worker, |e| matches!(e, Event::Updated { .. })) {
        Event::Updated {
            snapshot: Some(s),
            error: None,
            ..
        } => assert!(s.database.as_ref().unwrap().main_cost.known_usd > db.main_cost.known_usd),
        other => panic!("{other:?}"),
    };
    worker.shutdown().unwrap();
    let mut reopened = Worker::spawn_database_with_progress(true);
    load(&reopened, &path, &changed);
    let after = view(&reopened, t, 0);
    assert!(after.database.as_ref().unwrap().main_cost.known_usd > db.main_cost.known_usd);
    reopened.shutdown().unwrap();
}
#[test]
fn database_worker_read_error_preserves_every_original() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("tokens.json");
    fs::write(&path, b"broken legacy").unwrap();
    fs::write(path.with_extension("sqlite3"), b"broken sqlite").unwrap();
    let mut worker = Worker::spawn_database_with_progress(true);
    worker
        .send(Command::Load {
            token_path: path.clone(),
            history_path: None,
            pricing: Pricing::default(),
        })
        .unwrap();
    assert!(matches!(
        next(&worker, |e| matches!(e, Event::Loaded { .. })),
        Event::Loaded { error: Some(_), .. }
    ));
    worker.shutdown().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"broken legacy");
    assert_eq!(
        fs::read(path.with_extension("sqlite3")).unwrap(),
        b"broken sqlite"
    );
}

#[test]
fn database_worker_group_caches_and_advance_time_reconcile_after_append() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("logs");
    let path = temp.path().join("tokens.json");
    let t = 1_700_000_000;
    append(&root, "first", "a", t, "gpt-6-luna");
    append(&root, "future", "b", t + 20, "gpt-6.1-sol");
    let pricing = Pricing::demo();
    let mut full = TokenStore::default();
    full.import_dir(&root, &pricing).unwrap();
    full.save(&path).unwrap();
    drop(TokenDb::open_or_migrate(&path, true, &mut |_| {}).unwrap());
    let mut worker = Worker::spawn_database_with_progress(true);
    load(&worker, &path, &pricing);
    let request = |group, now| ViewRequest {
        day_start: t - 1,
        day_end: t + 100,
        group,
        excluded: false,
        page: 0,
        page_size: 500,
        boundary: Some(t - 1),
        fallback_start: t - 1,
        now,
    };
    for group in [GroupBy::Task, GroupBy::Turn] {
        worker.send(Command::View(request(group, t + 1))).unwrap();
        let initial = next(&worker, |e| matches!(e, Event::Updated { .. }));
        let Event::Updated {
            snapshot: Some(s),
            error: None,
            ..
        } = initial
        else {
            panic!("{initial:?}")
        };
        assert_eq!(s.database.as_ref().unwrap().main_cost.events, 1);
        assert_eq!(
            s.database.as_ref().unwrap().daily.summary.event_count,
            if group == GroupBy::Task { 2 } else { 3 }
        );
        append(
            &root,
            if group == GroupBy::Task {
                "later-task"
            } else {
                "later-turn"
            },
            "a",
            t + 2,
            "gpt-6-luna",
        );
        worker
            .send(Command::Import {
                root: root.clone(),
                metadata: None,
                pricing: pricing.clone(),
                save_path: Some(path.clone()),
                history_save_path: None,
            })
            .unwrap();
        let Event::Updated {
            snapshot: Some(s),
            error: None,
            ..
        } = next(&worker, |e| {
            matches!(
                e,
                Event::Updated {
                    report: Some(_),
                    ..
                }
            )
        })
        else {
            panic!("import")
        };
        let d = s.database.as_ref().unwrap();
        assert_eq!(
            d.main_cost.events, 1,
            "future tail not counted before clock advance"
        );
        assert_eq!(
            d.daily.rows.iter().map(|r| r.event_count).sum::<usize>(),
            d.daily.summary.event_count
        );
        worker.send(Command::View(request(group, t + 30))).unwrap();
        let Event::Updated {
            snapshot: Some(s),
            error: None,
            ..
        } = next(&worker, |e| matches!(e, Event::Updated { .. }))
        else {
            panic!("view")
        };
        let d = s.database.as_ref().unwrap();
        assert_eq!(d.main_cost.events, d.daily.summary.event_count);
        assert!((d.main_cost.known_usd - d.daily.summary.priced_cost_usd).abs() < 1e-9);
    }
    worker.shutdown().unwrap();
}

#[test]
fn runtime_ignores_legacy_json_without_migrating_it() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("tokens.json");
    fs::write(&path, b"obsolete malformed JSON").unwrap();
    fs::write(temp.path().join("history.json"), b"obsolete history").unwrap();
    let mut worker = Worker::spawn_database_with_progress(true);
    let snapshot = load(&worker, &path, &Pricing::default());
    assert_eq!(snapshot.database.as_ref().unwrap().total_included, 0);
    worker.shutdown().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"obsolete malformed JSON");
}
