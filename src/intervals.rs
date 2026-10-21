//! Rebuildable, model-wise accounting for completed, observed quota decrements.
//!
//! Callers supply the normalized presentation history, never cumulative token
//! snapshots. Each result is linked to that history by index and a stable key.
//! Prices are the normal per-event `cost_usd` values; this module never chooses a
//! parent model, invents a price, or charges reasoning a second time. Token events
//! have no account/source identity, so the caller must select the appropriate
//! local token dataset. Quota boundaries themselves never cross account, source,
//! window, observer session, or epoch. Different quota windows are alternative
//! views of the same usage, and their dollar subtotals must not be added together.
use crate::{
    history::is_confirmed_quota_change,
    model::{HistoryEvent, HistoryKind},
    tokens::TokenEvent,
    usage_views::coverage_incomplete,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
    io,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntervalReason {
    StartupIncomplete,
    MissingStartBoundary,
    AmbiguousBoundary,
    SessionChanged,
    EpochChanged,
    UncertainQuotaObservation,
    QuotaObservationGap,
    QuotaBoundaryMismatch,
    TokenCoverageStartsLate,
    NoTokenEvents,
    UnknownModelPrice,
    AssumedModel,
    PartialTokenRecords,
    TokenCoverageGap,
    InvalidBoundaryTime,
    ResetBoundary,
    AmbiguousTokenAccount,
}
impl IntervalReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::StartupIncomplete => "首次觀察，未涵蓋完整扣額區間",
            Self::MissingStartBoundary => "缺少已記錄的起點",
            Self::AmbiguousBoundary => "同秒額度邊界無法區分",
            Self::SessionChanged => "額度觀察 session 已變更",
            Self::EpochChanged => "額度週期已變更",
            Self::UncertainQuotaObservation => "區間含不確定額度觀察",
            Self::QuotaObservationGap => "額度觀察覆蓋不完整",
            Self::QuotaBoundaryMismatch => "前後額度邊界不連續",
            Self::TokenCoverageStartsLate => "Token 記錄晚於區間起點",
            Self::NoTokenEvents => "區間沒有可歸屬 Token 記錄",
            Self::UnknownModelPrice => "部分模型費用未知",
            Self::AssumedModel => "部分模型使用假設值",
            Self::PartialTokenRecords => "部分 Token 記錄不完整",
            Self::TokenCoverageGap => "Token 記錄有覆蓋缺口",
            Self::InvalidBoundaryTime => "起迄時間無法形成區間",
            Self::ResetBoundary => "回補後首個區間，不與一般扣額區間比較",
            Self::AmbiguousTokenAccount => "多個帳戶的 Token 歸屬無法確認",
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StartBoundaryKind {
    QuotaDecrease,
    QuotaReset,
    /// A real recorded observation, but not a previous completed decrement.
    ObservedBaseline,
    Unavailable,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelIntervalUsage {
    pub model: String,
    pub input: u64,
    pub cached: u64,
    pub uncached: u64,
    pub output: u64,
    /// Subset of output, displayed for inspection and never added to cost.
    pub reasoning: u64,
    pub known_usd: f64,
    pub unknown_cost_events: usize,
    pub events: usize,
    pub assumed_model_events: usize,
    pub model_assumed: bool,
    pub partial_events: usize,
    pub coverage_gap_events: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompletedInterval {
    /// Index in the normalized history slice passed to this function.
    pub history_index: usize,
    pub history_event_key: String,
    pub previous_history_event_key: Option<String>,
    pub session: String,
    pub account_id: Option<String>,
    pub source_key: String,
    pub window_key: Option<String>,
    pub window_label: String,
    pub epoch: u64,
    /// Usage selection is (start, end]. None means no defensible recorded start.
    pub start: Option<i64>,
    pub end: i64,
    pub start_boundary_kind: StartBoundaryKind,
    pub before: f64,
    pub after: f64,
    /// Actual observed drop. A 2pp drop stays one interval, never two invented ones.
    pub percentage_points: f64,
    /// Sum of the model subtotals, even when some costs or coverage are unknown.
    pub known_usd: f64,
    pub unknown_cost_events: usize,
    pub events: usize,
    pub assumed_model_events: usize,
    pub partial: bool,
    pub comparable: bool,
    /// Available only for complete comparable intervals; multi-pp is an average.
    pub cost_per_point: Option<f64>,
    pub reasons: Vec<IntervalReason>,
    pub models: Vec<ModelIntervalUsage>,
}
/// Deterministic identity for a normalized quota history event, independent of
/// vector ordering. Meaningful normalization changes deliberately change the key.
pub fn history_event_key(event: &HistoryEvent) -> String {
    serde_json::to_string(&(
        &event.session,
        event.observed_at_unix,
        event.evidence.as_ref().and_then(|e| e.observed_at_ms),
        account(event),
        &event.source_key,
        &event.window_key,
        &event.window_label,
        event.epoch,
        event.kind,
        &event.before,
        &event.after,
    ))
    .expect("history identity consists of serializable strings and integers")
}
fn account(event: &HistoryEvent) -> Option<&str> {
    event
        .evidence
        .as_ref()
        .and_then(|e| e.account_id.as_deref())
}

#[derive(Debug, Clone, Copy, Default)]
struct Totals {
    input: u128,
    cached: u128,
    uncached: u128,
    output: u128,
    reasoning: u128,
    known_usd: f64,
    unknown: usize,
    events: usize,
    assumed: usize,
    partial: usize,
    coverage: usize,
}
impl Totals {
    fn add(&mut self, e: &TokenEvent) {
        self.input += u128::from(e.input);
        self.cached += u128::from(e.cached);
        self.uncached += u128::from(e.uncached);
        self.output += u128::from(e.output);
        self.reasoning += u128::from(e.reasoning);
        if let Some(cost) = e.cost_usd.filter(|c| c.is_finite() && *c >= 0.) {
            self.known_usd += cost;
        } else {
            self.unknown += 1;
        }
        self.events += 1;
        self.assumed += usize::from(e.model_assumed);
        self.partial += usize::from(e.is_partial());
        self.coverage += usize::from(coverage_incomplete(e));
    }
    fn difference(self, before: Self) -> Self {
        Self {
            input: self.input - before.input,
            cached: self.cached - before.cached,
            uncached: self.uncached - before.uncached,
            output: self.output - before.output,
            reasoning: self.reasoning - before.reasoning,
            known_usd: (self.known_usd - before.known_usd).max(0.),
            unknown: self.unknown - before.unknown,
            events: self.events - before.events,
            assumed: self.assumed - before.assumed,
            partial: self.partial - before.partial,
            coverage: self.coverage - before.coverage,
        }
    }
    fn model_usage(self, model: String) -> ModelIntervalUsage {
        let capped = |n: u128| n.min(u128::from(u64::MAX)) as u64;
        ModelIntervalUsage {
            model,
            input: capped(self.input),
            cached: capped(self.cached),
            uncached: capped(self.uncached),
            output: capped(self.output),
            reasoning: capped(self.reasoning),
            known_usd: self.known_usd,
            unknown_cost_events: self.unknown,
            events: self.events,
            assumed_model_events: self.assumed,
            model_assumed: self.assumed != 0,
            partial_events: self.partial,
            coverage_gap_events: self.coverage,
        }
    }
}
/// Aggregate an already selected range of token events into sorted model rows.
/// Recorded automatic-review events are excluded, just as in `IntervalIndex`.
/// This consumes an iterator without retaining its events: additional memory is
/// proportional to the number of distinct models. Supply events in a stable
/// order (normally timestamp, then ID) for reproducible floating-point subtotals.
pub fn model_usage<'a>(
    events: impl IntoIterator<Item = &'a TokenEvent>,
) -> Vec<ModelIntervalUsage> {
    let mut models: BTreeMap<&str, Totals> = BTreeMap::new();
    for event in events {
        if !event.is_excluded_auto_review() {
            models.entry(&event.model).or_default().add(event);
        }
    }
    models
        .into_iter()
        .map(|(model, totals)| totals.model_usage(model.to_owned()))
        .collect()
}
#[derive(Debug, Clone)]
struct ModelSeries {
    model: String,
    positions: Vec<usize>,
    prefixes: Vec<Totals>,
}
impl ModelSeries {
    fn range(&self, lo: usize, hi: usize) -> ModelIntervalUsage {
        let a = self.positions.partition_point(|p| *p < lo);
        let b = self.positions.partition_point(|p| *p < hi);
        let t = self.prefixes[b].difference(self.prefixes[a]);
        t.model_usage(self.model.clone())
    }
}
/// Immutable owned index, suitable for caching on a background token snapshot.
/// Build: O(E log E), memory: O(E). For H history rows and R emitted model rows,
/// accounting is O(H log H + (H + R) log E), rather than H full scans of E events.
/// Distinct-model range reporting visits only the first occurrence of each model
/// in a queried interval, even if the dataset has many unrelated model names.
#[derive(Debug, Clone, Default)]
pub struct IntervalIndex {
    times: Vec<i64>,
    model_at: Vec<usize>,
    models: Vec<ModelSeries>,
    /// Minimum previous-same-model position + 1; zero means no predecessor.
    previous_min: Vec<usize>,
    leaf_count: usize,
}
impl IntervalIndex {
    pub fn new(events: &[TokenEvent]) -> Self {
        let mut rows: Vec<_> = events
            .iter()
            .filter(|e| !e.is_excluded_auto_review())
            .collect();
        rows.sort_by(|a, b| (a.timestamp, &a.id).cmp(&(b.timestamp, &b.id)));
        let leaf_count = rows.len().max(1).next_power_of_two();
        let mut index = Self {
            times: Vec::with_capacity(rows.len()),
            model_at: Vec::with_capacity(rows.len()),
            previous_min: vec![usize::MAX; leaf_count * 2],
            leaf_count,
            ..Self::default()
        };
        let mut identities: BTreeMap<&str, usize> = BTreeMap::new();
        for (position, event) in rows.into_iter().enumerate() {
            let next = index.models.len();
            let model = *identities.entry(&event.model).or_insert_with(|| {
                index.models.push(ModelSeries {
                    model: event.model.clone(),
                    positions: Vec::new(),
                    prefixes: vec![Totals::default()],
                });
                next
            });
            let series = &mut index.models[model];
            index.previous_min[leaf_count + position] =
                series.positions.last().map_or(0, |p| p + 1);
            series.positions.push(position);
            let mut totals = *series.prefixes.last().unwrap();
            totals.add(event);
            series.prefixes.push(totals);
            index.times.push(event.timestamp);
            index.model_at.push(model);
        }
        for node in (1..leaf_count).rev() {
            index.previous_min[node] =
                index.previous_min[node * 2].min(index.previous_min[node * 2 + 1]);
        }
        index
    }
    fn distinct_models(
        &self,
        node: usize,
        from: usize,
        to: usize,
        lo: usize,
        hi: usize,
        out: &mut Vec<usize>,
    ) {
        if from >= hi || to <= lo || self.previous_min[node] > lo {
            return;
        }
        if to - from == 1 {
            out.push(self.model_at[from]);
            return;
        }
        let middle = from + (to - from) / 2;
        self.distinct_models(node * 2, from, middle, lo, hi, out);
        self.distinct_models(node * 2 + 1, middle, to, lo, hi, out);
    }
    fn usage(&self, start: i64, end: i64) -> Vec<ModelIntervalUsage> {
        if self.times.is_empty() || end <= start {
            return Vec::new();
        }
        let lo = self.times.partition_point(|t| *t <= start);
        let hi = self.times.partition_point(|t| *t <= end);
        let mut identities = Vec::new();
        self.distinct_models(1, 0, self.leaf_count, lo, hi, &mut identities);
        let mut rows: Vec<_> = identities
            .into_iter()
            .map(|m| self.models[m].range(lo, hi))
            .collect();
        rows.sort_by(|a, b| a.model.cmp(&b.model));
        rows
    }
    /// `history` must be the already reconciled `history::normalized` view.
    /// Unconfirmed observations never establish boundaries, and partial flags
    /// from observations/usage remain visible as non-comparability reasons.
    pub fn completed_intervals(&self, history: &[HistoryEvent]) -> Vec<CompletedInterval> {
        match completed_with_usage(history, self.times.first().copied(), &mut |start, end| {
            Ok::<_, Infallible>(self.usage(start, end))
        }) {
            Ok(intervals) => intervals,
            Err(never) => match never {},
        }
    }
}
/// Convenience API. Reuse `IntervalIndex` when only history changes.
/// Pass normalized history to match the UI's indices and reconciled changes.
pub fn completed_intervals(
    events: &[TokenEvent],
    history: &[HistoryEvent],
) -> Vec<CompletedInterval> {
    IntervalIndex::new(events).completed_intervals(history)
}

/// Build completed intervals from normalized history using bounded token queries.
///
/// `earliest_token_timestamp` is the earliest non-excluded event in the complete
/// token dataset, not just the queried ranges. `usage` must return sorted model
/// rows for events in `(start, end]`, with automatic-review exclusions and totals
/// equivalent to `model_usage`. It is called only for defensible, nonempty time
/// ranges. No token query is needed for an unavailable or invalid boundary.
///
/// Callers may cache queries by `(start, end)` and invalidate them when token
/// changes intersect that range. History/account/session/epoch/partial semantics
/// use the same core as `IntervalIndex::completed_intervals`. Any query error is
/// returned immediately; a failed query is never reported as an empty interval.
pub fn completed_intervals_with_provider(
    history: &[HistoryEvent],
    earliest_token_timestamp: Option<i64>,
    mut usage: impl FnMut(i64, i64) -> io::Result<Vec<ModelIntervalUsage>>,
) -> io::Result<Vec<CompletedInterval>> {
    completed_with_usage(history, earliest_token_timestamp, &mut usage)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum WindowIdentity<'a> {
    Key(&'a str),
    LegacyLabel(&'a str),
}
type BaseScope<'a> = (Option<&'a str>, &'a str, WindowIdentity<'a>, &'a str);
type Scope<'a> = (BaseScope<'a>, &'a str, u64);
type Alias<'a> = (Option<&'a str>, &'a str, &'a str);
fn base_scope<'a>(
    e: &'a HistoryEvent,
    aliases: &BTreeMap<Alias<'a>, BTreeSet<&'a str>>,
) -> BaseScope<'a> {
    let key = if let Some(key) = e.window_key.as_deref() {
        WindowIdentity::Key(key)
    } else {
        match aliases
            .get(&(account(e), &e.source_key, &e.window_label))
            .filter(|keys| keys.len() == 1)
            .and_then(|keys| keys.first())
        {
            Some(key) => WindowIdentity::Key(key),
            None => WindowIdentity::LegacyLabel(&e.window_label),
        }
    };
    (
        account(e),
        e.source_key.as_str(),
        key,
        e.window_label.as_str(),
    )
}
fn completed_with_usage<E>(
    history: &[HistoryEvent],
    earliest_token_timestamp: Option<i64>,
    usage: &mut impl FnMut(i64, i64) -> Result<Vec<ModelIntervalUsage>, E>,
) -> Result<Vec<CompletedInterval>, E> {
    let mut aliases: BTreeMap<Alias<'_>, BTreeSet<&str>> = BTreeMap::new();
    for e in history.iter().filter(|e| is_confirmed_quota_change(e)) {
        if let Some(key) = e.window_key.as_deref() {
            aliases
                .entry((account(e), &e.source_key, &e.window_label))
                .or_default()
                .insert(key);
        }
    }
    let mut streams: BTreeMap<Scope<'_>, Vec<usize>> = BTreeMap::new();
    for (position, e) in history.iter().enumerate() {
        if matches!(e.kind, HistoryKind::QuotaDecrease | HistoryKind::QuotaReset)
            && (is_confirmed_quota_change(e) || e.reconciliation.is_some())
        {
            streams
                .entry((base_scope(e, &aliases), &e.session, e.epoch))
                .or_default()
                .push(position);
        }
    }
    let mut result = Vec::new();
    for mut rows in streams.into_values() {
        rows.sort_by_key(|i| history[*i].observed_at_unix);
        let mut previous: Option<usize> = None;
        let mut uncertain = false;
        let mut ambiguous_predecessor = false;
        let mut at = 0;
        while at < rows.len() {
            let mut end = at + 1;
            while end < rows.len()
                && history[rows[end]].observed_at_unix == history[rows[at]].observed_at_unix
            {
                end += 1;
            }
            let confirmed: Vec<_> = rows[at..end]
                .iter()
                .copied()
                .filter(|i| is_confirmed_quota_change(&history[*i]))
                .collect();
            if confirmed.is_empty() {
                uncertain = true;
            } else if confirmed.len() > 1 {
                for current in confirmed {
                    if history[current].kind == HistoryKind::QuotaDecrease {
                        result.push(build_interval(
                            earliest_token_timestamp,
                            usage,
                            history,
                            BoundaryContext {
                                current,
                                previous: None,
                                ambiguous: true,
                                uncertain,
                                ambiguous_predecessor: false,
                            },
                        )?);
                    }
                }
                previous = None;
                ambiguous_predecessor = true;
                uncertain = true;
            } else {
                let current = confirmed[0];
                uncertain |= end - at > 1;
                if history[current].kind == HistoryKind::QuotaDecrease {
                    result.push(build_interval(
                        earliest_token_timestamp,
                        usage,
                        history,
                        BoundaryContext {
                            current,
                            previous,
                            ambiguous: false,
                            uncertain,
                            ambiguous_predecessor,
                        },
                    )?);
                }
                previous = Some(current);
                uncertain = false;
                ambiguous_predecessor = false;
            }
            at = end;
        }
    }
    // Explain a first boundary in a new observer/cycle without using another
    // scope's timestamp as its start. The lookup is logarithmic, never per-row scans.
    let mut prior_by_base: BTreeMap<BaseScope<'_>, Vec<usize>> = BTreeMap::new();
    for (i, e) in history
        .iter()
        .enumerate()
        .filter(|(_, e)| is_confirmed_quota_change(e))
    {
        prior_by_base
            .entry(base_scope(e, &aliases))
            .or_default()
            .push(i);
    }
    for rows in prior_by_base.values_mut() {
        rows.sort_by_key(|i| history[*i].observed_at_unix);
    }
    let account_count = history
        .iter()
        .filter(|e| is_confirmed_quota_change(e))
        .filter_map(account)
        .collect::<BTreeSet<_>>()
        .len();
    for interval in &mut result {
        if account_count > 1 {
            add_reason(&mut interval.reasons, IntervalReason::AmbiguousTokenAccount);
            interval.partial = true;
            interval.comparable = false;
            interval.cost_per_point = None;
        }
        if interval.previous_history_event_key.is_some() {
            continue;
        }
        let e = &history[interval.history_index];
        if let Some(rows) = prior_by_base.get(&base_scope(e, &aliases)) {
            let before =
                rows.partition_point(|i| history[*i].observed_at_unix < e.observed_at_unix);
            if let Some(prior) = before.checked_sub(1).map(|i| &history[rows[i]]) {
                if prior.session != e.session {
                    add_reason(&mut interval.reasons, IntervalReason::SessionChanged);
                }
                if prior.epoch != e.epoch {
                    add_reason(&mut interval.reasons, IntervalReason::EpochChanged);
                }
            }
        }
    }
    result.sort_by_key(|i| i.history_index);
    Ok(result)
}
fn add_reason(reasons: &mut Vec<IntervalReason>, reason: IntervalReason) {
    if !reasons.contains(&reason) {
        reasons.push(reason);
    }
}
/// Boundary identity and reconciliation flags from one normalized history stream.
struct BoundaryContext {
    current: usize,
    previous: Option<usize>,
    ambiguous: bool,
    uncertain: bool,
    ambiguous_predecessor: bool,
}
fn build_interval<E>(
    earliest_token_timestamp: Option<i64>,
    usage: &mut impl FnMut(i64, i64) -> Result<Vec<ModelIntervalUsage>, E>,
    history: &[HistoryEvent],
    boundary: BoundaryContext,
) -> Result<CompletedInterval, E> {
    let BoundaryContext {
        current,
        previous,
        ambiguous,
        uncertain,
        ambiguous_predecessor,
    } = boundary;
    let e = &history[current];
    let before = e.before.parse::<f64>().unwrap();
    let after = e.after.parse::<f64>().unwrap();
    let mut reasons = Vec::new();
    let (start, kind) = if ambiguous {
        add_reason(&mut reasons, IntervalReason::AmbiguousBoundary);
        (None, StartBoundaryKind::Unavailable)
    } else if let Some(previous) = previous {
        let p = &history[previous];
        if p.kind == HistoryKind::QuotaReset {
            add_reason(&mut reasons, IntervalReason::ResetBoundary);
        }
        if p.after
            .parse::<f64>()
            .is_ok_and(|v| (v - before).abs() > 0.000001)
        {
            add_reason(&mut reasons, IntervalReason::QuotaBoundaryMismatch);
        }
        (
            Some(p.observed_at_unix),
            if p.kind == HistoryKind::QuotaReset {
                StartBoundaryKind::QuotaReset
            } else {
                StartBoundaryKind::QuotaDecrease
            },
        )
    } else {
        add_reason(&mut reasons, IntervalReason::StartupIncomplete);
        if ambiguous_predecessor {
            add_reason(&mut reasons, IntervalReason::AmbiguousBoundary);
        }
        (
            e.before_observed_at_unix,
            if e.before_observed_at_unix.is_some() {
                StartBoundaryKind::ObservedBaseline
            } else {
                StartBoundaryKind::Unavailable
            },
        )
    };
    if start.is_none() {
        add_reason(&mut reasons, IntervalReason::MissingStartBoundary);
    }
    if uncertain {
        add_reason(&mut reasons, IntervalReason::UncertainQuotaObservation);
    }
    if e.partial {
        add_reason(&mut reasons, IntervalReason::QuotaObservationGap);
    }
    let models = if let Some(start) = start {
        if start >= e.observed_at_unix {
            add_reason(&mut reasons, IntervalReason::InvalidBoundaryTime);
        }
        if earliest_token_timestamp.is_none_or(|first| first > start) {
            add_reason(&mut reasons, IntervalReason::TokenCoverageStartsLate);
        }
        if start < e.observed_at_unix {
            usage(start, e.observed_at_unix)?
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    let known_usd: f64 = models.iter().map(|m| m.known_usd).sum();
    // f64 Sum uses negative zero for an empty iterator; a known zero subtotal
    // must not render as a negative charge.
    let known_usd = if known_usd == 0.0 { 0.0 } else { known_usd };
    let unknown_cost_events = models.iter().map(|m| m.unknown_cost_events).sum::<usize>();
    let events = models.iter().map(|m| m.events).sum::<usize>();
    let assumed_model_events = models.iter().map(|m| m.assumed_model_events).sum::<usize>();
    if events == 0 {
        add_reason(&mut reasons, IntervalReason::NoTokenEvents);
    }
    if unknown_cost_events > 0 {
        add_reason(&mut reasons, IntervalReason::UnknownModelPrice);
    }
    if assumed_model_events > 0 {
        add_reason(&mut reasons, IntervalReason::AssumedModel);
    }
    if models.iter().any(|m| m.partial_events > 0) {
        add_reason(&mut reasons, IntervalReason::PartialTokenRecords);
    }
    if models.iter().any(|m| m.coverage_gap_events > 0) {
        add_reason(&mut reasons, IntervalReason::TokenCoverageGap);
    }
    let comparable = reasons.is_empty();
    let percentage_points = before - after;
    Ok(CompletedInterval {
        history_index: current,
        history_event_key: history_event_key(e),
        previous_history_event_key: previous.map(|p| history_event_key(&history[p])),
        session: e.session.clone(),
        account_id: account(e).map(str::to_owned),
        source_key: e.source_key.clone(),
        window_key: e.window_key.clone(),
        window_label: e.window_label.clone(),
        epoch: e.epoch,
        start,
        end: e.observed_at_unix,
        start_boundary_kind: kind,
        before,
        after,
        percentage_points,
        known_usd,
        unknown_cost_events,
        events,
        assumed_model_events,
        partial: !comparable,
        comparable,
        cost_per_point: comparable.then_some(known_usd / percentage_points),
        reasons,
        models,
    })
}
