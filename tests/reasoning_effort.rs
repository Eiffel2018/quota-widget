//! Synthetic reasoning metadata regressions. No real sessions or credentials.
use quota_widget::tokens::{Pricing, TokenEvent, TokenStore};
use serde_json::{json, Value};
use std::{fs, io::Write};
use tempfile::TempDir;

const BASE: i64 = 1_790_000_000;
fn counts(input: u64) -> Value {
    json!({"input_tokens":input,"cached_input_tokens":input/2,
        "output_tokens":input/10,"reasoning_output_tokens":input/20})
}
fn meta(task: &str) -> Value {
    json!({"timestamp":BASE,"type":"session_meta","payload":{"id":task}})
}
fn context(t: i64, turn: Option<&str>, model: Option<&str>, effort: Option<&str>) -> Value {
    json!({"timestamp":BASE+t,"type":"turn_context","payload":{
        "turn_id":turn,"model":model,"effort":effort}})
}
fn response(t: i64, task: &str, turn: &str, id: &str, effort: Option<&str>) -> Value {
    json!({"timestamp":BASE+t,"type":"token_usage_record","payload":{
        "thread_id":task,"turn_id":turn,"response_id":id,
        "reasoning_effort":effort,"usage":counts(100)}})
}
fn legacy(t: i64, turn: Option<&str>, id: &str, total: u64, last: u64) -> Value {
    json!({"timestamp":BASE+t,"type":"event_msg","payload":{"type":"token_count",
        "turn_id":turn,"request_id":id,"info":{
            "total_token_usage":counts(total),"last_token_usage":counts(last)}}})
}
fn write(dir: &TempDir, name: &str, rows: &[Value]) {
    fs::write(
        dir.path().join(name),
        rows.iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
}
fn imported(rows: &[Value]) -> (TempDir, TokenStore) {
    let dir = TempDir::new().unwrap();
    write(&dir, "synthetic.jsonl", rows);
    let mut store = TokenStore::default();
    store.import_dir(dir.path(), &Pricing::demo()).unwrap();
    (dir, store)
}
fn event<'a>(store: &'a TokenStore, id: &str) -> &'a TokenEvent {
    store
        .events()
        .iter()
        .find(|event| {
            event.response_id.as_deref() == Some(id)
                || event.id.ends_with(&format!(":request:{id}"))
        })
        .unwrap()
}

#[test]
fn each_response_and_legacy_event_keeps_its_own_turn_model_and_effort() {
    for modern in [true, false] {
        let mut rows = vec![
            meta("mixed"),
            context(1, Some("sol"), Some("gpt-6.1-sol"), Some("high")),
            context(2, Some("luna"), Some("gpt-6-luna"), Some("max")),
        ];
        rows.extend(if modern {
            vec![
                response(3, "mixed", "sol", "s", None),
                response(4, "mixed", "luna", "l", None),
            ]
        } else {
            vec![
                legacy(3, Some("sol"), "s", 100, 100),
                legacy(4, Some("luna"), "l", 200, 100),
            ]
        });
        let (_, store) = imported(&rows);
        assert_eq!(event(&store, "s").reasoning_effort.as_deref(), Some("high"));
        assert_eq!(event(&store, "l").reasoning_effort.as_deref(), Some("max"));
        assert_eq!(event(&store, "s").model, "gpt-6.1-sol");
        assert_eq!(event(&store, "l").model, "gpt-6-luna");
    }
}

#[test]
fn seven_luna_max_children_do_not_inherit_parent_sol_high() {
    let mut rows = vec![
        meta("parent"),
        context(1, Some("parent-turn"), Some("gpt-6.1-sol"), Some("high")),
        response(2, "parent", "parent-turn", "parent", None),
    ];
    for i in 0..7 {
        let task = format!("luna-{i}");
        let mut metadata = meta(&task);
        metadata["payload"]["parent_thread_id"] = json!("parent");
        rows.extend([
            metadata,
            context(3, Some("child-turn"), Some("gpt-6-luna"), Some("max")),
            response(4, &task, "child-turn", &task, None),
        ]);
    }
    let (_, store) = imported(&rows);
    assert_eq!(store.events().len(), 8);
    for i in 0..7 {
        let e = event(&store, &format!("luna-{i}"));
        assert_eq!(e.reasoning_effort.as_deref(), Some("max"));
        assert_eq!(e.model, "gpt-6-luna");
        assert_eq!(e.cost_usd, Pricing::demo().cost(&e.model, 50, 50, 10));
    }
    assert_eq!(
        event(&store, "parent").reasoning_effort.as_deref(),
        Some("high")
    );
}

#[test]
fn thread_defaults_parent_context_and_later_turns_do_not_fill_missing_effort() {
    let mut metadata = meta("parent");
    metadata["payload"]["reasoning_effort"] = json!("ultra");
    let mut child = response(3, "child", "child-turn", "child", None);
    child["payload"]["session_id"] = json!("parent");
    child["payload"]["root_turn_id"] = json!("old");
    let (_, store) = imported(&[
        metadata,
        response(1, "parent", "old", "before", None),
        context(2, Some("other"), Some("gpt-6.1-sol"), Some("high")),
        child,
        response(4, "parent", "old", "older-turn", None),
        context(5, Some("empty"), Some("gpt-6.1-sol"), None),
        json!({"timestamp":BASE+6,"type":"thread_settings_applied","payload":{"thread_settings":{"reasoning_effort":"max"}}}),
        response(7, "parent", "empty", "empty", None),
    ]);
    assert!(store.events().iter().all(|e| e.reasoning_effort.is_none()));
}

#[test]
fn context_effort_alias_is_scoped_and_does_not_require_model() {
    let mut alias = context(1, Some("t"), None, None);
    alias["payload"]["reasoning_effort"] = json!("xhigh");
    let mut unrelated = response(4, "s", "u", "unknown", None);
    unrelated["payload"]["effort"] = json!("max");
    let (_, store) = imported(&[
        meta("s"),
        alias,
        response(2, "s", "t", "known", None),
        unrelated,
    ]);
    assert_eq!(
        event(&store, "known").reasoning_effort.as_deref(),
        Some("xhigh")
    );
    assert_eq!(event(&store, "unknown").reasoning_effort, None);
}

#[test]
fn missing_or_placeholder_effort_is_unknown_while_recorded_none_is_literal() {
    let labels = [
        None,
        Some(""),
        Some("default"),
        Some("unknown"),
        Some("none"),
        Some("custom-effort"),
    ];
    let rows = labels
        .iter()
        .enumerate()
        .map(|(i, effort)| response(i as i64, "s", "t", &i.to_string(), *effort))
        .collect::<Vec<_>>();
    let (_, store) = imported(&rows);
    for i in 0..4 {
        assert!(event(&store, &i.to_string()).reasoning_effort.is_none());
    }
    assert_eq!(event(&store, "4").reasoning_effort.as_deref(), Some("none"));
    assert_eq!(
        event(&store, "5").reasoning_effort.as_deref(),
        Some("custom-effort")
    );
}

#[test]
fn response_update_keeps_explicit_effort_and_first_request_context() {
    let (_, store) = imported(&[
        meta("s"),
        context(1, Some("t"), Some("gpt-6-luna"), Some("high")),
        response(2, "s", "t", "context", None),
        response(2, "s", "t", "explicit", Some("max")),
        context(3, Some("t"), Some("gpt-6-luna"), Some("ultra")),
        response(4, "s", "t", "new", None),
        response(5, "s", "t", "context", None),
        response(5, "s", "t", "explicit", None),
    ]);
    assert_eq!(
        event(&store, "context").reasoning_effort.as_deref(),
        Some("high")
    );
    assert_eq!(
        event(&store, "explicit").reasoning_effort.as_deref(),
        Some("max")
    );
    assert_eq!(
        event(&store, "new").reasoning_effort.as_deref(),
        Some("ultra")
    );
}

#[test]
fn conflicting_duplicate_efforts_keep_raw_evidence_but_do_not_change_financial_coverage() {
    let (dir, mut store) = imported(&[response(1, "s", "t", "r", Some("high"))]);
    let before = event(&store, "r").clone();
    write(
        &dir,
        "copied.jsonl",
        &[response(1, "s", "t", "r", Some("max"))],
    );
    store.import_dir(dir.path(), &Pricing::demo()).unwrap();
    let after = event(&store, "r");
    assert_eq!(after.reasoning_effort, None);
    assert_eq!(after.partial_flags, before.partial_flags);
    assert_eq!(after.cost_usd, before.cost_usd);
    assert_eq!(after.total_tokens(), before.total_tokens());
    assert_eq!(store.observation_count(), 1);
    let raw = serde_json::to_value(&store).unwrap();
    assert_eq!(
        raw["observations"][0]["reasoning_efforts"],
        json!(["high", "max"])
    );
    let state = dir.path().join("state.json");
    store.save(&state).unwrap();
    let loaded = TokenStore::load(state).unwrap();
    assert_eq!(loaded.events(), store.events());
}

#[test]
fn conflicting_response_updates_and_context_aliases_remain_unknown() {
    let mut conflicting = context(1, Some("t"), None, Some("high"));
    conflicting["payload"]["reasoning_effort"] = json!("max");
    let (_, store) = imported(&[
        meta("s"),
        conflicting,
        response(2, "s", "t", "context", None),
        response(3, "s", "t", "explicit", Some("high")),
        response(4, "s", "t", "explicit", Some("max")),
    ]);
    assert!(store.events().iter().all(|e| e.reasoning_effort.is_none()));
}

#[test]
fn same_millisecond_context_switches_preserve_captured_request_order() {
    let (_, store) = imported(&[
        meta("s"),
        context(1, Some("t"), None, Some("high")),
        response(1, "s", "t", "a", None),
        context(1, Some("t"), None, Some("max")),
        response(1, "s", "t", "b", None),
    ]);
    assert_eq!(event(&store, "a").reasoning_effort.as_deref(), Some("high"));
    assert_eq!(event(&store, "b").reasoning_effort.as_deref(), Some("max"));
}

#[test]
fn legacy_anonymous_turn_context_is_local_and_cumulative_gaps_are_unknown() {
    let (dir, mut store) = imported(&[
        meta("s"),
        context(1, None, None, Some("high")),
        legacy(2, None, "a", 100, 100),
        context(3, None, None, Some("max")),
        legacy(4, None, "b", 400, 100),
    ]);
    assert_eq!(event(&store, "a").reasoning_effort.as_deref(), Some("high"));
    assert_eq!(event(&store, "b").reasoning_effort, None);
    write(
        &dir,
        "unrelated.jsonl",
        &[meta("s"), legacy(5, None, "c", 500, 100)],
    );
    store.import_dir(dir.path(), &Pricing::demo()).unwrap();
    assert_eq!(event(&store, "c").reasoning_effort, None);
}

#[test]
fn save_load_append_and_reprice_keep_effort_without_any_price_multiplier() {
    let (dir, mut store) = imported(&[
        meta("s"),
        context(1, Some("t"), Some("gpt-6-luna"), Some("max")),
        response(2, "s", "t", "r", None),
    ]);
    let before = store.events().to_vec();
    let state = dir.path().join("state.json");
    store.save(&state).unwrap();
    store = TokenStore::load(&state).unwrap();
    assert_eq!(store.events(), before);
    let report = store.import_dir(dir.path(), &Pricing::demo()).unwrap();
    assert_eq!(report.lines_read, 0);
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(dir.path().join("synthetic.jsonl"))
        .unwrap();
    writeln!(file, "{}", response(3, "s", "t", "later", None)).unwrap();
    store.import_dir(dir.path(), &Pricing::demo()).unwrap();
    assert_eq!(
        event(&store, "later").reasoning_effort.as_deref(),
        Some("max")
    );
    let mut price = Pricing::demo();
    price.model_override = Some("gpt-6.1-sol".into());
    store.reprice(&price);
    for e in store.events() {
        assert_eq!(e.reasoning_effort.as_deref(), Some("max"));
        assert_eq!(e.cost_usd, price.cost("gpt-6.1-sol", 50, 50, 10));
    }
}

#[test]
fn v3_old_cache_replays_already_seen_files_and_enriches_without_duplicate_usage() {
    let rows = [
        meta("s"),
        context(1, Some("t"), Some("gpt-6-luna"), Some("max")),
        response(2, "s", "t", "context", None),
        response(3, "s", "t", "explicit", Some("high")),
    ];
    let (dir, store) = imported(&rows);
    let before = store.events().to_vec();
    let original = fs::read(dir.path().join("synthetic.jsonl")).unwrap();
    let mut old = serde_json::to_value(&store).unwrap();
    old["normalization_version"] = json!(1);
    old.as_object_mut().unwrap().remove("effort_contexts");
    for source in old["sources"].as_object_mut().unwrap().values_mut() {
        source.as_object_mut().unwrap().remove("effort_context");
    }
    for row in old["observations"].as_array_mut().unwrap() {
        let obj = row.as_object_mut().unwrap();
        obj.remove("reasoning_efforts");
        obj.remove("source_effort_contexts");
    }
    for event in old["events"].as_array_mut().unwrap() {
        event.as_object_mut().unwrap().remove("reasoning_effort");
    }
    let state = dir.path().join("old.json");
    fs::write(&state, serde_json::to_vec(&old).unwrap()).unwrap();
    let mut recovered = TokenStore::load(&state).unwrap();
    assert!(recovered
        .events()
        .iter()
        .all(|e| e.reasoning_effort.is_none()));
    let report = recovered.import_dir(dir.path(), &Pricing::demo()).unwrap();
    assert_eq!(report.events_added, 0);
    assert_eq!(report.duplicate_observations, 2);
    assert_eq!(recovered.observation_count(), store.observation_count());
    assert_eq!(recovered.events(), before);
    assert_eq!(
        fs::read(dir.path().join("synthetic.jsonl")).unwrap(),
        original
    );
    recovered.save(&state).unwrap();
    let mut restarted = TokenStore::load(&state).unwrap();
    assert_eq!(restarted.events(), before);
    assert_eq!(
        restarted
            .import_dir(dir.path(), &Pricing::demo())
            .unwrap()
            .lines_read,
        0
    );
}

#[test]
fn absent_reasoning_token_counter_is_unknown_but_recorded_zero_is_known() {
    for modern in [true, false] {
        let mut missing = if modern {
            response(1, "s", "t", "missing", Some("high"))
        } else {
            legacy(1, Some("t"), "missing", 100, 100)
        };
        let mut zero = if modern {
            response(2, "s", "t", "zero", Some("max"))
        } else {
            legacy(2, Some("t"), "zero", 200, 100)
        };
        if modern {
            missing["payload"]["usage"]
                .as_object_mut()
                .unwrap()
                .remove("reasoning_output_tokens");
            zero["payload"]["usage"]["reasoning_output_tokens"] = json!(0);
        } else {
            for field in ["total_token_usage", "last_token_usage"] {
                missing["payload"]["info"][field]
                    .as_object_mut()
                    .unwrap()
                    .remove("reasoning_output_tokens");
                zero["payload"]["info"][field]["reasoning_output_tokens"] = json!(0);
            }
        }
        let (_, store) = imported(&[meta("s"), missing, zero]);
        assert!(event(&store, "missing")
            .partial_flags
            .contains(&"reasoning_count_unavailable".into()));
        assert!(!event(&store, "zero")
            .partial_flags
            .contains(&"reasoning_count_unavailable".into()));
        assert_eq!(event(&store, "zero").reasoning, 0);
    }
}
