//! Incremental, local-only Codex JSONL usage import. No authentication is read.
//!
//! Offsets, normalized observations and request identities survive restarts. We
//! reconcile all retained observations chronologically, so an older archive found
//! later does not silently turn into a counter reset. The 30-day window is a query
//! default, never a retention/deletion policy. Prices are deliberately unconfigured.
use chrono::{DateTime, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const DEFAULT_ASSUMED_MODEL: &str = "gpt-6.1-sol";
pub const DEFAULT_BROWSE_DAYS: i64 = 30;
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const STORE_VERSION: u32 = 3;
// Independent of the storage schema: rebuild derived model/effort attribution once when
// opening old caches, without dropping raw observations or trusting old offsets.
const NORMALIZATION_VERSION: u32 = 3;
const AUTO_REVIEW_MODEL: &str = "codex-auto-review";
const AUTO_REVIEW_EXCLUDED: &str = "excluded_recorded_codex_auto_review";
const UNATTRIBUTED_MODEL: &str = "unattributed-model";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelPrice {
    /// USD per one million *uncached* input tokens.
    pub uncached_input_per_million: f64,
    /// USD per one million cached input tokens.
    pub cached_input_per_million: f64,
    /// USD per one million output tokens, including reasoning output.
    pub output_per_million: f64,
}
impl ModelPrice {
    pub fn is_valid(&self) -> bool {
        [
            self.uncached_input_per_million,
            self.cached_input_per_million,
            self.output_per_million,
        ]
        .iter()
        .all(|v| v.is_finite() && *v >= 0.0)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Pricing {
    /// Empty means UNCONFIGURED, not free. Never populated with invented rates.
    pub rates: BTreeMap<String, ModelPrice>,
    /// An explicit user-selected model attribution override. None uses recorded models.
    pub model_override: Option<String>,
    pub assumed_model: String,
    /// Global API-equivalent service-tier assumption, never subscription billing.
    pub tier_label: String,
    pub apply_long_context: bool,
    pub verified_at: Option<String>,
}
impl Default for Pricing {
    fn default() -> Self {
        Self {
            rates: BTreeMap::new(),
            model_override: None,
            assumed_model: DEFAULT_ASSUMED_MODEL.into(),
            tier_label: "未設定／自訂".into(),
            apply_long_context: false,
            verified_at: None,
        }
    }
}
impl Pricing {
    /// Fictional screenshot/test prices; never a provider rate claim.
    pub fn demo() -> Self {
        let mut pricing = Pricing {
            tier_label: "DEMO synthetic prices".into(),
            ..Self::default()
        };
        // Fictional demonstration rates, deliberately not claimed to be current provider prices.
        pricing.rates.insert(
            "gpt-6.1-sol".into(),
            ModelPrice {
                uncached_input_per_million: 1.0,
                cached_input_per_million: 0.25,
                output_per_million: 2.0,
            },
        );
        pricing.rates.insert(
            "gpt-6-luna".into(),
            ModelPrice {
                uncached_input_per_million: 0.5,
                cached_input_per_million: 0.1,
                output_per_million: 1.0,
            },
        );
        pricing
    }
    /// Official API rates checked 2026-10-17. Standard is an explicit assumption
    /// when the log does not expose a trustworthy service tier. Preserves no secrets.
    pub fn official_standard() -> Self {
        let mut p = Self {
            tier_label: "Standard（假設）".into(),
            apply_long_context: true,
            verified_at: Some("2026-10-17".into()),
            ..Self::default()
        };
        p.rates.insert(
            "gpt-6.1-sol".into(),
            ModelPrice {
                uncached_input_per_million: 2.0,
                cached_input_per_million: 0.10,
                output_per_million: 10.0,
            },
        );
        p.rates.insert(
            "gpt-6-luna".into(),
            ModelPrice {
                uncached_input_per_million: 0.10,
                cached_input_per_million: 0.01,
                output_per_million: 0.50,
            },
        );
        p.rates.insert(
            "gpt-6-astra".into(),
            ModelPrice {
                uncached_input_per_million: 10.0,
                cached_input_per_million: 1.0,
                output_per_million: 50.0,
            },
        );
        p
    }
    pub fn official_fast() -> Self {
        let mut p = Self::official_standard();
        p.tier_label = "Fast / Priority（假設）".into();
        for r in p.rates.values_mut() {
            r.uncached_input_per_million *= 2.;
            r.cached_input_per_million *= 2.;
            r.output_per_million *= 2.;
        }
        p
    }
    pub fn set_rate(&mut self, model: impl Into<String>, rate: ModelPrice) -> Result<(), String> {
        let model = model.into();
        if model.trim().is_empty() || !rate.is_valid() {
            return Err("Model and nonnegative, finite USD-per-million rates are required".into());
        }
        self.rates.insert(model, rate);
        Ok(())
    }
    pub fn cost(&self, model: &str, uncached: u64, cached: u64, output: u64) -> Option<f64> {
        let rate = self.rates.get(model)?;
        if !rate.is_valid() {
            return None;
        }
        let long = self.apply_long_context
            && matches!(model, "gpt-6.1-sol" | "gpt-6-luna" | "gpt-6-astra")
            && uncached.saturating_add(cached) > 272_000;
        let input_multiplier = if long { 2.0 } else { 1.0 };
        let output_multiplier = if long { 1.5 } else { 1.0 };
        let cost = (((uncached as f64 * rate.uncached_input_per_million)
            + (cached as f64 * rate.cached_input_per_million))
            * input_multiplier
            + (output as f64 * rate.output_per_million) * output_multiplier)
            / 1_000_000.0;
        cost.is_finite().then_some(cost)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TokenEvent {
    pub id: String,
    /// UTC Unix seconds. Import order internally retains millisecond precision.
    pub timestamp: i64,
    pub model: String,
    pub model_assumed: bool,
    /// Recorded request/turn effort, never a thread default or pricing multiplier.
    /// Missing or conflicting evidence remains unknown, independently of token counts.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Logical task/thread ID (`thread_id`, or legacy `session_meta.id`).
    /// This is never the root-session ID of a child thread.
    pub session: String,
    #[serde(default)]
    pub response_id: Option<String>,
    #[serde(default)]
    pub turn_id: Option<String>,
    /// `token_usage_record` is response-scoped; `token_count` is legacy fallback.
    #[serde(default)]
    pub source_kind: Option<String>,
    /// Full request context, including cached input, not merely new prompt text.
    pub input: u64,
    pub cached: u64,
    pub uncached: u64,
    /// Total output, including reasoning output.
    pub output: u64,
    /// A subset of output. Never added again for cost or total-token calculations.
    pub reasoning: u64,
    pub cost_usd: Option<f64>,
    /// Earliest explicit turn_context time, only when all recorded response
    /// counters exactly reconcile to a valid, monotone complete turn snapshot.
    /// This bounds old overlap uncertainty; it never invents a turn or usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage_start_timestamp_ms: Option<i64>,
    pub partial_flags: Vec<String>,
}
impl TokenEvent {
    pub fn total_tokens(&self) -> u64 {
        self.input.saturating_add(self.output)
    }
    pub fn is_partial(&self) -> bool {
        self.partial_flags
            .iter()
            .any(|flag| flag != AUTO_REVIEW_EXCLUDED)
    }
    /// Derived from the exact recorded model BEFORE model overrides. This audit
    /// marker survives saved caches and repricing; task names never classify usage.
    pub fn is_excluded_auto_review(&self) -> bool {
        self.partial_flags
            .iter()
            .any(|flag| flag == AUTO_REVIEW_EXCLUDED)
    }
}

/// An actually recorded quota snapshot, not a quota inferred from token totals.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImportedQuotaObservation {
    pub timestamp: i64,
    /// The actual `rate_limits` object, without a synthetic wrapper or modifications.
    pub payload: Value,
    /// Logical thread that logged the snapshot, not a claim of account ownership.
    #[serde(default)]
    pub origin_session: Option<String>,
    /// Stable hash of the explicitly selected source path; never a raw path.
    #[serde(default)]
    pub source_id: Option<String>,
    /// Log-write timestamp precision. This is NOT the quota provider's fetch time.
    #[serde(default)]
    pub timestamp_ms: Option<i64>,
}
/// Latest turn counters for a task tooltip. `None` means unavailable, never zero.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskTurnUsage {
    pub turn_id: Option<String>,
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub recorded_snapshot: bool,
    pub partial: bool,
    pub timestamp: Option<i64>,
}
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ImportReport {
    pub files_scanned: usize,
    /// Metadata-only fast path; no file was opened, hashed, or JSON-parsed.
    pub files_unchanged: usize,
    pub lines_read: usize,
    pub events_added: usize,
    pub duplicate_observations: usize,
    pub malformed_lines: usize,
    pub partial_lines: usize,
    pub source_resets: usize,
    pub warnings: Vec<String>,
    /// Newly imported real snapshots only. All retained snapshots are also queryable.
    pub quota_observations: Vec<ImportedQuotaObservation>,
}

impl ImportReport {
    fn malformed_json_line(&mut self, path: &Path, offset: u64, reason: impl std::fmt::Display) {
        const PREFIX: &str = "Skipped malformed JSONL line in ";
        self.malformed_lines += 1;
        // Share one five-sample budget across every source and the metadata
        // index. Once full, stop after finding those five; do not build or retain
        // an error string for every further bad line. Never include input bytes.
        if self
            .warnings
            .iter()
            .filter(|warning| warning.starts_with(PREFIX))
            .take(5)
            .count()
            < 5
        {
            self.warnings.push(format!(
                "{PREFIX}{} at byte {offset}: {reason}",
                path.display()
            ));
        }
    }
}

/// Small, transient progress only. Never clones history or exposes log content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportPhase {
    Discovering,
    Reading,
    Reconciling,
    Metadata,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportProgress {
    pub phase: ImportPhase,
    pub files_done: usize,
    pub files_total: usize,
    pub lines_read: usize,
    pub observations: usize,
    pub events: usize,
}
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TokenSummary {
    pub event_count: usize,
    pub input: u64,
    pub cached: u64,
    pub uncached: u64,
    pub output: u64,
    pub reasoning: u64,
    pub total_tokens: u64,
    pub partial_events: usize,
    pub assumed_model_events: usize,
    pub unpriced_events: usize,
    /// None if any included event is unpriced or attribution is uncertain.
    pub cost_usd: Option<f64>,
    /// Known priced portion, never presented as the complete cost.
    pub priced_cost_usd: f64,
}

#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Counts {
    input: u64,
    cached: u64,
    output: u64,
    reasoning: u64,
}
impl Counts {
    fn signature(self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.input, self.cached, self.output, self.reasoning
        )
    }
    fn is_zero(self) -> bool {
        self.input == 0 && self.output == 0
    }
    fn decreased_from(self, old: Self) -> bool {
        self.input < old.input
            || self.cached < old.cached
            || self.output < old.output
            || self.reasoning < old.reasoning
    }
    fn sub(self, old: Self) -> Self {
        Self {
            input: self.input.saturating_sub(old.input),
            cached: self.cached.saturating_sub(old.cached),
            output: self.output.saturating_sub(old.output),
            reasoning: self.reasoning.saturating_sub(old.reasoning),
        }
    }
    fn add(self, other: Self) -> Self {
        Self {
            input: self.input.saturating_add(other.input),
            cached: self.cached.saturating_add(other.cached),
            output: self.output.saturating_add(other.output),
            reasoning: self.reasoning.saturating_add(other.reasoning),
        }
    }
    fn normalized(mut self, flags: &mut Vec<String>) -> Self {
        if self.cached > self.input {
            self.cached = self.input;
            flag(flags, "cached_exceeded_input");
        }
        if self.reasoning > self.output {
            self.reasoning = self.output;
            flag(flags, "reasoning_exceeded_output");
        }
        self
    }
}
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct SourceCursor {
    offset: u64,
    generation: u64,
    prefix_len: usize,
    prefix_hash: u64,
    anchor_len: usize,
    anchor_hash: u64,
    session: Option<String>,
    turn_id: Option<String>,
    model: Option<String>,
    model_timestamp_ms: Option<i64>,
    model_turn_id: Option<String>,
    /// Captured only from a turn_context in this logical thread.
    effort_context: Option<EffortContext>,
    observed_len: u64,
    modified_ns: Option<u64>,
    file_identity: Option<String>,
    /// Unix ctime catches same-size rewrites even if mtime is deliberately restored.
    changed_ns: Option<i128>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RawTurnCounts {
    input: Option<u64>,
    output: Option<u64>,
    #[serde(default)]
    cached: Option<u64>,
    #[serde(default)]
    reasoning: Option<u64>,
}
impl RawTurnCounts {
    fn complete(&self) -> Option<Counts> {
        Some(Counts {
            input: self.input?,
            cached: self.cached?,
            output: self.output?,
            reasoning: self.reasoning?,
        })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UsageObservation {
    identity: String,
    timestamp_ms: i64,
    #[serde(default)]
    sequence: u64,
    session: String,
    request_id: Option<String>,
    #[serde(default)]
    response_id: Option<String>,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    source_kind: Option<String>,
    /// Cumulative snapshots are overlap evidence, never additive events.
    #[serde(default)]
    turn_total: Option<Counts>,
    /// Preserve missing raw turn fields separately from normalized arithmetic.
    #[serde(default)]
    raw_turn_counts: Option<RawTurnCounts>,
    #[serde(default)]
    thread_total: Option<Counts>,
    model: Option<String>,
    source_model: Option<String>,
    #[serde(default)]
    source_model_timestamp_ms: Option<i64>,
    #[serde(default)]
    source_model_turn_id: Option<String>,
    /// All explicitly recorded values survive duplicate enrichment and conflicts.
    #[serde(default)]
    reasoning_efforts: BTreeSet<String>,
    #[serde(default)]
    source_effort_contexts: Vec<EffortContext>,
    total: Option<Counts>,
    last: Option<Counts>,
    flags: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModelContext {
    timestamp_ms: i64,
    session: String,
    #[serde(default)]
    turn_id: Option<String>,
    model: String,
}

/// Separate from model defaults: absence in a new turn context is meaningful and
/// must not inherit a previous request's effort. Empty values therefore persist.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
struct EffortContext {
    timestamp_ms: i64,
    session: String,
    turn_id: Option<String>,
    model: Option<String>,
    values: BTreeSet<String>,
}

struct RecordedEfforts<'a> {
    turns: BTreeMap<(&'a str, &'a str), Vec<&'a EffortContext>>,
}
impl<'a> RecordedEfforts<'a> {
    fn new(contexts: &'a [EffortContext]) -> Self {
        let mut turns: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for context in contexts {
            if let Some(turn) = context.turn_id.as_deref() {
                turns
                    .entry((context.session.as_str(), turn))
                    .or_default()
                    .push(context);
            }
        }
        Self { turns }
    }

    fn context_values(&self, row: &UsageObservation, at: i64) -> BTreeSet<String> {
        let rows = row
            .turn_id
            .as_deref()
            .and_then(|turn| self.turns.get(&(row.session.as_str(), turn)));
        let end = rows.map_or(0, |rows| {
            rows.partition_point(|context| context.timestamp_ms <= at)
        });
        let latest = rows.and_then(|rows| end.checked_sub(1).map(|i| rows[i]));
        let sources: Vec<_> = row
            .source_effort_contexts
            .iter()
            .filter(|context| {
                context.session == row.session
                    && context.turn_id == row.turn_id
                    && context.timestamp_ms <= at
                    && latest.is_none_or(|latest| context.timestamp_ms >= latest.timestamp_ms)
            })
            .collect();
        let matches_model = |context: &EffortContext| {
            row.model.as_ref().is_none_or(|model| {
                context
                    .model
                    .as_ref()
                    .is_none_or(|recorded| recorded == model)
            })
        };
        // Captured file order resolves same-millisecond contexts. Without it,
        // disagreeing contexts at the latest timestamp stay unknown.
        if let Some(time) = sources.iter().map(|source| source.timestamp_ms).max() {
            let mut values = BTreeSet::new();
            for source in sources.iter().filter(|source| source.timestamp_ms == time) {
                if source.values.is_empty() || !matches_model(source) {
                    return BTreeSet::new();
                }
                values.extend(source.values.iter().cloned());
            }
            return values;
        }
        let Some(latest) = latest else {
            return BTreeSet::new();
        };
        let mut values = BTreeSet::new();
        for context in rows.unwrap()[..end]
            .iter()
            .rev()
            .take_while(|context| context.timestamp_ms == latest.timestamp_ms)
        {
            if context.values.is_empty() || !matches_model(context) {
                return BTreeSet::new();
            }
            values.extend(context.values.iter().cloned());
        }
        values
    }

    fn for_versions(&self, rows: &[&UsageObservation]) -> Option<String> {
        // An update that omits effort must not erase the exact response's prior
        // explicit value, nor pick up a setting changed after it was first seen.
        let explicit: BTreeSet<_> = rows
            .iter()
            .flat_map(|row| row.reasoning_efforts.iter().cloned())
            .collect();
        if !explicit.is_empty() {
            return (explicit.len() == 1).then(|| explicit.into_iter().next().unwrap());
        }
        let first = rows.iter().map(|row| row.timestamp_ms).min()?;
        let values: BTreeSet<_> = rows
            .iter()
            .filter(|row| row.timestamp_ms == first)
            .flat_map(|row| self.context_values(row, first))
            .collect();
        (values.len() == 1).then(|| values.into_iter().next().unwrap())
    }
}

/// Recorded attribution is independent of whichever pricing override is selected.
/// Per-turn evidence wins over a later unrelated turn's file cursor. Captured
/// source timestamps preserve same-millisecond ordering without trusting file order.
struct RecordedModels<'a> {
    sessions: BTreeMap<&'a str, Vec<&'a ModelContext>>,
    turns: BTreeMap<(&'a str, &'a str), Vec<&'a ModelContext>>,
}
impl<'a> RecordedModels<'a> {
    fn new(contexts: &'a [ModelContext]) -> Self {
        let mut result = Self {
            sessions: BTreeMap::new(),
            turns: BTreeMap::new(),
        };
        for context in contexts {
            result
                .sessions
                .entry(&context.session)
                .or_default()
                .push(context);
            if let Some(turn) = context.turn_id.as_deref() {
                result
                    .turns
                    .entry((&context.session, turn))
                    .or_default()
                    .push(context);
            }
        }
        result
    }
    fn recorded<'b>(&'b self, row: &'b UsageObservation) -> Option<&'b str> {
        if let Some(model) = row.model.as_deref() {
            return Some(model);
        }
        fn latest<'a>(rows: &[&'a ModelContext], timestamp_ms: i64) -> Option<&'a ModelContext> {
            rows.partition_point(|context| context.timestamp_ms <= timestamp_ms)
                .checked_sub(1)
                .map(|index| rows[index])
        }
        let turn_context = row
            .turn_id
            .as_deref()
            .and_then(|turn| self.turns.get(&(row.session.as_str(), turn)))
            .and_then(|rows| latest(rows, row.timestamp_ms));
        let source_matches_turn =
            row.source_model_turn_id.is_none() || row.source_model_turn_id == row.turn_id;
        if let Some(context) = turn_context {
            // Explicit turn context is stronger than a cursor belonging to some
            // other turn. For ties, source order identifies the actual request.
            if source_matches_turn
                && row.source_model.is_some()
                && (row
                    .source_model_timestamp_ms
                    .is_some_and(|time| time >= context.timestamp_ms)
                    || (row.source_model_timestamp_ms.is_none()
                        && row.source_model.as_deref() == Some(context.model.as_str())))
            {
                return row.source_model.as_deref();
            }
            return Some(&context.model);
        }
        let context = self
            .sessions
            .get(row.session.as_str())
            .and_then(|rows| latest(rows, row.timestamp_ms));
        if source_matches_turn {
            if let Some(source) = row.source_model.as_deref() {
                if context.is_none_or(|context| {
                    row.source_model_timestamp_ms
                        .is_none_or(|time| time >= context.timestamp_ms)
                }) {
                    return Some(source);
                }
            }
        }
        context.map(|context| context.model.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecordedTaskName {
    name: String,
    timestamp_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RecordedTaskParent {
    /// None preserves explicitly conflicting evidence without inventing an edge.
    parent: Option<String>,
    timestamp_ms: Option<i64>,
}

/// Compact, typed global state for the incremental database backend. Raw usage
/// stays in thread shards. This header retains quota evidence and parser cursors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreHeader {
    version: u32,
    normalization_version: u32,
    sources: BTreeMap<String, SourceCursor>,
    recorded_task_models: BTreeMap<String, BTreeSet<String>>,
    recorded_names: BTreeMap<String, RecordedTaskName>,
    recorded_parents: BTreeMap<String, RecordedTaskParent>,
    metadata_path: Option<String>,
    metadata_cursor: SourceCursor,
    metadata_names: BTreeMap<String, String>,
    pub revision: u64,
    pub next_sequence: u64,
    pub pricing: Option<Pricing>,
    pub quota: Vec<ImportedQuotaObservation>,
    pub response_threads: BTreeSet<String>,
    pub anonymous_threads: BTreeSet<String>,
    /// Latest raw legacy observation per anonymous owner. Missing entries retain
    /// unbounded uncertainty when reading an older header.
    #[serde(default)]
    pub anonymous_bounds_ms: BTreeMap<String, i64>,
    #[serde(default)]
    excluded_tasks: BTreeSet<String>,
}
impl StoreHeader {
    /// Apply the only cross-thread usage dependency without rewriting all rows.
    /// False means that anonymous legacy usage is retained for audit but is not
    /// additive alongside authoritative responses with unprovable ownership.
    pub fn apply_event_policy(&self, event: &mut TokenEvent) -> bool {
        self.event_policy().apply(event)
    }
    /// Bulk queries should prepare this once and reuse it for every event.
    pub fn event_policy(&self) -> EventPolicy<'_> {
        EventPolicy::new(
            &self.response_threads,
            &self.anonymous_threads,
            &self.anonymous_bounds_ms,
        )
    }
    pub fn same_event_policy(&self, other: &Self) -> bool {
        self.response_threads == other.response_threads
            && self.anonymous_threads == other.anonymous_threads
            && self.anonymous_bounds_ms == other.anonymous_bounds_ms
    }
    pub fn task_names(&self) -> BTreeMap<String, String> {
        let mut names: BTreeMap<_, _> = self
            .recorded_names
            .iter()
            .map(|(id, row)| (id.clone(), row.name.clone()))
            .collect();
        names.extend(self.metadata_names.clone());
        names
    }
    pub fn task_parents(&self) -> BTreeMap<String, String> {
        self.recorded_parents
            .iter()
            .filter_map(|(child, row)| {
                row.parent
                    .as_ref()
                    .filter(|parent| {
                        !self.excluded_tasks.contains(child)
                            && !self.excluded_tasks.contains(*parent)
                    })
                    .map(|parent| (child.clone(), parent.clone()))
            })
            .collect()
    }
    pub fn excluded_task_ids(&self) -> &BTreeSet<String> {
        &self.excluded_tasks
    }
    /// Rate/tier changes do not alter recorded attribution. An override/default
    /// model change requires a coherent all-shard projection before publication.
    pub fn requires_attribution_rebuild(&self, pricing: &Pricing) -> bool {
        let default = Pricing::default();
        let old = self.pricing.as_ref().unwrap_or(&default);
        self.normalization_version != NORMALIZATION_VERSION
            || old.model_override != pricing.model_override
            || old.assumed_model != pricing.assumed_model
    }
}

/// Lossless thread accounting evidence, plus its locally normalized cache.
/// Event IDs/observation identity strings retain their exact legacy tie ordering.
/// `events` do not include the header's cross-thread anonymous-overlap overlay.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ThreadShard {
    pub thread_id: String,
    #[serde(default)]
    observations: Vec<UsageObservation>,
    #[serde(default)]
    contexts: Vec<ModelContext>,
    #[serde(default)]
    effort_contexts: Vec<EffortContext>,
    #[serde(default)]
    pub events: Vec<TokenEvent>,
    #[serde(default)]
    pub turn_usage: Option<TaskTurnUsage>,
}
impl ThreadShard {
    pub fn turn_usage(&self) -> Option<&TaskTurnUsage> {
        self.turn_usage.as_ref()
    }
    pub fn observation_count(&self) -> usize {
        self.observations.len()
    }
}

type ThreadLoader<'a> = dyn FnMut(&str) -> io::Result<Option<ThreadShard>> + 'a;

/// Nondestructive export. A failed database transaction can retry this exact
/// state; only `mark_committed` is allowed to evict hydrated raw observations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirtyExport {
    pub header: StoreHeader,
    pub threads: Vec<ThreadShard>,
}

fn anonymous_bounds(
    owners: &BTreeSet<String>,
    observations: &[UsageObservation],
    events: &[TokenEvent],
) -> BTreeMap<String, i64> {
    let mut bounds: BTreeMap<String, i64> = BTreeMap::new();
    for row in observations.iter().filter(|row| {
        owners.contains(&row.session)
            && row
                .flags
                .iter()
                .any(|flag| flag == "session_identity_missing")
    }) {
        bounds
            .entry(row.session.clone())
            .and_modify(|time| *time = (*time).max(row.timestamp_ms))
            .or_insert(row.timestamp_ms);
    }
    // Events-only compatibility caches have second precision. Preserve the
    // entire final second rather than inventing a millisecond cutoff.
    let raw_owners: BTreeSet<_> = bounds.keys().cloned().collect();
    for event in events.iter().filter(|event| {
        owners.contains(&event.session)
            && !raw_owners.contains(&event.session)
            && event
                .partial_flags
                .iter()
                .any(|flag| flag == "session_identity_missing")
    }) {
        let end = event.timestamp.saturating_mul(1000).saturating_add(999);
        bounds
            .entry(event.session.clone())
            .and_modify(|time| *time = (*time).max(end))
            .or_insert(end);
    }
    bounds
}

/// Prepare once per query/reconciliation pass. Anonymous-bound lookup is O(A)
/// once, then authoritative event policy is O(1), not O(events * A).
pub struct EventPolicy<'a> {
    response_present: bool,
    anonymous_threads: &'a BTreeSet<String>,
    anonymous_bound_ms: Option<i64>,
}
#[cfg(test)]
thread_local! {
    static POLICY_BOUND_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
impl<'a> EventPolicy<'a> {
    fn new(
        response_threads: &BTreeSet<String>,
        anonymous_threads: &'a BTreeSet<String>,
        anonymous_bounds_ms: &BTreeMap<String, i64>,
    ) -> Self {
        let anonymous_bound_ms = anonymous_threads.iter().try_fold(i64::MIN, |bound, id| {
            #[cfg(test)]
            POLICY_BOUND_LOOKUPS.with(|count| count.set(count.get() + 1));
            anonymous_bounds_ms.get(id).map(|value| bound.max(*value))
        });
        Self {
            response_present: !response_threads.is_empty(),
            anonymous_threads,
            anonymous_bound_ms,
        }
    }
    pub fn apply(&self, event: &mut TokenEvent) -> bool {
        if !self.response_present {
            return true;
        }
        if event.source_kind.as_deref() != Some("token_usage_record") {
            return !(self.anonymous_threads.contains(&event.session)
                && event
                    .partial_flags
                    .iter()
                    .any(|f| f == "session_identity_missing"));
        }
        let proven_disjoint = self
            .anonymous_bound_ms
            .zip(event.coverage_start_timestamp_ms)
            .is_some_and(|(bound, start)| start > bound);
        if !self.anonymous_threads.is_empty() && !proven_disjoint {
            flag(
                &mut event.partial_flags,
                "mixed_schema_legacy_overlap_suppressed",
            );
        }
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenStore {
    version: u32,
    #[serde(default)]
    normalization_version: u32,
    recorded_task_models: BTreeMap<String, BTreeSet<String>>,
    sources: BTreeMap<String, SourceCursor>,
    observations: Vec<UsageObservation>,
    contexts: Vec<ModelContext>,
    effort_contexts: Vec<EffortContext>,
    events: Vec<TokenEvent>,
    quota: Vec<ImportedQuotaObservation>,
    last_pricing: Option<Pricing>,
    revision: u64,
    /// Global ingestion ordering survives lazy hydration and restarts.
    #[serde(default)]
    next_sequence: u64,
    /// Disk revision before any in-memory migration or derived-cache repair.
    #[serde(skip)]
    persisted_revision_at_load: Option<u64>,
    recorded_names: BTreeMap<String, RecordedTaskName>,
    recorded_parents: BTreeMap<String, RecordedTaskParent>,
    metadata_path: Option<String>,
    metadata_cursor: SourceCursor,
    metadata_names: BTreeMap<String, String>,
    // Rebuild once, lazily, after deserialization. Never clone all history each poll.
    #[serde(skip)]
    indexes_ready: bool,
    #[serde(skip)]
    known_observations: HashMap<String, usize>,
    #[serde(skip)]
    known_quota: BTreeSet<String>,
    #[serde(skip)]
    legacy_quota: BTreeMap<String, usize>,
    #[serde(skip)]
    mixed_schema_warning: bool,
    #[serde(skip)]
    usage_dirty: bool,
    #[serde(skip)]
    database_mode: bool,
    #[serde(skip)]
    hydrated_threads: BTreeSet<String>,
    #[serde(skip)]
    dirty_threads: BTreeSet<String>,
    #[serde(skip)]
    local_events: BTreeMap<String, Vec<TokenEvent>>,
    #[serde(skip)]
    response_threads: BTreeSet<String>,
    #[serde(skip)]
    anonymous_threads: BTreeSet<String>,
    #[serde(skip)]
    anonymous_bounds_ms: BTreeMap<String, i64>,
    #[serde(skip)]
    anonymous_policy_bound_ms: Option<i64>,
    #[serde(skip)]
    excluded_task_cache: BTreeSet<String>,
}
impl Default for TokenStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            normalization_version: NORMALIZATION_VERSION,
            recorded_task_models: BTreeMap::new(),
            sources: BTreeMap::new(),
            observations: Vec::new(),
            contexts: Vec::new(),
            effort_contexts: Vec::new(),
            events: Vec::new(),
            quota: Vec::new(),
            last_pricing: None,
            revision: 0,
            next_sequence: 0,
            persisted_revision_at_load: None,
            recorded_names: BTreeMap::new(),
            recorded_parents: BTreeMap::new(),
            metadata_path: None,
            metadata_cursor: SourceCursor::default(),
            metadata_names: BTreeMap::new(),
            indexes_ready: false,
            known_observations: HashMap::new(),
            known_quota: BTreeSet::new(),
            legacy_quota: BTreeMap::new(),
            mixed_schema_warning: false,
            usage_dirty: false,
            database_mode: false,
            hydrated_threads: BTreeSet::new(),
            dirty_threads: BTreeSet::new(),
            local_events: BTreeMap::new(),
            response_threads: BTreeSet::new(),
            anonymous_threads: BTreeSet::new(),
            anonymous_bounds_ms: BTreeMap::new(),
            anonymous_policy_bound_ms: None,
            excluded_task_cache: BTreeSet::new(),
        }
    }
}
impl TokenStore {
    /// Start a lazy database-backed importer without loading historical usage.
    pub fn from_header(header: StoreHeader) -> Self {
        let anonymous_policy_bound_ms = header.event_policy().anonymous_bound_ms;
        Self {
            version: header.version,
            normalization_version: header.normalization_version,
            sources: header.sources,
            recorded_task_models: header.recorded_task_models,
            recorded_names: header.recorded_names,
            recorded_parents: header.recorded_parents,
            metadata_path: header.metadata_path,
            metadata_cursor: header.metadata_cursor,
            metadata_names: header.metadata_names,
            revision: header.revision,
            next_sequence: header.next_sequence,
            last_pricing: header.pricing,
            quota: header.quota,
            response_threads: header.response_threads,
            anonymous_threads: header.anonymous_threads,
            anonymous_bounds_ms: header.anonymous_bounds_ms,
            anonymous_policy_bound_ms,
            excluded_task_cache: header.excluded_tasks,
            database_mode: true,
            ..Self::default()
        }
    }

    pub fn header(&self) -> StoreHeader {
        let (response_threads, anonymous_threads) = if self.database_mode {
            (
                self.response_threads.clone(),
                self.anonymous_threads.clone(),
            )
        } else {
            let responses: BTreeSet<_> = self
                .observations
                .iter()
                .filter(|r| r.source_kind.as_deref() == Some("token_usage_record"))
                .map(|r| r.session.clone())
                .chain(
                    self.events
                        .iter()
                        .filter(|e| e.source_kind.as_deref() == Some("token_usage_record"))
                        .map(|e| e.session.clone()),
                )
                .collect();
            let candidates: BTreeSet<_> = self
                .observations
                .iter()
                .filter(|r| r.flags.iter().any(|f| f == "session_identity_missing"))
                .map(|r| r.session.clone())
                .collect();
            let mut anonymous = BTreeSet::new();
            // Only anonymous candidates require local replay to recover evidence
            // that the global normalizer intentionally suppressed.
            let mut candidate_shards: BTreeMap<String, ThreadShard> = BTreeMap::new();
            if !candidates.is_empty() {
                for row in self
                    .observations
                    .iter()
                    .filter(|row| candidates.contains(&row.session))
                {
                    candidate_shards
                        .entry(row.session.clone())
                        .or_insert_with(|| ThreadShard {
                            thread_id: row.session.clone(),
                            ..ThreadShard::default()
                        })
                        .observations
                        .push(row.clone());
                }
                for row in self
                    .contexts
                    .iter()
                    .filter(|row| candidates.contains(&row.session))
                {
                    if let Some(shard) = candidate_shards.get_mut(&row.session) {
                        shard.contexts.push(row.clone());
                    }
                }
                for row in self
                    .effort_contexts
                    .iter()
                    .filter(|row| candidates.contains(&row.session))
                {
                    if let Some(shard) = candidate_shards.get_mut(&row.session) {
                        shard.effort_contexts.push(row.clone());
                    }
                }
            }
            for mut shard in candidate_shards.into_values() {
                self.normalize_shard(
                    &mut shard,
                    self.last_pricing.as_ref().unwrap_or(&Pricing::default()),
                );
                if !responses.contains(&shard.thread_id)
                    && shard.events.iter().any(|e| {
                        e.partial_flags
                            .iter()
                            .any(|f| f == "session_identity_missing")
                    })
                {
                    anonymous.insert(shard.thread_id);
                }
            }
            (responses, anonymous)
        };
        let anonymous_bounds_ms = if self.database_mode {
            self.anonymous_bounds_ms.clone()
        } else {
            anonymous_bounds(&anonymous_threads, &self.observations, &self.events)
        };
        StoreHeader {
            version: self.version,
            normalization_version: self.normalization_version,
            sources: self.sources.clone(),
            recorded_task_models: self.recorded_task_models.clone(),
            recorded_names: self.recorded_names.clone(),
            recorded_parents: self.recorded_parents.clone(),
            metadata_path: self.metadata_path.clone(),
            metadata_cursor: self.metadata_cursor.clone(),
            metadata_names: self.metadata_names.clone(),
            revision: self.revision,
            next_sequence: self.next_sequence.max(
                self.observations
                    .iter()
                    .map(|row| row.sequence.saturating_add(1))
                    .max()
                    .unwrap_or(0),
            ),
            pricing: self.last_pricing.clone(),
            quota: self.quota.clone(),
            response_threads,
            anonymous_threads,
            anonymous_bounds_ms,
            excluded_tasks: if self.database_mode {
                self.excluded_task_cache.clone()
            } else {
                self.excluded_auto_review_tasks()
            },
        }
    }

    fn collect_loaded_shards(&self, include_metadata: bool) -> Vec<ThreadShard> {
        fn shard<'a>(map: &'a mut BTreeMap<String, ThreadShard>, id: &str) -> &'a mut ThreadShard {
            map.entry(id.to_owned()).or_insert_with(|| ThreadShard {
                thread_id: id.to_owned(),
                ..ThreadShard::default()
            })
        }
        let mut shards = BTreeMap::new();
        for row in &self.observations {
            shard(&mut shards, &row.session)
                .observations
                .push(row.clone());
        }
        for row in &self.contexts {
            shard(&mut shards, &row.session).contexts.push(row.clone());
        }
        for row in &self.effort_contexts {
            shard(&mut shards, &row.session)
                .effort_contexts
                .push(row.clone());
        }
        if self.database_mode {
            for (thread, events) in &self.local_events {
                shard(&mut shards, thread).events = events.clone();
            }
            for thread in &self.hydrated_threads {
                shard(&mut shards, thread);
            }
        } else {
            for event in &self.events {
                shard(&mut shards, &event.session)
                    .events
                    .push(event.clone());
            }
        }
        if include_metadata {
            for id in self
                .sources
                .values()
                .filter_map(|cursor| cursor.session.as_deref())
            {
                shard(&mut shards, id);
            }
            for id in self
                .recorded_task_models
                .keys()
                .chain(self.recorded_names.keys())
                .chain(self.recorded_parents.keys())
                .chain(self.metadata_names.keys())
            {
                shard(&mut shards, id);
            }
            for parent in self
                .recorded_parents
                .values()
                .filter_map(|row| row.parent.as_deref())
            {
                shard(&mut shards, parent);
            }
        }
        shards.into_values().collect()
    }

    fn normalize_shard(&self, shard: &mut ThreadShard, pricing: &Pricing) {
        let mut local = Self {
            observations: std::mem::take(&mut shard.observations),
            contexts: std::mem::take(&mut shard.contexts),
            effort_contexts: std::mem::take(&mut shard.effort_contexts),
            events: std::mem::take(&mut shard.events),
            ..Self::default()
        };
        if let Some(models) = self.recorded_task_models.get(&shard.thread_id) {
            local
                .recorded_task_models
                .insert(shard.thread_id.clone(), models.clone());
        }
        local.reconcile_full(pricing);
        shard.turn_usage = local.included_task_turn_usage().remove(&shard.thread_id);
        shard.observations = local.observations;
        shard.contexts = local.contexts;
        shard.effort_contexts = local.effort_contexts;
        shard.events = local.events;
    }

    /// One-time migration/export. Group in one pass, including metadata-only
    /// tasks and events-only legacy caches; never serialize the whole store to Value.
    pub fn thread_shards(&self) -> Vec<ThreadShard> {
        let mut shards = self.collect_loaded_shards(!self.database_mode);
        let pricing = self.last_pricing.clone().unwrap_or_default();
        for shard in &mut shards {
            self.normalize_shard(shard, &pricing);
        }
        shards
    }

    /// Consuming migration path: move retained evidence instead of making a
    /// second complete in-memory copy. Capture `header()` before this call.
    /// Parsing the source JSON itself is still a separate migration concern.
    pub fn into_thread_shards(mut self) -> Vec<ThreadShard> {
        fn shard<'a>(map: &'a mut BTreeMap<String, ThreadShard>, id: &str) -> &'a mut ThreadShard {
            map.entry(id.to_owned()).or_insert_with(|| ThreadShard {
                thread_id: id.to_owned(),
                ..ThreadShard::default()
            })
        }
        let mut shards = BTreeMap::new();
        for row in std::mem::take(&mut self.observations) {
            shard(&mut shards, &row.session).observations.push(row);
        }
        for row in std::mem::take(&mut self.contexts) {
            shard(&mut shards, &row.session).contexts.push(row);
        }
        for row in std::mem::take(&mut self.effort_contexts) {
            shard(&mut shards, &row.session).effort_contexts.push(row);
        }
        if self.database_mode {
            for (thread, events) in std::mem::take(&mut self.local_events) {
                shard(&mut shards, &thread).events = events;
            }
        } else {
            for event in std::mem::take(&mut self.events) {
                shard(&mut shards, &event.session).events.push(event);
            }
        }
        for id in self
            .sources
            .values()
            .filter_map(|cursor| cursor.session.as_deref())
        {
            shard(&mut shards, id);
        }
        for id in self
            .recorded_task_models
            .keys()
            .chain(self.recorded_names.keys())
            .chain(self.recorded_parents.keys())
            .chain(self.metadata_names.keys())
        {
            shard(&mut shards, id);
        }
        for parent in self
            .recorded_parents
            .values()
            .filter_map(|row| row.parent.as_deref())
        {
            shard(&mut shards, parent);
        }
        let pricing = self.last_pricing.clone().unwrap_or_default();
        let mut result: Vec<_> = shards.into_values().collect();
        for shard in &mut result {
            self.normalize_shard(shard, &pricing);
        }
        result
    }

    pub fn hydrate_thread(&mut self, shard: ThreadShard) -> io::Result<()> {
        if !self.database_mode {
            return Err(invalid_data("Thread hydration requires database mode"));
        }
        if self.hydrated_threads.contains(&shard.thread_id) {
            return Ok(());
        }
        if shard
            .observations
            .iter()
            .any(|r| r.session != shard.thread_id)
            || shard.contexts.iter().any(|r| r.session != shard.thread_id)
            || shard
                .effort_contexts
                .iter()
                .any(|r| r.session != shard.thread_id)
            || shard.events.iter().any(|r| r.session != shard.thread_id)
        {
            return Err(invalid_data("Thread shard contains another logical thread"));
        }
        self.ensure_indexes();
        // Validate the complete shard before mutating any importer state. A bad
        // row halfway through must not create a partial replacement on retry.
        let mut identities = BTreeSet::new();
        if shard.observations.iter().any(|row| {
            self.known_observations.contains_key(&row.identity) || !identities.insert(&row.identity)
        }) {
            return Err(invalid_data(
                "Thread shard contains duplicate observation identities",
            ));
        }
        for row in shard.observations {
            self.next_sequence = self.next_sequence.max(row.sequence.saturating_add(1));
            let at = self.observations.len();
            self.known_observations.insert(row.identity.clone(), at);
            self.observations.push(row);
        }
        self.contexts.extend(shard.contexts);
        self.effort_contexts.extend(shard.effort_contexts);
        let policy = EventPolicy {
            response_present: !self.response_threads.is_empty(),
            anonymous_threads: &self.anonymous_threads,
            anonymous_bound_ms: self.anonymous_policy_bound_ms,
        };
        for event in &shard.events {
            let mut event = event.clone();
            if policy.apply(&mut event) {
                self.events.push(event);
            }
        }
        self.local_events
            .insert(shard.thread_id.clone(), shard.events);
        self.hydrated_threads.insert(shard.thread_id);
        Ok(())
    }

    fn reconcile_hydrated(&mut self, pricing: &Pricing) {
        let mut shards = self.collect_loaded_shards(false);
        for shard in &mut shards {
            self.normalize_shard(shard, pricing);
            let response = shard
                .observations
                .iter()
                .any(|r| r.source_kind.as_deref() == Some("token_usage_record"))
                || shard
                    .events
                    .iter()
                    .any(|e| e.source_kind.as_deref() == Some("token_usage_record"));
            if response {
                self.response_threads.insert(shard.thread_id.clone());
            } else {
                self.response_threads.remove(&shard.thread_id);
            }
            let anonymous = !response
                && shard.events.iter().any(|e| {
                    e.partial_flags
                        .iter()
                        .any(|f| f == "session_identity_missing")
                });
            if anonymous {
                self.anonymous_threads.insert(shard.thread_id.clone());
                self.anonymous_bounds_ms.extend(anonymous_bounds(
                    &BTreeSet::from([shard.thread_id.clone()]),
                    &shard.observations,
                    &shard.events,
                ));
            } else {
                self.anonymous_threads.remove(&shard.thread_id);
                self.anonymous_bounds_ms.remove(&shard.thread_id);
            }
            let mut models = self
                .recorded_task_models
                .get(&shard.thread_id)
                .cloned()
                .unwrap_or_default();
            models.extend(shard.contexts.iter().map(|context| context.model.clone()));
            let only_review_models = models.len() == 1 && models.contains(AUTO_REVIEW_MODEL);
            let any_review = shard.events.iter().any(TokenEvent::is_excluded_auto_review);
            let any_included = shard.events.iter().any(|e| !e.is_excluded_auto_review());
            if (only_review_models || any_review) && !any_included {
                self.excluded_task_cache.insert(shard.thread_id.clone());
            } else {
                self.excluded_task_cache.remove(&shard.thread_id);
            }
            self.dirty_threads.insert(shard.thread_id.clone());
            self.local_events
                .insert(shard.thread_id.clone(), shard.events.clone());
        }
        // Keep the persisted evidence in the same canonical order/deduplicated
        // context form as the full normalizer; otherwise copied contexts would
        // accumulate indefinitely even though their event output is unchanged.
        self.observations.clear();
        self.contexts.clear();
        self.effort_contexts.clear();
        for shard in shards {
            self.observations.extend(shard.observations);
            self.contexts.extend(shard.contexts);
            self.effort_contexts.extend(shard.effort_contexts);
        }
        self.observations
            .sort_by_key(|a| (a.timestamp_ms, a.sequence));
        self.contexts.sort_by(|a, b| {
            a.timestamp_ms
                .cmp(&b.timestamp_ms)
                .then(a.session.cmp(&b.session))
                .then(a.model.cmp(&b.model))
        });
        self.effort_contexts.sort();
        self.known_observations = self
            .observations
            .iter()
            .enumerate()
            .map(|(index, row)| (row.identity.clone(), index))
            .collect();
        self.events.clear();
        let policy = EventPolicy::new(
            &self.response_threads,
            &self.anonymous_threads,
            &self.anonymous_bounds_ms,
        );
        self.anonymous_policy_bound_ms = policy.anonymous_bound_ms;
        for events in self.local_events.values() {
            for event in events {
                let mut event = event.clone();
                if policy.apply(&mut event) {
                    self.events.push(event);
                }
            }
        }
        self.events
            .sort_by(|a, b| (a.timestamp, &a.id).cmp(&(b.timestamp, &b.id)));
        self.last_pricing = Some(pricing.clone());
        self.normalization_version = NORMALIZATION_VERSION;
        self.usage_dirty = false;
        self.refresh_mixed_schema_warning();
        self.revision = self.revision.saturating_add(1);
    }

    pub fn dirty_export(&self) -> DirtyExport {
        let mut shards: Vec<_> = self
            .collect_loaded_shards(false)
            .into_iter()
            .filter(|s| self.dirty_threads.contains(&s.thread_id))
            .collect();
        // Derive tooltip data from each already-normalized local event cache.
        for shard in &mut shards {
            let local = Self {
                observations: shard.observations.clone(),
                events: shard.events.clone(),
                ..Self::default()
            };
            shard.turn_usage = local.included_task_turn_usage().remove(&shard.thread_id);
        }
        DirtyExport {
            header: self.header(),
            threads: shards,
        }
    }

    /// Finish an all-shard compatibility export without changing its committed
    /// revision or recomputing accounting. Exporters must hydrate every shard.
    pub fn finish_hydration_for_export(&mut self) {
        self.observations
            .sort_by_key(|a| (a.timestamp_ms, a.sequence));
        self.contexts.sort_by(|a, b| {
            a.timestamp_ms
                .cmp(&b.timestamp_ms)
                .then(a.session.cmp(&b.session))
                .then(a.model.cmp(&b.model))
        });
        self.effort_contexts.sort();
        self.events
            .sort_by(|a, b| (a.timestamp, &a.id).cmp(&(b.timestamp, &b.id)));
        self.indexes_ready = false;
        // A complete compatibility export is an ordinary standalone TokenStore.
        // It can be inspected/repriced without a database hydration callback.
        self.database_mode = false;
        self.local_events.clear();
        self.hydrated_threads.clear();
        self.dirty_threads.clear();
        if let Some(pricing) = self.last_pricing.clone() {
            self.refresh_cached_prices(&pricing);
        }
    }

    /// Refresh derived display prices without replaying accounting evidence.
    /// Model override/default changes must be reconciled before calling this.
    pub fn refresh_cached_prices(&mut self, pricing: &Pricing) {
        for event in &mut self.events {
            event.cost_usd = event_cost(event, pricing);
        }
    }

    /// Call only after the matching DirtyExport commits successfully. A failed
    /// commit must keep the hydrated state and its cursors available for retry.
    pub fn mark_committed(&mut self) {
        if !self.database_mode {
            return;
        }
        self.observations = Vec::new();
        self.contexts = Vec::new();
        self.effort_contexts = Vec::new();
        self.events = Vec::new();
        self.known_observations.clear();
        self.local_events.clear();
        self.hydrated_threads.clear();
        self.dirty_threads.clear();
        self.usage_dirty = false;
        self.persisted_revision_at_load = Some(self.revision);
    }

    /// Explicitly reproject a loaded shard after a model override/default change.
    /// Callers hydrate every affected shard and atomically publish one revision.
    pub fn reprice_hydrated(&mut self, pricing: &Pricing) {
        if self.database_mode {
            self.reconcile_hydrated(pricing);
        } else {
            self.reprice(pricing);
        }
    }

    pub fn import_dir_hydrated(
        &mut self,
        root: &Path,
        metadata_path: Option<&Path>,
        pricing: &Pricing,
        loader: &mut dyn FnMut(&str) -> io::Result<Option<ThreadShard>>,
        progress: &mut dyn FnMut(ImportProgress),
    ) -> io::Result<ImportReport> {
        if !self.database_mode {
            return Err(invalid_data("Hydrated import requires database mode"));
        }
        let mut report = self.import_dir_progress_inner(root, pricing, progress, Some(loader))?;
        if let Some(path) = metadata_path {
            self.report_progress(
                ImportPhase::Metadata,
                &report,
                report.files_scanned,
                progress,
            );
            if let Err(error) = self.import_metadata_index(path, &mut report) {
                if error.kind() != io::ErrorKind::NotFound {
                    report.warnings.push(format!(
                        "Could not read task-name metadata {}: {error}",
                        path.display()
                    ));
                }
            }
        } else if self.metadata_path.take().is_some() {
            self.metadata_cursor = SourceCursor::default();
            self.metadata_names.clear();
            self.revision = self.revision.saturating_add(1);
        }
        Ok(report)
    }

    pub fn load(path: impl AsRef<Path>) -> io::Result<Self> {
        match File::open(path.as_ref()) {
            Ok(file) => {
                let mut store: Self =
                    serde_json::from_reader(BufReader::new(file)).map_err(invalid_data)?;
                store.persisted_revision_at_load = Some(store.revision);
                store.next_sequence = store.next_sequence.max(
                    store
                        .observations
                        .iter()
                        .map(|row| row.sequence.saturating_add(1))
                        .max()
                        .unwrap_or(0),
                );
                if matches!(store.version, 1 | 2) {
                    // v1 skipped response records; v2 skipped official names and
                    // quota provenance. Replay selected roots once, retaining history.
                    // Replay only the explicit import roots on the next import;
                    // keep all historical observations, contexts, events and quotas.
                    store.sources.clear();
                    // Older stores collapsed absent optional counters to zero.
                    // Until selected source replay enriches them, do not claim
                    // those cached/reasoning counts or exact prices are known.
                    for row in &mut store.observations {
                        flag(&mut row.flags, "cached_count_unavailable");
                        flag(&mut row.flags, "reasoning_count_unavailable");
                    }
                    for event in &mut store.events {
                        flag(&mut event.partial_flags, "cached_count_unavailable");
                        flag(&mut event.partial_flags, "reasoning_count_unavailable");
                        event.cost_usd = None;
                    }
                    store.version = STORE_VERSION;
                    // Keep the user's explicit rates/override; only derived usage
                    // is stale and needs a replay/recalculation.
                    store.usage_dirty = true;
                    store.revision = store.revision.saturating_add(1);
                } else if store.version != STORE_VERSION {
                    return Err(invalid_data("Unsupported token-store version"));
                }
                if store.normalization_version < NORMALIZATION_VERSION {
                    // Replay only user-selected roots on the next import. Existing
                    // observations repair model attribution immediately; effort
                    // omitted by old caches is recovered only from selected files.
                    store.sources.clear();
                    let pricing = store.last_pricing.clone().unwrap_or_default();
                    store.reconcile(&pricing);
                }
                // Events are a derived cache. A current-version file may still
                // have retained observations but a missing/empty event cache
                // (for example, an interrupted older migration). Source cursors
                // can already be at EOF, so an unchanged scan will never repair
                // this on its own. Recover from retained inputs once on load;
                // do not clear cursors or touch the source file.
                if store.events.is_empty() && !store.observations.is_empty() {
                    let pricing = store.last_pricing.clone().unwrap_or_default();
                    store.reconcile(&pricing);
                }
                store.refresh_mixed_schema_warning();
                Ok(store)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }
    /// Atomic replacement; the application must hold its shared writer lock around load/mutate/save.
    pub fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(invalid_data)?
            .as_nanos();
        let temporary = path.with_extension(format!("tmp-{}-{suffix}", std::process::id()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            write_token_store(&mut file, self)?;
            file.sync_all()?;
            // In particular on Windows, release the completed temporary file
            // before attempting the same-directory atomic replacement.
            drop(file);
            fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
    /// Publish a new compatibility export atomically without ever replacing an
    /// existing destination, including one created during serialization.
    /// Filesystems without same-directory hard links fail explicitly.
    pub fn save_new(&self, path: impl AsRef<Path>) -> io::Result<()> {
        self.save_new_before_publish(path.as_ref(), || {})
    }

    fn save_new_before_publish(
        &self,
        path: &Path,
        before_publish: impl FnOnce(),
    ) -> io::Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(invalid_data)?
            .as_nanos();
        let temporary = path.with_extension(format!("tmp-export-{}-{suffix}", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let result = (|| {
            write_token_store(&mut file, self)?;
            file.sync_all()?;
            // Windows requires the completed temporary file to be closed before
            // publishing it. A hard link creates a new name or fails atomically.
            drop(file);
            before_publish();
            fs::hard_link(&temporary, path).map_err(|error| {
                io::Error::new(error.kind(), format!(
                    "Could not publish export without replacing existing data; atomic same-directory hard links are required: {error}"))
            })
        })();
        // This is only the unique temporary file created by this call. Earlier
        // failed/cache temporary files are never discovered or removed here.
        let _ = fs::remove_file(&temporary);
        result
    }

    /// In-memory screenshot/test data. Every count and price is fictional; never live account data.
    /// This constructor does not read files or populate actual rate-limit observations.
    pub fn demo(now: i64) -> Self {
        let mut store = Self::default();
        for (index, (model, input, cached, output, reasoning)) in [
            ("gpt-6.1-sol", 12_400, 8_200, 1_840, 840),
            ("gpt-6-luna", 4_800, 3_200, 620, 180),
            ("gpt-6.1-sol", 18_200, 14_100, 2_320, 1_120),
        ]
        .into_iter()
        .enumerate()
        {
            let session = format!(
                "task-{}: DEMO synthetic usage",
                if index == 1 { "B" } else { "A" }
            );
            store
                .recorded_names
                .entry(session.clone())
                .or_insert_with(|| RecordedTaskName {
                    name: if index == 1 {
                        "DEMO：檢查額度紀錄"
                    } else {
                        "DEMO：修復匯入效能"
                    }
                    .into(),
                    timestamp_ms: Some(now.saturating_mul(1000)),
                });
            let turn_total = if index == 2 {
                Counts {
                    input: 30_600,
                    cached: 22_300,
                    output: 4_160,
                    reasoning: 1_960,
                }
            } else {
                Counts {
                    input,
                    cached,
                    output,
                    reasoning,
                }
            };
            store.observations.push(UsageObservation {
                identity: format!("demo-synthetic-{index}"),
                timestamp_ms: now
                    .saturating_sub((3 - index) as i64 * 120)
                    .saturating_mul(1000),
                sequence: index as u64,
                session,
                request_id: Some(format!("demo-request-{index}")),
                response_id: Some(format!("demo-response-{index}")),
                turn_id: Some(format!("demo-turn-{}", if index == 1 { "b" } else { "a" })),
                source_kind: Some("token_usage_record".into()),
                turn_total: Some(turn_total),
                raw_turn_counts: Some(RawTurnCounts {
                    input: Some(turn_total.input),
                    output: Some(turn_total.output),
                    cached: Some(turn_total.cached),
                    reasoning: Some(turn_total.reasoning),
                }),
                thread_total: None,
                model: Some(model.into()),
                source_model: None,
                source_model_timestamp_ms: None,
                source_model_turn_id: None,
                reasoning_efforts: BTreeSet::new(),
                source_effort_contexts: Vec::new(),
                total: None,
                last: Some(Counts {
                    input,
                    cached,
                    output,
                    reasoning,
                }),
                flags: vec!["demo_synthetic_not_prices".into()],
            });
        }
        store.recorded_parents.insert(
            "task-B: DEMO synthetic usage".into(),
            RecordedTaskParent {
                parent: Some("task-A: DEMO synthetic usage".into()),
                timestamp_ms: Some(now.saturating_mul(1000)),
            },
        );
        let pricing = Pricing::demo();
        store.reconcile(&pricing);
        store
    }
    /// Monotonic persisted revision for data/cursor/name/price cache invalidation.
    /// Pricing assumption used to produce the current immutable event snapshot.
    pub fn pricing(&self) -> Option<&Pricing> {
        self.last_pricing.as_ref()
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// A load may repair data before returning. Only this original revision is
    /// known to be on disk; authorized worker saves track later revisions.
    pub(crate) fn persisted_revision_at_load(&self) -> Option<u64> {
        self.persisted_revision_at_load
    }
    /// Explicit official metadata only. Never inferred from a prompt, cwd or ID.
    pub fn task_name(&self, task: &str) -> Option<&str> {
        self.metadata_names
            .get(task)
            .map(String::as_str)
            .or_else(|| self.recorded_names.get(task).map(|row| row.name.as_str()))
    }
    /// An owned metadata snapshot for background-worker/UI handoff.
    pub fn task_names(&self) -> BTreeMap<String, String> {
        let mut names: BTreeMap<_, _> = self
            .recorded_names
            .iter()
            .map(|(id, row)| (id.clone(), row.name.clone()))
            .collect();
        names.extend(self.metadata_names.clone());
        names
    }
    /// Authoritative immediate parent relationships from official thread metadata.
    /// Parent IDs may lack imported usage; root/session IDs and fork IDs never
    /// imply a parent edge. Graph consumers must handle recorded cycles safely.
    pub fn task_parents(&self) -> BTreeMap<String, String> {
        self.recorded_parents
            .iter()
            .filter_map(|(task, row)| {
                row.parent
                    .as_ref()
                    .map(|parent| (task.clone(), parent.clone()))
            })
            .collect()
    }

    /// Latest event's matching turn, using official cumulative turn metadata when
    /// available. A sum of retained response events is explicitly only partial.
    pub fn task_turn_usage(&self) -> BTreeMap<String, TaskTurnUsage> {
        self.task_turn_usage_filtered(false)
    }
    /// Same tooltip semantics, using only responses included in the default view.
    pub fn included_task_turn_usage(&self) -> BTreeMap<String, TaskTurnUsage> {
        self.task_turn_usage_filtered(true)
    }
    /// Review-only tasks may be hidden, including metadata-only review tasks.
    /// Any included event or contradictory recorded model evidence keeps the task.
    pub fn excluded_auto_review_tasks(&self) -> BTreeSet<String> {
        let mut models = self.recorded_task_models.clone();
        for context in &self.contexts {
            models
                .entry(context.session.clone())
                .or_default()
                .insert(context.model.clone());
        }
        let mut excluded: BTreeSet<String> = models
            .iter()
            .filter(|(_, models)| models.len() == 1 && models.contains(AUTO_REVIEW_MODEL))
            .map(|(task, _)| task.clone())
            .collect();
        let mut included = BTreeSet::new();
        for event in &self.events {
            if event.is_excluded_auto_review() {
                excluded.insert(event.session.clone());
            } else {
                included.insert(event.session.clone());
            }
        }
        excluded.retain(|task| !included.contains(task));
        excluded
    }
    fn task_turn_usage_filtered(&self, exclude_review: bool) -> BTreeMap<String, TaskTurnUsage> {
        let included = |event: &&TokenEvent| !exclude_review || !event.is_excluded_auto_review();
        let excluded_turns: BTreeSet<(&str, &str)> = if exclude_review {
            self.events
                .iter()
                .filter(|event| event.is_excluded_auto_review())
                .filter_map(|event| {
                    event
                        .turn_id
                        .as_deref()
                        .map(|turn| (event.session.as_str(), turn))
                })
                .collect()
        } else {
            BTreeSet::new()
        };
        let mut latest: BTreeMap<&str, &TokenEvent> = BTreeMap::new();
        for event in self.events.iter().filter(included) {
            latest.insert(&event.session, event);
        }
        let mut sums: BTreeMap<(&str, &str), (u64, u64, bool, bool)> = BTreeMap::new();
        for event in self.events.iter().filter(included) {
            if let Some(turn) = event.turn_id.as_deref() {
                let counts = sums
                    .entry((&event.session, turn))
                    .or_insert((0, 0, true, true));
                counts.0 = counts.0.saturating_add(event.input);
                counts.1 = counts.1.saturating_add(event.output);
                counts.2 &= !event
                    .partial_flags
                    .iter()
                    .any(|f| f == "input_count_unavailable");
                counts.3 &= !event
                    .partial_flags
                    .iter()
                    .any(|f| f == "output_count_unavailable");
            }
        }
        let mut snapshots: BTreeMap<(&str, &str), &UsageObservation> = BTreeMap::new();
        for row in &self.observations {
            if let Some(turn) = row
                .turn_id
                .as_deref()
                .filter(|_| row.turn_total.is_some() || row.raw_turn_counts.is_some())
            {
                snapshots
                    .entry((&row.session, turn))
                    .and_modify(|old| {
                        if (row.timestamp_ms, &row.identity) > (old.timestamp_ms, &old.identity) {
                            *old = row;
                        }
                    })
                    .or_insert(row);
            }
        }
        latest
            .into_iter()
            .map(|(task, event)| {
                let mut usage = TaskTurnUsage {
                    turn_id: event.turn_id.clone(),
                    input: None,
                    output: None,
                    recorded_snapshot: false,
                    partial: true,
                    timestamp: None,
                };
                if let Some(turn) = event.turn_id.as_deref() {
                    let contains_excluded = excluded_turns.contains(&(task, turn));
                    if let Some(snapshot) =
                        snapshots.get(&(task, turn)).filter(|_| !contains_excluded)
                    {
                        usage.recorded_snapshot = true;
                        usage.timestamp = Some(snapshot.timestamp_ms.div_euclid(1000));
                        // Old observations do not retain per-field validity. If any
                        // snapshot field was invalid, do not silently show missing=0.
                        if let Some(raw) = &snapshot.raw_turn_counts {
                            usage.input = raw.input;
                            usage.output = raw.output;
                            usage.partial = raw.input.is_none() || raw.output.is_none();
                        } else if !snapshot
                            .flags
                            .iter()
                            .any(|flag| flag == "invalid_usage_snapshot")
                        {
                            if let Some(counts) = snapshot.turn_total {
                                usage.input = Some(counts.input);
                                usage.output = Some(counts.output);
                                usage.partial = false;
                            }
                        }
                    } else if let Some((input, output, input_known, output_known)) =
                        sums.get(&(task, turn))
                    {
                        usage.input = input_known.then_some(*input);
                        usage.output = output_known.then_some(*output);
                        usage.timestamp = Some(event.timestamp);
                    }
                }
                (task.to_owned(), usage)
            })
            .collect()
    }
    pub fn events(&self) -> &[TokenEvent] {
        &self.events
    }
    pub fn quota_observations(&self) -> &[ImportedQuotaObservation] {
        &self.quota
    }
    pub fn source_count(&self) -> usize {
        self.sources.len()
    }
    pub fn observation_count(&self) -> usize {
        self.observations.len()
    }
    /// Inclusive start, exclusive end, in UTC Unix seconds. No stored data is deleted.
    pub fn select_events(&self, from: Option<i64>, to: Option<i64>) -> Vec<&TokenEvent> {
        self.events
            .iter()
            .filter(|e| from.is_none_or(|v| e.timestamp >= v) && to.is_none_or(|v| e.timestamp < v))
            .collect()
    }
    pub fn default_window_start(now: i64) -> i64 {
        now.saturating_sub(DEFAULT_BROWSE_DAYS * 86400)
    }
    pub fn summary(&self, from: Option<i64>, to: Option<i64>, pricing: &Pricing) -> TokenSummary {
        let mut result = TokenSummary::default();
        for event in self
            .select_events(from, to)
            .into_iter()
            .filter(|event| !event.is_excluded_auto_review())
        {
            result.event_count += 1;
            result.input = result.input.saturating_add(event.input);
            result.cached = result.cached.saturating_add(event.cached);
            result.uncached = result.uncached.saturating_add(event.uncached);
            result.output = result.output.saturating_add(event.output);
            result.reasoning = result.reasoning.saturating_add(event.reasoning);
            result.partial_events += usize::from(event.is_partial());
            result.assumed_model_events += usize::from(event.model_assumed);
            match event_cost(event, pricing) {
                Some(cost) => result.priced_cost_usd += cost,
                None => result.unpriced_events += 1,
            }
        }
        result.total_tokens = result.input.saturating_add(result.output);
        let coverage_incomplete = self.select_events(from, to).iter().any(|e| {
            !e.is_excluded_auto_review()
                && e.partial_flags.iter().any(|f| {
                    matches!(
                        f.as_str(),
                        "response_coverage_gap" | "mixed_schema_legacy_overlap_suppressed"
                    )
                })
        });
        result.cost_usd =
            (result.unpriced_events == 0 && !coverage_incomplete).then_some(result.priced_cost_usd);
        result
    }
    pub fn day_summary(&self, day: NaiveDate, pricing: &Pricing) -> TokenSummary {
        let start = day
            .and_hms_opt(0, 0, 0)
            .expect("midnight is valid")
            .and_utc()
            .timestamp();
        self.summary(Some(start), Some(start.saturating_add(86400)), pricing)
    }
    pub fn reprice(&mut self, pricing: &Pricing) {
        if self.usage_dirty || self.last_pricing.as_ref() != Some(pricing) {
            self.reconcile(pricing);
        }
    }

    fn ensure_indexes(&mut self) {
        if !self.indexes_ready {
            self.next_sequence = self.next_sequence.max(
                self.observations
                    .iter()
                    .map(|row| row.sequence.saturating_add(1))
                    .max()
                    .unwrap_or(0),
            );
            self.known_observations = self
                .observations
                .iter()
                .enumerate()
                .map(|(index, row)| (row.identity.clone(), index))
                .collect();
            self.known_quota = self.quota.iter().map(quota_identity).collect();
            self.legacy_quota = self
                .quota
                .iter()
                .enumerate()
                .filter(|(_, row)| row.origin_session.is_none() && row.source_id.is_none())
                .map(|(index, row)| (legacy_quota_identity(row), index))
                .collect();
            self.indexes_ready = true;
        }
    }
    fn record_task_parent(
        &mut self,
        task: &str,
        payload: &Value,
        timestamp_ms: Option<i64>,
        report: &mut ImportReport,
    ) {
        // Official current protocol uses ThreadSpawn serialized as `thread_spawn`:
        // https://github.com/openai/codex/blob/main/codex-rs/protocol/src/protocol.rs
        // SessionMeta and SessionConfiguredEvent also expose parent_thread_id.
        let explicit = text_field(payload, &["parent_thread_id"]);
        let spawned = payload
            .pointer("/source/subagent/thread_spawn")
            .and_then(|source| text_field(source, &["parent_thread_id"]));
        if explicit.is_none() && spawned.is_none() {
            return;
        }
        let conflicting_fields = explicit
            .as_ref()
            .zip(spawned.as_ref())
            .is_some_and(|(a, b)| a != b);
        let mut candidate = RecordedTaskParent {
            parent: if conflicting_fields {
                None
            } else {
                explicit.or(spawned)
            },
            timestamp_ms,
        };
        if let Some(old) = self.recorded_parents.get(task) {
            if old.timestamp_ms > timestamp_ms {
                return;
            }
            if old.timestamp_ms == timestamp_ms && old.parent != candidate.parent {
                candidate.parent = None;
            }
            if old == &candidate {
                return;
            }
        }
        if candidate.parent.is_none() {
            report.warnings.push(format!("Conflicting recorded parent metadata for task {task}; no parent relationship was inferred"));
        }
        self.recorded_parents.insert(task.to_owned(), candidate);
        self.revision = self.revision.saturating_add(1);
    }
    fn refresh_mixed_schema_warning(&mut self) {
        self.mixed_schema_warning = self.events.iter().any(|e| {
            e.partial_flags
                .iter()
                .any(|f| f == "mixed_schema_legacy_overlap_suppressed")
        });
    }

    /// Read only .jsonl files below the explicit root; never follows symlinks.
    /// Unfinished final lines stay unconsumed and are retried on the next import.
    pub fn import_dir(
        &mut self,
        root: impl AsRef<Path>,
        pricing: &Pricing,
    ) -> io::Result<ImportReport> {
        self.import_dir_progress(root, pricing, &mut |_| {})
    }

    fn report_progress(
        &self,
        phase: ImportPhase,
        report: &ImportReport,
        files_total: usize,
        progress: &mut dyn FnMut(ImportProgress),
    ) {
        progress(ImportProgress {
            phase,
            files_done: report.files_scanned,
            files_total,
            lines_read: report.lines_read,
            observations: self.observations.len(),
            events: self.events.len(),
        });
    }

    fn import_dir_progress(
        &mut self,
        root: impl AsRef<Path>,
        pricing: &Pricing,
        progress: &mut dyn FnMut(ImportProgress),
    ) -> io::Result<ImportReport> {
        self.import_dir_progress_inner(root.as_ref(), pricing, progress, None)
    }

    fn import_dir_progress_inner(
        &mut self,
        root: &Path,
        pricing: &Pricing,
        progress: &mut dyn FnMut(ImportProgress),
        mut loader: Option<&mut ThreadLoader<'_>>,
    ) -> io::Result<ImportReport> {
        let mut report = ImportReport::default();
        self.report_progress(ImportPhase::Discovering, &report, 0, progress);
        if fs::symlink_metadata(root)?.file_type().is_symlink() {
            return Err(invalid_data(
                "Selected sessions root is a symbolic link; select its actual directory explicitly",
            ));
        }
        let mut paths = Vec::new();
        collect_jsonl(root, &mut paths, &mut report)?;
        paths.sort();
        let files_total = paths.len();
        self.report_progress(ImportPhase::Reading, &report, files_total, progress);
        let mut last_progress = Instant::now();
        let old_observation_count = self.observations.len();
        let old_context_count = self.contexts.len();
        let old_effort_context_count = self.effort_contexts.len();
        for path in paths {
            if let Err(error) = self.import_file(
                &path,
                &mut report,
                files_total,
                &mut last_progress,
                progress,
                &mut loader,
            ) {
                report
                    .warnings
                    .push(format!("Could not read {}: {error}", path.display()));
            }
            report.files_scanned += 1;
            if last_progress.elapsed() >= Duration::from_millis(250) {
                self.report_progress(ImportPhase::Reading, &report, files_total, progress);
                last_progress = Instant::now();
            }
        }
        self.report_progress(ImportPhase::Reading, &report, files_total, progress);
        if self.usage_dirty
            || self.observations.len() != old_observation_count
            || self.contexts.len() != old_context_count
            || self.effort_contexts.len() != old_effort_context_count
            || self.last_pricing.as_ref() != Some(pricing)
        {
            self.report_progress(ImportPhase::Reconciling, &report, files_total, progress);
            // Only a real reconciliation needs an old-ID set, never an idle poll.
            let old_ids: BTreeSet<_> = self.events.iter().map(|event| event.id.clone()).collect();
            self.reconcile(pricing);
            report.events_added = self
                .events
                .iter()
                .filter(|event| !old_ids.contains(&event.id))
                .count();
        }
        if self.mixed_schema_warning {
            report.warnings.push("Some legacy usage overlaps response records without comparable IDs/counters; it was excluded to prevent double counting. Token totals are a lower bound and complete cost is unknown.".into());
        }
        if !report.quota_observations.is_empty() {
            self.quota
                .sort_by_key(|row| (row.timestamp, row.timestamp_ms));
            // Sorting invalidates legacy enrichment positions, only relevant at migration.
            self.legacy_quota = self
                .quota
                .iter()
                .enumerate()
                .filter(|(_, row)| row.origin_session.is_none() && row.source_id.is_none())
                .map(|(index, row)| (legacy_quota_identity(row), index))
                .collect();
        }
        report.quota_observations.sort_by_key(|row| row.timestamp);
        Ok(report)
    }

    /// Add an explicitly selected official `session_index.jsonl`. No Codex home,
    /// sibling file, database, credential or conversation is discovered implicitly.
    /// Official schema: codex-rs/rollout/src/session_index.rs (OpenAI Codex).
    pub fn import_dir_with_metadata(
        &mut self,
        root: impl AsRef<Path>,
        metadata_path: Option<&Path>,
        pricing: &Pricing,
    ) -> io::Result<ImportReport> {
        self.import_dir_with_metadata_progress(root, metadata_path, pricing, &mut |_| {})
    }

    /// Report bounded metadata-only progress while preserving the ordinary
    /// import's complete, chronological reconciliation and accounting semantics.
    pub fn import_dir_with_metadata_progress(
        &mut self,
        root: impl AsRef<Path>,
        metadata_path: Option<&Path>,
        pricing: &Pricing,
        progress: &mut dyn FnMut(ImportProgress),
    ) -> io::Result<ImportReport> {
        let mut report = self.import_dir_progress(root, pricing, progress)?;
        if let Some(path) = metadata_path {
            self.report_progress(
                ImportPhase::Metadata,
                &report,
                report.files_scanned,
                progress,
            );
            if let Err(error) = self.import_metadata_index(path, &mut report) {
                if error.kind() != io::ErrorKind::NotFound {
                    report.warnings.push(format!(
                        "Could not read task-name metadata {}: {error}",
                        path.display()
                    ));
                }
            }
        } else if self.metadata_path.take().is_some() {
            self.metadata_cursor = SourceCursor::default();
            self.metadata_names.clear();
            self.revision = self.revision.saturating_add(1);
        }
        Ok(report)
    }

    fn import_metadata_index(&mut self, path: &Path, report: &mut ImportReport) -> io::Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if self.metadata_path.take().is_some() {
                    self.metadata_names.clear();
                    self.metadata_cursor = SourceCursor::default();
                    self.revision = self.revision.saturating_add(1);
                }
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(invalid_data(
                "Task metadata must be an explicitly selected regular file, not a symlink",
            ));
        }
        let key = fs::canonicalize(path)?.to_string_lossy().into_owned();
        report.files_scanned += 1;
        if self.metadata_path.as_ref() != Some(&key) {
            self.metadata_cursor = SourceCursor::default();
            self.metadata_names.clear();
            self.metadata_path = Some(key);
            self.revision = self.revision.saturating_add(1);
        }
        if source_unchanged(&self.metadata_cursor, &metadata) {
            report.files_unchanged += 1;
            report.partial_lines += usize::from(self.metadata_cursor.offset < metadata.len());
            return Ok(());
        }
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        let mut cursor = self.metadata_cursor.clone();
        if source_rewritten(&mut file, &cursor, &metadata)? {
            cursor = SourceCursor {
                generation: cursor.generation.saturating_add(1),
                ..Default::default()
            };
            self.metadata_names.clear();
            report.source_resets += 1;
        }
        file.seek(SeekFrom::Start(cursor.offset))?;
        // Pin a finite high-water mark from the opened file. Concurrent append
        // belongs to the next import, and a partial boundary line is retried.
        let available = metadata.len().saturating_sub(cursor.offset);
        let mut reader = BufReader::new(file.take(available));
        loop {
            let (bytes, length, complete, oversized) = read_bounded_line(&mut reader)?;
            if length == 0 {
                break;
            }
            if !complete {
                report.partial_lines += 1;
                break;
            }
            let offset = cursor.offset;
            cursor.offset = cursor.offset.saturating_add(length);
            report.lines_read += 1;
            if oversized {
                report.malformed_json_line(path, offset, "oversized line exceeds the 4 MiB limit");
                continue;
            }
            if bytes.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let value: Value = match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(error) => {
                    report.malformed_json_line(path, offset, error);
                    continue;
                }
            };
            // The upstream index is append-only; last nonempty entry wins,
            // regardless of timestamp order. Arbitrary `title`/message fields
            // are intentionally not interpreted as task-name metadata.
            if let (Some(id), Some(name)) = (
                text_field(&value, &["id"]),
                text_field(&value, &["thread_name"]),
            ) {
                self.metadata_names.insert(id, name.trim().to_owned());
            }
        }
        finish_cursor(
            &mut reader.into_inner().into_inner(),
            &mut cursor,
            &metadata,
        )?;
        if self.metadata_cursor != cursor {
            self.metadata_cursor = cursor;
            self.revision = self.revision.saturating_add(1);
        }
        Ok(())
    }

    fn import_file(
        &mut self,
        path: &Path,
        report: &mut ImportReport,
        files_total: usize,
        last_progress: &mut Instant,
        progress: &mut dyn FnMut(ImportProgress),
        loader: &mut Option<&mut ThreadLoader<'_>>,
    ) -> io::Result<()> {
        let canonical = fs::canonicalize(path)?;
        let key = canonical.to_string_lossy().into_owned();
        let mut cursor = self.sources.get(&key).cloned().unwrap_or_default();
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Ok(());
        }
        if source_unchanged(&cursor, &metadata) {
            report.files_unchanged += 1;
            report.partial_lines += usize::from(cursor.offset < metadata.len());
            return Ok(());
        }
        self.ensure_indexes();
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        let changed = source_rewritten(&mut file, &cursor, &metadata)?;
        if changed {
            let generation = cursor.generation + 1;
            cursor = SourceCursor {
                generation,
                ..Default::default()
            };
            report.source_resets += 1;
            report.warnings.push(format!(
                "Source was truncated or rewritten; safely replaying {}",
                path.display()
            ));
        }
        file.seek(SeekFrom::Start(cursor.offset))?;
        // Pin a finite high-water mark from the opened file. Concurrent append
        // belongs to the next import, and a partial boundary line is retried.
        let available = metadata.len().saturating_sub(cursor.offset);
        let mut reader = BufReader::new(file.take(available));
        loop {
            let (bytes, length, complete, oversized) = read_bounded_line(&mut reader)?;
            if length == 0 {
                break;
            }
            if !complete {
                report.partial_lines += 1;
                break;
            }
            let offset = cursor.offset;
            cursor.offset = cursor.offset.saturating_add(length);
            report.lines_read += 1;
            if report.lines_read.is_multiple_of(512)
                && last_progress.elapsed() >= Duration::from_millis(250)
            {
                self.report_progress(ImportPhase::Reading, report, files_total, progress);
                *last_progress = Instant::now();
            }
            if oversized {
                report.malformed_json_line(path, offset, "oversized line exceeds the 4 MiB limit");
                continue;
            }
            if bytes.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let value: Value = match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(error) => {
                    report.malformed_json_line(path, offset, error);
                    continue;
                }
            };
            self.consume_value(&value, &key, &mut cursor, changed, report, loader)?;
        }
        finish_cursor(
            &mut reader.into_inner().into_inner(),
            &mut cursor,
            &metadata,
        )?;
        if self.sources.get(&key) != Some(&cursor) {
            self.revision = self.revision.saturating_add(1);
            self.sources.insert(key, cursor);
        }
        Ok(())
    }

    fn consume_value(
        &mut self,
        value: &Value,
        source: &str,
        cursor: &mut SourceCursor,
        rewritten: bool,
        report: &mut ImportReport,
        loader: &mut Option<&mut ThreadLoader<'_>>,
    ) -> io::Result<()> {
        let payload = value.get("payload").unwrap_or(value);
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let payload_kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let response_scoped = kind == "token_usage_record" || payload_kind == "token_usage_record";
        if kind == "session_meta"
            || kind == "session_configured"
            || payload_kind == "session_configured"
        {
            cursor.session = if kind == "session_meta" {
                text_field(payload, &["id", "thread_id", "session_id"])
            } else {
                text_field(payload, &["thread_id", "session_id", "id"])
            };
            cursor.turn_id = None;
            cursor.model = None;
            cursor.model_timestamp_ms = None;
            cursor.model_turn_id = None;
            cursor.effort_context = None;
        } else if !response_scoped && cursor.session.is_none() {
            cursor.session = text_field(payload, &["thread_id", "session_id"])
                .or_else(|| text_field(value, &["thread_id", "session_id"]));
        }
        let timestamp_ms = parse_timestamp(value).or_else(|| parse_timestamp(payload));
        // A copied record keeps its original logical owner. Its root `session_id`
        // must not reassign this record, the file cursor, or subsequent legacy rows.
        let explicit_thread =
            text_field(payload, &["thread_id"]).or_else(|| text_field(value, &["thread_id"]));
        let session = explicit_thread
            .clone()
            .or_else(|| cursor.session.clone())
            .unwrap_or_else(|| format!("unidentified:{}", stable_hash(source.as_bytes())));
        if self.database_mode && !self.hydrated_threads.contains(&session) {
            let loader = loader
                .as_deref_mut()
                .ok_or_else(|| invalid_data("Database import requires a thread loader"))?;
            if let Some(shard) = loader(&session)? {
                if shard.thread_id != session {
                    return Err(invalid_data(
                        "Thread loader returned a different logical identity",
                    ));
                }
                self.hydrate_thread(shard)?;
            } else {
                self.hydrated_threads.insert(session.clone());
            }
            self.dirty_threads.insert(session.clone());
            self.usage_dirty = true;
        }
        if cursor.session.as_deref() == Some(session.as_str())
            && (kind == "turn_context" || matches!(payload_kind, "task_started" | "turn_started"))
        {
            cursor.turn_id = text_field(payload, &["turn_id"]);
            cursor.effort_context = None;
        }
        if matches!(
            payload_kind,
            "task_complete" | "turn_complete" | "turn_completed" | "turn_aborted"
        ) {
            cursor.effort_context = None;
        }
        // Only this official, request/turn-scoped record supplies inherited effort.
        // session_meta/configured and thread_settings_applied are defaults, not
        // proof of the effort that any particular response actually used.
        // Official writer: codex-rs/core/src/session/turn_context.rs (`effort`).
        if kind == "turn_context" {
            if let Some(timestamp_ms) = timestamp_ms {
                let context = EffortContext {
                    timestamp_ms,
                    session: session.clone(),
                    turn_id: text_field(payload, &["turn_id"]),
                    model: text_field(payload, &["model", "model_slug"]),
                    values: effort_fields(payload, &["reasoning_effort", "effort"]),
                };
                if cursor.session.as_deref() == Some(session.as_str()) {
                    cursor.effort_context = Some(context.clone());
                }
                self.effort_contexts.push(context);
            }
        }
        if matches!(kind, "session_meta" | "session_configured")
            || payload_kind == "session_configured"
        {
            self.record_task_parent(&session, payload, timestamp_ms, report);
            if let Some(name) = text_field(payload, &["thread_name"]) {
                let entry = RecordedTaskName { name, timestamp_ms };
                if self
                    .recorded_names
                    .get(&session)
                    .is_none_or(|old| entry.timestamp_ms >= old.timestamp_ms)
                {
                    self.recorded_names.insert(session.clone(), entry);
                }
            }
        }
        if matches!(
            kind,
            "turn_context" | "session_meta" | "session_configured" | "thread_settings_applied"
        ) || matches!(
            payload_kind,
            "session_configured" | "thread_settings_applied"
        ) {
            let settings = payload.get("thread_settings").unwrap_or(payload);
            if let Some(model) = text_field(settings, &["model", "model_slug"]) {
                let turn_id = text_field(payload, &["turn_id"]).or_else(|| {
                    (cursor.session.as_deref() == Some(session.as_str()))
                        .then(|| cursor.turn_id.clone())
                        .flatten()
                });
                self.recorded_task_models
                    .entry(session.clone())
                    .or_default()
                    .insert(model.clone());
                if cursor.session.as_deref() == Some(session.as_str()) {
                    cursor.model = Some(model.clone());
                    cursor.model_timestamp_ms = timestamp_ms;
                    cursor.model_turn_id = turn_id.clone();
                }
                if let Some(timestamp_ms) = timestamp_ms {
                    self.contexts.push(ModelContext {
                        timestamp_ms,
                        session: session.clone(),
                        turn_id,
                        model,
                    });
                }
            }
        }
        let info = payload
            .get("info")
            .filter(|v| v.is_object())
            .unwrap_or(payload);
        if let Some(rate_limits) = payload
            .get("rate_limits")
            .or_else(|| info.get("rate_limits"))
            .or_else(|| value.get("rate_limits"))
        {
            if rate_limits.is_object() {
                if let Some(timestamp_ms) = timestamp_ms {
                    let observation = ImportedQuotaObservation {
                        timestamp: timestamp_ms.div_euclid(1000),
                        payload: rate_limits.clone(),
                        origin_session: (cursor.session.is_some() || explicit_thread.is_some())
                            .then(|| session.clone()),
                        source_id: Some(format!("{:016x}", stable_hash(source.as_bytes()))),
                        timestamp_ms: Some(timestamp_ms),
                    };
                    if self.known_quota.insert(quota_identity(&observation)) {
                        if let Some(index) = self
                            .legacy_quota
                            .remove(&legacy_quota_identity(&observation))
                        {
                            self.known_quota.remove(&quota_identity(&self.quota[index]));
                            self.quota[index] = observation.clone();
                        } else {
                            self.quota.push(observation.clone());
                        }
                        report.quota_observations.push(observation);
                    }
                } else {
                    report
                        .warnings
                        .push("Ignored a rate-limit observation without a valid timestamp".into());
                }
            }
        }
        let total_value = if response_scoped {
            None
        } else {
            info.get("total_token_usage").filter(|v| !v.is_null())
        };
        let last_value = if response_scoped {
            payload.get("usage").filter(|v| !v.is_null())
        } else {
            info.get("last_token_usage").filter(|v| !v.is_null())
        };
        if !response_scoped
            && payload_kind != "token_count"
            && kind != "token_count"
            && total_value.is_none()
            && last_value.is_none()
        {
            return Ok(());
        }
        if total_value.is_none() && last_value.is_none() {
            return Ok(());
        }
        let Some(timestamp_ms) = timestamp_ms else {
            report.malformed_lines += 1;
            report
                .warnings
                .push("Ignored token usage without a valid timestamp; no date was invented".into());
            return Ok(());
        };
        let mut flags = Vec::new();
        let total = total_value.and_then(|v| parse_counts(v, &mut flags));
        let last = last_value.and_then(|v| parse_counts(v, &mut flags));
        if total.is_none() && last.is_none() {
            report.malformed_lines += 1;
            return Ok(());
        }
        if cursor.session.is_none() && explicit_thread.is_none() {
            flag(&mut flags, "session_identity_missing");
        }
        let response_id = text_field(payload, &["response_id"])
            .or_else(|| text_field(info, &["response_id"]))
            .or_else(|| text_field(value, &["response_id"]));
        let turn_id = text_field(payload, &["turn_id"])
            .or_else(|| text_field(value, &["turn_id"]))
            .or_else(|| {
                (cursor.session.as_deref() == Some(session.as_str()))
                    .then(|| cursor.turn_id.clone())
                    .flatten()
            });
        if response_scoped && (explicit_thread.is_none() || response_id.is_none()) {
            report.malformed_lines += 1;
            report.warnings.push(
                "Ignored response usage without thread_id or response_id; no identity was invented"
                    .into(),
            );
            return Ok(());
        }
        if response_scoped && turn_id.is_none() {
            flag(&mut flags, "turn_identity_missing");
        }
        let source_kind = if response_scoped {
            "token_usage_record"
        } else {
            "token_count"
        };
        let mut snapshot_flags = Vec::new();
        let raw_turn_counts = payload
            .get("turn_token_usage")
            .filter(|v| v.is_object())
            .map(|value| RawTurnCounts {
                input: raw_count(value, &["input_tokens", "input"]),
                output: raw_count(value, &["output_tokens", "output"]),
                cached: raw_count(
                    value,
                    &["cached_input_tokens", "cache_read_input_tokens", "cached"],
                ),
                reasoning: raw_count(
                    value,
                    &["reasoning_output_tokens", "reasoning_tokens", "reasoning"],
                ),
            });
        let turn_total = payload
            .get("turn_token_usage")
            .filter(|v| !v.is_null())
            .and_then(|v| parse_counts(v, &mut snapshot_flags));
        let thread_total = payload
            .get("thread_token_usage")
            .filter(|v| !v.is_null())
            .and_then(|v| parse_counts(v, &mut snapshot_flags));
        if snapshot_flags.iter().any(|f| {
            !matches!(
                f.as_str(),
                "cache_write_pricing_unmodeled"
                    | "cached_count_unavailable"
                    | "reasoning_count_unavailable"
            )
        }) {
            flag(&mut flags, "invalid_usage_snapshot");
        }
        if rewritten {
            flag(&mut flags, "source_rewritten");
        }
        let model = text_field(payload, &["model", "model_slug"])
            .or_else(|| text_field(info, &["model", "model_slug"]))
            .or_else(|| text_field(value, &["model", "model_slug"]));
        // A turn_id is deliberately not used: one turn can contain multiple API requests.
        let request_id = text_field(payload, &["request_id", "response_id", "event_id", "id"])
            .or_else(|| text_field(info, &["request_id", "response_id", "event_id"]))
            .or_else(|| text_field(value, &["request_id", "response_id", "event_id", "id"]));
        let identity = format!(
            "{source_kind}|{session}|{timestamp_ms}|{}|{}|{}|{}|{}|{}|{}",
            request_id.as_deref().unwrap_or(""),
            total.map(Counts::signature).unwrap_or_default(),
            last.map(Counts::signature).unwrap_or_default(),
            model.as_deref().unwrap_or(""),
            turn_id.as_deref().unwrap_or(""),
            turn_total.map(Counts::signature).unwrap_or_default(),
            thread_total.map(Counts::signature).unwrap_or_default()
        );
        let source_model = (cursor.session.as_deref() == Some(session.as_str()))
            .then(|| cursor.model.clone())
            .flatten();
        let source_model_timestamp_ms = source_model.as_ref().and(cursor.model_timestamp_ms);
        let source_model_turn_id = source_model.as_ref().and(cursor.model_turn_id.clone());
        let reasoning_efforts = [payload, info, value]
            .into_iter()
            .flat_map(|value| effort_fields(value, &["reasoning_effort"]))
            .collect::<BTreeSet<_>>();
        let source_effort_context = cursor
            .effort_context
            .as_ref()
            .filter(|context| {
                context.session == session
                    && context.turn_id == turn_id
                    && context.timestamp_ms <= timestamp_ms
            })
            .cloned();
        if let Some(index) = self.known_observations.get(&identity).copied() {
            let existing = &mut self.observations[index];
            // During migration, old cursors replay without duplicating usage,
            // while restoring raw-field availability that older stores lost.
            let old_flags = existing.flags.clone();
            let old_effort_count = existing.reasoning_efforts.len();
            existing.reasoning_efforts.extend(reasoning_efforts);
            if existing.reasoning_efforts.len() != old_effort_count {
                self.usage_dirty = true;
            }
            if let Some(context) = source_effort_context {
                if !existing.source_effort_contexts.contains(&context) {
                    existing.source_effort_contexts.push(context);
                    self.usage_dirty = true;
                }
            }
            if existing.source_model != source_model
                || existing.source_model_timestamp_ms != source_model_timestamp_ms
                || existing.source_model_turn_id != source_model_turn_id
            {
                // Prefer concrete replay evidence to an older cache's missing model.
                if source_model.is_some() {
                    existing.source_model = source_model;
                    existing.source_model_timestamp_ms = source_model_timestamp_ms;
                    existing.source_model_turn_id = source_model_turn_id;
                    self.usage_dirty = true;
                }
            }
            existing
                .flags
                .retain(|value| !value.ends_with("_count_unavailable") || flags.contains(value));
            let old_raw = existing.raw_turn_counts.clone();
            if let Some(raw) = raw_turn_counts {
                if let Some(existing_raw) = &mut existing.raw_turn_counts {
                    existing_raw.input = existing_raw.input.or(raw.input);
                    existing_raw.output = existing_raw.output.or(raw.output);
                    existing_raw.cached = existing_raw.cached.or(raw.cached);
                    existing_raw.reasoning = existing_raw.reasoning.or(raw.reasoning);
                } else {
                    existing.raw_turn_counts = Some(raw);
                }
            }
            if old_flags != existing.flags || old_raw != existing.raw_turn_counts {
                self.usage_dirty = true;
            }
            report.duplicate_observations += 1;
            return Ok(());
        }
        self.known_observations
            .insert(identity.clone(), self.observations.len());
        self.observations.push(UsageObservation {
            identity,
            timestamp_ms,
            sequence: self.next_sequence,
            session,
            request_id,
            response_id,
            turn_id,
            source_kind: Some(source_kind.into()),
            turn_total,
            raw_turn_counts,
            thread_total,
            model,
            source_model,
            source_model_timestamp_ms,
            source_model_turn_id,
            reasoning_efforts,
            source_effort_contexts: source_effort_context.into_iter().collect(),
            total,
            last,
            flags,
        });
        self.next_sequence = self.next_sequence.saturating_add(1);
        Ok(())
    }

    fn reconcile(&mut self, pricing: &Pricing) {
        if self.database_mode {
            self.reconcile_hydrated(pricing);
        } else {
            self.reconcile_full(pricing);
        }
    }

    /// Whole-store correctness oracle, also used inside one logical thread.
    fn reconcile_full(&mut self, pricing: &Pricing) {
        if self.observations.is_empty() {
            for event in &mut self.events {
                if !event.model_assumed
                    && event.model == AUTO_REVIEW_MODEL
                    && !event
                        .partial_flags
                        .iter()
                        .any(|flag| flag == "model_override")
                {
                    flag(&mut event.partial_flags, AUTO_REVIEW_EXCLUDED);
                }
                event.cost_usd = event_cost(event, pricing);
            }
            self.normalization_version = NORMALIZATION_VERSION;
            self.last_pricing = Some(pricing.clone());
            self.usage_dirty = false;
            self.revision = self.revision.saturating_add(1);
            return;
        }

        #[derive(Default)]
        struct SessionState {
            total: Option<Counts>,
            pending_last: Counts,
            seen_totals: BTreeSet<String>,
            seen_requests: BTreeSet<String>,
            seen_last: BTreeSet<String>,
        }
        self.observations.sort_by(|a, b| {
            a.timestamp_ms
                .cmp(&b.timestamp_ms)
                .then(a.sequence.cmp(&b.sequence))
        });
        self.contexts.sort_by(|a, b| {
            a.timestamp_ms
                .cmp(&b.timestamp_ms)
                .then(a.session.cmp(&b.session))
                .then(a.model.cmp(&b.model))
        });
        self.contexts.dedup_by(|a, b| {
            a.timestamp_ms == b.timestamp_ms
                && a.session == b.session
                && a.turn_id == b.turn_id
                && a.model == b.model
        });
        let recorded_models = RecordedModels::new(&self.contexts);
        let mut model_evidence = self.recorded_task_models.clone();
        for context in &self.contexts {
            model_evidence
                .entry(context.session.clone())
                .or_default()
                .insert(context.model.clone());
        }
        for row in &self.observations {
            if let Some(model) = recorded_models.recorded(row) {
                model_evidence
                    .entry(row.session.clone())
                    .or_default()
                    .insert(model.to_owned());
            }
        }
        let mut states: BTreeMap<String, SessionState> = BTreeMap::new();
        let mut result: Vec<TokenEvent> = Vec::new();
        let mut event_positions: BTreeMap<String, usize> = BTreeMap::new();
        // Legacy reconciliation is unchanged. Authoritative response records are
        // selected independently and reconciled with this fallback below.
        for row in self
            .observations
            .iter()
            .filter(|r| r.source_kind.as_deref() != Some("token_usage_record"))
        {
            let state = states.entry(row.session.clone()).or_default();
            let mut flags = row.flags.clone();
            let request_seen = row
                .request_id
                .as_ref()
                .is_some_and(|id| state.seen_requests.contains(id));
            let (counts, id) = if let Some(total) = row.total {
                let signature = total.signature();
                // Repeated cumulative snapshots can have new timestamps or live in copied files.
                // Without a distinct request ID, a repeated old total is conservatively a duplicate.
                let known_total = state.seen_totals.contains(&signature);
                if known_total
                    && (row.request_id.is_none() || request_seen || state.total == Some(total))
                {
                    continue;
                }
                let reset = state.total.is_some_and(|old| total.decreased_from(old));
                let counts = if reset {
                    flag(&mut flags, "counter_reset_or_compaction");
                    let counts = row.last.unwrap_or(total);
                    if row.last.is_none() {
                        flag(&mut flags, "reset_baseline_uncertain");
                    }
                    counts
                } else {
                    let delta = state.total.map_or(total, |old| total.sub(old));
                    let counts = delta.sub(state.pending_last);
                    if state.total.is_none()
                        && row.last.is_none_or(|last| counts != last)
                        && !counts.is_zero()
                    {
                        flag(&mut flags, "initial_snapshot_contains_unattributed_usage");
                    } else if row.last.is_some_and(|last| counts != last) && !counts.is_zero() {
                        flag(&mut flags, "gap_spans_unobserved_requests");
                    }
                    counts
                };
                state.total = Some(total);
                state.pending_last = Counts::default();
                state.seen_totals.insert(signature.clone());
                if request_seen && !counts.is_zero() {
                    flag(&mut flags, "request_usage_updated");
                }
                let id = row
                    .request_id
                    .as_ref()
                    .map(|id| format!("{}:request:{id}", row.session))
                    .unwrap_or_else(|| format!("{}:cumulative:{signature}", row.session));
                (counts, id)
            } else if let Some(last) = row.last {
                if request_seen {
                    continue;
                }
                let fallback = format!(
                    "{}:{}:{}",
                    row.timestamp_ms,
                    last.signature(),
                    row.model.as_deref().unwrap_or("")
                );
                if !state.seen_last.insert(fallback.clone()) && row.request_id.is_none() {
                    continue;
                }
                if row.request_id.is_none() {
                    flag(&mut flags, "request_identity_missing");
                }
                state.pending_last = state.pending_last.add(last);
                let id = row
                    .request_id
                    .as_ref()
                    .map(|id| format!("{}:request:{id}", row.session))
                    .unwrap_or_else(|| format!("{}:last:{fallback}", row.session));
                (last, id)
            } else {
                continue;
            };
            if let Some(request_id) = &row.request_id {
                state.seen_requests.insert(request_id.clone());
            }
            if counts.is_zero() {
                continue;
            }
            let counts = counts.normalized(&mut flags);
            let recorded_model = recorded_models.recorded(row);
            let mut counts = counts;
            if recorded_model == Some(AUTO_REVIEW_MODEL) {
                let aggregate_scope_uncertain = flags.iter().any(|flag| {
                    matches!(
                        flag.as_str(),
                        "initial_snapshot_contains_unattributed_usage"
                            | "gap_spans_unobserved_requests"
                            | "reset_baseline_uncertain"
                    )
                });
                let pure_review_evidence = model_evidence
                    .get(&row.session)
                    .is_some_and(|models| models.len() == 1 && models.contains(AUTO_REVIEW_MODEL));
                if !aggregate_scope_uncertain || pure_review_evidence {
                    flag(&mut flags, AUTO_REVIEW_EXCLUDED);
                    if aggregate_scope_uncertain {
                        flag(&mut flags, "excluded_review_aggregate_scope_uncertain");
                    }
                } else if let Some(last) = row.last.filter(|last| !counts.decreased_from(*last)) {
                    // A mixed-model cumulative gap is not wholly attributable to
                    // its latest review request. Keep the residual visibly unknown.
                    let residual = counts.sub(last);
                    if !residual.is_zero() {
                        let mut residual_flags = flags.clone();
                        flag(&mut residual_flags, "legacy_model_scope_uncertain");
                        result.push(TokenEvent {
                            id: format!("{id}:unattributed"),
                            timestamp: row.timestamp_ms.div_euclid(1000),
                            model: UNATTRIBUTED_MODEL.into(),
                            model_assumed: false,
                            reasoning_effort: None,
                            session: row.session.clone(),
                            response_id: None,
                            turn_id: None,
                            source_kind: Some("token_count".into()),
                            input: residual.input,
                            cached: residual.cached,
                            uncached: residual.input.saturating_sub(residual.cached),
                            output: residual.output,
                            reasoning: residual.reasoning,
                            cost_usd: None,
                            coverage_start_timestamp_ms: None,
                            partial_flags: residual_flags,
                        });
                    }
                    counts = last;
                    flags.retain(|flag| {
                        !matches!(
                            flag.as_str(),
                            "initial_snapshot_contains_unattributed_usage"
                                | "gap_spans_unobserved_requests"
                                | "reset_baseline_uncertain"
                        )
                    });
                    flag(&mut flags, AUTO_REVIEW_EXCLUDED);
                } else {
                    flag(&mut flags, "legacy_model_scope_uncertain");
                }
            }
            let override_model = pricing
                .model_override
                .as_deref()
                .filter(|m| !m.trim().is_empty());
            let assumed_model = if pricing.assumed_model.trim().is_empty() {
                DEFAULT_ASSUMED_MODEL
            } else {
                &pricing.assumed_model
            };
            let model_assumed = override_model.is_none() && recorded_model.is_none();
            let model = override_model
                .or(recorded_model)
                .unwrap_or(assumed_model)
                .to_owned();
            if override_model.is_some() {
                flag(&mut flags, "model_override");
            }
            let mut event = TokenEvent {
                id,
                timestamp: row.timestamp_ms.div_euclid(1000),
                model,
                model_assumed,
                reasoning_effort: None,
                session: row.session.clone(),
                response_id: row.response_id.clone(),
                turn_id: row.turn_id.clone(),
                source_kind: Some("token_count".into()),
                input: counts.input,
                cached: counts.cached,
                uncached: counts.input.saturating_sub(counts.cached),
                output: counts.output,
                reasoning: counts.reasoning,
                cost_usd: None,
                coverage_start_timestamp_ms: None,
                partial_flags: flags,
            };
            if let Some(index) = event_positions.get(&event.id).copied() {
                let previous = &mut result[index];
                let conflicting_exclusion =
                    previous.is_excluded_auto_review() != event.is_excluded_auto_review();
                if previous.model != event.model || conflicting_exclusion {
                    flag(&mut event.partial_flags, "request_model_changed");
                }
                if conflicting_exclusion {
                    previous
                        .partial_flags
                        .retain(|flag| flag != AUTO_REVIEW_EXCLUDED);
                    event
                        .partial_flags
                        .retain(|flag| flag != AUTO_REVIEW_EXCLUDED);
                    flag(&mut event.partial_flags, "request_recorded_model_conflict");
                }
                if previous.timestamp.div_euclid(86400) != event.timestamp.div_euclid(86400) {
                    flag(&mut event.partial_flags, "request_spans_dates");
                }
                previous.input = previous.input.saturating_add(event.input);
                previous.cached = previous.cached.saturating_add(event.cached);
                previous.uncached = previous.uncached.saturating_add(event.uncached);
                previous.output = previous.output.saturating_add(event.output);
                previous.reasoning = previous.reasoning.saturating_add(event.reasoning);
                previous.model_assumed |= event.model_assumed;
                for partial in event.partial_flags {
                    flag(&mut previous.partial_flags, &partial);
                }
                previous.cost_usd = event_cost(previous, pricing);
            } else {
                event.cost_usd = event_cost(&event, pricing);
                event_positions.insert(event.id.clone(), result.len());
                result.push(event);
            }
        }
        self.reconcile_response_records(&mut result, &recorded_models, pricing);
        self.apply_recorded_efforts(&mut result);
        result.sort_by(|a, b| a.timestamp.cmp(&b.timestamp).then(a.id.cmp(&b.id)));
        self.events = result;
        self.normalization_version = NORMALIZATION_VERSION;
        self.last_pricing = Some(pricing.clone());
        self.refresh_mixed_schema_warning();
        if self.indexes_ready {
            for (index, row) in self.observations.iter().enumerate() {
                if let Some(position) = self.known_observations.get_mut(&row.identity) {
                    *position = index;
                }
            }
        }
        self.usage_dirty = false;
        self.revision = self.revision.saturating_add(1);
    }

    fn apply_recorded_efforts(&mut self, events: &mut [TokenEvent]) {
        self.effort_contexts.sort();
        self.effort_contexts.dedup();
        let recorded = RecordedEfforts::new(&self.effort_contexts);
        let mut versions: BTreeMap<String, Vec<&UsageObservation>> = BTreeMap::new();
        for row in &self.observations {
            let id = if row.source_kind.as_deref() == Some("token_usage_record") {
                row.response_id.as_ref().map(|response| {
                    format!("response:{}:{}:{response}", row.session.len(), row.session)
                })
            } else {
                legacy_event_id(row)
            };
            if let Some(id) = id {
                versions.entry(id).or_default().push(row);
            }
        }
        for event in events {
            // A cumulative gap/residual can span different requests or efforts.
            // Metadata uncertainty must not alter pricing, partial flags or IDs.
            let aggregate = event.partial_flags.iter().any(|flag| {
                matches!(
                    flag.as_str(),
                    "initial_snapshot_contains_unattributed_usage"
                        | "gap_spans_unobserved_requests"
                        | "reset_baseline_uncertain"
                        | "mixed_schema_residual_unattributed"
                        | "legacy_model_scope_uncertain"
                )
            });
            event.reasoning_effort = if aggregate {
                None
            } else {
                versions
                    .get(&event.id)
                    .and_then(|rows| recorded.for_versions(rows))
            };
        }
    }

    /// Only explicit turn boundaries and fully present recorded counters can
    /// certify independence from older ambiguous legacy evidence.
    fn complete_turn_starts<'a>(
        &'a self,
        selected: &BTreeMap<(&'a str, &'a str), &'a UsageObservation>,
        versions: &BTreeMap<(&'a str, &'a str), Vec<&'a UsageObservation>>,
    ) -> BTreeMap<(&'a str, &'a str), i64> {
        #[derive(Default)]
        struct Proof<'a> {
            start: Option<i64>,
            sum: Counts,
            invalid: bool,
            first_response: Option<i64>,
            last_response: Option<i64>,
            snapshots: Vec<&'a UsageObservation>,
        }
        fn add_exact(a: Counts, b: Counts) -> Option<Counts> {
            Some(Counts {
                input: a.input.checked_add(b.input)?,
                cached: a.cached.checked_add(b.cached)?,
                output: a.output.checked_add(b.output)?,
                reasoning: a.reasoning.checked_add(b.reasoning)?,
            })
        }
        let mut proof: BTreeMap<(&str, &str), Proof<'_>> = BTreeMap::new();
        for (&(thread, response), &row) in selected {
            let Some(turn) = row.turn_id.as_deref() else {
                continue;
            };
            let state = proof.entry((thread, turn)).or_default();
            if let Some(sum) = row.last.and_then(|counts| add_exact(state.sum, counts)) {
                state.sum = sum;
            } else {
                state.invalid = true;
            }
            for version in &versions[&(thread, response)] {
                state.first_response = Some(
                    state
                        .first_response
                        .map_or(version.timestamp_ms, |old| old.min(version.timestamp_ms)),
                );
                state.last_response = Some(
                    state
                        .last_response
                        .map_or(version.timestamp_ms, |old| old.max(version.timestamp_ms)),
                );
                if version.turn_id.as_deref() != Some(turn)
                    || version.flags.iter().any(|flag| {
                        flag.ends_with("_count_unavailable")
                            || matches!(
                                flag.as_str(),
                                "invalid_token_counts"
                                    | "missing_token_fields"
                                    | "cached_exceeded_input"
                                    | "reasoning_exceeded_output"
                                    | "invalid_usage_snapshot"
                            )
                    })
                {
                    state.invalid = true;
                }
            }
            // Conflicting simultaneous versions cannot certify a complete turn.
            let rows = &versions[&(thread, response)];
            let mut seen: BTreeMap<i64, (Option<Counts>, Option<&str>)> = BTreeMap::new();
            for version in rows {
                let value = (version.last, version.turn_id.as_deref());
                if seen
                    .insert(version.timestamp_ms, value)
                    .is_some_and(|old| old != value)
                {
                    state.invalid = true;
                }
            }
        }
        for context in &self.effort_contexts {
            if let Some(turn) = context.turn_id.as_deref() {
                if let Some(state) = proof.get_mut(&(context.session.as_str(), turn)) {
                    state.start = Some(
                        state
                            .start
                            .map_or(context.timestamp_ms, |old| old.min(context.timestamp_ms)),
                    );
                }
            }
        }
        for row in &self.observations {
            if row.source_kind.as_deref() == Some("token_usage_record") {
                if let Some(turn) = row.turn_id.as_deref() {
                    if let Some(state) = proof.get_mut(&(row.session.as_str(), turn)) {
                        if row.turn_total.is_some() || row.raw_turn_counts.is_some() {
                            state.snapshots.push(row);
                        }
                    }
                }
            }
        }
        proof
            .into_iter()
            .filter_map(|(key, mut state)| {
                let start = state.start?;
                if state.invalid || state.first_response.is_none_or(|time| time < start) {
                    return None;
                }
                state
                    .snapshots
                    .sort_by_key(|row| (row.timestamp_ms, &row.identity));
                let mut previous: Option<(i64, Counts)> = None;
                for row in &state.snapshots {
                    let raw = row.raw_turn_counts.as_ref()?.complete()?;
                    if row.turn_total != Some(raw)
                        || raw.cached > raw.input
                        || raw.reasoning > raw.output
                        || row
                            .flags
                            .iter()
                            .any(|flag| flag == "invalid_usage_snapshot")
                        || previous.is_some_and(|(time, counts)| {
                            raw.decreased_from(counts)
                                || (time == row.timestamp_ms && counts != raw)
                        })
                    {
                        return None;
                    }
                    previous = Some((row.timestamp_ms, raw));
                }
                let (time, snapshot) = previous?;
                (snapshot == state.sum && state.last_response.is_some_and(|latest| latest <= time))
                    .then_some((key, start))
            })
            .collect()
    }

    /// A response is one completed provider response, a turn is `(thread_id,
    /// turn_id)`, and a task is the logical thread. Root session/turn IDs do not
    /// create an inferred parent-task grouping. Official schema checked 2026-10-17:
    /// https://raw.githubusercontent.com/openai/codex/main/codex-rs/protocol/src/protocol.rs
    fn reconcile_response_records(
        &self,
        legacy: &mut Vec<TokenEvent>,
        recorded_models: &RecordedModels<'_>,
        pricing: &Pricing,
    ) {
        let mut selected: BTreeMap<(&str, &str), &UsageObservation> = BTreeMap::new();
        let mut first_seen: BTreeMap<(&str, &str), i64> = BTreeMap::new();
        let mut versions: BTreeMap<(&str, &str), Vec<&UsageObservation>> = BTreeMap::new();
        for row in self
            .observations
            .iter()
            .filter(|r| r.source_kind.as_deref() == Some("token_usage_record"))
        {
            let Some(response) = row.response_id.as_deref() else {
                continue;
            };
            let key = (row.session.as_str(), response);
            first_seen
                .entry(key)
                .and_modify(|t| *t = (*t).min(row.timestamp_ms))
                .or_insert(row.timestamp_ms);
            versions.entry(key).or_default().push(row);
            // Timestamp chooses an update; a deterministic tie-break makes file
            // order/restarts irrelevant. Conflicting ties remain visibly unknown.
            selected
                .entry(key)
                .and_modify(|old| {
                    if (row.timestamp_ms, &row.identity) > (old.timestamp_ms, &old.identity) {
                        *old = row;
                    }
                })
                .or_insert(row);
        }
        if selected.is_empty() {
            return;
        }
        let mut by_thread: BTreeMap<&str, Vec<&UsageObservation>> = BTreeMap::new();
        for ((thread, _), row) in &selected {
            by_thread.entry(thread).or_default().push(*row);
        }
        let response_turns: BTreeMap<_, _> = by_thread
            .iter()
            .map(|(thread, rows)| {
                (
                    *thread,
                    (
                        rows.iter().all(|r| r.turn_id.is_some()),
                        rows.iter()
                            .filter_map(|r| r.turn_id.as_deref())
                            .collect::<BTreeSet<_>>(),
                    ),
                )
            })
            .collect();
        let mut legacy_rows: BTreeMap<String, Vec<&UsageObservation>> = BTreeMap::new();
        let mut checkpoints: BTreeMap<&str, Vec<(i64, Counts)>> = BTreeMap::new();
        for row in &self.observations {
            if row.source_kind.as_deref() != Some("token_usage_record") {
                if let Some(id) = legacy_event_id(row) {
                    legacy_rows
                        .entry(format!("{id}:unattributed"))
                        .or_default()
                        .push(row);
                    legacy_rows.entry(id).or_default().push(row);
                }
            }
            if let Some(counts) = row.total.or(row.thread_total) {
                checkpoints
                    .entry(&row.session)
                    .or_default()
                    .push((row.timestamp_ms, counts));
            }
        }
        let monotonic: BTreeSet<_> = checkpoints
            .iter_mut()
            .filter_map(|(thread, rows)| {
                rows.sort_by_key(|(t, c)| (*t, c.input, c.output, c.cached, c.reasoning));
                rows.windows(2)
                    .all(|w| !w[1].1.decreased_from(w[0].1))
                    .then_some(*thread)
            })
            .collect();
        let coverage: BTreeMap<_, _> = by_thread
            .iter()
            .map(|(thread, responses)| {
                let ranges: Vec<_> = responses
                    .iter()
                    .filter_map(|r| {
                        let usage = r.last?;
                        let end = r.thread_total?;
                        (!end.decreased_from(usage)
                            && !r.flags.iter().any(|f| f == "invalid_usage_snapshot"))
                        .then_some((end.sub(usage), end))
                    })
                    .collect();
                let complete = ranges.len() == responses.len();
                (*thread, (CoverageRanges::new(ranges), complete))
            })
            .collect();
        let mut coverage_unknown: BTreeMap<String, i64> = BTreeMap::new();
        let mut conflicting_turns: BTreeSet<(String, String)> = BTreeSet::new();
        let mut conflicting_responses: BTreeSet<(String, String)> = BTreeSet::new();
        let mut anonymous_legacy_overlap: Option<i64> = None;
        let mut kept = Vec::new();
        for mut event in legacy.drain(..) {
            let rows = legacy_rows.get(&event.id).map(Vec::as_slice).unwrap_or(&[]);
            let upper_bound = rows
                .iter()
                .map(|r| r.timestamp_ms)
                .max()
                .unwrap_or_else(|| event.timestamp.saturating_mul(1000).saturating_add(999));
            let Some(_) = by_thread.get(event.session.as_str()) else {
                if event
                    .partial_flags
                    .iter()
                    .any(|f| f == "session_identity_missing")
                {
                    // An anonymous legacy archive cannot establish that it belongs
                    // to a different task. Preserve its observations, but avoid an
                    // unprovable addition when authoritative records are present.
                    // A later, explicitly started and fully reconciled turn is
                    // independent of this historical unknown. Ownership itself
                    // remains unknown; no cross-thread match is inferred.
                    anonymous_legacy_overlap = Some(
                        anonymous_legacy_overlap.map_or(upper_bound, |old| old.max(upper_bound)),
                    );
                } else {
                    kept.push(event);
                }
                continue;
            };
            let matched_response = event
                .response_id
                .as_deref()
                .and_then(|id| selected.get(&(event.session.as_str(), id)))
                .or_else(|| {
                    rows.iter().find_map(|r| {
                        r.request_id
                            .as_deref()
                            .and_then(|id| selected.get(&(event.session.as_str(), id)))
                    })
                });
            if let Some(response) = matched_response {
                if response.last != Some(event_counts(&event)) {
                    coverage_unknown
                        .entry(event.session.clone())
                        .and_modify(|old| *old = (*old).max(upper_bound))
                        .or_insert(upper_bound);
                    if let Some(response_id) = &response.response_id {
                        conflicting_responses.insert((event.session.clone(), response_id.clone()));
                    }
                    if let Some(turn) = &response.turn_id {
                        conflicting_turns.insert((event.session.clone(), turn.clone()));
                    }
                }
                continue;
            }
            let no_gap = !event.partial_flags.iter().any(|f| {
                matches!(
                    f.as_str(),
                    "initial_snapshot_contains_unattributed_usage"
                        | "gap_spans_unobserved_requests"
                        | "counter_reset_or_compaction"
                        | "reset_baseline_uncertain"
                )
            });
            // A trustworthy, explicitly different turn is disjoint. Never infer a
            // turn from a timestamp, filename, root_turn_id, or event ordinal.
            if no_gap
                && event.turn_id.as_deref().is_some_and(|turn| {
                    let (complete, turns) = &response_turns[event.session.as_str()];
                    *complete && !turns.contains(turn)
                })
            {
                kept.push(event);
                continue;
            }
            let endpoint = rows.last().and_then(|r| {
                r.total.map(|total| {
                    if event.id.ends_with(":unattributed") {
                        total.sub(r.last.unwrap_or_default())
                    } else {
                        total
                    }
                })
            });
            // One legacy aggregate endpoint plus non-reset cumulative coordinates
            // lets us remove covered response ranges without relying on file order.
            if let (Some((ranges, all_responses_comparable)), Some(end)) =
                (coverage.get(event.session.as_str()), endpoint)
            {
                if monotonic.contains(event.session.as_str())
                    && rows.iter().all(|r| r.total.is_some())
                {
                    let old = event_counts(&event);
                    if !end.decreased_from(old) {
                        let residual = ranges.uncovered(end.sub(old), end);
                        // Fully covered legacy usage remains proven overlap
                        // even if a newer response omits its thread snapshot.
                        // A residual, however, might overlap that unknown range;
                        // add it only when every response has comparable ranges.
                        if residual.is_zero() {
                            continue;
                        }
                        if *all_responses_comparable {
                            if residual == old {
                                kept.push(event);
                            } else {
                                set_event_counts(&mut event, residual);
                                flag(
                                    &mut event.partial_flags,
                                    "mixed_schema_residual_unattributed",
                                );
                                event.response_id = None;
                                event.cost_usd = None;
                                kept.push(event);
                            }
                            continue;
                        }
                    }
                }
            }
            // Without an ID or comparable cumulative axis the overlap is unknown.
            // Choosing response records prevents double counting; mark the result
            // incomplete instead of asserting that discarded legacy usage was zero.
            coverage_unknown
                .entry(event.session.clone())
                .and_modify(|old| *old = (*old).max(upper_bound))
                .or_insert(upper_bound);
            if let Some(turn) = &event.turn_id {
                conflicting_turns.insert((event.session.clone(), turn.clone()));
            }
        }
        let mut turn_sums: BTreeMap<(&str, &str), Counts> = BTreeMap::new();
        let mut thread_sums: BTreeMap<&str, Counts> = BTreeMap::new();
        for ((thread, _), row) in &selected {
            if let Some(counts) = row.last {
                thread_sums
                    .entry(thread)
                    .and_modify(|c| *c = c.add(counts))
                    .or_insert(counts);
                if let Some(turn) = row.turn_id.as_deref() {
                    turn_sums
                        .entry((thread, turn))
                        .and_modify(|c| *c = c.add(counts))
                        .or_insert(counts);
                }
            }
        }
        for event in &kept {
            let counts = event_counts(event);
            thread_sums
                .entry(&event.session)
                .and_modify(|c| *c = c.add(counts))
                .or_insert(counts);
            if let Some(turn) = event.turn_id.as_deref() {
                turn_sums
                    .entry((&event.session, turn))
                    .and_modify(|c| *c = c.add(counts))
                    .or_insert(counts);
            }
        }
        let mut turn_snapshots: BTreeMap<(&str, &str), &UsageObservation> = BTreeMap::new();
        let mut thread_snapshots: BTreeMap<&str, &UsageObservation> = BTreeMap::new();
        // Observations are chronological; ties use the same stable choice as responses.
        for row in self
            .observations
            .iter()
            .filter(|r| r.source_kind.as_deref() == Some("token_usage_record"))
        {
            if row.thread_total.is_some() {
                thread_snapshots
                    .entry(&row.session)
                    .and_modify(|old| {
                        if (row.timestamp_ms, &row.identity) > (old.timestamp_ms, &old.identity) {
                            *old = row;
                        }
                    })
                    .or_insert(row);
            }
            if let Some(turn) = row.turn_id.as_deref().filter(|_| row.turn_total.is_some()) {
                turn_snapshots
                    .entry((&row.session, turn))
                    .and_modify(|old| {
                        if (row.timestamp_ms, &row.identity) > (old.timestamp_ms, &old.identity) {
                            *old = row;
                        }
                    })
                    .or_insert(row);
            }
        }
        let thread_gaps: BTreeSet<_> = thread_snapshots
            .iter()
            .filter_map(|(thread, row)| {
                let observed = thread_sums.get(thread).copied().unwrap_or_default();
                row.thread_total
                    .filter(|snapshot| observed.decreased_from(*snapshot))
                    .map(|_| (*thread).to_owned())
            })
            .collect();
        let turn_gaps: BTreeSet<_> = turn_snapshots
            .iter()
            .filter_map(|((thread, turn), row)| {
                let observed = turn_sums
                    .get(&(*thread, *turn))
                    .copied()
                    .unwrap_or_default();
                row.turn_total
                    .filter(|snapshot| observed.decreased_from(*snapshot))
                    .map(|_| ((*thread).to_owned(), (*turn).to_owned()))
            })
            .collect();
        let complete_turn_starts = self.complete_turn_starts(&selected, &versions);
        for ((thread, response), row) in selected {
            let Some(counts) = row.last else { continue };
            let mut flags = row.flags.clone();
            if thread_gaps.contains(thread)
                || row
                    .turn_id
                    .as_ref()
                    .is_some_and(|turn| turn_gaps.contains(&(thread.to_owned(), turn.clone())))
            {
                flag(&mut flags, "response_coverage_gap");
            }
            let key = (thread, response);
            if versions[&key].iter().any(|other| other.last != row.last) {
                flag(&mut flags, "request_usage_updated");
            }
            if versions[&key].iter().any(|other| {
                other.timestamp_ms == row.timestamp_ms
                    && (other.last != row.last || other.turn_id != row.turn_id)
            }) {
                flag(&mut flags, "response_record_conflict");
            }
            let coverage_start = row
                .turn_id
                .as_deref()
                .and_then(|turn| complete_turn_starts.get(&(thread, turn)).copied());
            let uncertainty_bound = coverage_unknown
                .get(thread)
                .copied()
                .into_iter()
                .chain(anonymous_legacy_overlap)
                .max();
            let explicit_conflict = conflicting_responses
                .contains(&(thread.to_owned(), response.to_owned()))
                || row.turn_id.as_ref().is_some_and(|turn| {
                    conflicting_turns.contains(&(thread.to_owned(), turn.clone()))
                });
            if explicit_conflict
                || uncertainty_bound
                    .is_some_and(|bound| coverage_start.is_none_or(|start| start <= bound))
            {
                flag(&mut flags, "mixed_schema_legacy_overlap_suppressed");
            }
            // Updated/replayed records may omit the model that an earlier
            // version of this exact response explicitly recorded. Preserve that
            // identity rather than falling back to a later thread model or Sol.
            let explicit_models: BTreeSet<&str> = versions[&key]
                .iter()
                .filter_map(|version| version.model.as_deref())
                .collect();
            let explicit = versions[&key]
                .iter()
                .filter(|version| version.model.is_some())
                .max_by_key(|version| (version.timestamp_ms, &version.identity))
                .and_then(|version| version.model.as_deref());
            let recorded = explicit
                .or_else(|| recorded_models.recorded(row))
                .or_else(|| {
                    versions[&key]
                        .iter()
                        .find_map(|version| recorded_models.recorded(version))
                });
            if explicit_models.len() > 1 {
                flag(&mut flags, "response_model_conflict");
            } else if recorded == Some(AUTO_REVIEW_MODEL) {
                flag(&mut flags, AUTO_REVIEW_EXCLUDED);
            }
            let chosen = pricing
                .model_override
                .as_deref()
                .filter(|m| !m.trim().is_empty());
            let assumed = if pricing.assumed_model.trim().is_empty() {
                DEFAULT_ASSUMED_MODEL
            } else {
                &pricing.assumed_model
            };
            if chosen.is_some() {
                flag(&mut flags, "model_override");
            }
            let mut event = TokenEvent {
                // Length-prefix prevents separator collisions in opaque IDs.
                id: format!("response:{}:{thread}:{response}", thread.len()),
                timestamp: first_seen[&key].div_euclid(1000),
                model: chosen.or(recorded).unwrap_or(assumed).to_owned(),
                model_assumed: chosen.is_none() && recorded.is_none(),
                reasoning_effort: None,
                session: thread.into(),
                response_id: Some(response.into()),
                turn_id: row.turn_id.clone(),
                source_kind: Some("token_usage_record".into()),
                input: counts.input,
                cached: counts.cached,
                uncached: counts.input.saturating_sub(counts.cached),
                output: counts.output,
                reasoning: counts.reasoning,
                cost_usd: None,
                coverage_start_timestamp_ms: coverage_start,
                partial_flags: flags,
            };
            event.cost_usd = event_cost(&event, pricing);
            kept.push(event);
        }
        *legacy = kept;
    }
}

fn legacy_event_id(row: &UsageObservation) -> Option<String> {
    if let Some(id) = &row.request_id {
        return Some(format!("{}:request:{id}", row.session));
    }
    if let Some(total) = row.total {
        return Some(format!("{}:cumulative:{}", row.session, total.signature()));
    }
    row.last.map(|last| {
        format!(
            "{}:last:{}:{}:{}",
            row.session,
            row.timestamp_ms,
            last.signature(),
            row.model.as_deref().unwrap_or("")
        )
    })
}
fn event_counts(event: &TokenEvent) -> Counts {
    Counts {
        input: event.input,
        cached: event.cached,
        output: event.output,
        reasoning: event.reasoning,
    }
}
fn set_event_counts(event: &mut TokenEvent, counts: Counts) {
    let counts = counts.normalized(&mut event.partial_flags);
    event.input = counts.input;
    event.cached = counts.cached;
    event.uncached = counts.input.saturating_sub(counts.cached);
    event.output = counts.output;
    event.reasoning = counts.reasoning;
}
/// Subtract the union of half-open response ranges in each additive counter.
/// Snapshot values are coordinates, never values to sum as usage events.
struct CoverageRanges {
    input: Vec<(u64, u64)>,
    cached: Vec<(u64, u64)>,
    output: Vec<(u64, u64)>,
    reasoning: Vec<(u64, u64)>,
}
impl CoverageRanges {
    fn new(ranges: Vec<(Counts, Counts)>) -> Self {
        fn merge(mut rows: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
            rows.sort_unstable();
            let mut result: Vec<(u64, u64)> = Vec::new();
            for (a, b) in rows.into_iter().filter(|(a, b)| b > a) {
                if let Some(last) = result.last_mut().filter(|last| a <= last.1) {
                    last.1 = last.1.max(b);
                } else {
                    result.push((a, b));
                }
            }
            result
        }
        Self {
            input: merge(ranges.iter().map(|(a, b)| (a.input, b.input)).collect()),
            cached: merge(ranges.iter().map(|(a, b)| (a.cached, b.cached)).collect()),
            output: merge(ranges.iter().map(|(a, b)| (a.output, b.output)).collect()),
            reasoning: merge(
                ranges
                    .iter()
                    .map(|(a, b)| (a.reasoning, b.reasoning))
                    .collect(),
            ),
        }
    }
    fn uncovered(&self, start: Counts, end: Counts) -> Counts {
        fn one(start: u64, end: u64, ranges: &[(u64, u64)]) -> u64 {
            let at = ranges.partition_point(|(_, b)| *b <= start);
            let covered = ranges[at..]
                .iter()
                .take_while(|(a, _)| *a < end)
                .map(|(a, b)| b.min(&end).saturating_sub(*a.max(&start)))
                .fold(0u64, u64::saturating_add);
            end.saturating_sub(start).saturating_sub(covered)
        }
        Counts {
            input: one(start.input, end.input, &self.input),
            cached: one(start.cached, end.cached, &self.cached),
            output: one(start.output, end.output, &self.output),
            reasoning: one(start.reasoning, end.reasoning, &self.reasoning),
        }
    }
}

pub(crate) fn event_cost(event: &TokenEvent, pricing: &Pricing) -> Option<f64> {
    if event.partial_flags.iter().any(|s| {
        matches!(
            s.as_str(),
            "request_model_changed"
                | "request_recorded_model_conflict"
                | "legacy_model_scope_uncertain"
                | "initial_snapshot_contains_unattributed_usage"
                | "gap_spans_unobserved_requests"
                | "reset_baseline_uncertain"
                | "missing_token_fields"
                | "input_count_unavailable"
                | "cached_count_unavailable"
                | "output_count_unavailable"
                | "invalid_token_counts"
                | "cached_exceeded_input"
                | "reasoning_exceeded_output"
                | "mixed_schema_residual_unattributed"
                | "response_record_conflict"
                | "response_model_conflict"
                | "cache_write_pricing_unmodeled"
        )
    }) {
        return None;
    }
    pricing.cost(&event.model, event.uncached, event.cached, event.output)
}
fn flag(flags: &mut Vec<String>, value: &str) {
    if !flags.iter().any(|flag| flag == value) {
        flags.push(value.into());
    }
}
fn effort_fields(value: &Value, names: &[&str]) -> BTreeSet<String> {
    // Codex's ReasoningEffort schema permits nonempty custom labels. Keep the
    // recorded label without deriving anything from the model or token count.
    names
        .iter()
        .filter_map(|name| value.get(*name).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| {
            !value.is_empty() && !matches!(*value, "unknown" | "default" | "unavailable" | "n/a")
        })
        .map(str::to_owned)
        .collect()
}
/// serde_json issues small writes for individual tokens. Batch them before
/// reaching the filesystem, and propagate flush errors before sync/rename.
fn write_token_store(writer: impl Write, store: &TokenStore) -> io::Result<()> {
    let mut buffered = BufWriter::with_capacity(256 * 1024, writer);
    serde_json::to_writer(&mut buffered, store).map_err(invalid_data)?;
    buffered.write_all(b"\n")?;
    buffered.flush()
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}
fn text_field(value: &Value, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        value
            .get(*name)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
    })
}
fn parse_timestamp(value: &Value) -> Option<i64> {
    let value = value.get("timestamp").or_else(|| value.get("time"))?;
    if let Some(s) = value.as_str() {
        return DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.timestamp_millis());
    }
    let numeric = value.as_i64()?;
    if numeric.unsigned_abs() > 100_000_000_000 {
        Some(numeric)
    } else {
        numeric.checked_mul(1000)
    }
}
fn raw_count(value: &Value, names: &[&str]) -> Option<u64> {
    names
        .iter()
        .find_map(|name| value.get(*name))
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
}
fn parse_counts(value: &Value, flags: &mut Vec<String>) -> Option<Counts> {
    for (names, unavailable) in [
        (&["input_tokens", "input"][..], "input_count_unavailable"),
        (
            &["cached_input_tokens", "cache_read_input_tokens", "cached"][..],
            "cached_count_unavailable",
        ),
        (&["output_tokens", "output"][..], "output_count_unavailable"),
        (
            &["reasoning_output_tokens", "reasoning_tokens", "reasoning"][..],
            "reasoning_count_unavailable",
        ),
    ] {
        if raw_count(value, names).is_none() {
            flag(flags, unavailable);
        }
    }
    if !value.is_object() {
        flag(flags, "invalid_token_counts");
        return None;
    }
    fn count(
        value: &Value,
        names: &[&str],
        required: bool,
        flags: &mut Vec<String>,
    ) -> Option<u64> {
        if let Some(v) = names.iter().find_map(|name| value.get(*name)) {
            let n = v
                .as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()));
            if n.is_none() {
                flag(flags, "invalid_token_counts");
            }
            n
        } else {
            if required {
                flag(flags, "missing_token_fields");
            }
            Some(0)
        }
    }
    if count(value, &["cache_write_input_tokens"], false, flags)? > 0 {
        flag(flags, "cache_write_pricing_unmodeled");
    }
    Some(
        Counts {
            input: count(value, &["input_tokens", "input"], true, flags)?,
            cached: count(
                value,
                &["cached_input_tokens", "cache_read_input_tokens", "cached"],
                false,
                flags,
            )?,
            output: count(value, &["output_tokens", "output"], true, flags)?,
            reasoning: count(
                value,
                &["reasoning_output_tokens", "reasoning_tokens", "reasoning"],
                false,
                flags,
            )?,
        }
        .normalized(flags),
    )
}
fn modified_ns(metadata: &fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_nanos()).ok())
}
#[cfg(unix)]
fn changed_ns(metadata: &fs::Metadata) -> Option<i128> {
    use std::os::unix::fs::MetadataExt;
    Some(i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec()))
}
#[cfg(not(unix))]
fn changed_ns(_metadata: &fs::Metadata) -> Option<i128> {
    None
}
fn source_unchanged(cursor: &SourceCursor, metadata: &fs::Metadata) -> bool {
    // A missing filesystem timestamp cannot establish that the source is unchanged.
    cursor.modified_ns.is_some()
        && cursor.modified_ns == modified_ns(metadata)
        && cursor.observed_len == metadata.len()
        && cursor.file_identity == file_identity(metadata)
        && cursor.changed_ns == changed_ns(metadata)
        && cursor.offset <= metadata.len()
}
fn source_rewritten(
    file: &mut File,
    cursor: &SourceCursor,
    metadata: &fs::Metadata,
) -> io::Result<bool> {
    Ok(metadata.len() < cursor.offset
        || (cursor.file_identity.is_some() && cursor.file_identity != file_identity(metadata))
        || (cursor.offset > 0
            && cursor.modified_ns.is_some()
            && (cursor.modified_ns != modified_ns(metadata)
                || cursor.changed_ns != changed_ns(metadata))
            && metadata.len() <= cursor.observed_len)
        || (cursor.prefix_len > 0 && hash_range(file, 0, cursor.prefix_len)? != cursor.prefix_hash)
        || (cursor.anchor_len > 0
            && hash_range(
                file,
                cursor.offset.saturating_sub(cursor.anchor_len as u64),
                cursor.anchor_len,
            )? != cursor.anchor_hash))
}
fn finish_cursor(
    file: &mut File,
    cursor: &mut SourceCursor,
    metadata: &fs::Metadata,
) -> io::Result<()> {
    cursor.prefix_len = (cursor.offset as usize).min(256);
    cursor.anchor_len = (cursor.offset as usize).min(128);
    cursor.prefix_hash = hash_range(file, 0, cursor.prefix_len)?;
    cursor.anchor_hash = hash_range(
        file,
        cursor.offset.saturating_sub(cursor.anchor_len as u64),
        cursor.anchor_len,
    )?;
    cursor.observed_len = metadata.len();
    cursor.modified_ns = modified_ns(metadata);
    cursor.file_identity = file_identity(metadata);
    cursor.changed_ns = changed_ns(metadata);
    Ok(())
}
#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    Some(format!("{}:{}", metadata.dev(), metadata.ino()))
}
#[cfg(not(unix))]
fn file_identity(_metadata: &fs::Metadata) -> Option<String> {
    None
}
fn stable_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}
fn legacy_quota_identity(observation: &ImportedQuotaObservation) -> String {
    format!("{}:{}", observation.timestamp, observation.payload)
}
fn quota_identity(observation: &ImportedQuotaObservation) -> String {
    // Same snapshot copied to another source in the same logical session remains
    // one observation; different sessions retain independent provenance evidence.
    format!(
        "{}:{}:{}",
        observation
            .timestamp_ms
            .unwrap_or(observation.timestamp.saturating_mul(1000)),
        observation
            .origin_session
            .as_deref()
            .or(observation.source_id.as_deref())
            .unwrap_or(""),
        observation.payload
    )
}
fn hash_range(file: &mut File, offset: u64, len: usize) -> io::Result<u64> {
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; len];
    file.read_exact(&mut bytes)?;
    Ok(stable_hash(&bytes))
}
/// Bounded memory even for very large conversation records. Final incomplete rows are deferred.
fn read_bounded_line(reader: &mut impl BufRead) -> io::Result<(Vec<u8>, u64, bool, bool)> {
    let mut output = Vec::new();
    let mut length = 0u64;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok((output, length, false, length > MAX_LINE_BYTES as u64));
        }
        let found = buffer.iter().position(|byte| *byte == b'\n');
        let used = found.map_or(buffer.len(), |at| at + 1);
        let copy = used.min(MAX_LINE_BYTES.saturating_sub(output.len()));
        output.extend_from_slice(&buffer[..copy]);
        length = length.saturating_add(used as u64);
        reader.consume(used);
        if found.is_some() {
            return Ok((output, length, true, length > MAX_LINE_BYTES as u64));
        }
    }
}
fn collect_jsonl(
    path: &Path,
    output: &mut Vec<PathBuf>,
    report: &mut ImportReport,
) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_file() {
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        {
            output.push(path.to_owned());
        }
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            match entry {
                Ok(entry) => {
                    if let Err(error) = collect_jsonl(&entry.path(), output, report) {
                        report.warnings.push(format!(
                            "Could not inspect {}: {error}",
                            entry.path().display()
                        ));
                    }
                }
                Err(error) => report
                    .warnings
                    .push(format!("Could not list {}: {error}", path.display())),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn token_store_writer_batches_small_json_writes_and_preserves_bytes() {
        #[derive(Default)]
        struct CountWriter {
            calls: usize,
            bytes: Vec<u8>,
        }
        impl Write for CountWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.calls += 1;
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut store = TokenStore::demo(1_791_158_400);
        let row = store.events[0].clone();
        store.events = vec![row; 10_000];
        let mut expected = serde_json::to_vec(&store).unwrap();
        expected.push(b'\n');
        let mut writer = CountWriter::default();
        write_token_store(&mut writer, &store).unwrap();
        assert_eq!(writer.bytes, expected);
        assert!(writer.calls > 1, "exercise multiple buffer flushes");
        assert!(
            writer.calls <= expected.len() / (128 * 1024) + 2,
            "small JSON tokens must not become individual filesystem writes: {} writes for {} bytes",
            writer.calls,
            expected.len()
        );
    }

    #[test]
    fn token_store_writer_propagates_buffer_write_and_flush_failures() {
        struct FailingWriter {
            fail_write: bool,
        }
        impl Write for FailingWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.fail_write {
                    Err(io::Error::other("synthetic write failure"))
                } else {
                    Ok(bytes.len())
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("synthetic flush failure"))
            }
        }
        for fail_write in [true, false] {
            let error = write_token_store(FailingWriter { fail_write }, &TokenStore::default())
                .unwrap_err();
            assert!(error.to_string().contains(if fail_write {
                "synthetic write failure"
            } else {
                "synthetic flush failure"
            }));
        }
    }

    fn c(input: u64, cached: u64, output: u64, reasoning: u64) -> Value {
        json!({"input_tokens":input,"cached_input_tokens":cached,"output_tokens":output,"reasoning_output_tokens":reasoning})
    }
    fn usage(
        second: u32,
        total: Option<Value>,
        last: Option<Value>,
        request: Option<&str>,
    ) -> String {
        json!({"timestamp":format!("2026-10-01T09:00:{second:02}Z"),"type":"event_msg","payload":{
            "type":"token_count","request_id":request,"info":{"total_token_usage":total,"last_token_usage":last}}}).to_string()
    }
    fn metadata() -> &'static str {
        "{\"timestamp\":\"2026-10-01T09:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"test-session\"}}"
    }
    fn write_log(dir: &TempDir, name: &str, rows: &[String]) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, format!("{}\n{}\n", metadata(), rows.join("\n"))).unwrap();
        path
    }
    fn sample_pricing() -> Pricing {
        let mut pricing = Pricing::default();
        // Deliberately synthetic TEST rates. These are not supplied as real model prices.
        pricing
            .set_rate(
                DEFAULT_ASSUMED_MODEL,
                ModelPrice {
                    uncached_input_per_million: 2.0,
                    cached_input_per_million: 0.5,
                    output_per_million: 6.0,
                },
            )
            .unwrap();
        pricing
    }
    #[test]
    fn repeated_cumulative_at_other_times_and_in_other_files_counts_once() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("one.jsonl"),
            include_str!("../fixtures/token_mixed_models.jsonl"),
        )
        .unwrap();
        fs::write(
            dir.path().join("archive.jsonl"),
            include_str!("../fixtures/token_duplicate_archive.jsonl"),
        )
        .unwrap();
        let mut store = TokenStore::default();
        let report = store.import_dir(dir.path(), &Pricing::default()).unwrap();
        assert_eq!(report.events_added, 2);
        assert_eq!(store.events().iter().map(|e| e.input).sum::<u64>(), 160);
        assert_eq!(store.events()[0].model, "gpt-6.1-sol");
        assert_eq!(store.events()[1].model, "gpt-6-luna");
        assert_eq!(store.events()[1].input, 60);
        assert_eq!(store.quota_observations().len(), 2);
        assert_eq!(report.quota_observations.len(), 2);
        assert!(store.events().iter().all(|event| event.cost_usd.is_none()));
    }
    #[test]
    fn equal_last_usage_distinct_requests_both_count_and_replayed_id_does_not() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "requests.jsonl",
            &[
                usage(1, None, Some(c(10, 2, 3, 1)), Some("first")),
                usage(1, None, Some(c(10, 2, 3, 1)), Some("second")),
                usage(2, None, Some(c(10, 2, 3, 1)), Some("first")),
            ],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &Pricing::default()).unwrap();
        assert_eq!(store.events().len(), 2);
        assert_eq!(store.events().iter().map(|e| e.input).sum::<u64>(), 20);
    }
    #[test]
    fn equal_last_usage_without_ids_counts_via_cumulative_deltas() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "cumulative.jsonl",
            &[
                usage(1, Some(c(10, 2, 3, 1)), Some(c(10, 2, 3, 1)), None),
                usage(2, Some(c(20, 4, 6, 2)), Some(c(10, 2, 3, 1)), None),
            ],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &Pricing::default()).unwrap();
        assert_eq!(store.events().len(), 2);
        assert_eq!(store.events()[0].input, 10);
        assert_eq!(store.events()[1].input, 10);
    }
    #[test]
    fn restart_preserves_offsets_and_session_baselines() {
        let dir = TempDir::new().unwrap();
        let log = write_log(
            &dir,
            "restart.jsonl",
            &[usage(1, Some(c(10, 2, 3, 1)), Some(c(10, 2, 3, 1)), None)],
        );
        let state = dir.path().join("state.json");
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &Pricing::default()).unwrap();
        store.save(&state).unwrap();
        let mut store = TokenStore::load(&state).unwrap();
        let no_change = store.import_dir(dir.path(), &Pricing::default()).unwrap();
        assert_eq!(no_change.lines_read, 0);
        assert_eq!(no_change.events_added, 0);
        let mut file = OpenOptions::new().append(true).open(log).unwrap();
        writeln!(
            file,
            "{}",
            usage(2, Some(c(20, 4, 6, 2)), Some(c(10, 2, 3, 1)), None)
        )
        .unwrap();
        let report = store.import_dir(dir.path(), &Pricing::default()).unwrap();
        assert_eq!(report.lines_read, 1);
        assert_eq!(report.events_added, 1);
        assert_eq!(store.events().iter().map(|e| e.input).sum::<u64>(), 20);
    }
    #[test]
    fn session_cumulative_baseline_crosses_files() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "a.jsonl",
            &[usage(1, Some(c(10, 2, 3, 1)), Some(c(10, 2, 3, 1)), None)],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &Pricing::default()).unwrap();
        write_log(
            &dir,
            "b.jsonl",
            &[usage(2, Some(c(20, 4, 6, 2)), Some(c(10, 2, 3, 1)), None)],
        );
        store.import_dir(dir.path(), &Pricing::default()).unwrap();
        assert_eq!(store.events().iter().map(|e| e.input).sum::<u64>(), 20);
        assert_eq!(store.events()[1].input, 10);
    }
    #[test]
    fn partial_final_line_waits_for_newline_then_imports_once() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("partial.jsonl");
        let line = usage(1, Some(c(10, 0, 2, 1)), Some(c(10, 0, 2, 1)), None);
        fs::write(
            &path,
            format!("{}\n{}", metadata(), &line[..line.len() / 2]),
        )
        .unwrap();
        let mut store = TokenStore::default();
        let report = store.import_dir(dir.path(), &Pricing::default()).unwrap();
        assert_eq!(report.partial_lines, 1);
        assert!(store.events().is_empty());
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{}", &line[line.len() / 2..]).unwrap();
        store.import_dir(dir.path(), &Pricing::default()).unwrap();
        assert_eq!(store.events().len(), 1);
        assert_eq!(
            store
                .import_dir(dir.path(), &Pricing::default())
                .unwrap()
                .lines_read,
            0
        );
    }
    #[test]
    fn truncation_replays_without_double_counting_and_counter_reset_is_partial() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "truncate.jsonl",
            &[
                usage(1, Some(c(100, 20, 40, 10)), Some(c(100, 20, 40, 10)), None),
                usage(2, Some(c(150, 30, 60, 15)), Some(c(50, 10, 20, 5)), None),
            ],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &Pricing::default()).unwrap();
        write_log(
            &dir,
            "truncate.jsonl",
            &[usage(
                3,
                Some(c(5, 1, 2, 1)),
                Some(c(5, 1, 2, 1)),
                Some("after-reset"),
            )],
        );
        let report = store.import_dir(dir.path(), &Pricing::default()).unwrap();
        assert_eq!(report.source_resets, 1);
        assert_eq!(store.events().iter().map(|e| e.input).sum::<u64>(), 155);
        assert!(store.events()[2]
            .partial_flags
            .contains(&"counter_reset_or_compaction".into()));
        assert!(store.events()[2]
            .partial_flags
            .contains(&"source_rewritten".into()));
        assert_eq!(store.events()[2].uncached, 4);
    }
    #[test]
    fn rewritten_same_size_file_is_detected_and_repeated_usage_stays_deduped() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "rewrite.jsonl",
            &[usage(1, Some(c(10, 0, 2, 0)), Some(c(10, 0, 2, 0)), None)],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &Pricing::default()).unwrap();
        write_log(
            &dir,
            "rewrite.jsonl",
            &[usage(2, Some(c(10, 0, 2, 0)), Some(c(10, 0, 2, 0)), None)],
        );
        let report = store.import_dir(dir.path(), &Pricing::default()).unwrap();
        assert_eq!(report.source_resets, 1);
        assert_eq!(store.events().len(), 1);
    }
    #[test]
    fn historical_archive_added_later_reconciles_instead_of_resetting() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "new.jsonl",
            &[usage(2, Some(c(20, 4, 6, 2)), Some(c(10, 2, 3, 1)), None)],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &Pricing::default()).unwrap();
        assert!(store.events()[0].is_partial());
        write_log(
            &dir,
            "old.jsonl",
            &[usage(1, Some(c(10, 2, 3, 1)), Some(c(10, 2, 3, 1)), None)],
        );
        store.import_dir(dir.path(), &Pricing::default()).unwrap();
        assert_eq!(store.events().iter().map(|e| e.input).sum::<u64>(), 20);
        assert!(store.events().iter().all(|event| !event.is_partial()));
    }
    #[test]
    fn gap_is_counted_once_with_unknown_cost_and_no_fabricated_quota() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "gap.jsonl",
            &[
                usage(1, Some(c(10, 2, 3, 1)), Some(c(10, 2, 3, 1)), None),
                usage(2, Some(c(40, 8, 12, 4)), Some(c(10, 2, 3, 1)), None),
            ],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &sample_pricing()).unwrap();
        assert_eq!(store.events()[1].input, 30);
        assert!(store.events()[1]
            .partial_flags
            .contains(&"gap_spans_unobserved_requests".into()));
        assert_eq!(store.events()[1].cost_usd, None);
        assert!(store.quota_observations().is_empty());
    }
    #[test]
    fn malformed_lines_and_bad_counters_do_not_poison_following_events() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "bad.jsonl",
            &[
                "this is not json".into(),
                usage(
                    1,
                    None,
                    Some(json!({"input_tokens":-1,"output_tokens":1})),
                    Some("bad"),
                ),
                usage(2, Some(c(10, 50, 3, 9)), Some(c(10, 50, 3, 9)), None),
            ],
        );
        let mut store = TokenStore::default();
        let report = store.import_dir(dir.path(), &Pricing::default()).unwrap();
        assert_eq!(report.malformed_lines, 2);
        assert_eq!(store.events().len(), 1);
        assert_eq!(store.events()[0].cached, 10);
        assert_eq!(store.events()[0].reasoning, 3);
        assert_eq!(store.events()[0].uncached, 0);
        assert!(store.events()[0].is_partial());
    }
    #[test]
    fn unknown_model_is_visibly_assumed_sol_and_pricing_is_editable() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "unknown.jsonl",
            &[usage(
                1,
                Some(c(100, 40, 20, 8)),
                Some(c(100, 40, 20, 8)),
                None,
            )],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &Pricing::default()).unwrap();
        let event = &store.events()[0];
        assert_eq!(event.model, DEFAULT_ASSUMED_MODEL);
        assert!(event.model_assumed);
        assert_eq!(event.cost_usd, None);
        let mut pricing = sample_pricing();
        store.reprice(&pricing);
        assert!((store.events()[0].cost_usd.unwrap() - 0.00026).abs() < 1e-12);
        // Reasoning is already included in 20 output tokens, never billed again.
        assert_eq!(store.events()[0].total_tokens(), 120);
        pricing.model_override = Some("gpt-6-luna".into());
        store.reprice(&pricing);
        assert_eq!(store.events()[0].model, "gpt-6-luna");
        assert!(!store.events()[0].model_assumed);
        assert_eq!(store.events()[0].cost_usd, None);
        assert!(pricing
            .set_rate(
                "bad",
                ModelPrice {
                    uncached_input_per_million: f64::NAN,
                    cached_input_per_million: 0.0,
                    output_per_million: 0.0
                }
            )
            .is_err());
    }
    #[test]
    fn last_only_followed_by_cumulative_does_not_count_last_twice() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "mixed.jsonl",
            &[
                usage(1, None, Some(c(10, 2, 3, 1)), Some("first")),
                usage(
                    2,
                    Some(c(20, 4, 6, 2)),
                    Some(c(10, 2, 3, 1)),
                    Some("second"),
                ),
            ],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &Pricing::default()).unwrap();
        assert_eq!(store.events().iter().map(|e| e.input).sum::<u64>(), 20);
        assert!(store.events().iter().all(|event| !event.is_partial()));
    }
    #[test]
    fn old_history_is_retained_and_day_query_is_half_open_utc() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "old.jsonl",
            &[usage(1, Some(c(10, 2, 3, 1)), Some(c(10, 2, 3, 1)), None)],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &Pricing::default()).unwrap();
        let timestamp = store.events()[0].timestamp;
        assert_eq!(
            store
                .select_events(Some(timestamp), Some(timestamp + 1))
                .len(),
            1
        );
        assert!(store
            .select_events(Some(timestamp - 1), Some(timestamp))
            .is_empty());
        let now = timestamp + 90 * 86400;
        assert!(store
            .select_events(Some(TokenStore::default_window_start(now)), None)
            .is_empty());
        assert_eq!(store.events().len(), 1);
        let day = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let summary = store.day_summary(day, &Pricing::default());
        assert_eq!(summary.total_tokens, 13);
        assert_eq!(summary.unpriced_events, 1);
        assert_eq!(summary.cost_usd, None);
    }
    #[test]
    fn actual_quota_observations_persist_and_reimport_is_idempotent() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("quota.jsonl"),
            include_str!("../fixtures/token_mixed_models.jsonl"),
        )
        .unwrap();
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &Pricing::default()).unwrap();
        let file = dir.path().join("state.json");
        store.save(&file).unwrap();
        let mut store = TokenStore::load(file).unwrap();
        let report = store.import_dir(dir.path(), &Pricing::default()).unwrap();
        assert!(report.quota_observations.is_empty());
        assert_eq!(store.quota_observations().len(), 2);
        assert!(store.quota_observations()[0]
            .payload
            .get("primary")
            .is_some());
    }
    #[test]
    fn timestamps_are_required_instead_of_inventing_today() {
        let dir = TempDir::new().unwrap();
        write_log(&dir, "no-time.jsonl", &[json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":c(10,0,2,0)}}}).to_string()]);
        let mut store = TokenStore::default();
        let report = store.import_dir(dir.path(), &Pricing::default()).unwrap();
        assert_eq!(report.malformed_lines, 1);
        assert!(store.events().is_empty());
    }
    #[test]
    fn same_timestamp_keeps_cumulative_request_order_and_local_model_context() {
        let dir = TempDir::new().unwrap();
        write_log(&dir, "same-time.jsonl", &[
            json!({"timestamp":"2026-10-01T09:00:01Z","type":"turn_context","payload":{"model":"gpt-6.1-sol"}}).to_string(),
            usage(1, Some(c(2,0,1,0)), Some(c(2,0,1,0)), None),
            json!({"timestamp":"2026-10-01T09:00:01Z","type":"turn_context","payload":{"model":"gpt-6-luna"}}).to_string(),
            usage(1, Some(c(12,0,6,0)), Some(c(10,0,5,0)), None),
        ]);
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &Pricing::default()).unwrap();
        assert_eq!(store.events().iter().map(|e| e.input).sum::<u64>(), 12);
        let sol = store
            .events()
            .iter()
            .find(|e| e.model == "gpt-6.1-sol")
            .unwrap();
        assert_eq!(sol.input, 2);
        let luna = store
            .events()
            .iter()
            .find(|e| e.model == "gpt-6-luna")
            .unwrap();
        assert_eq!(luna.input, 10);
        assert!(store.events().iter().all(|e| !e.is_partial()));
    }
    #[test]
    fn demo_prices_and_counts_are_flagged_and_never_become_quota_observations() {
        let mut store = TokenStore::demo(1_800_000_000);
        assert_eq!(store.events().len(), 3);
        assert!(store.events().iter().all(|e| e.cost_usd.is_some()
            && e.partial_flags
                .contains(&"demo_synthetic_not_prices".into())));
        assert!(store.quota_observations().is_empty());
        assert_eq!(store.source_count(), 0);
        store.reprice(&Pricing::default());
        assert!(store.events().iter().all(|e| e.cost_usd.is_none()));
    }
    #[test]
    fn missing_counters_remain_marked_unavailable_instead_of_claiming_zero() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "optional.jsonl",
            &[
                usage(
                    1,
                    None,
                    Some(json!({"input_tokens":10,"output_tokens":2})),
                    Some("complete"),
                ),
                usage(
                    2,
                    None,
                    Some(json!({"input_tokens":10})),
                    Some("incomplete"),
                ),
            ],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &sample_pricing()).unwrap();
        assert_eq!(store.events()[0].cached, 0);
        assert_eq!(store.events()[0].reasoning, 0);
        assert!(store.events()[0]
            .partial_flags
            .contains(&"cached_count_unavailable".into()));
        assert!(store.events()[0]
            .partial_flags
            .contains(&"reasoning_count_unavailable".into()));
        assert_eq!(store.events()[0].cost_usd, None);
        assert!(store.events()[1]
            .partial_flags
            .contains(&"missing_token_fields".into()));
        assert_eq!(store.events()[1].cost_usd, None);
    }
    #[test]
    fn same_request_cumulative_updates_merge_positive_deltas_without_double_billing() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "updated-request.jsonl",
            &[
                usage(
                    1,
                    Some(c(10, 2, 3, 1)),
                    Some(c(10, 2, 3, 1)),
                    Some("streamed"),
                ),
                usage(
                    2,
                    Some(c(20, 4, 6, 2)),
                    Some(c(20, 4, 6, 2)),
                    Some("streamed"),
                ),
                usage(
                    3,
                    Some(c(20, 4, 6, 2)),
                    Some(c(20, 4, 6, 2)),
                    Some("streamed"),
                ),
            ],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &Pricing::default()).unwrap();
        assert_eq!(store.events().len(), 1);
        assert_eq!(store.events()[0].input, 20);
        assert_eq!(store.events()[0].output, 6);
        assert!(store.events()[0]
            .partial_flags
            .contains(&"request_usage_updated".into()));
    }
    fn response(
        second: u32,
        thread: &str,
        turn: &str,
        id: &str,
        usage: Value,
        turn_total: Value,
        thread_total: Option<Value>,
    ) -> String {
        json!({"timestamp":format!("2026-10-01T09:00:{second:02}Z"),"type":"token_usage_record","payload":{
            "thread_id":thread,"session_id":"root-session","turn_id":turn,"root_turn_id":"root-turn",
            "response_id":id,"usage":usage,"turn_token_usage":turn_total,"thread_token_usage":thread_total
        }}).to_string()
    }
    #[test]
    fn new_schema_authoritative_fixture_dedupes_responses_and_never_sums_turn_snapshots() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("responses.jsonl"),
            include_str!("../fixtures/new_schema_responses.jsonl"),
        )
        .unwrap();
        let mut store = TokenStore::default();
        let report = store.import_dir(dir.path(), &sample_pricing()).unwrap();
        assert_eq!(report.events_added, 3);
        assert_eq!(report.duplicate_observations, 1);
        assert_eq!(store.summary(None, None, &sample_pricing()).input, 350);
        assert_eq!(
            store
                .events()
                .iter()
                .filter(|e| e.session == "task-A")
                .map(|e| e.input)
                .sum::<u64>(),
            300
        );
        assert!(store
            .events()
            .iter()
            .all(|e| e.source_kind.as_deref() == Some("token_usage_record")));
        assert!(store
            .events()
            .iter()
            .all(|e| e.response_id.is_some() && e.turn_id.is_some()));
        assert_eq!(
            store
                .events()
                .iter()
                .find(|e| e.session == "task-A")
                .unwrap()
                .model,
            "gpt-6.1-sol"
        );
        assert!(store.events().iter().all(|e| e.session != "root-session"));
    }
    #[test]
    fn new_schema_replay_across_files_restarts_and_rewrites_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let row = response(
            1,
            "test-session",
            "t1",
            "r1",
            c(10, 2, 3, 1),
            c(10, 2, 3, 1),
            Some(c(10, 2, 3, 1)),
        );
        let path = write_log(&dir, "one.jsonl", std::slice::from_ref(&row));
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &sample_pricing()).unwrap();
        let original = store.events()[0].clone();
        let state = dir.path().join("state.json");
        store.save(&state).unwrap();
        let mut store = TokenStore::load(state).unwrap();
        write_log(&dir, "copy.jsonl", std::slice::from_ref(&row));
        fs::write(path, format!("{row}\n")).unwrap();
        let report = store.import_dir(dir.path(), &sample_pricing()).unwrap();
        assert_eq!(report.events_added, 0);
        assert_eq!(store.events().len(), 1);
        assert_eq!(store.events()[0].id, original.id);
        assert_eq!(store.events()[0].input, 10);
        assert_eq!(store.events()[0].cost_usd, original.cost_usd);
    }
    #[test]
    fn new_schema_same_response_latest_update_replaces_including_downward_corrections() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "updates.jsonl",
            &[
                response(
                    1,
                    "test-session",
                    "t1",
                    "r1",
                    c(10, 2, 3, 1),
                    c(10, 2, 3, 1),
                    Some(c(10, 2, 3, 1)),
                ),
                response(
                    2,
                    "test-session",
                    "t1",
                    "r1",
                    c(20, 4, 6, 2),
                    c(20, 4, 6, 2),
                    Some(c(20, 4, 6, 2)),
                ),
                response(
                    3,
                    "test-session",
                    "t1",
                    "r1",
                    c(15, 3, 5, 1),
                    c(15, 3, 5, 1),
                    Some(c(15, 3, 5, 1)),
                ),
            ],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &sample_pricing()).unwrap();
        assert_eq!(store.events().len(), 1);
        assert_eq!(store.events()[0].input, 15);
        assert_eq!(store.events()[0].output, 5);
        assert!(store.events()[0]
            .partial_flags
            .contains(&"request_usage_updated".into()));
        assert!(store.events()[0].cost_usd.is_some());
    }
    #[test]
    fn new_schema_same_response_different_threads_remains_two_events() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "threads.jsonl",
            &[
                response(
                    1,
                    "task-A",
                    "t1",
                    "same",
                    c(10, 2, 3, 1),
                    c(10, 2, 3, 1),
                    None,
                ),
                response(
                    1,
                    "task-B",
                    "t1",
                    "same",
                    c(10, 2, 3, 1),
                    c(10, 2, 3, 1),
                    None,
                ),
            ],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &sample_pricing()).unwrap();
        assert_eq!(store.events().len(), 2);
        assert_ne!(store.events()[0].id, store.events()[1].id);
    }
    #[test]
    fn new_schema_updates_are_independent_of_file_discovery_order_and_restart() {
        let older = response(
            1,
            "test-session",
            "t1",
            "r1",
            c(10, 2, 3, 1),
            c(10, 2, 3, 1),
            None,
        );
        let newer = response(
            2,
            "test-session",
            "t1",
            "r1",
            c(20, 4, 6, 2),
            c(20, 4, 6, 2),
            None,
        );
        for reverse in [false, true] {
            let dir = TempDir::new().unwrap();
            write_log(
                &dir,
                "a.jsonl",
                &[if reverse {
                    newer.clone()
                } else {
                    older.clone()
                }],
            );
            let mut store = TokenStore::default();
            store.import_dir(dir.path(), &sample_pricing()).unwrap();
            let state = dir.path().join("state.json");
            store.save(&state).unwrap();
            let mut store = TokenStore::load(state).unwrap();
            write_log(
                &dir,
                "b.jsonl",
                &[if reverse {
                    older.clone()
                } else {
                    newer.clone()
                }],
            );
            store.import_dir(dir.path(), &sample_pricing()).unwrap();
            assert_eq!(store.events().len(), 1);
            assert_eq!(store.events()[0].input, 20);
        }
    }
    #[test]
    fn new_schema_mixed_cumulative_overlap_is_removed_in_both_file_orders() {
        let old = vec![
            usage(1, Some(c(10, 2, 3, 1)), Some(c(10, 2, 3, 1)), None),
            usage(3, Some(c(30, 6, 9, 3)), Some(c(20, 4, 6, 2)), None),
        ];
        let new = vec![response(
            2,
            "test-session",
            "t1",
            "r2",
            c(20, 4, 6, 2),
            c(20, 4, 6, 2),
            Some(c(30, 6, 9, 3)),
        )];
        for reverse in [false, true] {
            let dir = TempDir::new().unwrap();
            write_log(&dir, "a.jsonl", if reverse { &new } else { &old });
            let mut store = TokenStore::default();
            store.import_dir(dir.path(), &sample_pricing()).unwrap();
            let state = dir.path().join("state.json");
            store.save(&state).unwrap();
            let mut store = TokenStore::load(state).unwrap();
            write_log(&dir, "b.jsonl", if reverse { &old } else { &new });
            store.import_dir(dir.path(), &sample_pricing()).unwrap();
            assert_eq!(store.summary(None, None, &sample_pricing()).input, 30);
            assert_eq!(store.events().len(), 2);
            assert_eq!(
                store
                    .events()
                    .iter()
                    .filter(|e| e.source_kind.as_deref() == Some("token_count"))
                    .count(),
                1
            );
        }
    }
    #[test]
    fn new_schema_mixed_legacy_before_response_and_after_response_both_dedupe() {
        for old_time in [1, 3] {
            let dir = TempDir::new().unwrap();
            write_log(
                &dir,
                "mixed.jsonl",
                &[
                    usage(old_time, Some(c(20, 4, 6, 2)), Some(c(20, 4, 6, 2)), None),
                    response(
                        2,
                        "test-session",
                        "t1",
                        "r1",
                        c(20, 4, 6, 2),
                        c(20, 4, 6, 2),
                        Some(c(20, 4, 6, 2)),
                    ),
                ],
            );
            let mut store = TokenStore::default();
            store.import_dir(dir.path(), &sample_pricing()).unwrap();
            assert_eq!(store.events().len(), 1);
            assert_eq!(store.events()[0].input, 20);
        }
    }
    #[test]
    fn new_schema_mixed_missing_thread_snapshot_is_explicitly_incomplete() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "mixed.jsonl",
            &[
                usage(1, Some(c(50, 10, 15, 5)), Some(c(20, 4, 6, 2)), None),
                response(
                    2,
                    "test-session",
                    "t1",
                    "r1",
                    c(20, 4, 6, 2),
                    c(20, 4, 6, 2),
                    None,
                ),
            ],
        );
        let mut store = TokenStore::default();
        let report = store.import_dir(dir.path(), &sample_pricing()).unwrap();
        assert_eq!(store.events().len(), 1);
        assert_eq!(store.events()[0].input, 20);
        assert!(store.events()[0]
            .partial_flags
            .contains(&"mixed_schema_legacy_overlap_suppressed".into()));
        assert!(report.warnings.iter().any(|w| w.contains("lower bound")));
        assert!(store.events()[0].cost_usd.is_some());
        assert_eq!(store.summary(None, None, &sample_pricing()).cost_usd, None);
    }
    #[test]
    fn new_schema_mixed_residual_is_counted_once_with_unknown_attribution_cost() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "mixed.jsonl",
            &[
                usage(3, Some(c(30, 6, 9, 3)), Some(c(20, 4, 6, 2)), None),
                response(
                    2,
                    "test-session",
                    "t1",
                    "r1",
                    c(20, 4, 6, 2),
                    c(20, 4, 6, 2),
                    Some(c(30, 6, 9, 3)),
                ),
            ],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &sample_pricing()).unwrap();
        assert_eq!(store.summary(None, None, &sample_pricing()).input, 30);
        assert!(store
            .events()
            .iter()
            .any(|e| e.input == 10 && e.cost_usd.is_none()));
        assert_eq!(store.summary(None, None, &sample_pricing()).cost_usd, None);
    }
    #[test]
    fn new_schema_turn_and_thread_gaps_are_unknown_without_summing_snapshots() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "gaps.jsonl",
            &[
                response(
                    1,
                    "test-session",
                    "t1",
                    "r1",
                    c(10, 2, 3, 1),
                    c(100, 20, 30, 10),
                    Some(c(100, 20, 30, 10)),
                ),
                response(
                    2,
                    "test-session",
                    "t1",
                    "r2",
                    c(20, 4, 6, 2),
                    c(120, 24, 36, 12),
                    Some(c(120, 24, 36, 12)),
                ),
            ],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &sample_pricing()).unwrap();
        assert_eq!(store.summary(None, None, &sample_pricing()).input, 30);
        assert!(store
            .events()
            .iter()
            .all(|e| e.partial_flags.contains(&"response_coverage_gap".into())));
        assert_eq!(store.summary(None, None, &sample_pricing()).cost_usd, None);
    }
    #[test]
    fn new_schema_root_session_does_not_merge_child_tasks_or_reassign_file_cursor() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "child.jsonl",
            &[
                response(
                    1,
                    "child-thread",
                    "t1",
                    "r1",
                    c(10, 2, 3, 1),
                    c(10, 2, 3, 1),
                    None,
                ),
                usage(2, Some(c(20, 4, 6, 2)), Some(c(20, 4, 6, 2)), None),
            ],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &sample_pricing()).unwrap();
        assert_eq!(store.events().len(), 2);
        assert!(store.events().iter().any(|e| e.session == "child-thread"));
        assert!(store.events().iter().any(|e| e.session == "test-session"));
        assert!(store.events().iter().all(|e| e.session != "root-session"));
    }
    #[test]
    fn new_schema_partial_line_waits_and_invalid_identity_is_not_invented() {
        let dir = TempDir::new().unwrap();
        let row = response(
            1,
            "test-session",
            "t1",
            "r1",
            c(10, 2, 3, 1),
            c(10, 2, 3, 1),
            None,
        );
        let path = dir.path().join("partial.jsonl");
        fs::write(&path, &row).unwrap();
        let mut store = TokenStore::default();
        assert_eq!(
            store
                .import_dir(dir.path(), &sample_pricing())
                .unwrap()
                .partial_lines,
            1
        );
        assert!(store.events().is_empty());
        let mut f = OpenOptions::new().append(true).open(path).unwrap();
        writeln!(f).unwrap();
        let mut invalid: Value = serde_json::from_str(&row).unwrap();
        invalid["payload"]
            .as_object_mut()
            .unwrap()
            .remove("response_id");
        writeln!(f, "{invalid}").unwrap();
        let report = store.import_dir(dir.path(), &sample_pricing()).unwrap();
        assert_eq!(report.malformed_lines, 1);
        assert_eq!(store.events().len(), 1);
    }
    #[test]
    fn new_schema_token_event_deserializes_previous_store_fields() {
        let event:TokenEvent=serde_json::from_value(json!({"id":"old","timestamp":1,"model":"gpt-6.1-sol","model_assumed":true,"session":"t","input":10,"cached":2,"uncached":8,"output":3,"reasoning":1,"cost_usd":null,"partial_flags":[]})).unwrap();
        assert_eq!(event.response_id, None);
        assert_eq!(event.turn_id, None);
        assert_eq!(event.source_kind, None);
    }
    #[test]
    fn new_schema_single_request_full_input_controls_long_context_price() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "long.jsonl",
            &[response(
                1,
                "test-session",
                "t1",
                "r1",
                c(272_001, 200_000, 100, 20),
                c(272_001, 200_000, 100, 20),
                Some(c(272_001, 200_000, 100, 20)),
            )],
        );
        let pricing = Pricing::official_standard();
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &pricing).unwrap();
        let e = &store.events()[0];
        assert_eq!(e.input, 272_001);
        assert_eq!(e.uncached, 72_001);
        let expected =
            (72_001.0 * 2.0 * 2.0 + 200_000.0 * 0.1 * 2.0 + 100.0 * 10.0 * 1.5) / 1_000_000.0;
        assert!((e.cost_usd.unwrap() - expected).abs() < 1e-12);
    }
    #[test]
    fn new_schema_v1_upgrade_replays_ignored_rows_without_losing_retained_history() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "upgrade.jsonl",
            &[
                usage(1, Some(c(10, 2, 3, 1)), Some(c(10, 2, 3, 1)), None),
                response(
                    2,
                    "test-session",
                    "t1",
                    "r1",
                    c(20, 4, 6, 2),
                    c(20, 4, 6, 2),
                    Some(c(30, 6, 9, 3)),
                ),
            ],
        );
        let mut old = TokenStore::default();
        old.import_dir(dir.path(), &sample_pricing()).unwrap();
        old.observations
            .retain(|r| r.source_kind.as_deref() != Some("token_usage_record"));
        old.reconcile(&sample_pricing());
        old.version = 1;
        let checkpoint = dir.path().join("state.json");
        old.save(&checkpoint).unwrap();
        let mut loaded = TokenStore::load(&checkpoint).unwrap();
        assert_eq!(loaded.version, STORE_VERSION);
        assert_eq!(loaded.sources.len(), 0);
        assert_eq!(loaded.observation_count(), 1);
        let report = loaded.import_dir(dir.path(), &sample_pricing()).unwrap();
        assert!(report.lines_read >= 3);
        assert_eq!(loaded.events().len(), 2);
        assert_eq!(loaded.summary(None, None, &sample_pricing()).input, 30);
        loaded.save(&checkpoint).unwrap();
        let mut again = TokenStore::load(&checkpoint).unwrap();
        assert_eq!(
            again
                .import_dir(dir.path(), &sample_pricing())
                .unwrap()
                .lines_read,
            0
        );
        assert_eq!(again.events().len(), 2);
    }
    #[test]
    fn new_schema_future_store_version_is_rejected_without_rewriting_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, "{\"version\":999}").unwrap();
        assert!(TokenStore::load(&path).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "{\"version\":999}");
    }
    #[test]
    fn new_schema_anonymous_legacy_cannot_be_added_as_a_fabricated_separate_task() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("no-meta.jsonl"),
            format!(
                "{}\n{}\n",
                usage(1, Some(c(20, 4, 6, 2)), Some(c(20, 4, 6, 2)), None),
                response(
                    2,
                    "real-task",
                    "t1",
                    "r1",
                    c(20, 4, 6, 2),
                    c(20, 4, 6, 2),
                    None
                )
            ),
        )
        .unwrap();
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &sample_pricing()).unwrap();
        assert_eq!(store.events().len(), 1);
        assert_eq!(store.events()[0].session, "real-task");
        assert_eq!(store.events()[0].input, 20);
        assert!(store.events()[0].cost_usd.is_some());
        assert_eq!(store.summary(None, None, &sample_pricing()).cost_usd, None);
    }
    #[test]
    fn new_schema_equal_timestamp_conflicts_are_deterministic_and_unpriced() {
        let one = response(
            1,
            "test-session",
            "t1",
            "r1",
            c(10, 2, 3, 1),
            c(10, 2, 3, 1),
            None,
        );
        let two = response(
            1,
            "test-session",
            "t1",
            "r1",
            c(20, 4, 6, 2),
            c(20, 4, 6, 2),
            None,
        );
        let mut counts = Vec::new();
        for reverse in [false, true] {
            let dir = TempDir::new().unwrap();
            let rows = if reverse {
                [two.clone(), one.clone()]
            } else {
                [one.clone(), two.clone()]
            };
            write_log(&dir, "tie.jsonl", &rows);
            let mut store = TokenStore::default();
            store.import_dir(dir.path(), &sample_pricing()).unwrap();
            assert_eq!(store.events().len(), 1);
            assert!(store.events()[0]
                .partial_flags
                .contains(&"response_record_conflict".into()));
            assert_eq!(store.events()[0].cost_usd, None);
            counts.push(store.events()[0].input);
        }
        assert_eq!(counts[0], counts[1]);
    }
    #[test]
    fn new_schema_explicit_old_only_turn_is_kept_without_new_thread_snapshot() {
        let dir = TempDir::new().unwrap();
        let mut old: Value =
            serde_json::from_str(&usage(1, Some(c(10, 2, 3, 1)), Some(c(10, 2, 3, 1)), None))
                .unwrap();
        old["payload"]["turn_id"] = json!("old-turn");
        write_log(
            &dir,
            "turns.jsonl",
            &[
                old.to_string(),
                response(
                    2,
                    "test-session",
                    "new-turn",
                    "r1",
                    c(20, 4, 6, 2),
                    c(20, 4, 6, 2),
                    None,
                ),
            ],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &sample_pricing()).unwrap();
        assert_eq!(store.events().len(), 2);
        assert_eq!(store.summary(None, None, &sample_pricing()).input, 30);
    }
    #[test]
    fn new_schema_zero_usage_update_replaces_previous_response_without_stale_counts() {
        let dir = TempDir::new().unwrap();
        write_log(
            &dir,
            "zero.jsonl",
            &[
                response(
                    1,
                    "test-session",
                    "t1",
                    "r1",
                    c(10, 2, 3, 1),
                    c(10, 2, 3, 1),
                    None,
                ),
                response(
                    2,
                    "test-session",
                    "t1",
                    "r1",
                    c(0, 0, 0, 0),
                    c(0, 0, 0, 0),
                    None,
                ),
            ],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &sample_pricing()).unwrap();
        assert_eq!(store.events().len(), 1);
        assert_eq!(store.events()[0].total_tokens(), 0);
    }
    #[test]
    fn new_schema_copied_child_without_turn_never_inherits_parent_turn() {
        let dir = TempDir::new().unwrap();
        let mut row: Value = serde_json::from_str(&response(
            2,
            "child-thread",
            "child-turn",
            "r1",
            c(10, 2, 3, 1),
            c(10, 2, 3, 1),
            None,
        ))
        .unwrap();
        row["payload"].as_object_mut().unwrap().remove("turn_id");
        write_log(&dir,"child.jsonl",&[
            json!({"timestamp":"2026-10-01T09:00:01Z","type":"turn_context","payload":{"turn_id":"parent-turn","model":"gpt-6-luna"}}).to_string(),row.to_string()]);
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &sample_pricing()).unwrap();
        assert_eq!(store.events()[0].turn_id, None);
        assert!(store.events()[0].model_assumed);
        assert!(store.events()[0]
            .partial_flags
            .contains(&"turn_identity_missing".into()));
    }
    #[test]
    fn new_schema_positive_cache_write_usage_keeps_counts_but_cost_is_unknown() {
        let dir = TempDir::new().unwrap();
        let mut counts = c(100, 20, 10, 5);
        counts["cache_write_input_tokens"] = json!(30);
        write_log(
            &dir,
            "writes.jsonl",
            &[response(
                1,
                "test-session",
                "t1",
                "r1",
                counts.clone(),
                counts.clone(),
                Some(counts),
            )],
        );
        let mut store = TokenStore::default();
        store.import_dir(dir.path(), &sample_pricing()).unwrap();
        assert_eq!(store.events()[0].input, 100);
        assert_eq!(store.events()[0].cached, 20);
        assert_eq!(store.events()[0].cost_usd, None);
        assert!(store.events()[0]
            .partial_flags
            .contains(&"cache_write_pricing_unmodeled".into()));
    }
    #[test]
    fn new_export_publication_never_clobbers_a_racing_destination() {
        let dir = TempDir::new().unwrap();
        let destination = dir.path().join("export.json");
        let prior_temporary = dir.path().join("tokens.tmp-older-preserve");
        fs::write(&prior_temporary, b"unassessed prior temporary").unwrap();
        let result = TokenStore::default().save_new_before_publish(&destination, || {
            fs::write(&destination, b"another writer won the race").unwrap();
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"another writer won the race"
        );
        assert_eq!(
            fs::read(&prior_temporary).unwrap(),
            b"unassessed prior temporary"
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
        let new = dir.path().join("new-export.json");
        let store = TokenStore::demo(1000);
        store.save_new(&new).unwrap();
        assert_eq!(TokenStore::load(new).unwrap().events(), store.events());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
    }

    #[test]
    fn prepared_policy_looks_up_anonymous_bounds_once_not_per_event() {
        let mut header = TokenStore::default().header();
        header.response_threads.insert("response-thread".into());
        for n in 0..10_000 {
            let id = format!("anonymous-{n:05}");
            header.anonymous_threads.insert(id.clone());
            header.anonymous_bounds_ms.insert(id, n);
        }
        POLICY_BOUND_LOOKUPS.with(|count| count.set(0));
        let policy = header.event_policy();
        assert_eq!(POLICY_BOUND_LOOKUPS.with(std::cell::Cell::get), 10_000);
        let mut event = TokenStore::demo(1000).events()[0].clone();
        event.coverage_start_timestamp_ms = Some(10_001);
        for _ in 0..10_000 {
            assert!(policy.apply(&mut event));
        }
        assert_eq!(POLICY_BOUND_LOOKUPS.with(std::cell::Cell::get), 10_000);
        assert!(!event
            .partial_flags
            .iter()
            .any(|f| f == "mixed_schema_legacy_overlap_suppressed"));
        header.anonymous_bounds_ms.remove("anonymous-09999");
        let missing = header.event_policy();
        assert!(missing.apply(&mut event));
        assert!(event
            .partial_flags
            .iter()
            .any(|f| f == "mixed_schema_legacy_overlap_suppressed"));
    }
}
