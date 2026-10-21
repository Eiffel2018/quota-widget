//! Explicit synthetic benchmark. Never discovers or accesses default user logs.
use quota_widget::{
    background::{Command, Event, Worker},
    db_view::ViewRequest,
    token_db::TokenDb,
    tokens::Pricing,
    usage_views::GroupBy,
};
use serde_json::json;
use std::{
    env, fs,
    path::PathBuf,
    time::{Duration, Instant},
};
fn receive(w: &Worker, want: impl Fn(&Event) -> bool) -> Event {
    let t = Instant::now();
    loop {
        if let Ok(e) = w.try_recv() {
            if want(&e) {
                return e;
            }
        }
        assert!(t.elapsed() < Duration::from_secs(600), "worker deadline");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn main() {
    let args: Vec<_> = env::args().collect();
    assert_eq!(
        args.len(),
        4,
        "STATE_JSON SYNTHETIC_SESSIONS OUTPUT_DIRECTORY required"
    );
    let state = PathBuf::from(&args[1]);
    let root = PathBuf::from(&args[2]);
    let output = PathBuf::from(&args[3]);
    fs::create_dir_all(&output).unwrap();
    let pricing = TokenDb::open_readonly(&state.with_extension("sqlite3"))
        .ok()
        .and_then(|db| db.header().ok())
        .and_then(|h| h.pricing)
        .unwrap_or_else(Pricing::demo);
    let mut worker = Worker::spawn_database_with_progress(true);
    let t = Instant::now();
    worker
        .send(Command::Load {
            token_path: state.clone(),
            history_path: None,
            pricing: pricing.clone(),
        })
        .unwrap();
    let loaded = receive(&worker, |e| matches!(e, Event::Loaded { .. }));
    let elapsed = t.elapsed().as_secs_f64();
    match loaded {
        Event::Loaded {
            snapshot,
            error: None,
            ..
        } => println!(
            "{}",
            json!({"stage":"cached_loaded","seconds":elapsed,"included":snapshot.database.as_ref().unwrap().total_included,"excluded":snapshot.database.as_ref().unwrap().total_excluded,"gui_events":snapshot.events.len()})
        ),
        other => panic!("{other:?}"),
    };
    let db = TokenDb::open_readonly(&state.with_extension("sqlite3")).unwrap();
    let latest = db.latest_per_thread(false).unwrap();
    let now = latest.values().map(|e| e.timestamp).max().unwrap();
    let task = latest
        .values()
        .find(|e| !e.is_excluded_auto_review())
        .unwrap()
        .session
        .clone();
    let day = (now + 28800).div_euclid(86400) * 86400 - 28800;
    drop(db);
    let group = match env::var("QUOTA_BENCH_GROUP").as_deref() {
        Ok("turn") => GroupBy::Turn,
        Ok("task") => GroupBy::Task,
        _ => GroupBy::Response,
    };
    let request = ViewRequest {
        day_start: day,
        day_end: day + 86400,
        group,
        excluded: false,
        page: 0,
        page_size: 500,
        boundary: Some(now - 3600),
        fallback_start: now - 3600,
        now: now + 2,
    };
    let t = Instant::now();
    worker.send(Command::View(request)).unwrap();
    match receive(&worker, |e| matches!(e, Event::Updated { .. })) {
        Event::Updated {
            snapshot: Some(s),
            error: None,
            ..
        } => {
            let d = s.database.as_ref().unwrap();
            println!(
                "{}",
                json!({"stage":"view_recent_day","seconds":t.elapsed().as_secs_f64(),"daily_events":d.daily.summary.event_count,"gui_events":s.events.len(),"gui_daily_groups":d.daily.rows.len(),"main_tasks":d.main_tasks.len(),"known_usd":d.main_cost.known_usd})
            );
        }
        other => panic!("{other:?}"),
    };
    let import = |root: PathBuf| Command::Import {
        root,
        metadata: None,
        pricing: pricing.clone(),
        save_path: Some(state.clone()),
        history_save_path: None,
    };
    let t = Instant::now();
    worker.send(import(root.clone())).unwrap();
    match receive(&worker, |e| {
        matches!(
            e,
            Event::Updated {
                report: Some(_),
                ..
            }
        )
    }) {
        Event::Updated {
            snapshot,
            report: Some(r),
            error: None,
            ..
        } => println!(
            "{}",
            json!({"stage":"first_source_poll","seconds":t.elapsed().as_secs_f64(),"lines_read":r.lines_read,"files_unchanged":r.files_unchanged,"snapshot_rebuilt":snapshot.is_some()})
        ),
        other => panic!("{other:?}"),
    };
    let t = Instant::now();
    worker.send(import(root)).unwrap();
    match receive(&worker, |e| {
        matches!(
            e,
            Event::Updated {
                report: Some(_),
                ..
            }
        )
    }) {
        Event::Updated {
            snapshot,
            report: Some(r),
            error: None,
            ..
        } => println!(
            "{}",
            json!({"stage":"unchanged_poll","seconds":t.elapsed().as_secs_f64(),"lines_read":r.lines_read,"files_unchanged":r.files_unchanged,"snapshot_rebuilt":snapshot.is_some()})
        ),
        other => panic!("{other:?}"),
    };
    let extra = output.join("synthetic-benchmark-input");
    fs::create_dir_all(&extra).unwrap();
    fs::write(extra.join("one.jsonl"),format!("{}\n",json!({"timestamp":now+1,"type":"token_usage_record","payload":{"thread_id":task,"turn_id":"synthetic-benchmark-turn","response_id":format!("synthetic-benchmark-{}",std::process::id()),"model":"gpt-6-luna","usage":{"input_tokens":1000,"cached_input_tokens":800,"output_tokens":100,"reasoning_output_tokens":30}}}))).unwrap();
    let t = Instant::now();
    worker.send(import(extra)).unwrap();
    match receive(&worker, |e| {
        matches!(
            e,
            Event::Updated {
                report: Some(_),
                ..
            }
        )
    }) {
        Event::Updated {
            snapshot: Some(s),
            report: Some(r),
            error: None,
            ..
        } => println!(
            "{}",
            json!({"stage":"one_response_durable_and_display","seconds":t.elapsed().as_secs_f64(),"lines_read":r.lines_read,"included":s.database.as_ref().unwrap().total_included,"gui_events":s.events.len()})
        ),
        other => panic!("{other:?}"),
    };
    worker.shutdown().unwrap();
}
