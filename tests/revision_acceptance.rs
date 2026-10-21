//! Synthetic-only acceptance and repeatable UI-data performance checks.
//! This file never reads real sessions, accounts, credentials, or conversation text.
use quota_widget::{
    accounting,
    model::{HistoryEvent, HistoryEvidence, HistoryKind, HistoryReconciliation, QuotaModel},
    tokens::{ImportedQuotaObservation, Pricing, TokenEvent, TokenStore},
    usage_views::{self, coverage_incomplete},
};
use serde_json::json;
use std::{collections::BTreeMap, fs, hint::black_box, time::Instant};
use tempfile::TempDir;

const BASE: i64 = 1_790_000_000;
fn event(index: usize, tasks: usize) -> TokenEvent {
    TokenEvent {
        id: format!("synthetic-response-{index:06}"),
        timestamp: BASE + index as i64,
        model: "gpt-6.1-sol".into(),
        model_assumed: false,
        reasoning_effort: None,
        session: format!("synthetic-task-{:04}", index % tasks),
        response_id: Some(format!("synthetic-response-{index:06}")),
        turn_id: Some(format!("synthetic-turn-{}", index / 3)),
        source_kind: Some("token_usage_record".into()),
        input: 10_000,
        cached: 8_000,
        uncached: 2_000,
        output: 400,
        reasoning: 100,
        cost_usd: Some(0.01),
        partial_flags: vec![],
    }
}
fn history_event(index: usize) -> HistoryEvent {
    HistoryEvent {
        session: "synthetic-observer".into(),
        observed_at_unix: BASE + index as i64 * 50,
        source_key: "synthetic-provider".into(),
        epoch: 0,
        kind: HistoryKind::QuotaDecrease,
        window_label: "week".into(),
        window_key: Some("secondary".into()),
        before: format!("{:.2}", 100. - index as f64 / 100.),
        after: format!("{:.2}", 100. - (index + 1) as f64 / 100.),
        delta: "0.01".into(),
        partial: false,
        before_observed_at_unix: Some(BASE + index as i64 * 50 - 50),
        evidence: None,
        reconciliation: None,
    }
}
fn synthetic_workload() -> (Vec<TokenEvent>, Vec<HistoryEvent>) {
    (
        (0..100_000).map(|i| event(i, 1_000)).collect(),
        (0..2_000).map(history_event).collect(),
    )
}
fn measure(mut f: impl FnMut(), rounds: usize) -> (f64, f64) {
    f();
    let mut ms = (0..rounds)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed().as_secs_f64() * 1000.
        })
        .collect::<Vec<_>>();
    ms.sort_by(f64::total_cmp);
    (ms[rounds / 2], ms[rounds - 1])
}
fn history_frame(events: &[TokenEvent], history: &[HistoryEvent], legacy: bool) -> f64 {
    let mut cost = 0.;
    // Mirrors the previous weighted-summary plus visible-row accounting work.
    for _ in 0..2 {
        for row in history.iter().rev().take(200) {
            cost += if legacy {
                legacy_event_cost(events, row, history)
            } else {
                accounting::event_cost(events, row, history)
            }
            .0
            .unwrap_or(0.);
        }
    }
    cost
}
fn indexed_history_frame(index: &accounting::CostIndex, history: &[HistoryEvent]) -> f64 {
    let mut cost = 0.;
    for _ in 0..2 {
        for row in history.iter().rev().take(200) {
            cost += accounting::event_cost_indexed(index, row, history)
                .0
                .unwrap_or(0.);
        }
    }
    cost
}
#[test]
#[ignore = "100k synthetic UI-data benchmark; run explicitly in release mode"]
fn synthetic_ui_data_benchmark() {
    let (events, history) = synthetic_workload();
    let end = BASE + 100_000;
    let boundary = Some(BASE + 99_950);
    let index_build = measure(
        || {
            black_box((
                usage_views::UsageIndex::new(&events),
                accounting::CostIndex::new(&events),
            ));
        },
        5,
    );
    let usage = usage_views::UsageIndex::new(&events);
    let costs = accounting::CostIndex::new(&events);
    let old_tasks = measure(
        || {
            black_box(legacy_task_rows(&events, boundary, end));
        },
        11,
    );
    let indexed_tasks = measure(
        || {
            black_box(usage.task_rows(&events, boundary, BASE, end));
        },
        11,
    );
    let old_frame = measure(
        || {
            black_box(history_frame(&events, &history, true));
        },
        5,
    );
    let batch_frame = measure(
        || {
            black_box(accounting::history_costs(&costs, &history));
        },
        11,
    );
    let indexed_frame = measure(
        || {
            black_box(indexed_history_frame(&costs, &history));
        },
        5,
    );
    // A cached repaint uses already-built rows, with no historical accounting query.
    let cached = history
        .iter()
        .rev()
        .take(200)
        .map(|row| accounting::event_cost_indexed(&costs, row, &history))
        .collect::<Vec<_>>();
    let cached_repaint = measure(
        || {
            black_box(
                cached
                    .iter()
                    .map(|(cost, _)| cost.unwrap_or(0.))
                    .sum::<f64>(),
            );
        },
        101,
    );
    println!("synthetic_ui_data_benchmark events={} tasks=1000 history={} displayed_history_rows=200 accounting_calls_per_uncached_frame=400 index_build_median_ms={:.3} index_build_max_ms={:.3} legacy_task_median_ms={:.3} legacy_task_max_ms={:.3} indexed_task_median_ms={:.3} indexed_task_max_ms={:.3} legacy_frame_median_ms={:.3} legacy_frame_max_ms={:.3} indexed_frame_median_ms={:.3} indexed_frame_max_ms={:.3} batch_all_history_median_ms={:.3} batch_all_history_max_ms={:.3} cached_accounting_repaint_median_ms={:.6} cached_accounting_repaint_max_ms={:.6}",events.len(),history.len(),index_build.0,index_build.1,old_tasks.0,old_tasks.1,indexed_tasks.0,indexed_tasks.1,old_frame.0,old_frame.1,indexed_frame.0,indexed_frame.1,batch_frame.0,batch_frame.1,cached_repaint.0,cached_repaint.1);
    assert_eq!(usage.task_rows(&events, boundary, BASE, end).len(), 1_000);
    assert!((indexed_history_frame(&costs, &history) - 200.).abs() < 1e-8);
}

fn quota_payload(remaining: f64, reset_at: i64) -> serde_json::Value {
    json!({"accountId":"synthetic-account", "rateLimits":{"limitId":"codex", "primary":{
        "usedPercent":100.-remaining,"windowDurationMins":300,"resetsAt":reset_at}}})
}
fn observation(time: i64, remaining: f64, origin: &str, reset_at: i64) -> ImportedQuotaObservation {
    ImportedQuotaObservation {
        timestamp: time,
        payload: quota_payload(remaining, reset_at),
        origin_session: Some(origin.into()),
        source_id: Some(format!("synthetic-source-{origin}")),
        timestamp_ms: Some(time * 1000),
    }
}
#[test]
fn stale_concurrent_snapshots_never_create_repeated_cycles_or_erase_raw_evidence() {
    let reset = BASE + 10_000;
    let observations = vec![
        observation(BASE, 3., "a", reset),
        observation(BASE + 10, 2., "a", reset),
        observation(BASE + 11, 3., "b", reset),
        observation(BASE + 12, 2., "a", reset),
        observation(BASE + 13, 3., "b", reset),
        observation(BASE + 14, 2., "a", reset),
    ];
    let original = serde_json::to_string(&observations).unwrap();
    let raw = quota_widget::history::replay(&observations);
    let raw_before = serde_json::to_string(&raw).unwrap();
    assert_eq!(
        raw.len(),
        5,
        "retain every distinct observed transition as evidence"
    );
    let visible = quota_widget::history::normalized(&raw);
    let confirmed: Vec<_> = visible
        .iter()
        .filter(|e| quota_widget::history::is_confirmed_quota_change(e))
        .collect();
    assert_eq!(confirmed.len(), 1);
    assert_eq!(confirmed[0].kind, HistoryKind::QuotaDecrease);
    assert_eq!((&*confirmed[0].before, &*confirmed[0].after), ("3", "2"));
    assert_eq!(
        visible
            .iter()
            .filter(|e| e.reconciliation == Some(HistoryReconciliation::UncertainRecovery))
            .count(),
        1
    );
    assert_eq!(serde_json::to_string(&raw).unwrap(), raw_before);
    assert_eq!(serde_json::to_string(&observations).unwrap(), original);
    let merged = quota_widget::history::merge(raw.clone(), raw.clone());
    assert_eq!(merged, raw, "re-importing identical evidence is idempotent");
    assert_eq!(quota_widget::history::normalized(&merged), visible);
    let mut reordered = observations.clone();
    reordered.reverse();
    assert_eq!(
        quota_widget::history::normalized(&quota_widget::history::replay(&reordered)),
        visible
    );
}
#[test]
fn imported_rollover_and_live_recovery_remain_real_boundaries() {
    let observations = vec![
        observation(BASE, 3., "a", BASE + 20),
        observation(BASE + 10, 2., "a", BASE + 20),
        observation(BASE + 21, 3., "b", BASE + 1000),
        observation(BASE + 22, 2., "b", BASE + 1000),
    ];
    let visible = quota_widget::history::normalized(&quota_widget::history::replay(&observations));
    assert_eq!(
        visible
            .iter()
            .filter(|e| quota_widget::history::is_confirmed_quota_change(e))
            .map(|e| e.kind)
            .collect::<Vec<_>>(),
        vec![
            HistoryKind::QuotaDecrease,
            HistoryKind::QuotaReset,
            HistoryKind::QuotaDecrease
        ]
    );
    let mut live = QuotaModel::new("synthetic-live");
    for (offset, remaining) in [(0, 2.), (10, 3.), (20, 2.)] {
        live.ingest_json(&quota_payload(remaining, BASE + 10_000), BASE + offset)
            .unwrap();
    }
    let live_rows = quota_widget::history::normalized(&live.history);
    assert_eq!(
        live_rows.iter().map(|e| e.kind).collect::<Vec<_>>(),
        vec![HistoryKind::QuotaReset, HistoryKind::QuotaDecrease]
    );
    assert!(live_rows
        .iter()
        .all(quota_widget::history::is_confirmed_quota_change));
}
#[test]
fn indexed_task_startup_has_numeric_known_subtotal_and_explicit_partial_state() {
    let mut events = vec![
        event(1, 1),
        event(5, 1),
        event(10, 1),
        event(11, 1),
        event(12, 1),
        event(13, 1),
    ];
    events[3].cost_usd = None;
    events[4].partial_flags.push("response_coverage_gap".into());
    // Deliberately unsorted to check the index returns the real latest event.
    events.swap(0, 5);
    let index = usage_views::UsageIndex::new(&events);
    let startup = index.task_rows(&events, None, BASE + 13, BASE + 13);
    assert_eq!(startup.len(), 1);
    assert_eq!(startup[0].interval_cost_usd, Some(0.));
    assert_eq!(startup[0].interval_events, 0);
    assert!(startup[0].partial);
    assert_eq!(events[startup[0].latest_event_index].timestamp, BASE + 13);
    let known_boundary = index.task_rows(&events, Some(BASE + 10), BASE + 13, BASE + 12);
    assert_eq!(
        known_boundary[0].interval_events, 2,
        "start excluded, end included"
    );
    assert!((known_boundary[0].interval_cost_usd.unwrap() - 0.01).abs() < 1e-12);
    assert!(
        known_boundary[0].partial,
        "unknown prices and gaps are separate from known dollars"
    );
    assert_eq!(
        events[known_boundary[0].latest_event_index].timestamp,
        BASE + 12,
        "future response must not leak into an older view"
    );
    let after_boundary = index.task_rows(&events, Some(BASE + 13), BASE, BASE + 13);
    assert_eq!(after_boundary[0].interval_cost_usd, Some(0.));
    assert!(
        !after_boundary[0].partial,
        "old partial events do not contaminate the new interval"
    );
}
#[test]
fn indexed_costs_keep_unknowns_separate_and_match_strict_accounting() {
    let mut events = vec![event(0, 1), event(10, 1), event(20, 1), event(30, 1)];
    events[2].cost_usd = None;
    let index = accounting::CostIndex::new(&events);
    let observed = index.observed(BASE, BASE + 30);
    assert!((observed.known_usd - 0.02).abs() < 1e-12);
    assert_eq!((observed.events, observed.unknown_events), (3, 1));
    assert!(observed.partial);
    assert_eq!(index.strict(BASE, BASE + 30), (None, true, 3));
    let no_new = index.observed(BASE + 30, BASE + 30);
    assert_eq!(no_new.known_usd, 0.);
    assert_eq!(no_new.events, 0);
    assert!(!no_new.partial);
    let empty = accounting::CostIndex::new(&[]).observed(BASE, BASE + 1);
    assert_eq!(empty.known_usd, 0.);
    assert!(empty.partial);
    events[2].cost_usd = Some(0.01);
    let index = accounting::CostIndex::new(&events);
    for (start, end) in [
        (BASE, BASE + 10),
        (BASE + 10, BASE + 30),
        (BASE - 1, BASE + 30),
    ] {
        let expected = legacy_sum_cost(&events, start, end);
        let actual = index.strict(start, end);
        assert_eq!((actual.1, actual.2), (expected.1, expected.2));
        assert!((actual.0.unwrap() - expected.0.unwrap()).abs() < 1e-12);
    }
}
#[test]
fn widget_grows_one_row_at_a_time_then_scrolls_at_configured_threshold() {
    let defaults: quota_widget::storage::Settings = serde_json::from_str("{}").unwrap();
    assert_eq!(defaults.task_visible_rows, 10);
    assert_eq!(usage_views::visible_task_rows(0, 10), 1);
    for count in 1..=10 {
        assert_eq!(usage_views::visible_task_rows(count, 10), count);
        if count > 1 {
            assert_eq!(
                usage_views::widget_height(count, 10) - usage_views::widget_height(count - 1, 10),
                18.
            );
        }
    }
    assert_eq!(
        usage_views::widget_height(11, 10),
        usage_views::widget_height(10, 10)
    );
    assert_eq!(
        usage_views::widget_height(1000, 10),
        usage_views::widget_height(10, 10)
    );
    assert_eq!(usage_views::visible_task_rows(20, 4), 4);
    assert_eq!(usage_views::visible_task_rows(20, 15), 15);
    assert_eq!(usage_views::visible_task_rows(20, 0), 1);
}
#[test]
fn explicit_task_names_survive_restart_without_guessing_from_conversation_text() {
    let home = TempDir::new().unwrap();
    let logs = home.path().join("synthetic-logs");
    fs::create_dir(&logs).unwrap();
    let path = logs.join("one.jsonl");
    let rows = [
        json!({"timestamp":BASE,"type":"session_meta","payload":{"id":"synthetic-named","thread_name":"Initial official task name"}}),
        json!({"timestamp":BASE+1,"type":"event_msg","payload":{"type":"session_configured","thread_id":"synthetic-named","thread_name":"Actual renamed task"}}),
        json!({"timestamp":BASE+2,"type":"response_item","payload":{"content":"SYNTHETIC_PRIVATE_PROMPT_DO_NOT_STORE","title":"Never infer this title"}}),
    ];
    fs::write(
        &path,
        rows.iter()
            .map(|r| r.to_string() + "\n")
            .collect::<String>(),
    )
    .unwrap();
    let pricing = Pricing::official_standard();
    let mut store = TokenStore::default();
    store
        .import_dir_with_metadata(&logs, None, &pricing)
        .unwrap();
    assert_eq!(
        store.task_name("synthetic-named"),
        Some("Actual renamed task")
    );
    let revision = store.revision();
    let idle = store
        .import_dir_with_metadata(&logs, None, &pricing)
        .unwrap();
    assert_eq!(idle.lines_read, 0);
    assert_eq!(store.revision(), revision);
    let index_path = home.path().join("explicit-synthetic-session-index.jsonl");
    fs::write(&index_path,json!({"id":"synthetic-named","thread_name":"Current indexed task name","updated_at":"2026-10-17T00:00:00Z"}).to_string()+"\n").unwrap();
    store
        .import_dir_with_metadata(&logs, Some(&index_path), &pricing)
        .unwrap();
    assert_eq!(
        store.task_name("synthetic-named"),
        Some("Current indexed task name")
    );
    assert!(store.revision() > revision);
    let state = serde_json::to_string(&store).unwrap();
    assert!(!state.contains("SYNTHETIC_PRIVATE_PROMPT_DO_NOT_STORE"));
    assert!(!state.contains("Never infer this title"));
    let mut restored: TokenStore = serde_json::from_str(&state).unwrap();
    assert_eq!(
        restored.task_name("synthetic-named"),
        Some("Current indexed task name")
    );
    restored
        .import_dir_with_metadata(&logs, None, &pricing)
        .unwrap();
    assert_eq!(
        restored.task_name("synthetic-named"),
        Some("Actual renamed task")
    );
    assert_eq!(restored.task_name("unknown-task"), None);
}

#[test]
fn uncertain_recovery_never_restarts_task_or_history_accounting() {
    let reset = BASE + 10_000;
    let observations = vec![
        observation(BASE, 3., "a", reset),
        observation(BASE + 10, 2., "a", reset),
        observation(BASE + 11, 3., "b", reset),
        observation(BASE + 12, 2., "a", reset),
        observation(BASE + 30, 1., "a", reset),
    ];
    let visible = quota_widget::history::normalized(&quota_widget::history::replay(&observations));
    let mut events = vec![event(0, 1), event(11, 1), event(14, 1), event(30, 1)];
    events[0].cost_usd = Some(1.);
    let costs = accounting::CostIndex::new(&events);
    let last = visible
        .iter()
        .rfind(|e| quota_widget::history::is_confirmed_quota_change(e))
        .unwrap();
    assert_eq!(last.observed_at_unix, BASE + 30);
    let attributed = accounting::event_cost_indexed(&costs, last, &visible);
    assert!(
        (attributed.0.unwrap() - 0.03).abs() < 1e-12,
        "stale rise must not discard priced events since the last real drop"
    );
    assert!(attributed.1, "uncertain coverage must still be disclosed");
    let boundary = visible
        .iter()
        .filter(|e| {
            e.observed_at_unix <= BASE + 14 && quota_widget::history::is_confirmed_quota_change(e)
        })
        .map(|e| e.observed_at_unix)
        .max();
    assert_eq!(boundary, Some(BASE + 10));
    let tasks =
        usage_views::UsageIndex::new(&events).task_rows(&events, boundary, BASE + 14, BASE + 14);
    assert!((tasks[0].interval_cost_usd.unwrap() - 0.02).abs() < 1e-12);
}
#[test]
fn substantial_indexed_history_matches_reference_for_all_tasks() {
    let (events, history) = synthetic_workload();
    assert_eq!(quota_widget::history::normalized(&history).len(), 2_000);
    let costs = accounting::history_costs(&accounting::CostIndex::new(&events), &history);
    assert_eq!(costs[0], (None, true));
    assert!(costs[1..]
        .iter()
        .all(|(cost, partial)| cost.is_some_and(|cost| (cost - 0.5).abs() < 1e-9) && !*partial));

    let end = BASE + 100_000;
    let boundary = Some(BASE + 90_000);
    let indexed = usage_views::UsageIndex::new(&events).task_rows(&events, boundary, BASE, end);
    let reference = legacy_task_rows(&events, boundary, end);
    assert_eq!(indexed.len(), 1_000);
    for (actual, expected) in indexed.iter().zip(reference) {
        assert_eq!(
            (
                &actual.task_id,
                actual.latest_timestamp,
                actual.interval_events,
                actual.partial
            ),
            (
                &expected.task_id,
                expected.latest_timestamp,
                expected.interval_events,
                expected.partial
            )
        );
        assert_eq!(actual.latest_cost_usd, expected.latest_cost_usd);
        assert!(
            (actual.interval_cost_usd.unwrap() - expected.interval_cost_usd.unwrap()).abs() < 1e-10
        );
        assert_eq!(events[actual.latest_event_index].session, actual.task_id);
        assert_eq!(
            events[actual.latest_event_index].timestamp,
            actual.latest_timestamp
        );
    }
}

fn scoped_history(time: i64, account_id: Option<&str>, window: &str) -> HistoryEvent {
    let mut row = history_event(0);
    row.observed_at_unix = BASE + time;
    row.window_key = Some(window.into());
    row.before = "50".into();
    row.after = "49".into();
    row.delta = "1".into();
    row.evidence = Some(HistoryEvidence {
        account_id: account_id.map(str::to_owned),
        ..HistoryEvidence::default()
    });
    row
}
#[test]
fn history_batch_is_aligned_and_never_borrows_another_accounts_boundary() {
    let events = [
        event(0, 1),
        event(15, 1),
        event(25, 1),
        event(35, 1),
        event(45, 1),
    ];
    let index = accounting::CostIndex::new(&events);
    let rows = vec![
        scoped_history(40, Some("b"), "secondary"),
        scoped_history(30, Some("a"), "secondary"),
        scoped_history(10, Some("a"), "secondary"),
        scoped_history(20, Some("b"), "secondary"),
        scoped_history(30, Some("b"), "primary"),
    ];
    let costs = accounting::history_costs(&index, &rows);
    assert_eq!(costs.len(), rows.len());
    for position in [0, 1] {
        assert!((costs[position].0.unwrap() - 0.02).abs() < 1e-12);
        assert!(!costs[position].1);
    }
    for position in [2, 3, 4] {
        assert_eq!(costs[position], (None, true));
    }
    for (position, row) in rows.iter().enumerate() {
        assert_eq!(
            accounting::event_cost_indexed(&index, row, &rows),
            costs[position]
        );
    }
    let unknown_account = vec![
        scoped_history(10, None, "secondary"),
        scoped_history(30, Some("a"), "secondary"),
    ];
    assert!(accounting::history_costs(&index, &unknown_account)
        .iter()
        .all(|cost| *cost == (None, true)));
}
#[test]
fn history_batch_does_not_price_ambiguous_same_second_boundaries_twice() {
    let events = [
        event(0, 1),
        event(15, 1),
        event(25, 1),
        event(35, 1),
        event(45, 1),
    ];
    let index = accounting::CostIndex::new(&events);
    let mut duplicate = scoped_history(20, Some("a"), "secondary");
    duplicate.before = "49".into();
    duplicate.after = "48".into();
    let rows = vec![
        scoped_history(10, Some("a"), "secondary"),
        scoped_history(20, Some("a"), "secondary"),
        duplicate,
        scoped_history(30, Some("a"), "secondary"),
        scoped_history(40, Some("a"), "secondary"),
    ];
    let costs = accounting::history_costs(&index, &rows);
    assert!(costs[..4].iter().all(|cost| *cost == (None, true)));
    assert!((costs[4].0.unwrap() - 0.01).abs() < 1e-12);
    for (position, row) in rows.iter().enumerate() {
        assert_eq!(
            accounting::event_cost_indexed(&index, row, &rows),
            costs[position]
        );
    }
}
#[test]
fn history_batch_resolves_legacy_window_labels_only_when_unambiguous() {
    let index = accounting::CostIndex::new(&[event(0, 1), event(15, 1)]);
    let mut legacy = scoped_history(10, None, "secondary");
    legacy.window_key = None;
    let current = scoped_history(20, None, "secondary");
    let unique = vec![legacy.clone(), current.clone()];
    assert!((accounting::history_costs(&index, &unique)[1].0.unwrap() - 0.01).abs() < 1e-12);
    let ambiguous = vec![legacy, current, scoped_history(20, None, "primary")];
    assert!(accounting::history_costs(&index, &ambiguous)
        .iter()
        .all(|cost| *cost == (None, true)));
}

#[test]
fn task_name_index_append_rewrite_partial_tail_and_removal_invalidate_correctly() {
    use std::io::Write;
    let home = TempDir::new().unwrap();
    let logs = home.path().join("synthetic-logs");
    fs::create_dir(&logs).unwrap();
    fs::write(logs.join("session.jsonl"),json!({"timestamp":BASE,"type":"session_meta","payload":{"id":"synthetic-named","thread_name":"Recorded rollout name"}}).to_string()+"\n").unwrap();
    let path = home.path().join("synthetic-index.jsonl");
    let title = |name: &str| {
        json!({"id":"synthetic-named","thread_name":name,"updated_at":"2026-10-17T00:00:00Z"})
            .to_string()
    };
    fs::write(&path, title("First indexed name") + "\n").unwrap();
    let pricing = Pricing::official_standard();
    let mut store = TokenStore::default();
    store
        .import_dir_with_metadata(&logs, Some(&path), &pricing)
        .unwrap();
    assert_eq!(
        store.task_name("synthetic-named"),
        Some("First indexed name")
    );
    let first_revision = store.revision();
    {
        let mut append = fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(append, "{}", title("Latest appended name")).unwrap();
    }
    store
        .import_dir_with_metadata(&logs, Some(&path), &pricing)
        .unwrap();
    assert_eq!(
        store.task_name("synthetic-named"),
        Some("First indexed name"),
        "incomplete final line must not replace a committed name"
    );
    {
        let mut append = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(append).unwrap();
    }
    store
        .import_dir_with_metadata(&logs, Some(&path), &pricing)
        .unwrap();
    assert_eq!(
        store.task_name("synthetic-named"),
        Some("Latest appended name")
    );
    assert!(store.revision() > first_revision);
    {
        let mut append = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(append, "{}", title("")).unwrap();
    }
    store
        .import_dir_with_metadata(&logs, Some(&path), &pricing)
        .unwrap();
    assert_eq!(
        store.task_name("synthetic-named"),
        Some("Latest appended name")
    );
    fs::write(&path, title("Rewritten name") + "\n").unwrap();
    store
        .import_dir_with_metadata(&logs, Some(&path), &pricing)
        .unwrap();
    assert_eq!(store.task_name("synthetic-named"), Some("Rewritten name"));
    let before_removal = store.revision();
    fs::remove_file(&path).unwrap();
    store
        .import_dir_with_metadata(&logs, Some(&path), &pricing)
        .unwrap();
    assert_eq!(
        store.task_name("synthetic-named"),
        Some("Recorded rollout name")
    );
    assert!(store.revision() > before_removal);
    let after_removal = store.revision();
    store
        .import_dir_with_metadata(&logs, Some(&path), &pricing)
        .unwrap();
    assert_eq!(
        store.revision(),
        after_removal,
        "repeated missing optional index must not invalidate every repaint"
    );
}

// Frozen pre-revision algorithms retained solely to reproduce the exact baseline.
#[allow(dead_code)]
#[derive(Debug)]
struct LegacyTaskRow {
    task_id: String,
    latest_timestamp: i64,
    latest_cost_usd: Option<f64>,
    interval_cost_usd: Option<f64>,
    interval_events: usize,
    partial: bool,
}
fn legacy_task_rows(events: &[TokenEvent], boundary: Option<i64>, end: i64) -> Vec<LegacyTaskRow> {
    let mut by_task: BTreeMap<&str, Vec<&TokenEvent>> = BTreeMap::new();
    for e in events.iter().filter(|e| e.timestamp <= end) {
        by_task.entry(&e.session).or_default().push(e);
    }
    let mut rows: Vec<_> = by_task
        .into_iter()
        .filter_map(|(task, mut events)| {
            events.sort_by(|a, b| (a.timestamp, &a.id).cmp(&(b.timestamp, &b.id)));
            let latest = *events.last()?;
            if latest.timestamp < end.saturating_sub(86400)
                && boundary.is_none_or(|b| latest.timestamp <= b)
            {
                return None;
            }
            let selected: Vec<_> = events
                .iter()
                .filter(|e| boundary.is_some_and(|b| e.timestamp > b))
                .collect();
            let unknown = selected
                .iter()
                .any(|e| e.cost_usd.is_none() || coverage_incomplete(e));
            Some(LegacyTaskRow {
                task_id: task.into(),
                latest_timestamp: latest.timestamp,
                latest_cost_usd: latest.cost_usd,
                interval_cost_usd: boundary.and_then(|_| {
                    (!unknown).then(|| selected.iter().filter_map(|e| e.cost_usd).sum())
                }),
                interval_events: selected.len(),
                partial: boundary.is_none()
                    || unknown
                    || selected.iter().any(|e| !e.partial_flags.is_empty()),
            })
        })
        .collect();
    rows.sort_by(|a, b| (b.latest_timestamp, &b.task_id).cmp(&(a.latest_timestamp, &a.task_id)));
    rows
}

fn legacy_sum_cost(events: &[TokenEvent], start: i64, end: i64) -> (Option<f64>, bool, usize) {
    let selected: Vec<_> = events
        .iter()
        .filter(|e| e.timestamp > start && e.timestamp <= end)
        .collect();
    if selected.is_empty() {
        return (None, true, 0);
    }
    let partial = selected.iter().any(|e| !e.partial_flags.is_empty())
        || events.first().map(|e| e.timestamp > start).unwrap_or(true);
    if selected
        .iter()
        .any(|e| e.cost_usd.is_none() || coverage_incomplete(e))
    {
        (None, true, selected.len())
    } else {
        (
            Some(selected.iter().filter_map(|e| e.cost_usd).sum()),
            partial,
            selected.len(),
        )
    }
}

fn legacy_event_cost(
    events: &[TokenEvent],
    event: &quota_widget::model::HistoryEvent,
    all: &[quota_widget::model::HistoryEvent],
) -> (Option<f64>, bool) {
    use quota_widget::model::HistoryKind::{QuotaDecrease, QuotaReset};
    let relevant = |e: &&quota_widget::model::HistoryEvent| {
        e.source_key == event.source_key
            && match (&e.window_key, &event.window_key) {
                (Some(a), Some(b)) => a == b,
                _ => e.window_label == event.window_label,
            }
            && matches!(e.kind, QuotaDecrease | QuotaReset)
            && e.delta.parse::<f64>().is_ok_and(|d| d > 0.0)
            && e.before.parse::<f64>().ok() != e.after.parse::<f64>().ok()
    };
    if all
        .iter()
        .filter(relevant)
        .filter(|e| e.observed_at_unix == event.observed_at_unix)
        .count()
        != 1
    {
        return (None, true);
    }
    let boundary = all
        .iter()
        .filter(relevant)
        .filter(|e| e.observed_at_unix < event.observed_at_unix)
        .max_by_key(|e| e.observed_at_unix);
    let Some(previous) = boundary else {
        return (None, true);
    };
    if previous.session != event.session
        || (previous.epoch != event.epoch && previous.kind != QuotaReset)
    {
        return (None, true);
    }
    let (cost, partial, _) =
        legacy_sum_cost(events, previous.observed_at_unix, event.observed_at_unix);
    (cost, partial || event.partial)
}
