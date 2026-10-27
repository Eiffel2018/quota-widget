//! Synthetic model-attribution and auto-review regression coverage only.
//! No real account, credentials, or Codex session directories are accessed.
use quota_widget::tokens::{ModelPrice, Pricing, TokenStore};
use serde_json::{json, Value};
use std::{fs, io::Write};
use tempfile::TempDir;

const BASE: i64 = 1_790_000_000;
fn counts(input: u64) -> Value {
    json!({"input_tokens": input, "cached_input_tokens": input / 2,
        "output_tokens": input / 10, "reasoning_output_tokens": input / 20})
}
fn meta(task: &str, model: Option<&str>, name: &str) -> Value {
    json!({"timestamp": BASE, "type":"session_meta", "payload":{
        "id":task, "model":model, "thread_name":name}})
}
fn context(t: i64, turn: &str, model: &str) -> Value {
    json!({"timestamp":BASE+t,"type":"turn_context","payload":{"turn_id":turn,"model":model}})
}
fn response(t: i64, task: &str, turn: &str, id: &str, model: Option<&str>, input: u64) -> Value {
    json!({"timestamp":BASE+t,"type":"token_usage_record","payload":{
        "thread_id":task,"turn_id":turn,"response_id":id,"model":model,"usage":counts(input)}})
}
fn legacy(t: i64, turn: Option<&str>, total: u64, last: Option<u64>) -> Value {
    json!({"timestamp":BASE+t,"type":"event_msg","payload":{"type":"token_count",
        "turn_id":turn,"info":{"total_token_usage":counts(total),"last_token_usage":last.map(counts)}}})
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
fn imported(rows: &[Value], pricing: &Pricing) -> (TempDir, TokenStore) {
    let dir = TempDir::new().unwrap();
    write(&dir, "synthetic.jsonl", rows);
    let mut store = TokenStore::default();
    store.import_dir(dir.path(), pricing).unwrap();
    (dir, store)
}

#[test]
fn huge_exact_review_usage_is_retained_but_omitted_from_summary_even_with_custom_price() {
    let mut pricing = Pricing::demo();
    pricing
        .set_rate(
            "codex-auto-review",
            ModelPrice {
                uncached_input_per_million: 99.,
                cached_input_per_million: 20.,
                output_per_million: 400.,
            },
        )
        .unwrap();
    let (_, store) = imported(
        &[
            meta("review", Some("codex-auto-review"), "arbitrary task"),
            response(1, "review", "r", "review-response", None, 1_000_000_000),
            meta("ordinary", Some("gpt-6.1-sol"), "Review the changes"),
            response(2, "ordinary", "s", "sol-response", None, 1000),
        ],
        &pricing,
    );
    assert_eq!(store.observation_count(), 2);
    assert_eq!(store.events().len(), 2);
    let review = store
        .events()
        .iter()
        .find(|e| e.session == "review")
        .unwrap();
    assert!(review.is_excluded_auto_review());
    assert!(review.cost_usd.unwrap() > 1_000.);
    assert!(review
        .partial_flags
        .contains(&"excluded_recorded_codex_auto_review".to_owned()));
    let summary = store.summary(None, None, &pricing);
    assert_eq!(summary.event_count, 1);
    assert_eq!(summary.input, 1000);
    assert!(summary.cost_usd.unwrap() < 1.);
    assert!(store.excluded_auto_review_tasks().contains("review"));
    assert!(!store.excluded_auto_review_tasks().contains("ordinary"));
}

#[test]
fn only_exact_recorded_model_excludes_not_names_roots_prefixes_or_overrides() {
    let rows = [
        meta("ordinary", Some("gpt-6.1-sol"), "codex-auto-review"),
        response(1, "ordinary", "t", "a", None, 100),
        response(2, "near", "t", "b", Some("codex-auto-review-extra"), 100),
        response(3, "case", "t", "c", Some("Codex-Auto-Review"), 100),
        response(
            4,
            "known-unknown",
            "t",
            "d",
            Some("not-a-published-model"),
            100,
        ),
        response(5, "absent", "t", "e", None, 100),
    ];
    let (_, mut store) = imported(&rows, &Pricing::demo());
    assert!(store.events().iter().all(|e| !e.is_excluded_auto_review()));
    for event in store.events() {
        if event.session == "absent" {
            assert!(event.model_assumed);
            assert!(event.cost_usd.is_some());
        } else if event.session != "ordinary" {
            assert!(!event.model_assumed);
            assert!(event.cost_usd.is_none());
        }
    }
    let mut override_price = Pricing::demo();
    override_price.model_override = Some("codex-auto-review".into());
    store.reprice(&override_price);
    assert!(store.events().iter().all(|e| !e.is_excluded_auto_review()));
}

#[test]
fn sol_override_cannot_erase_recorded_review_classification_after_restart_or_reprice() {
    let mut pricing = Pricing::demo();
    pricing.model_override = Some("gpt-6.1-sol".into());
    let (dir, mut store) = imported(
        &[
            meta("review", Some("codex-auto-review"), "normal name"),
            response(1, "review", "t", "r", None, 500_000_000),
        ],
        &pricing,
    );
    let checkpoint = dir.path().join("state.json");
    store.save(&checkpoint).unwrap();
    store = TokenStore::load(&checkpoint).unwrap();
    assert_eq!(store.events()[0].model, "gpt-6.1-sol");
    assert!(store.events()[0].is_excluded_auto_review());
    assert_eq!(store.summary(None, None, &pricing).input, 0);
    store.reprice(&Pricing::demo());
    assert_eq!(store.events()[0].model, "codex-auto-review");
    assert!(store.events()[0].cost_usd.is_none());
    assert!(store.events()[0].is_excluded_auto_review());
}

#[test]
fn response_and_legacy_attribution_follow_explicit_turn_even_after_another_turn_switch() {
    for response_schema in [true, false] {
        let mut rows = vec![
            meta("mixed", None, "mixed"),
            context(1, "sol", "gpt-6.1-sol"),
            context(2, "luna", "gpt-6-luna"),
        ];
        if response_schema {
            rows.push(response(3, "mixed", "sol", "s", None, 100));
            rows.push(response(4, "mixed", "luna", "l", None, 200));
        } else {
            rows.push(legacy(3, Some("sol"), 100, Some(100)));
            rows.push(legacy(4, Some("luna"), 300, Some(200)));
        }
        let (_, store) = imported(&rows, &Pricing::demo());
        let sol = store
            .events()
            .iter()
            .find(|e| e.turn_id.as_deref() == Some("sol"))
            .unwrap();
        let luna = store
            .events()
            .iter()
            .find(|e| e.turn_id.as_deref() == Some("luna"))
            .unwrap();
        assert_eq!(sol.model, "gpt-6.1-sol");
        assert_eq!(luna.model, "gpt-6-luna");
        assert_eq!(sol.input, 100);
        assert_eq!(luna.input, 200);
        assert_eq!(sol.cost_usd, Pricing::demo().cost(&sol.model, 50, 50, 10));
        assert_eq!(
            luna.cost_usd,
            Pricing::demo().cost(&luna.model, 100, 100, 20)
        );
    }
}

#[test]
fn legacy_mixed_thread_excludes_review_delta_without_erasing_prior_sol_cost() {
    let (_, store) = imported(
        &[
            meta("mixed", None, "Review this code"),
            context(1, "sol", "gpt-6.1-sol"),
            legacy(2, None, 100, Some(100)),
            context(3, "review", "codex-auto-review"),
            legacy(4, None, 1_000_000_100, Some(1_000_000_000)),
            context(5, "luna", "gpt-6-luna"),
            legacy(6, None, 1_000_000_300, Some(200)),
        ],
        &Pricing::demo(),
    );
    assert_eq!(store.events().len(), 3);
    assert_eq!(store.summary(None, None, &Pricing::demo()).input, 300);
    assert!(!store.excluded_auto_review_tasks().contains("mixed"));
    assert_eq!(
        store
            .events()
            .iter()
            .filter(|e| e.is_excluded_auto_review())
            .count(),
        1
    );
}

#[test]
fn huge_first_legacy_snapshot_of_recorded_review_task_is_excluded_with_scope_audit() {
    for last in [None, Some(100)] {
        let (_, store) = imported(
            &[
                meta("review", None, "not a review name"),
                context(1, "r", "codex-auto-review"),
                legacy(2, None, 9_000_000_000, last),
            ],
            &Pricing::demo(),
        );
        assert_eq!(store.events().len(), 1);
        assert!(store.events()[0].is_excluded_auto_review());
        assert!(store.events()[0]
            .partial_flags
            .contains(&"excluded_review_aggregate_scope_uncertain".into()));
        assert_eq!(store.events()[0].input, 9_000_000_000);
        assert_eq!(store.summary(None, None, &Pricing::demo()).input, 0);
        assert!(store.excluded_auto_review_tasks().contains("review"));
    }
}

#[test]
fn mixed_model_legacy_gap_retains_unattributed_residual_instead_of_excluding_everything() {
    let (_, store) = imported(
        &[
            meta("mixed", None, "mixed"),
            context(1, "sol", "gpt-6.1-sol"),
            legacy(2, None, 100, Some(100)),
            context(3, "review", "codex-auto-review"),
            legacy(4, None, 1000, Some(200)),
        ],
        &Pricing::demo(),
    );
    assert_eq!(store.events().iter().map(|e| e.input).sum::<u64>(), 1000);
    assert_eq!(
        store
            .events()
            .iter()
            .filter(|e| e.is_excluded_auto_review())
            .map(|e| e.input)
            .sum::<u64>(),
        200
    );
    let summary = store.summary(None, None, &Pricing::demo());
    assert_eq!(summary.input, 800);
    assert_eq!(summary.unpriced_events, 1);
    assert_eq!(summary.cost_usd, None);
    assert_eq!(
        summary.priced_cost_usd,
        Pricing::demo().cost("gpt-6.1-sol", 50, 50, 10).unwrap()
    );
}

#[test]
fn metadata_only_exclusion_is_exact_and_does_not_guess_from_task_names() {
    let (_, store) = imported(
        &[
            meta("review-only", Some("codex-auto-review"), "anything"),
            meta("ordinary-review", Some("gpt-6.1-sol"), "codex-auto-review"),
            meta("mixed-meta", Some("codex-auto-review"), "mixed"),
            context(1, "t", "gpt-6-luna"),
            json!({"timestamp":BASE+2,"type":"event_msg","payload":{"type":"session_configured","session_id":"configured-review","model":"codex-auto-review"}}),
        ],
        &Pricing::demo(),
    );
    let excluded = store.excluded_auto_review_tasks();
    assert!(excluded.contains("review-only"));
    assert!(!excluded.contains("ordinary-review"));
    assert!(!excluded.contains("mixed-meta"));
    assert!(excluded.contains("configured-review"));
}

#[test]
fn old_v1_v2_v3_caches_reclassify_and_replay_without_deleting_history_or_custom_rates() {
    for version in [1, 2, 3] {
        let mut pricing = Pricing::demo();
        pricing.model_override = Some("gpt-6.1-sol".into());
        pricing
            .set_rate(
                "gpt-6.1-sol",
                ModelPrice {
                    uncached_input_per_million: 7.,
                    cached_input_per_million: 3.,
                    output_per_million: 11.,
                },
            )
            .unwrap();
        let (dir, store) = imported(
            &[
                meta("review", Some("codex-auto-review"), "review"),
                response(1, "review", "r", "r", None, 100_000_000),
                meta("ordinary", Some("gpt-6-luna"), "review ordinary"),
                response(2, "ordinary", "o", "o", None, 100),
            ],
            &pricing,
        );
        let mut old = serde_json::to_value(&store).unwrap();
        old["version"] = json!(version);
        old.as_object_mut().unwrap().remove("normalization_version");
        for event in old["events"].as_array_mut().unwrap() {
            event["partial_flags"]
                .as_array_mut()
                .unwrap()
                .retain(|flag| flag != "excluded_recorded_codex_auto_review");
        }
        let checkpoint = dir.path().join("state.json");
        fs::write(&checkpoint, old.to_string()).unwrap();
        let mut loaded = TokenStore::load(&checkpoint).unwrap();
        assert_eq!(loaded.events().len(), 2);
        assert_eq!(
            loaded
                .events()
                .iter()
                .filter(|e| e.is_excluded_auto_review())
                .count(),
            1
        );
        assert_eq!(loaded.pricing().unwrap().rates, pricing.rates);
        assert!(loaded.import_dir(dir.path(), &pricing).unwrap().lines_read > 0);
        assert_eq!(loaded.events().len(), 2);
        assert_eq!(loaded.observation_count(), 2);
        assert_eq!(loaded.summary(None, None, &pricing).input, 100);
        loaded.save(&checkpoint).unwrap();
        let mut restarted = TokenStore::load(&checkpoint).unwrap();
        assert_eq!(
            restarted
                .import_dir(dir.path(), &pricing)
                .unwrap()
                .lines_read,
            0
        );
        assert_eq!(
            restarted
                .events()
                .iter()
                .filter(|e| e.is_excluded_auto_review())
                .count(),
            1
        );
    }
}

#[test]
fn source_context_survives_append_and_recompile_style_reload() {
    let (dir, mut store) = imported(
        &[
            meta("review", None, "review"),
            context(1, "r", "codex-auto-review"),
            legacy(2, None, 100, Some(100)),
        ],
        &Pricing::demo(),
    );
    let checkpoint = dir.path().join("state.json");
    store.save(&checkpoint).unwrap();
    store = TokenStore::load(&checkpoint).unwrap();
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("synthetic.jsonl"))
            .unwrap(),
        "{}",
        legacy(3, None, 200, Some(100))
    )
    .unwrap();
    assert_eq!(
        store
            .import_dir(dir.path(), &Pricing::demo())
            .unwrap()
            .lines_read,
        1
    );
    assert_eq!(store.events().len(), 2);
    assert!(store.events().iter().all(|e| e.is_excluded_auto_review()));
}

#[test]
fn astra_standard_fast_and_long_context_presets_preserve_unknown_model_behavior() {
    let standard = Pricing::official_standard();
    let fast = Pricing::official_fast();
    assert_eq!(
        standard.cost("gpt-6-astra", 100_000, 100_000, 10_000),
        Some(1.6)
    );
    assert_eq!(
        fast.cost("gpt-6-astra", 100_000, 100_000, 10_000),
        Some(3.2)
    );
    assert_eq!(
        standard.cost("gpt-6-astra", 200_000, 100_000, 10_000),
        Some(2.6)
    );
    let mut api = standard.clone();
    api.apply_long_context = true;
    assert_eq!(
        api.cost("gpt-6-astra", 200_000, 100_000, 10_000),
        Some(4.95)
    );
    assert_eq!(standard.cost("codex-auto-review", 100, 0, 10), None);
    assert_eq!(standard.cost("new-unknown-model", 100, 0, 10), None);
}

#[test]
fn response_update_omitting_model_keeps_earlier_exact_recorded_identity() {
    let (_, store) = imported(
        &[
            response(
                1,
                "review",
                "t",
                "same-response",
                Some("codex-auto-review"),
                100,
            ),
            response(2, "review", "t", "same-response", None, 200),
        ],
        &Pricing::demo(),
    );
    assert_eq!(store.observation_count(), 2);
    assert_eq!(store.events().len(), 1);
    assert_eq!(store.events()[0].input, 200);
    assert_eq!(store.events()[0].model, "codex-auto-review");
    assert!(!store.events()[0].model_assumed);
    assert!(store.events()[0].is_excluded_auto_review());
    assert_eq!(store.summary(None, None, &Pricing::demo()).event_count, 0);
}

#[test]
fn event_only_older_cache_retains_all_history_and_recovers_explicit_review_marker() {
    let (dir, store) = imported(
        &[
            response(1, "review", "t", "r", Some("codex-auto-review"), 100),
            response(2, "ordinary", "t", "s", Some("gpt-6.1-sol"), 200),
        ],
        &Pricing::demo(),
    );
    let mut cache = serde_json::to_value(&store).unwrap();
    cache
        .as_object_mut()
        .unwrap()
        .remove("normalization_version");
    cache["observations"] = json!([]);
    cache["contexts"] = json!([]);
    for event in cache["events"].as_array_mut().unwrap() {
        event["partial_flags"] = json!([]);
    }
    let checkpoint = dir.path().join("event-only-state.json");
    fs::write(&checkpoint, cache.to_string()).unwrap();
    let mut loaded = TokenStore::load(&checkpoint).unwrap();
    assert_eq!(loaded.events().len(), 2);
    assert_eq!(loaded.events().iter().map(|e| e.input).sum::<u64>(), 300);
    assert_eq!(
        loaded
            .events()
            .iter()
            .filter(|e| e.is_excluded_auto_review())
            .count(),
        1
    );
    loaded.reprice(&Pricing::official_standard());
    assert_eq!(loaded.events().len(), 2);
    assert_eq!(
        loaded
            .summary(None, None, &Pricing::official_standard())
            .input,
        200
    );
}
