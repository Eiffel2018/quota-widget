//! Progress must be observational only: no extra reconciliation or history clones.
use quota_widget::tokens::{ImportPhase, Pricing, TokenStore};
use serde_json::json;
use std::fs;
use tempfile::TempDir;

#[test]
fn progress_reports_real_counts_without_changing_import_or_idle_semantics() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    fs::create_dir(&sessions).unwrap();
    for i in 0..3 {
        let row = json!({"timestamp":100+i,"type":"token_usage_record","payload":{
            "thread_id":format!("synthetic-{i}"),"response_id":format!("response-{i}"),
            "model":"gpt-6-luna","usage":{"input_tokens":100,"cached_input_tokens":80,
            "output_tokens":10,"reasoning_output_tokens":5}}});
        fs::write(sessions.join(format!("{i}.jsonl")), format!("{row}\n")).unwrap();
    }
    let index = dir.path().join("session_index.jsonl");
    fs::write(
        &index,
        format!(
            "{}\n",
            json!({"id":"synthetic-0","thread_name":"Synthetic task"})
        ),
    )
    .unwrap();
    let pricing = Pricing::official_standard();
    let mut plain = TokenStore::default();
    let expected = plain
        .import_dir_with_metadata(&sessions, Some(&index), &pricing)
        .unwrap();
    let mut observed = TokenStore::default();
    let mut progress = vec![];
    let actual = observed
        .import_dir_with_metadata_progress(&sessions, Some(&index), &pricing, &mut |p| {
            progress.push(p)
        })
        .unwrap();
    assert_eq!(
        serde_json::to_value(&actual).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&observed).unwrap(),
        serde_json::to_value(&plain).unwrap()
    );
    assert_eq!(progress.first().unwrap().phase, ImportPhase::Discovering);
    let final_read = progress
        .iter()
        .rfind(|p| p.phase == ImportPhase::Reading)
        .unwrap();
    assert_eq!(
        (
            final_read.files_done,
            final_read.files_total,
            final_read.lines_read
        ),
        (3, 3, 3)
    );
    assert_eq!(final_read.observations, 3);
    assert!(progress.iter().any(|p| p.phase == ImportPhase::Reconciling));
    let metadata = progress.last().unwrap();
    assert_eq!(metadata.phase, ImportPhase::Metadata);
    assert_eq!(metadata.events, 3);
    let revision = observed.revision();
    progress.clear();
    let idle = observed
        .import_dir_with_metadata_progress(&sessions, Some(&index), &pricing, &mut |p| {
            progress.push(p)
        })
        .unwrap();
    assert_eq!(idle.lines_read, 0);
    assert_eq!(idle.files_unchanged, 4);
    assert_eq!(observed.revision(), revision);
    assert!(!progress.iter().any(|p| p.phase == ImportPhase::Reconciling));
}

#[test]
fn invalid_root_reports_discovery_then_returns_error_without_fake_completed_counts() {
    let dir = TempDir::new().unwrap();
    let mut stages = vec![];
    let result = TokenStore::default().import_dir_with_metadata_progress(
        dir.path().join("missing"),
        None,
        &Pricing::default(),
        &mut |p| stages.push(p),
    );
    assert!(result.is_err());
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0].phase, ImportPhase::Discovering);
    assert_eq!((stages[0].files_done, stages[0].lines_read), (0, 0));
}
