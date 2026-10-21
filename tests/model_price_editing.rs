//! Public-API pricing regressions using only generated synthetic usage records.
//! Never imports real sessions, user data, or credentials.
use quota_widget::{
    storage::{load_json, save_json, Settings},
    tokens::{ModelPrice, Pricing, TokenEvent, TokenStore},
};
use serde_json::json;
use std::fs;
use tempfile::TempDir;

const SOL: &str = "gpt-6.1-sol";
const LUNA: &str = "gpt-6-luna";
const ASTRA: &str = "gpt-6-astra";
const CUSTOM: &str = "synthetic-unpublished-model";

fn rate(input: f64, cached: f64, output: f64) -> ModelPrice {
    ModelPrice {
        uncached_input_per_million: input,
        cached_input_per_million: cached,
        output_per_million: output,
    }
}

fn imported(models: &[&str], pricing: &Pricing) -> (TempDir, TokenStore) {
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let rows = models
        .iter()
        .enumerate()
        .map(|(index, model)| {
            json!({
                "timestamp": 1_790_000_000 + index as i64,
                "type": "token_usage_record",
                "payload": {
                    "thread_id": "synthetic-mixed-model-thread",
                    "turn_id": format!("turn-{index}"),
                    "response_id": format!("response-{index}"),
                    "model": model,
                    "usage": {
                        "input_tokens": 100_000,
                        "cached_input_tokens": 20_000,
                        "output_tokens": 10_000,
                        "reasoning_output_tokens": 5_000
                    }
                }
            })
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(dir.path().join("synthetic.jsonl"), rows + "\n").unwrap();
    let mut store = TokenStore::default();
    store.import_dir(dir.path(), pricing).unwrap();
    assert_eq!(store.events().len(), models.len());
    (dir, store)
}

fn event<'a>(store: &'a TokenStore, model: &str) -> &'a TokenEvent {
    store.events().iter().find(|e| e.model == model).unwrap()
}

fn assert_cost(actual: Option<f64>, expected: f64) {
    assert!((actual.expect("event should have an explicit price") - expected).abs() < 1e-12);
}

#[test]
fn editing_luna_rates_changes_only_luna_events_in_a_mixed_model_thread() {
    let mut pricing = Pricing::official_standard();
    let (_dir, mut store) = imported(&[SOL, LUNA, ASTRA], &pricing);
    let before = store.events().to_vec();
    let previous_rates = pricing.rates.clone();

    pricing.set_rate(LUNA, rate(7., 2., 17.)).unwrap();
    store.reprice(&pricing);

    assert_eq!(pricing.rates[SOL], previous_rates[SOL]);
    assert_eq!(pricing.rates[ASTRA], previous_rates[ASTRA]);
    assert!(pricing.model_override.is_none());
    assert_eq!(store.observation_count(), 3);
    for old in before {
        let current = event(&store, &old.model);
        let mut expected = old.clone();
        if old.model == LUNA {
            assert_ne!(old.cost_usd, current.cost_usd);
            assert_cost(current.cost_usd, 0.77);
            expected.cost_usd = current.cost_usd;
        }
        assert_eq!(
            current, &expected,
            "editing a rate must preserve attribution and tokens"
        );
    }
    let summary = store.summary(None, None, &pricing);
    assert_eq!(summary.event_count, 3);
    assert_eq!(summary.unpriced_events, 0);
    assert_cost(summary.cost_usd, 0.262 + 0.77 + 1.32);
}

#[test]
fn explicitly_recorded_unpublished_model_accepts_its_own_custom_rates() {
    let mut pricing = Pricing::official_standard();
    let (_dir, mut store) = imported(&[SOL, CUSTOM], &pricing);
    let sol_before = event(&store, SOL).clone();
    let unknown_before = event(&store, CUSTOM).clone();
    assert!(!unknown_before.model_assumed);
    assert!(unknown_before.cost_usd.is_none());
    assert_eq!(store.summary(None, None, &pricing).unpriced_events, 1);
    assert!(store.summary(None, None, &pricing).cost_usd.is_none());

    pricing.set_rate(CUSTOM, rate(3., 0.5, 9.)).unwrap();
    store.reprice(&pricing);

    assert_eq!(event(&store, SOL), &sol_before);
    let mut expected = unknown_before;
    expected.cost_usd = Some(0.34);
    assert_eq!(event(&store, CUSTOM), &expected);
    assert!(pricing.model_override.is_none());
    assert_eq!(store.summary(None, None, &pricing).unpriced_events, 0);
    assert_cost(store.summary(None, None, &pricing).cost_usd, 0.262 + 0.34);
}

#[test]
fn settings_restart_and_recomputation_preserve_all_custom_rates_and_override() {
    let mut pricing = Pricing::official_standard();
    pricing.set_rate(LUNA, rate(7., 2., 17.)).unwrap();
    pricing.set_rate(ASTRA, rate(11., 3., 23.)).unwrap();
    pricing.set_rate(CUSTOM, rate(3., 0.5, 9.)).unwrap();
    pricing.model_override = Some(LUNA.into());
    let (dir, store) = imported(&[SOL, LUNA, ASTRA, CUSTOM], &pricing);
    let settings = Settings {
        sessions_path: dir.path().to_string_lossy().into_owned(),
        pricing,
        ..Settings::default()
    };
    let settings_path = dir.path().join("settings.json");
    let store_path = dir.path().join("tokens.json");
    save_json(&settings_path, &settings).unwrap();
    store.save(&store_path).unwrap();

    let restored: Settings = load_json(&settings_path).unwrap();
    assert_eq!(restored.pricing, settings.pricing);
    let mut restarted = TokenStore::load(&store_path).unwrap();
    assert_eq!(restarted.events(), store.events());
    restarted.reprice(&restored.pricing);
    for current in restarted.events() {
        assert_eq!(current.model, LUNA);
        assert_cost(current.cost_usd, 0.77);
    }

    // Force reconstruction from retained observations without the source file.
    fs::remove_file(dir.path().join("synthetic.jsonl")).unwrap();
    let mut recorded = restored.pricing.clone();
    recorded.model_override = None;
    restarted.reprice(&recorded);
    assert_eq!(restarted.observation_count(), 4);
    for (model, expected) in [(SOL, 0.262), (LUNA, 0.77), (ASTRA, 1.17), (CUSTOM, 0.34)] {
        let current = event(&restarted, model);
        assert!(!current.model_assumed);
        assert_cost(current.cost_usd, expected);
    }
    assert_cost(restarted.summary(None, None, &recorded).cost_usd, 2.542);

    // Reapplying the persisted override must not discard the other model rates.
    restarted.reprice(&restored.pricing);
    assert_eq!(restarted.events(), store.events());
    restarted.save(&store_path).unwrap();
    let mut restarted_again = TokenStore::load(&store_path).unwrap();
    assert_eq!(restarted_again.events(), restarted.events());
    restarted_again.reprice(&recorded);
    assert_cost(event(&restarted_again, ASTRA).cost_usd, 1.17);
    assert_cost(event(&restarted_again, CUSTOM).cost_usd, 0.34);
    assert_eq!(
        load_json::<Settings>(&settings_path).unwrap().pricing,
        settings.pricing
    );
}
