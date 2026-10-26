//! Account-rate-limit normalization and continuous-observation state.
//!
//! Metadata changes are not replenishments. Only an observed, nonzero increase
//! in remaining quota emits `QuotaReset`, except the user's exact one-point
//! same-cycle rebound policy; source/window boundaries only reset
//! the estimator. Persisted history is never used to seed live rate estimates.
use crate::prediction::{self, DropSample, Prediction, PredictionContext};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

pub const CONTINUITY_GAP_SECONDS: i64 = 90;
pub const MAX_DROP_SAMPLES: usize = 120;
const PERCENT_EPSILON: f64 = 0.000001;

// Reject an old pre-reset endpoint until the new cycle reaches its neighbourhood.
pub(crate) fn stale_pre_reset_echo(current: f64, incoming: f64, before_reset: f64) -> bool {
    current > before_reset + 2.0 + PERCENT_EPSILON
        && (incoming - before_reset).abs() <= PERCENT_EPSILON
}

pub(crate) fn quota_deadline_rollover(before: Option<i64>, after: Option<i64>, at: i64) -> bool {
    before.zip(after).is_some_and(|(before, after)| {
        before <= at && after > at && after.saturating_sub(before) > 90
    })
}

/// User policy: a same-cycle exact +1pp cached rebound is canonicalized to the
/// last accepted value. Fractional/larger rises and proven new cycles are kept.
pub(crate) fn ignored_one_point_rebound(
    before: f64,
    after: f64,
    before_reset: Option<i64>,
    reset: Option<i64>,
    at: i64,
) -> bool {
    (after - before - 1.0).abs() <= PERCENT_EPSILON
        && !quota_deadline_rollover(before_reset, reset, at)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum HistoryKind {
    QuotaDecrease,
    CreditDecrease,
    QuotaReset,
    CreditTopUp,
}
impl HistoryKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::QuotaDecrease => "額度下降",
            Self::CreditDecrease => "點數下降",
            Self::QuotaReset => "額度回補",
            Self::CreditTopUp => "點數增加",
        }
    }
}

/// Provenance is evidence about an observation, never a server-issued sequence.
/// JSONL timestamps identify when a cached value was logged, not when fetched.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HistoryEvidence {
    #[serde(default)]
    pub imported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_origin_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_observed_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_reset_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_duration_mins: Option<i64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HistoryReconciliation {
    /// Cached log snapshots disagree. This is not a confirmed replenishment or
    /// a usable accounting boundary; original evidence remains in raw history.
    UncertainRecovery,
    /// Independent observations in one instant have no defensible ordering.
    ConflictingObservation,
}

/// Preserve legacy integer IDs exactly, without coercing floats, nulls or booleans.
/// This compatibility rule belongs to history run IDs, not Codex request/model IDs.
fn deserialize_history_session<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    struct HistorySession;
    impl serde::de::Visitor<'_> for HistorySession {
        type Value = String;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a string or nonnegative integer history session ID")
        }
        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<String, E> {
            Ok(value.to_owned())
        }
        fn visit_string<E: serde::de::Error>(self, value: String) -> Result<String, E> {
            Ok(value)
        }
        fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<String, E> {
            Ok(value.to_string())
        }
        fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<String, E> {
            if value < 0 {
                return Err(E::invalid_value(
                    serde::de::Unexpected::Signed(value),
                    &self,
                ));
            }
            Ok(value.to_string())
        }
    }
    deserializer.deserialize_any(HistorySession)
}

/// Compatible with the recovered version-1 history event shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryEvent {
    // 0.2.x persisted a numeric run/session counter; rebuilt versions use text.
    #[serde(deserialize_with = "deserialize_history_session")]
    pub session: String,
    pub observed_at_unix: i64,
    pub source_key: String,
    pub epoch: u64,
    pub kind: HistoryKind,
    pub window_label: String,
    pub before: String,
    pub after: String,
    pub delta: String,
    /// The comparison spans missing observation coverage; cost attribution and
    /// exact change time are unknown even though the endpoint delta is real.
    #[serde(default)]
    pub partial: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_observed_at_unix: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<HistoryEvidence>,
    /// Set on a derived presentation only, never required to load old history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconciliation: Option<HistoryReconciliation>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WindowStatus {
    Fresh,
    Stale,
    Unavailable,
    ResetDue,
}
impl WindowStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Fresh => "已更新",
            Self::Stale => "資料過期",
            Self::Unavailable => "不可用",
            Self::ResetDue => "等待重設資料",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowView {
    pub source_key: String,
    pub source_label: String,
    pub window_key: String,
    pub window_label: String,
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub window_duration_mins: Option<i64>,
    pub reset_at: Option<i64>,
    pub observed_at: i64,
    pub age_seconds: Option<i64>,
    pub epoch: u64,
    pub status: WindowStatus,
    pub rate_per_hour: Option<f64>,
    pub prediction: Prediction,
    pub plan_type: Option<String>,
    pub ordinary_usage_allowed: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreditView {
    pub source_key: String,
    pub source_label: String,
    pub balance: Option<String>,
    pub has_credits: Option<bool>,
    pub unlimited: bool,
    pub observed_at: i64,
    pub epoch: u64,
    pub status: WindowStatus,
    pub rate_per_minute: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IngestReport {
    pub accepted: bool,
    pub ignored_duplicate: bool,
    pub windows_updated: usize,
    pub events_added: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct WindowState {
    source_key: String,
    source_label: String,
    window_key: String,
    window_label: String,
    used_percent: Option<f64>,
    pre_reset_used: Option<f64>,
    duration: Option<i64>,
    reset_at: Option<i64>,
    // Retain rollover evidence if an unchanged poll publishes the next deadline
    // before the corresponding quota rise is observed.
    reset_evidence_at: Option<i64>,
    observed_at: i64,
    epoch: u64,
    valid: bool,
    available: bool,
    reset_due: bool,
    baseline_ready: bool,
    drops: Vec<DropSample>,
    plan_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct CreditState {
    source_key: String,
    source_label: String,
    balance: Option<Decimal>,
    has_credits: Option<bool>,
    unlimited: bool,
    observed_at: i64,
    epoch: u64,
    available: bool,
    baseline_ready: bool,
    drops: Vec<(i64, f64)>,
}

#[derive(Debug, Clone)]
pub struct QuotaModel {
    pub session: String,
    /// Importing history here does not import a live baseline or predictor state.
    pub history: Vec<HistoryEvent>,
    pub last_observed_at: Option<i64>,
    pub ordinary_usage_allowed: Option<bool>,
    windows: BTreeMap<(String, String), WindowState>,
    credit_states: BTreeMap<String, CreditState>,
    account_id: Option<String>,
    reset_guards: BTreeMap<(String, String), (f64, f64)>,
}

impl Default for QuotaModel {
    fn default() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self::new(format!("{}-{now}", std::process::id()))
    }
}

impl QuotaModel {
    /// Identity supplied by the official live payload, when available. Used to
    /// reject incompatible historical boundaries; never inferred from a source
    /// slot, task ID, or reset timestamp.
    pub fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    /// An explicit account-change notification invalidates the prior identity,
    /// even if subsequent quota payloads omit the optional account ID.
    pub fn account_changed(&mut self) {
        self.account_id = None;
        self.reset_guards.clear();
        for state in self.windows.values_mut() {
            state.pre_reset_used = None;
        }
        self.reset_continuity();
    }

    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            history: Vec::new(),
            last_observed_at: None,
            ordinary_usage_allowed: None,
            windows: BTreeMap::new(),
            credit_states: BTreeMap::new(),
            account_id: None,
            reset_guards: BTreeMap::new(),
        }
    }

    /// History seeds only the stale-snapshot rejection guard, never a live rate.
    pub fn remember_reset_guards(&mut self, history: &[HistoryEvent]) {
        self.reset_guards.clear();
        for e in history {
            if self
                .account_id
                .as_ref()
                .zip(e.evidence.as_ref().and_then(|p| p.account_id.as_ref()))
                .is_some_and(|(a, b)| a != b)
            {
                continue;
            }
            let (Some(window), Ok(before), Ok(after)) = (
                &e.window_key,
                e.before.parse::<f64>(),
                e.after.parse::<f64>(),
            ) else {
                continue;
            };
            let key = (e.source_key.clone(), window.clone());
            if e.kind == HistoryKind::QuotaReset
                && after >= 100. - PERCENT_EPSILON
                && before <= 98. + PERCENT_EPSILON
            {
                self.reset_guards.insert(key, (100. - before, 100. - after));
            } else if e.kind == HistoryKind::QuotaDecrease {
                if let Some(guard) = self.reset_guards.get_mut(&key) {
                    guard.1 = 100. - after;
                }
            }
        }
    }

    /// Break only live continuity (reconnect, reader error, wake, or account switch).
    /// The next successful observation is a baseline, never a cross-gap rate.
    pub fn reset_continuity(&mut self) {
        for state in self.windows.values_mut() {
            state.drops.clear();
            state.baseline_ready = false;
            state.available = false;
            state.epoch = state.epoch.saturating_add(1);
        }
        for state in self.credit_states.values_mut() {
            state.drops.clear();
            state.baseline_ready = false;
            state.available = false;
            state.epoch = state.epoch.saturating_add(1);
        }
    }

    pub fn end_session(&mut self) {
        self.reset_continuity();
    }

    /// Accept official account/rateLimits/read results, JSON-RPC envelopes,
    /// account/rateLimits/updated params, or Codex JSONL rate_limits payloads.
    /// No session file or credential is read by this module.
    pub fn ingest_json(
        &mut self,
        payload: &Value,
        observed_at: i64,
    ) -> Result<IngestReport, String> {
        if observed_at < 0 {
            return Err("negative_observation_time".into());
        }
        if self.last_observed_at.is_some_and(|last| observed_at < last) {
            return Err("out_of_order_observation".into());
        }
        let same_time = self.last_observed_at == Some(observed_at);
        let duplicate_state = same_time.then(|| {
            (
                self.windows.clone(),
                self.credit_states.clone(),
                self.ordinary_usage_allowed,
                self.account_id.clone(),
            )
        });
        let observation = parse_observation(payload)?;
        let mut report = IngestReport {
            accepted: true,
            warnings: observation.warnings,
            ..IngestReport::default()
        };
        if let Some(account) = observation.account_id {
            if self.account_id.as_ref().is_some_and(|old| old != &account) {
                self.windows.clear();
                self.reset_guards.clear();
                self.credit_states.clear();
                report.warnings.push("account_changed_new_baseline".into());
            }
            self.account_id = Some(account);
        }
        // Unknown backend permission stays unknown, never inferred from quota.
        self.ordinary_usage_allowed = observation.ordinary_usage_allowed;
        let history_before = self.history.len();
        let mut seen_windows = BTreeSet::new();
        let mut seen_credits = BTreeSet::new();
        for source in observation.sources {
            for incoming in source.windows {
                let key = (source.key.clone(), incoming.key.clone());
                seen_windows.insert(key.clone());
                report.windows_updated += 1;
                let old = self.windows.remove(&key);
                let history_guard = self.reset_guards.get(&key).copied();
                let pre = old
                    .as_ref()
                    .and_then(|s| s.pre_reset_used)
                    .or(history_guard.map(|g| g.0));
                let current = old
                    .as_ref()
                    .and_then(|s| s.used_percent)
                    .or(history_guard.map(|g| g.1));
                if pre.zip(current).zip(incoming.used_percent).is_some_and(
                    |((pre, current), incoming)| {
                        stale_pre_reset_echo(100. - current, 100. - incoming, 100. - pre)
                    },
                ) {
                    if let Some(old) = old {
                        self.windows.insert(key.clone(), old);
                    }
                    report.warnings.push("ignored_pre_reset_snapshot".into());
                    continue;
                }
                // Absent duration metadata must not forget an established
                // identity and allow an incompatible window to bridge it later.
                let duration = incoming
                    .duration
                    .or_else(|| old.as_ref().and_then(|old| old.duration));
                let label = window_label(&incoming.key, duration);
                let mut state = WindowState {
                    source_key: source.key.clone(),
                    source_label: source.label.clone(),
                    window_key: incoming.key.clone(),
                    window_label: label,
                    used_percent: incoming.used_percent,
                    pre_reset_used: pre,
                    duration,
                    reset_at: incoming.reset_at,
                    reset_evidence_at: old
                        .as_ref()
                        .filter(|old| {
                            old.available
                                && old.baseline_ready
                                && (old.duration.is_none()
                                    || duration.is_none()
                                    || old.duration == duration)
                        })
                        .and_then(|old| old.reset_evidence_at.or(old.reset_at))
                        .or(incoming.reset_at),
                    observed_at,
                    epoch: old.as_ref().map_or(0, |old| old.epoch),
                    valid: incoming.valid,
                    available: true,
                    reset_due: incoming.reset_at.is_some_and(|reset| reset <= observed_at),
                    baseline_ready: incoming.valid && incoming.used_percent.is_some(),
                    drops: Vec::new(),
                    plan_type: source.plan_type.clone(),
                };
                if let Some(old) = old {
                    let gap = observed_at.saturating_sub(old.observed_at);
                    let compatible_duration = old.duration.is_none()
                        || state.duration.is_none()
                        || old.duration == state.duration;
                    let continuous = old.available
                        && old.baseline_ready
                        && old.valid
                        && state.valid
                        && gap <= CONTINUITY_GAP_SECONDS
                        && compatible_duration;
                    let both = old.used_percent.zip(state.used_percent);
                    let ignored_rebound = old.available
                        && old.baseline_ready
                        && old.valid
                        && state.valid
                        && compatible_duration
                        && both.is_some_and(|(before, after)| {
                            ignored_one_point_rebound(
                                100.0 - before,
                                100.0 - after,
                                old.reset_evidence_at.or(old.reset_at),
                                state.reset_at,
                                observed_at,
                            )
                        });
                    if ignored_rebound {
                        // Keep the raw reported endpoints for audit, but never
                        // expose this row as a confirmed reset/accounting boundary.
                        if let Some(endpoints) = both {
                            let mut evidence = quota_event(
                                &self.session,
                                &state,
                                HistoryKind::QuotaReset,
                                endpoints,
                                old.observed_at,
                                old.reset_at,
                                !continuous,
                            );
                            evidence.reconciliation =
                                Some(HistoryReconciliation::UncertainRecovery);
                            self.history.push(evidence);
                        }
                        state.used_percent = old.used_percent;
                        state.reset_at = old.reset_at.or(state.reset_at);
                        state.reset_due = state.reset_at.is_some_and(|reset| reset <= observed_at);
                        if continuous {
                            state.drops = old.drops;
                        } else if gap > CONTINUITY_GAP_SECONDS {
                            // A real coverage gap still invalidates the estimator;
                            // it is not evidence of quota replenishment.
                            state.epoch = state.epoch.saturating_add(1);
                        }
                        report.warnings.push(format!(
                            "ignored_same_cycle_one_point_rebound:{}/{}",
                            state.source_key, state.window_key
                        ));
                    } else if continuous && gap == 0 {
                        if both.is_some_and(|(before, after)| {
                            (after - before).abs() <= PERCENT_EPSILON
                        }) {
                            state.drops = old.drops;
                        } else {
                            state.epoch = state.epoch.saturating_add(1);
                            report.warnings.push(format!(
                                "same_time_conflict_new_baseline:{}/{}",
                                state.source_key, state.window_key
                            ));
                        }
                    } else if continuous {
                        state.drops = old.drops;
                        if let Some((before_used, after_used)) = both {
                            let change = after_used - before_used;
                            if change < -PERCENT_EPSILON {
                                if after_used.abs() <= PERCENT_EPSILON && before_used >= 2.0 {
                                    state.pre_reset_used = Some(before_used);
                                }
                                state.reset_evidence_at = state.reset_at;
                                state.epoch = state.epoch.saturating_add(1);
                                state.drops.clear();
                                self.history.push(quota_event(
                                    &self.session,
                                    &state,
                                    HistoryKind::QuotaReset,
                                    (before_used, after_used),
                                    old.observed_at,
                                    old.reset_evidence_at.or(old.reset_at),
                                    false,
                                ));
                            } else if old.reset_due || state.reset_due {
                                // A deadline is not evidence of replenishment.
                                if !old.reset_due || !state.reset_due {
                                    state.epoch = state.epoch.saturating_add(1);
                                }
                                state.drops.clear();
                                if change > PERCENT_EPSILON {
                                    self.history.push(quota_event(
                                        &self.session,
                                        &state,
                                        HistoryKind::QuotaDecrease,
                                        (before_used, after_used),
                                        old.observed_at,
                                        old.reset_at,
                                        true,
                                    ));
                                }
                            } else if change > PERCENT_EPSILON {
                                self.history.push(quota_event(
                                    &self.session,
                                    &state,
                                    HistoryKind::QuotaDecrease,
                                    (before_used, after_used),
                                    old.observed_at,
                                    old.reset_at,
                                    false,
                                ));
                                state.drops.push(DropSample {
                                    observed_at,
                                    delta_percent: change,
                                    poll_interval_seconds: gap,
                                });
                                trim_front(&mut state.drops, MAX_DROP_SAMPLES);
                            }
                        }
                    } else {
                        if old.baseline_ready {
                            state.epoch = state.epoch.saturating_add(1);
                        }
                        // A sparse historical observation can establish a real
                        // endpoint delta without establishing an active usage rate.
                        if gap > CONTINUITY_GAP_SECONDS
                            && old.available
                            && old.baseline_ready
                            && old.valid
                            && state.valid
                            && compatible_duration
                        {
                            if let Some((before_used, after_used)) = both {
                                let change = after_used - before_used;
                                if change.abs() > PERCENT_EPSILON {
                                    let kind = if change > 0.0 {
                                        HistoryKind::QuotaDecrease
                                    } else {
                                        HistoryKind::QuotaReset
                                    };
                                    if kind == HistoryKind::QuotaReset {
                                        if after_used.abs() <= PERCENT_EPSILON && before_used >= 2.0
                                        {
                                            state.pre_reset_used = Some(before_used);
                                        }
                                        state.reset_evidence_at = state.reset_at;
                                    }
                                    self.history.push(quota_event(
                                        &self.session,
                                        &state,
                                        kind,
                                        (before_used, after_used),
                                        old.observed_at,
                                        if kind == HistoryKind::QuotaReset {
                                            old.reset_evidence_at.or(old.reset_at)
                                        } else {
                                            old.reset_at
                                        },
                                        true,
                                    ));
                                }
                            }
                        }
                    }
                }
                self.windows.insert(key, state);
            }
            if let Some(incoming) = source.credits {
                seen_credits.insert(source.key.clone());
                let old = self.credit_states.remove(&source.key);
                let baseline_ready =
                    incoming.valid && incoming.balance.is_some() && !incoming.unlimited;
                let mut state = CreditState {
                    source_key: source.key.clone(),
                    source_label: source.label.clone(),
                    balance: incoming.balance,
                    has_credits: incoming.has_credits,
                    unlimited: incoming.unlimited,
                    observed_at,
                    epoch: old.as_ref().map_or(0, |old| old.epoch),
                    available: incoming.valid,
                    baseline_ready,
                    drops: Vec::new(),
                };
                if let Some(old) = old {
                    let gap = observed_at.saturating_sub(old.observed_at);
                    let comparable = old.available
                        && old.baseline_ready
                        && baseline_ready
                        && old.unlimited == state.unlimited;
                    let continuous = comparable && gap <= CONTINUITY_GAP_SECONDS;
                    if continuous {
                        state.drops = old.drops.clone();
                    } else if old.baseline_ready {
                        state.epoch = state.epoch.saturating_add(1);
                    }
                    if comparable {
                        if let Some((before, after)) =
                            old.balance.as_ref().zip(state.balance.as_ref())
                        {
                            let ordering = after.cmp(before);
                            if gap == 0 && ordering != Ordering::Equal {
                                state.epoch = state.epoch.saturating_add(1);
                                state.drops.clear();
                                report.warnings.push(format!(
                                    "same_time_conflict_new_baseline:{}/credits",
                                    state.source_key
                                ));
                            } else if ordering == Ordering::Less {
                                let delta = before.subtract(after).unwrap();
                                self.history.push(credit_event(
                                    &self.session,
                                    &state,
                                    HistoryKind::CreditDecrease,
                                    (before, after),
                                    &delta,
                                    old.observed_at,
                                    !continuous,
                                ));
                                if continuous {
                                    if let Some(amount) = delta.as_f64() {
                                        state.drops.push((observed_at, amount));
                                    }
                                    trim_front(&mut state.drops, MAX_DROP_SAMPLES);
                                }
                            } else if ordering == Ordering::Greater {
                                if continuous {
                                    state.epoch = state.epoch.saturating_add(1);
                                }
                                state.drops.clear();
                                let delta = after.subtract(before).unwrap();
                                self.history.push(credit_event(
                                    &self.session,
                                    &state,
                                    HistoryKind::CreditTopUp,
                                    (before, after),
                                    &delta,
                                    old.observed_at,
                                    !continuous,
                                ));
                            }
                        }
                    }
                }
                self.credit_states.insert(source.key, state);
            }
        }
        for (key, state) in &mut self.windows {
            if !same_time && !seen_windows.contains(key) {
                if state.available {
                    state.epoch = state.epoch.saturating_add(1);
                }
                state.available = false;
                state.baseline_ready = false;
                state.drops.clear();
            }
        }
        for (key, state) in &mut self.credit_states {
            if !same_time && !seen_credits.contains(key) {
                if state.available {
                    state.epoch = state.epoch.saturating_add(1);
                }
                state.available = false;
                state.baseline_ready = false;
                state.drops.clear();
            }
        }
        for event in &mut self.history[history_before..] {
            if let Some(evidence) = &mut event.evidence {
                evidence.account_id = self.account_id.clone();
            }
        }
        report.events_added = self.history.len().saturating_sub(history_before);
        self.last_observed_at = Some(observed_at);
        if let Some((windows, credits, permission, account)) = duplicate_state {
            report.ignored_duplicate = windows == self.windows
                && credits == self.credit_states
                && permission == self.ordinary_usage_allowed
                && account == self.account_id
                && report.events_added == 0;
            if report.ignored_duplicate {
                report.accepted = false;
            }
        }
        Ok(report)
    }

    pub fn views(&self, now: i64) -> Vec<WindowView> {
        self.windows
            .values()
            .map(|state| {
                let age = now.checked_sub(state.observed_at).filter(|age| *age >= 0);
                let status = if !state.available
                    || !state.valid
                    || state.used_percent.is_none()
                    || age.is_none()
                {
                    WindowStatus::Unavailable
                } else if age.is_some_and(|age| age > prediction::STALE_AFTER_SECONDS) {
                    WindowStatus::Stale
                } else if state.reset_at.is_some_and(|reset| reset <= now) {
                    WindowStatus::ResetDue
                } else {
                    WindowStatus::Fresh
                };
                let remaining = state
                    .used_percent
                    .map(|used| (100.0 - used).clamp(0.0, 100.0));
                let prediction = prediction::predict(
                    &state.drops,
                    &PredictionContext {
                        now,
                        observed_at: state.observed_at,
                        remaining_percent: remaining,
                        reset_at: state.reset_at,
                        eligible: state.available && state.valid,
                    },
                );
                WindowView {
                    source_key: state.source_key.clone(),
                    source_label: state.source_label.clone(),
                    window_key: state.window_key.clone(),
                    window_label: state.window_label.clone(),
                    used_percent: state.used_percent,
                    remaining_percent: remaining,
                    window_duration_mins: state.duration,
                    reset_at: state.reset_at,
                    observed_at: state.observed_at,
                    age_seconds: age,
                    epoch: state.epoch,
                    status,
                    rate_per_hour: prediction.rate_per_hour,
                    prediction,
                    plan_type: state.plan_type.clone(),
                    ordinary_usage_allowed: self.ordinary_usage_allowed,
                }
            })
            .collect()
    }

    pub fn view(&self, source_key: &str, window_key: &str, now: i64) -> Option<WindowView> {
        self.views(now)
            .into_iter()
            .find(|view| view.source_key == source_key && view.window_key == window_key)
    }

    pub fn credits(&self, now: i64) -> Vec<CreditView> {
        self.credit_states
            .values()
            .map(|state| {
                let age = now.checked_sub(state.observed_at).filter(|age| *age >= 0);
                let status = if !state.available || age.is_none() {
                    WindowStatus::Unavailable
                } else if age.is_some_and(|age| age > prediction::STALE_AFTER_SECONDS) {
                    WindowStatus::Stale
                } else {
                    WindowStatus::Fresh
                };
                let recent = &state.drops[state.drops.len().saturating_sub(6)..];
                let rate = if status == WindowStatus::Fresh && !state.unlimited && recent.len() >= 2
                {
                    let elapsed = recent.last().unwrap().0.saturating_sub(recent[0].0) as f64;
                    let amount: f64 = recent.iter().skip(1).map(|drop| drop.1).sum();
                    let rate = amount * 60.0 / elapsed;
                    (elapsed > 0.0 && rate.is_finite() && rate > 0.0).then_some(rate)
                } else {
                    None
                };
                CreditView {
                    source_key: state.source_key.clone(),
                    source_label: state.source_label.clone(),
                    balance: state.balance.as_ref().map(Decimal::text),
                    has_credits: state.has_credits,
                    unlimited: state.unlimited,
                    observed_at: state.observed_at,
                    epoch: state.epoch,
                    status,
                    rate_per_minute: rate,
                }
            })
            .collect()
    }

    // No automatic history retention or count truncation: the UI chooses a
    // browsing range, while available older evidence remains available on disk.
}

fn trim_front<T>(items: &mut Vec<T>, maximum: usize) {
    if items.len() > maximum {
        items.drain(..items.len() - maximum);
    }
}

fn quota_event(
    session: &str,
    state: &WindowState,
    kind: HistoryKind,
    endpoints: (f64, f64),
    before_observed_at: i64,
    before_reset_at: Option<i64>,
    partial: bool,
) -> HistoryEvent {
    let (before_used, after_used) = endpoints;
    HistoryEvent {
        session: session.into(),
        observed_at_unix: state.observed_at,
        source_key: state.source_key.clone(),
        epoch: state.epoch,
        kind,
        window_label: state.window_label.clone(),
        before: percent_text(100.0 - before_used),
        after: percent_text(100.0 - after_used),
        delta: percent_text((after_used - before_used).abs()),
        partial,
        before_observed_at_unix: Some(before_observed_at),
        window_key: Some(state.window_key.clone()),
        evidence: Some(HistoryEvidence {
            before_reset_at,
            reset_at: state.reset_at,
            window_duration_mins: state.duration,
            ..HistoryEvidence::default()
        }),
        reconciliation: None,
    }
}
fn credit_event(
    session: &str,
    state: &CreditState,
    kind: HistoryKind,
    endpoints: (&Decimal, &Decimal),
    delta: &Decimal,
    before_observed_at: i64,
    partial: bool,
) -> HistoryEvent {
    let (before, after) = endpoints;
    HistoryEvent {
        session: session.into(),
        observed_at_unix: state.observed_at,
        source_key: state.source_key.clone(),
        epoch: state.epoch,
        kind,
        window_label: "點數".into(),
        before: before.text(),
        after: after.text(),
        delta: delta.text(),
        partial,
        before_observed_at_unix: Some(before_observed_at),
        window_key: Some("credits".into()),
        evidence: Some(HistoryEvidence::default()),
        reconciliation: None,
    }
}
pub(crate) fn percent_text(value: f64) -> String {
    let value = if value.abs() < PERCENT_EPSILON {
        0.0
    } else {
        value
    };
    format!("{value:.6}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}
fn window_label(key: &str, duration: Option<i64>) -> String {
    match duration {
        Some(10080) => "1 週".into(),
        Some(minutes) if minutes % 1440 == 0 => format!("{} 天", minutes / 1440),
        Some(minutes) if minutes % 60 == 0 => format!("{} 小時", minutes / 60),
        Some(minutes) => format!("{minutes} 分鐘"),
        None => if key == "primary" { "主要" } else { "次要" }.into(),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct HistoricalWindow {
    pub account_id: Option<String>,
    pub source_key: String,
    pub window_key: String,
    pub window_label: String,
    pub remaining: String,
    pub duration: Option<i64>,
    pub reset_at: Option<i64>,
}

pub(crate) fn historical_windows(payload: &Value) -> Result<Vec<HistoricalWindow>, String> {
    let observation = parse_observation(payload)?;
    let mut result = Vec::new();
    for source in observation.sources {
        for window in source.windows {
            if !window.valid {
                continue;
            }
            if let Some(used) = window.used_percent {
                result.push(HistoricalWindow {
                    account_id: observation.account_id.clone(),
                    source_key: source.key.clone(),
                    window_label: window_label(&window.key, window.duration),
                    window_key: window.key,
                    remaining: percent_text(100.0 - used),
                    duration: window.duration,
                    reset_at: window.reset_at,
                });
            }
        }
    }
    Ok(result)
}

struct Observation {
    sources: Vec<SourceInput>,
    account_id: Option<String>,
    ordinary_usage_allowed: Option<bool>,
    warnings: Vec<String>,
}
struct SourceInput {
    key: String,
    label: String,
    plan_type: Option<String>,
    windows: Vec<WindowInput>,
    credits: Option<CreditInput>,
}
struct WindowInput {
    key: String,
    used_percent: Option<f64>,
    duration: Option<i64>,
    reset_at: Option<i64>,
    valid: bool,
}
struct CreditInput {
    balance: Option<Decimal>,
    has_credits: Option<bool>,
    unlimited: bool,
    valid: bool,
}

fn field<'a>(value: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> {
    value.get(camel).or_else(|| value.get(snake))
}
fn text_field(value: &Value, camel: &str, snake: &str) -> Option<String> {
    field(value, camel, snake)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn parse_observation(payload: &Value) -> Result<Observation, String> {
    let mut root = payload;
    for _ in 0..4 {
        if let Some(error) = root.get("error").filter(|value| !value.is_null()) {
            let _ = error; // Do not echo arbitrary server payloads or secrets.
            return Err("rate_limit_rpc_error".into());
        }
        if field(root, "rateLimits", "rate_limits").is_some()
            || root.get("primary").is_some()
            || field(root, "rateLimitsByLimitId", "rate_limits_by_limit_id").is_some()
        {
            break;
        }
        if let Some(inner) = root
            .get("result")
            .or_else(|| root.get("params"))
            .or_else(|| root.get("payload"))
        {
            root = inner;
        } else {
            break;
        }
    }
    if !root.is_object() {
        return Err("expected_rate_limits_object".into());
    }
    let account_id = text_field(root, "accountId", "account_id");
    let ordinary_usage_allowed =
        field(root, "ordinaryUsageAllowed", "ordinary_usage_allowed").and_then(Value::as_bool);
    let mut sources = BTreeMap::new();
    let mut warnings = Vec::new();
    if let Some(buckets) = field(root, "rateLimitsByLimitId", "rate_limits_by_limit_id") {
        if let Some(buckets) = buckets.as_object() {
            if buckets.len() > 256 {
                return Err("too_many_rate_limit_sources".into());
            }
            for (key, source) in buckets {
                if !source.is_object() {
                    warnings.push(format!("invalid_source:{key}"));
                    continue;
                }
                sources.insert(key.clone(), parse_source(key, source, &mut warnings));
            }
        } else if !buckets.is_null() {
            return Err("invalid_rate_limits_by_limit_id".into());
        }
    }
    let aggregate = field(root, "rateLimits", "rate_limits").or_else(|| {
        (root.get("primary").is_some()
            || root.get("secondary").is_some()
            || root.get("credits").is_some())
        .then_some(root)
    });
    if let Some(aggregate) = aggregate.filter(|aggregate| !aggregate.is_null()) {
        if !aggregate.is_object() {
            return Err("invalid_rate_limits_snapshot".into());
        }
        let key = text_field(aggregate, "limitId", "limit_id").unwrap_or_else(|| "codex".into());
        // The official aggregate mirrors one named bucket. Never double-count it.
        sources
            .entry(key.clone())
            .or_insert_with(|| parse_source(&key, aggregate, &mut warnings));
    }
    if sources.is_empty() {
        return Err("missing_rate_limits_snapshot".into());
    }
    Ok(Observation {
        sources: sources.into_values().collect(),
        account_id,
        ordinary_usage_allowed,
        warnings,
    })
}

fn parse_source(key: &str, value: &Value, warnings: &mut Vec<String>) -> SourceInput {
    let label = text_field(value, "limitName", "limit_name").unwrap_or_else(|| key.to_owned());
    let mut windows = Vec::new();
    for window_key in ["primary", "secondary"] {
        if let Some(window) = value.get(window_key).filter(|window| !window.is_null()) {
            let used_percent = field(window, "usedPercent", "used_percent")
                .and_then(number)
                .filter(|value| (0.0..=100.0).contains(value));
            let duration_value = field(window, "windowDurationMins", "window_minutes")
                .or_else(|| window.get("window_duration_mins"));
            let reset_value = field(window, "resetsAt", "resets_at");
            let duration = duration_value
                .and_then(Value::as_i64)
                .filter(|duration| *duration > 0);
            let reset_at = reset_value
                .and_then(Value::as_i64)
                .filter(|reset| *reset >= 0);
            let valid = window.is_object()
                && used_percent.is_some()
                && duration_value.is_none_or(|value| value.is_null() || duration.is_some())
                && reset_value.is_none_or(|value| value.is_null() || reset_at.is_some());
            if !valid {
                warnings.push(format!("invalid_window:{key}/{window_key}"));
            }
            windows.push(WindowInput {
                key: window_key.into(),
                used_percent,
                duration,
                reset_at,
                valid,
            });
        }
    }
    let credits = value
        .get("credits")
        .filter(|value| !value.is_null())
        .map(|credits| {
            let raw = credits.get("balance");
            let balance = raw.and_then(|value| match value {
                Value::String(value) => Decimal::parse(value),
                Value::Number(value) => Decimal::parse(&value.to_string()),
                _ => None,
            });
            let valid =
                credits.is_object() && raw.is_none_or(|value| value.is_null() || balance.is_some());
            if !valid {
                warnings.push(format!("invalid_credits:{key}"));
            }
            CreditInput {
                balance,
                has_credits: field(credits, "hasCredits", "has_credits").and_then(Value::as_bool),
                unlimited: credits
                    .get("unlimited")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                valid,
            }
        });
    SourceInput {
        key: key.into(),
        label,
        plan_type: text_field(value, "planType", "plan_type"),
        windows,
        credits,
    }
}
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) if text.len() <= 64 => text.parse::<f64>().ok(),
        _ => None,
    }
    .filter(|number| number.is_finite())
}

pub(crate) fn decimal_text(text: &str) -> Option<String> {
    Decimal::parse(text).map(|value| value.text())
}

/// Small bounded nonnegative decimal implementation for exact credit history.
/// No binary-float comparison is used to classify a decrease or top-up.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Decimal {
    digits: String,
    scale: usize,
}
impl Decimal {
    fn parse(text: &str) -> Option<Self> {
        if text.is_empty() || text.len() > 256 {
            return None;
        }
        let (integer, fraction) = text.split_once('.').unwrap_or((text, ""));
        if integer.is_empty()
            || !integer.bytes().all(|byte| byte.is_ascii_digit())
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let mut digits = format!("{integer}{fraction}");
        let mut scale = fraction.len();
        while scale > 0 && digits.ends_with('0') {
            digits.pop();
            scale -= 1;
        }
        let trimmed = digits.trim_start_matches('0');
        if trimmed.is_empty() {
            return Some(Self {
                digits: "0".into(),
                scale: 0,
            });
        }
        Some(Self {
            digits: trimmed.into(),
            scale,
        })
    }
    fn aligned(&self, scale: usize) -> String {
        format!("{}{}", self.digits, "0".repeat(scale - self.scale))
    }
    fn cmp(&self, other: &Self) -> Ordering {
        let scale = self.scale.max(other.scale);
        let left = self.aligned(scale);
        let right = other.aligned(scale);
        left.len().cmp(&right.len()).then_with(|| left.cmp(&right))
    }
    fn subtract(&self, other: &Self) -> Option<Self> {
        if self.cmp(other) == Ordering::Less {
            return None;
        }
        let scale = self.scale.max(other.scale);
        let left = self.aligned(scale);
        let right = other.aligned(scale);
        let mut output = Vec::with_capacity(left.len());
        let mut rhs = right.bytes().rev();
        let mut borrow = 0i16;
        for byte in left.bytes().rev() {
            let mut value =
                i16::from(byte - b'0') - i16::from(rhs.next().unwrap_or(b'0') - b'0') - borrow;
            borrow = if value < 0 {
                value += 10;
                1
            } else {
                0
            };
            output.push((value as u8) + b'0');
        }
        output.reverse();
        let digits = String::from_utf8(output).ok()?;
        Self::parse(&Self { digits, scale }.text())
    }
    fn text(&self) -> String {
        if self.scale == 0 {
            return self.digits.clone();
        }
        if self.digits.len() <= self.scale {
            format!(
                "0.{}{}",
                "0".repeat(self.scale - self.digits.len()),
                self.digits
            )
        } else {
            let split = self.digits.len() - self.scale;
            format!("{}.{}", &self.digits[..split], &self.digits[split..])
        }
    }
    fn as_f64(&self) -> Option<f64> {
        self.text()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prediction::PredictionState;
    use serde_json::json;

    fn payload(used: f64, reset: i64) -> Value {
        json!({"rateLimits": {"limitId":"codex", "primary":{"usedPercent":used, "windowDurationMins":300,"resetsAt":reset}}})
    }
    fn view(model: &QuotaModel, now: i64) -> WindowView {
        model.view("codex", "primary", now).unwrap()
    }
    fn ingest(model: &mut QuotaModel, at: i64, used: f64) {
        model.ingest_json(&payload(used, 10000), at).unwrap();
    }

    #[test]
    fn reset_rejects_cached_old_endpoint_but_accepts_skips_and_later_real_usage() {
        assert!(!stale_pre_reset_echo(100., 98., 98.));
        let mut m = QuotaModel::new("test");
        for (t, used, reset) in [
            (100, 47., 1000),
            (110, 0., 9000),
            (120, 47., 1000),
            (130, 1., 9000),
            (140, 47., 1000),
            (150, 3., 9000),
        ] {
            m.ingest_json(&payload(used, reset), t).unwrap();
            if used == 47. && t > 100 {
                assert!(view(&m, t).remaining_percent.unwrap() >= 99.);
            }
        }
        assert_eq!(view(&m, 150).remaining_percent, Some(97.));
        let mut restarted = QuotaModel::new("restart");
        restarted.remember_reset_guards(&m.history);
        restarted.ingest_json(&payload(3., 9000), 150).unwrap();
        restarted.ingest_json(&payload(47., 1000), 155).unwrap();
        assert_eq!(view(&restarted, 155).remaining_percent, Some(97.));
        assert!(!m
            .history
            .iter()
            .any(|e| e.kind == HistoryKind::QuotaDecrease && e.after == "53"));
        m.ingest_json(&payload(46., 9000), 160).unwrap();
        m.ingest_json(&payload(47., 9000), 170).unwrap();
        assert_eq!(view(&m, 170).remaining_percent, Some(53.));
    }

    #[test]
    fn parses_official_wrapper_and_deduplicates_multi_bucket() {
        let snapshot = json!({"limitId":"codex", "primary":{"usedPercent":15,"windowDurationMins":300,"resetsAt":5000}});
        let data = json!({"id":1,"result":{"rateLimits":snapshot,"rateLimitsByLimitId":{"codex":snapshot,"other":{"limitName":"Other","secondary":{"usedPercent":25,"windowDurationMins":10080,"resetsAt":9000}}},"ordinaryUsageAllowed":false,"accountId":"synthetic"}});
        let mut model = QuotaModel::new("test");
        let report = model.ingest_json(&data, 100).unwrap();
        assert_eq!(report.windows_updated, 2);
        assert_eq!(model.views(100).len(), 2);
        assert_eq!(view(&model, 100).remaining_percent, Some(85.0));
        assert_eq!(view(&model, 100).ordinary_usage_allowed, Some(false));
        assert_eq!(
            model.view("other", "secondary", 100).unwrap().window_label,
            "1 週"
        );
    }
    #[test]
    fn accepts_snake_case_jsonl_and_notification_envelopes() {
        let raw = json!({"type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":30.5,"window_minutes":300,"resets_at":10000},"credits":{"has_credits":true,"unlimited":false,"balance":"123.4500"}}}});
        let mut model = QuotaModel::new("test");
        model.ingest_json(&raw, 100).unwrap();
        assert_eq!(view(&model, 100).remaining_percent, Some(69.5));
        assert_eq!(model.credits(100)[0].balance.as_deref(), Some("123.45"));
        let notification = json!({"method":"account/rateLimits/updated","params":{"rateLimits":{"primary":{"usedPercent":31,"windowDurationMins":300,"resetsAt":10000}}}});
        model.ingest_json(&notification, 130).unwrap();
        assert_eq!(model.history.len(), 1);
    }
    #[test]
    fn thousands_of_zero_remaining_metadata_changes_never_emit_reset() {
        let mut model = QuotaModel::new("test");
        for index in 0..5087 {
            model
                .ingest_json(&payload(100.0, 1_000_000 + index * 30), 100 + index * 30)
                .unwrap();
        }
        assert!(model.history.is_empty());
        assert_eq!(view(&model, 100 + 5086 * 30).epoch, 0);
    }
    #[test]
    fn metadata_timestamp_jitter_preserves_drop_evidence_and_history() {
        let mut model = QuotaModel::new("test");
        ingest(&mut model, 100, 10.0);
        ingest(&mut model, 130, 11.0);
        ingest(&mut model, 160, 12.0);
        let before = view(&model, 160);
        assert_eq!(before.prediction.state, PredictionState::Estimate);
        for (at, reset) in [(170, 10030), (180, 9998), (190, 10100)] {
            model.ingest_json(&payload(12.0, reset), at).unwrap();
        }
        let after = view(&model, 190);
        assert_eq!(before.epoch, after.epoch);
        assert_eq!(after.prediction.observed_drops, 2);
        assert_eq!(model.history.len(), 2);
        assert!(model
            .history
            .iter()
            .all(|event| event.kind == HistoryKind::QuotaDecrease));
    }
    #[test]
    fn genuine_refill_has_nonzero_delta_and_new_epoch() {
        let mut model = QuotaModel::new("test");
        ingest(&mut model, 100, 99.0);
        ingest(&mut model, 130, 100.0);
        ingest(&mut model, 160, 0.0);
        let event = model.history.last().unwrap();
        assert_eq!(event.kind, HistoryKind::QuotaReset);
        assert_eq!(
            (&event.before, &event.after, &event.delta),
            (&"0".to_string(), &"100".to_string(), &"100".to_string())
        );
        assert_eq!(view(&model, 160).epoch, 1);
        assert_eq!(view(&model, 160).prediction.observed_drops, 0);
        ingest(&mut model, 190, 1.0);
        assert_eq!(
            view(&model, 190).prediction.state,
            PredictionState::Insufficient
        );
        ingest(&mut model, 220, 2.0);
        assert_eq!(
            view(&model, 220).prediction.state,
            PredictionState::Estimate
        );
    }
    #[test]
    fn fractional_refill_is_real_but_float_noise_is_not() {
        let mut model = QuotaModel::new("test");
        ingest(&mut model, 100, 10.0);
        ingest(&mut model, 130, 9.999999999);
        assert!(model.history.is_empty());
        ingest(&mut model, 160, 9.5);
        assert_eq!(model.history.len(), 1);
        assert_eq!(model.history[0].kind, HistoryKind::QuotaReset);
        assert_eq!(model.history[0].delta, "0.5");
    }
    #[test]
    fn duration_change_is_not_a_visible_reset_or_cross_window_rate() {
        let mut model = QuotaModel::new("test");
        ingest(&mut model, 100, 10.0);
        ingest(&mut model, 130, 11.0);
        ingest(&mut model, 160, 12.0);
        let mut changed = payload(12.0, 10000);
        changed["rateLimits"]["primary"]["windowDurationMins"] = json!(10080);
        model.ingest_json(&changed, 190).unwrap();
        assert_eq!(model.history.len(), 2);
        assert_eq!(view(&model, 190).prediction.observed_drops, 0);
        assert_eq!(view(&model, 190).epoch, 1);
    }
    #[test]
    fn poll_gap_breaks_rate_without_erasing_history() {
        let mut model = QuotaModel::new("test");
        ingest(&mut model, 100, 10.0);
        ingest(&mut model, 130, 11.0);
        ingest(&mut model, 160, 12.0);
        ingest(&mut model, 251, 18.0);
        let resumed = view(&model, 251);
        assert_eq!(resumed.prediction.state, PredictionState::Insufficient);
        assert_eq!(resumed.prediction.observed_drops, 0);
        assert_eq!(model.history.len(), 3);
        assert!(model.history[2].partial);
        assert_eq!(model.history[2].before_observed_at_unix, Some(160));
        assert_eq!(resumed.epoch, 1);
        ingest(&mut model, 281, 19.0);
        ingest(&mut model, 311, 20.0);
        assert_eq!(
            view(&model, 311).prediction.state,
            PredictionState::Estimate
        );
        assert!((view(&model, 311).rate_per_hour.unwrap() - 120.0).abs() < 0.001);
    }
    #[test]
    fn ninety_second_poll_gap_preserves_continuity() {
        let mut model = QuotaModel::new("test");
        ingest(&mut model, 100, 10.0);
        ingest(&mut model, 190, 11.0);
        ingest(&mut model, 280, 12.0);
        assert_eq!(
            view(&model, 280).prediction.state,
            PredictionState::Estimate
        );
    }
    #[test]
    fn unchanged_polls_preserve_long_drop_interval_continuity() {
        let mut model = QuotaModel::new("test");
        ingest(&mut model, 100, 10.0);
        ingest(&mut model, 130, 11.0);
        for at in (160..=700).step_by(30) {
            ingest(&mut model, at, 11.0);
        }
        ingest(&mut model, 730, 12.0);
        assert_eq!(
            view(&model, 730).prediction.state,
            PredictionState::Estimate
        );
        assert!((view(&model, 730).prediction.interval_seconds.unwrap() - 600.0).abs() < 0.001);
    }
    #[test]
    fn stale_ui_retains_last_percent_but_hides_rate() {
        let mut model = QuotaModel::new("test");
        ingest(&mut model, 100, 10.0);
        ingest(&mut model, 130, 11.0);
        ingest(&mut model, 160, 12.0);
        assert_eq!(view(&model, 250).status, WindowStatus::Fresh);
        let stale = view(&model, 251);
        assert_eq!(stale.status, WindowStatus::Stale);
        assert!(stale.rate_per_hour.is_none());
        assert_eq!(stale.remaining_percent, Some(88.0));
    }
    #[test]
    fn explicit_reconnect_does_not_use_old_drop_or_previous_value() {
        let mut model = QuotaModel::new("test");
        ingest(&mut model, 100, 10.0);
        ingest(&mut model, 130, 11.0);
        ingest(&mut model, 160, 12.0);
        model.reset_continuity();
        assert_eq!(view(&model, 160).status, WindowStatus::Unavailable);
        ingest(&mut model, 190, 80.0);
        assert_eq!(model.history.len(), 2);
        assert_eq!(view(&model, 190).prediction.observed_drops, 0);
    }
    #[test]
    fn out_of_order_duplicate_and_rpc_error_do_not_mutate() {
        let mut model = QuotaModel::new("test");
        ingest(&mut model, 100, 10.0);
        assert!(model.ingest_json(&payload(20.0, 10000), 99).is_err());
        assert!(
            model
                .ingest_json(&payload(10.0, 10000), 100)
                .unwrap()
                .ignored_duplicate
        );
        assert!(model
            .ingest_json(&json!({"error":{"message":"synthetic"}}), 101)
            .is_err());
        assert_eq!(view(&model, 100).used_percent, Some(10.0));
        assert_eq!(model.last_observed_at, Some(100));
        assert!(model.history.is_empty());
    }
    #[test]
    fn malformed_window_becomes_unavailable_and_breaks_continuity() {
        for invalid in [
            json!(-1),
            json!(101),
            json!("NaN"),
            json!(null),
            json!(true),
        ] {
            let mut model = QuotaModel::new("test");
            ingest(&mut model, 100, 10.0);
            ingest(&mut model, 130, 11.0);
            let mut bad = payload(12.0, 10000);
            bad["rateLimits"]["primary"]["usedPercent"] = invalid;
            assert!(!model.ingest_json(&bad, 160).unwrap().warnings.is_empty());
            assert_eq!(view(&model, 160).status, WindowStatus::Unavailable);
            ingest(&mut model, 190, 20.0);
            assert_eq!(view(&model, 190).prediction.observed_drops, 0);
        }
    }
    #[test]
    fn missing_source_is_unavailable_and_restoration_starts_new_baseline() {
        let mut model = QuotaModel::new("test");
        ingest(&mut model, 100, 10.0);
        ingest(&mut model, 130, 11.0);
        model
            .ingest_json(
                &json!({"rateLimits":{"limitId":"other","primary":{"usedPercent":1}}}),
                160,
            )
            .unwrap();
        assert_eq!(view(&model, 160).status, WindowStatus::Unavailable);
        ingest(&mut model, 190, 20.0);
        assert_eq!(model.history.len(), 1);
        assert_eq!(view(&model, 190).prediction.observed_drops, 0);
    }
    #[test]
    fn passed_reset_metadata_never_creates_repeated_zero_resets() {
        let mut model = QuotaModel::new("test");
        model.ingest_json(&payload(100.0, 130), 100).unwrap();
        for at in [130, 160, 190, 220] {
            model.ingest_json(&payload(100.0, 130), at).unwrap();
        }
        assert!(model.history.is_empty());
        assert_eq!(view(&model, 220).epoch, 1);
        assert_eq!(view(&model, 220).status, WindowStatus::ResetDue);
    }
    #[test]
    fn account_switch_never_compares_separate_users() {
        let mut model = QuotaModel::new("test");
        let mut a = payload(90.0, 10000);
        a["accountId"] = json!("synthetic-A");
        model.ingest_json(&a, 100).unwrap();
        let mut b = payload(10.0, 10000);
        b["accountId"] = json!("synthetic-B");
        model.ingest_json(&b, 130).unwrap();
        assert!(model.history.is_empty());
        assert_eq!(view(&model, 130).prediction.observed_drops, 0);
    }
    #[test]
    fn historical_events_do_not_seed_live_estimate() {
        let mut old = QuotaModel::new("old");
        ingest(&mut old, 100, 10.0);
        ingest(&mut old, 130, 11.0);
        ingest(&mut old, 160, 12.0);
        let mut model = QuotaModel::new("new");
        model.history = old.history;
        ingest(&mut model, 190, 30.0);
        assert_eq!(model.history.len(), 2);
        assert_eq!(
            view(&model, 190).prediction.state,
            PredictionState::Insufficient
        );
    }
    #[test]
    fn exact_credit_decrease_and_top_up() {
        let mut model = QuotaModel::new("test");
        let credit = |balance: &str| json!({"rateLimits":{"credits":{"hasCredits":true,"unlimited":false,"balance":balance}}});
        model
            .ingest_json(&credit("9007199254740993.000000000000000001"), 100)
            .unwrap();
        model
            .ingest_json(&credit("9007199254740993.000000000000000000"), 130)
            .unwrap();
        assert_eq!(model.history[0].kind, HistoryKind::CreditDecrease);
        assert_eq!(model.history[0].delta, "0.000000000000000001");
        model.ingest_json(&credit("9007199254740994"), 160).unwrap();
        assert_eq!(model.history[1].kind, HistoryKind::CreditTopUp);
        assert_eq!(model.history[1].delta, "1");
    }
    #[test]
    fn equal_credit_spelling_does_not_create_events() {
        let mut model = QuotaModel::new("test");
        for (index, balance) in ["01.000", "1", "1.0", "0001.000000"].iter().enumerate() {
            model
                .ingest_json(
                    &json!({"rateLimits":{"credits":{"balance":balance}}}),
                    100 + index as i64 * 30,
                )
                .unwrap();
        }
        assert!(model.history.is_empty());
    }
    #[test]
    fn credit_rate_requires_two_drops_and_stops_after_gap() {
        let mut model = QuotaModel::new("test");
        let credit = |balance| json!({"rateLimits":{"credits":{"balance":balance}}});
        for (at, balance) in [(100, "10"), (130, "9"), (160, "8")] {
            model.ingest_json(&credit(balance), at).unwrap();
        }
        assert_eq!(model.credits(160)[0].rate_per_minute, Some(2.0));
        model.ingest_json(&credit("5"), 251).unwrap();
        assert!(model.credits(251)[0].rate_per_minute.is_none());
    }
    #[test]
    fn unlimited_credit_balance_change_is_not_a_consumption_event() {
        let mut model = QuotaModel::new("test");
        for (at, balance) in [(100, "10"), (130, "9"), (160, "8")] {
            model
                .ingest_json(
                    &json!({"rateLimits":{"credits":{"balance":balance,"unlimited":true}}}),
                    at,
                )
                .unwrap();
        }
        assert!(model.history.is_empty());
        assert!(model.credits(160)[0].unlimited);
    }
    #[test]
    fn decimal_arithmetic_is_exact_bounded_and_handles_borrow() {
        for (a, b, result) in [
            ("1000.001", "0.002", "999.999"),
            ("1", "0.0001", "0.9999"),
            ("1.0", "1.00", "0"),
            ("0.1", "0.01", "0.09"),
        ] {
            assert_eq!(
                Decimal::parse(a)
                    .unwrap()
                    .subtract(&Decimal::parse(b).unwrap())
                    .unwrap()
                    .text(),
                result
            );
        }
        for invalid in ["", "-1", "NaN", "Infinity", "1e3", "1.1.1", " 1", ".5"] {
            assert!(Decimal::parse(invalid).is_none());
        }
        assert!(Decimal::parse(&"1".repeat(257)).is_none());
    }
    #[test]
    fn history_shape_is_legacy_compatible_and_json_round_trips() {
        let mut model = QuotaModel::new("test");
        ingest(&mut model, 100, 10.0);
        ingest(&mut model, 130, 11.0);
        let json = serde_json::to_value(&model.history[0]).unwrap();
        assert_eq!(json["kind"], "QuotaDecrease");
        assert_eq!(json["before"], "90");
        assert_eq!(json["after"], "89");
        let restored: HistoryEvent = serde_json::from_value(json).unwrap();
        assert_eq!(restored, model.history[0]);
        assert!(serde_json::to_value(model.views(130)).is_ok());
    }
    #[test]
    fn window_metadata_validation_distinguishes_missing_from_malformed() {
        let mut model = QuotaModel::new("test");
        model.ingest_json(&json!({"rateLimits":{"primary":{"usedPercent":0,"windowDurationMins":null,"resetsAt":null}}}),100).unwrap();
        assert_eq!(view(&model, 100).status, WindowStatus::Fresh);
        for (at, duration, reset) in [(130, 0, 10000), (160, 300, -1)] {
            model.ingest_json(&json!({"rateLimits":{"primary":{"usedPercent":0,"windowDurationMins":duration,"resetsAt":reset}}}),at).unwrap();
            assert_eq!(view(&model, at).status, WindowStatus::Unavailable);
        }
    }
    #[test]
    fn same_second_different_windows_are_merged_and_identical_payload_is_idempotent() {
        let mut model = QuotaModel::new("test");
        let primary = payload(10.0, 10000);
        let secondary = json!({"rateLimits":{"secondary":{"usedPercent":20,"windowDurationMins":10080,"resetsAt":20000}}});
        model.ingest_json(&primary, 100).unwrap();
        let duplicate = model.ingest_json(&primary, 100).unwrap();
        assert!(duplicate.ignored_duplicate);
        let merged = model.ingest_json(&secondary, 100).unwrap();
        assert!(!merged.ignored_duplicate);
        assert_eq!(model.views(100).len(), 2);
        assert!(model
            .views(100)
            .iter()
            .all(|view| view.status == WindowStatus::Fresh));
        assert!(model.history.is_empty());
    }
    #[test]
    fn same_second_conflicting_window_is_not_assigned_an_invented_order() {
        let mut model = QuotaModel::new("test");
        ingest(&mut model, 100, 10.0);
        ingest(&mut model, 130, 11.0);
        ingest(&mut model, 160, 12.0);
        let report = model.ingest_json(&payload(1.0, 10000), 160).unwrap();
        assert!(!report.ignored_duplicate);
        assert!(report.warnings[0].contains("same_time_conflict"));
        assert_eq!(model.history.len(), 2);
        assert_eq!(view(&model, 160).remaining_percent, Some(99.0));
        assert_eq!(view(&model, 160).prediction.observed_drops, 0);
        assert_eq!(view(&model, 160).epoch, 1);
    }
    #[test]
    fn history_older_than_thirty_days_is_never_automatically_deleted() {
        let mut model = QuotaModel::new("test");
        ingest(&mut model, 100, 10.0);
        ingest(&mut model, 130, 11.0);
        ingest(&mut model, 130 + 180 * 24 * 60 * 60, 11.0);
        assert_eq!(model.history.len(), 1);
        assert_eq!(model.history[0].observed_at_unix, 130);
    }
    #[test]
    fn legacy_history_without_new_optional_fields_loads() {
        let event: HistoryEvent =
            serde_json::from_value(json!({"session":"old", "observed_at_unix":1,
            "source_key":"codex", "epoch":0, "kind":"QuotaDecrease", "window_label":"weekly",
            "before":"99", "after":"98", "delta":"1"}))
            .unwrap();
        assert!(!event.partial);
        assert_eq!(event.window_key, None);
        assert_eq!(event.before_observed_at_unix, None);
    }
    #[test]
    fn sparse_history_keeps_endpoint_changes_without_cross_gap_speed() {
        let mut model = QuotaModel::new("history");
        for (at, used) in [(100, 10.0), (1000, 12.0), (2000, 15.0), (3000, 0.0)] {
            ingest(&mut model, at, used);
        }
        assert_eq!(model.history.len(), 3);
        assert!(model.history.iter().all(|event| event.partial));
        assert_eq!(model.history[2].kind, HistoryKind::QuotaReset);
        assert_eq!(model.history[2].delta, "15");
        assert!(view(&model, 3000).rate_per_hour.is_none());
    }
    #[test]
    fn temporary_missing_duration_does_not_mix_distinct_window_lengths() {
        let mut model = QuotaModel::new("test");
        ingest(&mut model, 100, 10.0);
        ingest(&mut model, 130, 11.0);
        model
            .ingest_json(&json!({"rateLimits":{"primary":{"usedPercent":12}}}), 160)
            .unwrap();
        assert_eq!(view(&model, 160).prediction.observed_drops, 2);
        model
            .ingest_json(
                &json!({"rateLimits":{"primary":{"usedPercent":13,"windowDurationMins":10080}}}),
                190,
            )
            .unwrap();
        assert_eq!(view(&model, 190).prediction.observed_drops, 0);
        assert_eq!(model.history.len(), 2);
    }
    #[test]
    fn missing_credit_balance_breaks_rate_instead_of_reusing_old_samples() {
        let mut model = QuotaModel::new("test");
        for (at, balance) in [
            (100, json!("10")),
            (130, json!("9")),
            (160, json!("8")),
            (190, Value::Null),
            (220, json!("5")),
        ] {
            model
                .ingest_json(&json!({"rateLimits":{"credits":{"balance":balance}}}), at)
                .unwrap();
        }
        assert_eq!(model.history.len(), 2);
        assert!(model.credits(220)[0].rate_per_minute.is_none());
    }
    #[test]
    fn synthetic_mixed_sequence_never_records_zero_or_reverse_signed_events() {
        let mut model = QuotaModel::new("synthetic");
        let mut used = 0.0;
        let mut at = 100;
        for i in 0..10000 {
            at += if i % 71 == 0 { 120 } else { 30 };
            if i % 101 == 0 {
                used = 0.0;
            } else if i % 3 == 0 {
                used = (used + 1.25_f64).min(100.0);
            }
            model
                .ingest_json(&payload(used, at + 10000 + (i % 7)), at)
                .unwrap();
        }
        assert!(!model.history.is_empty());
        for event in &model.history {
            let before = event.before.parse::<f64>().unwrap();
            let after = event.after.parse::<f64>().unwrap();
            let delta = event.delta.parse::<f64>().unwrap();
            assert!(delta > 0.0);
            assert!(((before - after).abs() - delta).abs() < PERCENT_EPSILON);
            if event.kind == HistoryKind::QuotaReset {
                assert!(after > before);
            }
            if event.kind == HistoryKind::QuotaDecrease {
                assert!(after < before);
            }
        }
    }
}
