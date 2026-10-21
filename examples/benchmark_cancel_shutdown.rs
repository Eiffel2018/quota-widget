//! Own-cloud synthetic shutdown benchmark. Explicit fixture paths only.
use quota_widget::{
    background::{Command, Event, Worker},
    tokens::Pricing,
};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(
        args.len(),
        3,
        "STATE_JSON OUTPUT_JSON required (synthetic copy only)"
    );
    let state = PathBuf::from(&args[1]);
    let mut worker = Worker::spawn_database_with_progress(true);
    worker
        .send(Command::Load {
            token_path: state.clone(),
            history_path: None,
            pricing: Pricing::default(),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let progress = loop {
        if let Ok(Event::Progress(p)) = worker.try_recv() {
            break format!("{:?}", p.stage);
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    };
    let delay_ms = std::env::var("QUOTA_CANCEL_DELAY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(150);
    std::thread::sleep(Duration::from_millis(delay_ms));
    let mut latest_progress = progress;
    for event in std::iter::from_fn(|| worker.try_recv().ok()) {
        match event {
            Event::Progress(p)=>latest_progress=format!("{:?}",p.stage),
            Event::Loaded {error,..}=>panic!("load already finished before cancellation; not an active-load benchmark: {error:?}"),
            _=>{}
        }
    }
    let start = Instant::now();
    worker.request_cancel_shutdown();
    let request_seconds = start.elapsed().as_secs_f64();
    let mut polls = 0;
    while !worker.is_finished() {
        polls += 1;
        assert!(start.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(1));
    }
    worker.shutdown().unwrap();
    let result = serde_json::json!({"latest_progress_before_cancel":latest_progress,"delay_before_cancel_ms":delay_ms,"request_seconds":request_seconds,"safe_shutdown_seconds":start.elapsed().as_secs_f64(),"responsive_poll_iterations":polls,"source_bytes":std::fs::metadata(&state).unwrap().len(),"source_preserved":state.exists(),"active_database_exists":state.with_extension("sqlite3").exists()});
    std::fs::write(&args[2], serde_json::to_vec_pretty(&result).unwrap()).unwrap();
    println!("{result}");
}
