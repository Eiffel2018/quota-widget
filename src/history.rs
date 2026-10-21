//! Raw historical evidence and a separate, deterministic presentation timeline.
//!
//! A JSONL timestamp is the time a cached quota snapshot was logged. Concurrent
//! sessions can repeatedly report older values. Never feed those interleaved
//! snapshots to the live predictor or call every historical rise a replenishment.
use crate::model::{
    decimal_text, historical_windows, ignored_one_point_rebound, percent_text,
    quota_deadline_rollover, HistoricalWindow, HistoryEvent, HistoryEvidence, HistoryKind,
    HistoryReconciliation, QuotaModel, CONTINUITY_GAP_SECONDS,
};
use crate::tokens::ImportedQuotaObservation;
use std::collections::{BTreeMap, BTreeSet};

const EPSILON: f64 = 0.000001;
const RESET_JITTER_SECONDS: i64 = 90;
type Window = (Option<String>, String, String, String);
type ObservationScope = (Option<String>, String, String);

fn observer(o: &ImportedQuotaObservation) -> Option<(bool, &str)> {
    o.origin_session
        .as_deref()
        .map(|id| (true, id))
        .or_else(|| o.source_id.as_deref().map(|id| (false, id)))
}

fn window(e: &HistoryEvent) -> Window {
    (
        e.evidence.as_ref().and_then(|p| p.account_id.clone()),
        e.source_key.clone(),
        e.window_key
            .clone()
            .unwrap_or_else(|| format!("legacy:{}", e.window_label)),
        // The same slot can later describe a different duration.
        e.window_label.clone(),
    )
}
/// Recovered 0.2.x live history used numeric run IDs and anonymous source-slot
/// counters. These counters are local to a run, not account/bucket identities.
/// Keep this compatibility path narrower than ordinary unannotated history.
fn legacy_slot(e: &HistoryEvent) -> Option<&str> {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    if e.evidence.is_some()
        || e.window_key.is_some()
        || e.before_observed_at_unix.is_some()
        || e.reconciliation.is_some()
        || !digits(&e.session)
    {
        return None;
    }
    let (source, slot) = e.source_key.split_once(':')?;
    (digits(source) && matches!(slot, "primary" | "secondary" | "credits")).then_some(slot)
}

/// Old live history sometimes labeled real decreases as resets when metadata
/// changed. The recorded endpoints, not that label, determine the direction.
fn legacy_direction(e: &HistoryEvent, slot: &str) -> Option<HistoryKind> {
    match (slot, e.kind) {
        ("primary" | "secondary", HistoryKind::QuotaDecrease | HistoryKind::QuotaReset) => {
            quota_values(e).map(|(before, after)| {
                if after < before {
                    HistoryKind::QuotaDecrease
                } else {
                    HistoryKind::QuotaReset
                }
            })
        }
        ("credits", HistoryKind::CreditDecrease | HistoryKind::CreditTopUp) => {
            let before = decimal_text(&e.before)?;
            let after = decimal_text(&e.after)?;
            let (before_whole, before_fraction) = before.split_once('.').unwrap_or((&before, ""));
            let (after_whole, after_fraction) = after.split_once('.').unwrap_or((&after, ""));
            // Canonical nonnegative decimal text compares exactly by whole-part
            // length, whole digits, then fractional digits (no trailing zeroes).
            match after_whole
                .len()
                .cmp(&before_whole.len())
                .then_with(|| after_whole.cmp(before_whole))
                .then_with(|| after_fraction.cmp(before_fraction))
            {
                std::cmp::Ordering::Less => Some(HistoryKind::CreditDecrease),
                std::cmp::Ordering::Greater => Some(HistoryKind::CreditTopUp),
                std::cmp::Ordering::Equal => None,
            }
        }
        _ => None,
    }
}

fn imported(e: &HistoryEvent) -> bool {
    e.evidence.as_ref().map_or_else(
        || e.session.starts_with("imported-local-observations"),
        |p| p.imported,
    )
}
fn time_ms(e: &HistoryEvent) -> i64 {
    e.evidence
        .as_ref()
        .and_then(|p| p.observed_at_ms)
        .unwrap_or_else(|| e.observed_at_unix.saturating_mul(1000))
}
fn observation_ms(o: &ImportedQuotaObservation) -> i64 {
    o.timestamp_ms
        .unwrap_or_else(|| o.timestamp.saturating_mul(1000))
}
fn quota_values(e: &HistoryEvent) -> Option<(f64, f64)> {
    let before = e.before.parse::<f64>().ok()?;
    let after = e.after.parse::<f64>().ok()?;
    ((0.0..=100.0).contains(&before)
        && (0.0..=100.0).contains(&after)
        && (after - before).abs() > EPSILON)
        .then_some((before, after))
}
/// True only for usable, nonzero quota changes. Inconsistent imported snapshots
/// are visible as uncertainty when useful, but never reset a cost accumulator.
pub fn is_confirmed_quota_change(e: &HistoryEvent) -> bool {
    e.reconciliation.is_none()
        && quota_values(e).is_some_and(|(before, after)| match e.kind {
            HistoryKind::QuotaDecrease => after < before,
            HistoryKind::QuotaReset => after > before,
            _ => false,
        })
}

#[derive(Clone)]
struct PriorObservation<'a> {
    window: HistoricalWindow,
    observation: &'a ImportedQuotaObservation,
    epoch: u64,
}

/// Reconstruct raw transitions without destroying any imported observation.
/// Each account/source/window has its own stream: a partial snapshot from a
/// different source cannot invalidate an unrelated window's baseline.
pub fn replay(observations: &[ImportedQuotaObservation]) -> Vec<HistoryEvent> {
    let mut ordered: Vec<_> = observations.iter().collect();
    ordered.sort_by(|a, b| {
        observation_ms(a)
            .cmp(&observation_ms(b))
            .then_with(|| a.origin_session.cmp(&b.origin_session))
            .then_with(|| a.payload.to_string().cmp(&b.payload.to_string()))
            .then_with(|| a.source_id.cmp(&b.source_id))
    });
    // A copied log is the same observation only with the same logical observer,
    // exact log timestamp and payload. Sorting picks stable source provenance;
    // the caller retains every original payload independently of this replay.
    ordered.dedup_by(|a, b| {
        observation_ms(a) == observation_ms(b)
            && observer(a) == observer(b)
            && a.payload == b.payload
    });
    let mut previous: BTreeMap<ObservationScope, PriorObservation<'_>> = BTreeMap::new();
    let mut observer_previous: BTreeMap<(ObservationScope, (bool, String)), PriorObservation<'_>> =
        BTreeMap::new();
    let mut result = Vec::new();
    // Credits retain the live model's exact decimal arithmetic.
    let mut credit_model = QuotaModel::new("imported-local-observations-v2");
    for o in ordered {
        let _ = credit_model.ingest_json(&o.payload, o.timestamp);
        result.extend(
            credit_model
                .history
                .drain(..)
                .filter(|e| {
                    matches!(
                        e.kind,
                        HistoryKind::CreditDecrease | HistoryKind::CreditTopUp
                    )
                })
                .map(|mut e| {
                    if let Some(p) = &mut e.evidence {
                        p.imported = true;
                    }
                    e
                }),
        );
        let Ok(windows) = historical_windows(&o.payload) else {
            continue;
        };
        for mut snapshot in windows {
            let key = (
                snapshot.account_id.clone(),
                snapshot.source_key.clone(),
                snapshot.window_key.clone(),
            );
            let mut epoch = 0;
            if let Some(old) = previous.get(&key) {
                epoch = old.epoch;
                if snapshot.duration.is_none() {
                    snapshot.duration = old.window.duration;
                    snapshot.window_label = old.window.window_label.clone();
                }
                let compatible = old.window.duration.is_none()
                    || snapshot.duration.is_none()
                    || old.window.duration == snapshot.duration;
                let before = old.window.remaining.parse::<f64>().unwrap();
                let after = snapshot.remaining.parse::<f64>().unwrap();
                if compatible && (after - before).abs() > EPSILON {
                    let kind = if after > before {
                        HistoryKind::QuotaReset
                    } else {
                        HistoryKind::QuotaDecrease
                    };
                    if kind == HistoryKind::QuotaReset {
                        epoch = epoch.saturating_add(1);
                    }
                    let same_instant = observation_ms(o) == observation_ms(old.observation);
                    // A session is an observer, not an account. If this observer
                    // independently recorded the same preceding value/cycle,
                    // preserve that real baseline instead of calling mirrored
                    // logs an observation gap merely because another observer
                    // happened to write the cached snapshot most recently.
                    let own_baseline = observer(o)
                        .and_then(|(logical, id)| {
                            observer_previous.get(&(key.clone(), (logical, id.to_owned())))
                        })
                        .filter(|own| {
                            own.window.remaining == old.window.remaining
                                && own.window.duration == old.window.duration
                                && match (own.window.reset_at, old.window.reset_at) {
                                    (Some(a), Some(b)) => {
                                        a.abs_diff(b) <= RESET_JITTER_SECONDS as u64
                                    }
                                    (None, None) => true,
                                    _ => false,
                                }
                                && o.timestamp.saturating_sub(own.observation.timestamp)
                                    <= CONTINUITY_GAP_SECONDS
                        });
                    let baseline = own_baseline.unwrap_or(old);
                    result.push(HistoryEvent {
                        session: "imported-local-observations-v2".into(),
                        observed_at_unix: o.timestamp,
                        source_key: snapshot.source_key.clone(),
                        epoch,
                        kind,
                        window_label: snapshot.window_label.clone(),
                        before: old.window.remaining.clone(),
                        after: snapshot.remaining.clone(),
                        delta: percent_text((after - before).abs()),
                        partial: o.timestamp.saturating_sub(baseline.observation.timestamp)
                            > CONTINUITY_GAP_SECONDS
                            || observer(baseline.observation) != observer(o)
                            || same_instant,
                        before_observed_at_unix: Some(baseline.observation.timestamp),
                        window_key: Some(snapshot.window_key.clone()),
                        evidence: Some(HistoryEvidence {
                            imported: true,
                            account_id: snapshot.account_id.clone(),
                            origin_session: o.origin_session.clone(),
                            before_origin_session: baseline.observation.origin_session.clone(),
                            source_id: o.source_id.clone(),
                            observed_at_ms: Some(observation_ms(o)),
                            before_observed_at_ms: Some(observation_ms(baseline.observation)),
                            before_reset_at: baseline.window.reset_at,
                            reset_at: snapshot.reset_at,
                            window_duration_mins: snapshot.duration,
                        }),
                        reconciliation: same_instant
                            .then_some(HistoryReconciliation::ConflictingObservation),
                    });
                } else if !compatible {
                    epoch = epoch.saturating_add(1);
                }
            }
            let prior = PriorObservation {
                window: snapshot,
                observation: o,
                epoch,
            };
            if let Some((logical, id)) = observer(o) {
                observer_previous.insert((key.clone(), (logical, id.to_owned())), prior.clone());
            }
            previous.insert(key, prior);
        }
    }
    result.sort_by_key(time_ms);
    result
}

/// Append distinct RAW evidence. Presentation reconciliation is intentionally
/// separate, so upgrading the app cannot erase a disputed legacy transition.
pub fn merge(existing: Vec<HistoryEvent>, incoming: Vec<HistoryEvent>) -> Vec<HistoryEvent> {
    let mut keys = BTreeSet::new();
    let mut result = Vec::with_capacity(existing.len().saturating_add(incoming.len()));
    for e in existing.into_iter().chain(incoming) {
        let key = serde_json::to_string(&e).expect("history values are serializable");
        if keys.insert(key) {
            result.push(e);
        }
    }
    result.sort_by_key(time_ms);
    result
}

struct Timeline {
    remaining: f64,
    last_at: i64,
    cycle: u64,
    reset_at: Option<i64>,
    last_visible: Option<usize>,
    seen_uncertain: BTreeSet<(String, String)>,
    seen_remaining: BTreeSet<String>,
    pending_recovery: Option<(f64, i64)>,
    // A live epoch/session is a real continuity boundary, unlike the tentative
    // epochs produced while replaying cached imported observations.
    direct_scope: Option<(String, u64)>,
    uncertain: bool,
}
fn rollover(e: &HistoryEvent, accepted_reset_at: Option<i64>) -> bool {
    e.evidence.as_ref().is_some_and(|p| {
        // An unchanged snapshot may already carry the new deadline, so the
        // immediate raw pair can lose the rollover evidence. Prefer the last
        // accepted timeline deadline. Conversely, an ignored old-cycle snapshot
        // must not make a cached current-cycle value look like a second reset.
        quota_deadline_rollover(
            accepted_reset_at.or(p.before_reset_at),
            p.reset_at,
            e.observed_at_unix,
        )
    })
}
fn reset_for(e: &HistoryEvent) -> Option<i64> {
    e.evidence.as_ref().and_then(|p| p.reset_at)
}
fn trust(e: &HistoryEvent) -> u8 {
    if !imported(e) {
        2
    } else if e.evidence.is_some() {
        1
    } else {
        0
    }
}

/// A read-only derived view. Original events and all imported payloads are left
/// untouched. An unsupported imported increase is uncertainty, not proof of a
/// reset. Repeated old snapshots cannot create repeated new cost intervals.
pub fn normalized(raw: &[HistoryEvent]) -> Vec<HistoryEvent> {
    let mut ordered: Vec<_> = raw.iter().collect();
    ordered.sort_by_cached_key(|e| {
        (
            time_ms(e),
            std::cmp::Reverse(trust(e)),
            window(e),
            e.before.clone(),
            e.after.clone(),
            // Equal observations from overlapping imports must choose the same
            // provenance regardless of file/import discovery order.
            serde_json::to_string(e).expect("history values are serializable"),
        )
    });
    let mut timelines: BTreeMap<Window, Timeline> = BTreeMap::new();
    let mut result: Vec<HistoryEvent> = Vec::new();
    let mut legacy_cycles: BTreeMap<(String, String, String), u64> = BTreeMap::new();
    let mut legacy_rebound_anchors: BTreeMap<(String, String, String), f64> = BTreeMap::new();
    // A recorded zero-change reset cannot create a cost boundary indirectly by
    // lending its bogus epoch number to the next nonzero live observation.
    let mut zero_reset_epochs: BTreeMap<(Window, String, u64), u64> = BTreeMap::new();
    // Credits do not share a quota reset cycle. Preserve exact string precision.
    let mut credit_endpoints: BTreeMap<Window, String> = BTreeMap::new();
    for original in ordered {
        let mut event = original.clone();
        if let Some(slot) = legacy_slot(original) {
            let Some(kind) = legacy_direction(original, slot) else {
                continue;
            };
            event.kind = kind;
            if matches!(kind, HistoryKind::QuotaDecrease | HistoryKind::QuotaReset) {
                let key = (
                    event.session.clone(),
                    event.source_key.clone(),
                    event.window_label.clone(),
                );
                let (before, after) =
                    quota_values(&event).expect("legacy direction validated quota endpoints");
                let rebound_anchor = legacy_rebound_anchors.get(&key).copied();
                let anchor = rebound_anchor.unwrap_or(before);
                if ignored_one_point_rebound(anchor, after, None, None, event.observed_at_unix)
                    && (kind == HistoryKind::QuotaReset || rebound_anchor.is_some())
                {
                    legacy_rebound_anchors.insert(key, anchor);
                    continue;
                }
                if let Some(anchor) = rebound_anchor {
                    if (after - anchor).abs() <= EPSILON {
                        continue;
                    }
                    if after < anchor && before > anchor {
                        event.before = percent_text(anchor);
                        event.delta = percent_text(anchor - after);
                    }
                    legacy_rebound_anchors.remove(&key);
                }
                let cycle = legacy_cycles
                    .entry((
                        event.session.clone(),
                        event.source_key.clone(),
                        event.window_label.clone(),
                    ))
                    .or_default();
                if kind == HistoryKind::QuotaReset {
                    *cycle = cycle.saturating_add(1);
                }
                event.epoch = *cycle;
            }
            // Each row is an old live observation. Missing slots, anonymous
            // source reuse, and other runs cannot justify rewriting its before
            // value or suppressing it as an imported echo. Preserve all recorded
            // fields except the derived direction and meaningful quota cycle.
            result.push(event);
            continue;
        }
        if matches!(
            event.kind,
            HistoryKind::CreditDecrease | HistoryKind::CreditTopUp
        ) {
            let Some(before) = decimal_text(&event.before) else {
                continue;
            };
            let Some(after) = decimal_text(&event.after) else {
                continue;
            };
            if before == after || decimal_text(&event.delta).as_deref() == Some("0") {
                continue;
            }
            let key = window(&event);
            if credit_endpoints.get(&key) == Some(&after) {
                continue;
            }
            credit_endpoints.insert(key, after.clone());
            event.before = before;
            event.after = after;
            result.push(event);
            continue;
        }
        let Some((before, after)) = quota_values(&event) else {
            if !imported(&event) && event.kind == HistoryKind::QuotaReset {
                if let (Ok(before), Ok(after), Some(state)) = (
                    event.before.parse::<f64>(),
                    event.after.parse::<f64>(),
                    timelines.get(&window(&event)),
                ) {
                    if (0.0..=100.0).contains(&before)
                        && (after - before).abs() <= EPSILON
                        && (after - state.remaining).abs() <= EPSILON
                    {
                        if let Some((session, epoch)) = &state.direct_scope {
                            if session == &event.session {
                                zero_reset_epochs.insert(
                                    (window(&event), event.session.clone(), event.epoch),
                                    *epoch,
                                );
                            }
                        }
                    }
                }
            }
            continue;
        };
        if !matches!(
            event.kind,
            HistoryKind::QuotaDecrease | HistoryKind::QuotaReset
        ) {
            continue;
        }
        // Outside the verified legacy shape, malformed direction labels are
        // not evidence of a usable transition. Retain them only in raw history.
        if (event.kind == HistoryKind::QuotaDecrease) != (after < before) {
            continue;
        }
        let is_imported = imported(&event);
        let accepted = timelines.get(&window(&event));
        let accepted_remaining = accepted.map_or(before, |state| state.remaining);
        let accepted_reset = accepted
            .and_then(|state| state.reset_at)
            .or_else(|| event.evidence.as_ref().and_then(|p| p.before_reset_at));
        if event.kind == HistoryKind::QuotaReset
            && ignored_one_point_rebound(
                accepted_remaining,
                after,
                accepted_reset,
                reset_for(&event),
                event.observed_at_unix,
            )
        {
            // Persisted events remain untouched. The derived timeline treats
            // 86→87→86 as 86→86→86, with no uncertainty/cost reset of its own.
            if !is_imported {
                if let Some((session, epoch)) =
                    accepted.and_then(|state| state.direct_scope.as_ref())
                {
                    if session == &event.session {
                        zero_reset_epochs
                            .insert((window(&event), event.session.clone(), event.epoch), *epoch);
                    }
                }
            }
            timelines.entry(window(&event)).or_insert_with(|| Timeline {
                remaining: before,
                last_at: event
                    .before_observed_at_unix
                    .unwrap_or(event.observed_at_unix),
                cycle: 0,
                reset_at: accepted_reset,
                last_visible: None,
                seen_uncertain: BTreeSet::new(),
                seen_remaining: BTreeSet::from([percent_text(before)]),
                pending_recovery: None,
                direct_scope: (!is_imported).then(|| (event.session.clone(), event.epoch)),
                uncertain: false,
            });
            continue;
        }
        let direct_epoch = zero_reset_epochs
            .get(&(window(&event), event.session.clone(), event.epoch))
            .copied()
            .unwrap_or(event.epoch);
        let direct_scope = (!is_imported).then(|| (event.session.clone(), direct_epoch));
        let boundary = timelines.get(&window(&event)).is_some_and(|state| {
            let changed_scope = direct_scope
                .as_ref()
                .zip(state.direct_scope.as_ref())
                .is_some_and(|(new, old)| new != old);
            let corroborated_import = state.last_visible.is_some_and(|index| {
                let prior = &result[index];
                imported(prior)
                    && prior.kind == event.kind
                    && prior.before == event.before
                    && prior.after == event.after
                    && event
                        .observed_at_unix
                        .saturating_sub(prior.observed_at_unix)
                        <= CONTINUITY_GAP_SECONDS
            });
            // A new live epoch can be the direct observation of the very same
            // rollover just reconstructed from logs, rather than another cycle.
            changed_scope && !corroborated_import
        });
        if boundary {
            timelines.remove(&window(&event));
        }
        let state = timelines.entry(window(&event)).or_insert_with(|| Timeline {
            remaining: before,
            last_at: event
                .before_observed_at_unix
                .unwrap_or(event.observed_at_unix),
            cycle: if is_imported { 0 } else { direct_epoch },
            reset_at: event.evidence.as_ref().and_then(|p| p.before_reset_at),
            last_visible: None,
            seen_uncertain: BTreeSet::new(),
            seen_remaining: BTreeSet::from([percent_text(before)]),
            pending_recovery: None,
            direct_scope: direct_scope.clone(),
            uncertain: false,
        });
        if direct_scope.is_some() {
            state.direct_scope = direct_scope;
        }
        event.epoch = state.cycle;
        // A late log of a previous, already-expired cycle cannot spend most of a
        // newly reset window again. Metadata jitter within a cycle is harmless.
        if is_imported
            && reset_for(&event)
                .zip(state.reset_at)
                .is_some_and(|(incoming, current)| {
                    incoming <= event.observed_at_unix
                        && current.saturating_sub(incoming) > RESET_JITTER_SECONDS
                })
        {
            continue;
        }
        if event.reconciliation == Some(HistoryReconciliation::ConflictingObservation) {
            if state
                .seen_uncertain
                .insert((event.before.clone(), event.after.clone()))
            {
                event.partial = true;
                result.push(event);
            }
            if !is_imported {
                state.remaining = after;
                state.pending_recovery = None;
                state.last_at = original.observed_at_unix;
                state.last_visible = None;
                state.seen_remaining.insert(percent_text(after));
            }
            state.uncertain = true;
            continue;
        }
        // Only an actual raw rise with rollover evidence can establish a new
        // cycle at an endpoint already seen in the old cycle. Equal raw values
        // were excluded above, so metadata-only changes never create cycles.
        let deadline_rollover =
            event.kind == HistoryKind::QuotaReset && rollover(&event, state.reset_at);
        let real_reset =
            event.kind == HistoryKind::QuotaReset && (!is_imported || deadline_rollover);
        // The same endpoint from another session/read is not another change.
        if (after - state.remaining).abs() <= EPSILON && !deadline_rollover {
            if let Some(index) = state.last_visible {
                let previous = &result[index];
                if previous.kind == event.kind
                    && previous.before == event.before
                    && previous.after == event.after
                    && event
                        .observed_at_unix
                        .saturating_sub(previous.observed_at_unix)
                        <= 90
                    && trust(&event) > trust(previous)
                {
                    // Prefer the direct live observation, but disclose the timing
                    // disagreement rather than invent an exact shared instant.
                    event.partial |=
                        previous.partial || event.observed_at_unix != previous.observed_at_unix;
                    event.epoch = previous.epoch;
                    state.last_at = event.observed_at_unix;
                    result[index] = event;
                }
            }
            continue;
        }
        if after > state.remaining + EPSILON
            && !is_imported
            && event.kind == HistoryKind::QuotaDecrease
        {
            // A direct read disagrees with an imported baseline. Preserve the
            // conflict instead of relabeling a live decrease as replenishment,
            // and let the direct endpoint seed subsequent live comparisons.
            if let Some(index) = state.last_visible {
                if imported(&result[index]) {
                    result[index].reconciliation =
                        Some(HistoryReconciliation::ConflictingObservation);
                    result[index].partial = true;
                }
            }
            event.reconciliation = Some(HistoryReconciliation::ConflictingObservation);
            event.partial = true;
            state.remaining = after;
            state.pending_recovery = None;
            state.last_at = event.observed_at_unix;
            state.last_visible = None;
            state.seen_remaining.insert(percent_text(after));
            state.uncertain = true;
            result.push(event);
            continue;
        }
        if after > state.remaining + EPSILON && !real_reset {
            // Keep one visible uncertainty per repeated pair. It establishes no
            // new cycle or accounting boundary and cannot make the next echo a drop.
            if state
                .seen_uncertain
                .insert((percent_text(state.remaining), percent_text(after)))
            {
                // The conservative accounting anchor can sit below a disputed
                // trajectory. Keep the observed direction and both endpoints:
                // a raw 100→99 decrease is not an observed 0→99 replenishment.
                event.partial = true;
                event.reconciliation = Some(HistoryReconciliation::UncertainRecovery);
                result.push(event);
            }
            state.uncertain = true;
            // A never-before-seen endpoint might be a genuine recovery with
            // missing metadata. Preserve its subsequent trajectory as partial,
            // while repeats of an already-seen old value remain replay echoes.
            if state.seen_remaining.insert(percent_text(after)) {
                state
                    .pending_recovery
                    .get_or_insert((state.remaining, state.last_at));
                state.remaining = after;
                state.last_at = original.observed_at_unix;
                state.last_visible = None;
                state.reset_at = reset_for(original).or(state.reset_at);
            }
            continue;
        }
        if is_imported && !real_reset {
            if let Some((anchor, anchor_at)) = state.pending_recovery {
                if after <= anchor + EPSILON {
                    // An unsupported rise immediately returning to its own
                    // previous anchor is an echo, not new spend. If the new
                    // value is lower, charge only the drop below that anchor.
                    // Do not globally deduplicate values from earlier cycles.
                    state.remaining = anchor;
                    state.last_at = anchor_at;
                    state.pending_recovery = None;
                    state.last_visible = None;
                    if (after - anchor).abs() <= EPSILON {
                        if state
                            .seen_uncertain
                            .insert((event.before.clone(), event.after.clone()))
                        {
                            event.partial = true;
                            event.reconciliation = Some(HistoryReconciliation::UncertainRecovery);
                            result.push(event);
                        }
                        state.uncertain = true;
                        continue;
                    }
                }
            }
        }
        if real_reset {
            // The first event in a live scope may already carry its incremented
            // epoch. Retain that identity without incrementing it a second time.
            if is_imported || state.last_visible.is_some() || event.epoch == 0 {
                state.cycle = state.cycle.saturating_add(1);
            }
            state.seen_uncertain.clear();
            state.seen_remaining.clear();
            state.pending_recovery = None;
            state.uncertain = false;
            event.epoch = state.cycle;
        } else if after < state.remaining - EPSILON {
            // Rebase a drop that followed an ignored stale snapshot onto the
            // last defensible endpoint, never charge the replayed part twice.
            if (before - state.remaining).abs() > EPSILON {
                event.before = percent_text(state.remaining);
                event.delta = percent_text(state.remaining - after);
                event.before_observed_at_unix = Some(state.last_at);
                event.partial = true;
            }
            event.kind = HistoryKind::QuotaDecrease;
            event.partial |= state.uncertain;
            // This accepted endpoint is a new defensible anchor. A dispute in
            // an earlier interval must not poison every future interval forever.
            state.uncertain = false;
            state.pending_recovery = None;
        } else {
            continue;
        }
        state.remaining = after;
        state.seen_remaining.insert(percent_text(after));
        state.last_at = event.observed_at_unix;
        state.reset_at = reset_for(&event).or(state.reset_at);
        state.last_visible = Some(result.len());
        result.push(event);
    }
    result.sort_by_key(time_ms);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn e(t: i64, k: HistoryKind, b: &str, a: &str) -> HistoryEvent {
        HistoryEvent {
            session: "s".into(),
            observed_at_unix: t,
            source_key: "codex".into(),
            epoch: 0,
            kind: k,
            window_label: "week".into(),
            before: b.into(),
            after: a.into(),
            delta: "1".into(),
            partial: false,
            before_observed_at_unix: None,
            window_key: Some("primary".into()),
            evidence: None,
            reconciliation: None,
        }
    }
    #[test]
    fn duplicate_reset_can_merge_but_repeated_true_reset_is_retained() {
        let reset = e(10, HistoryKind::QuotaReset, "98", "100");
        let copy = e(15, HistoryKind::QuotaReset, "98", "100");
        let raw = merge(vec![reset.clone()], vec![copy]);
        assert_eq!(raw.len(), 2, "retain separate observation evidence");
        assert_eq!(normalized(&raw).len(), 1);
        let drop = e(20, HistoryKind::QuotaDecrease, "100", "98");
        let refill = e(30, HistoryKind::QuotaReset, "98", "100");
        assert_eq!(normalized(&merge(vec![reset, drop], vec![refill])).len(), 3);
    }
    #[test]
    fn same_transition_after_reset_is_not_erased() {
        let first = e(10, HistoryKind::QuotaDecrease, "100", "98");
        let reset = e(20, HistoryKind::QuotaReset, "98", "100");
        let second = e(30, HistoryKind::QuotaDecrease, "100", "98");
        assert_eq!(
            normalized(&merge(vec![first, reset], vec![second])).len(),
            3
        );
    }
    #[test]
    fn log_and_live_duplicate_are_merged_only_in_presentation() {
        let first = e(10, HistoryKind::QuotaDecrease, "100", "98");
        let second = e(20, HistoryKind::QuotaDecrease, "100", "98");
        let raw = merge(vec![first], vec![second]);
        assert_eq!(raw.len(), 2);
        assert_eq!(normalized(&raw).len(), 1);
    }
}
