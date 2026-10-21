//! Reversible, bounded display aggregates over normalized usage events.
//!
//! The database owner supplies already-repriced, policy-adjusted events and the
//! exact old/new rows from a commit. These caches retain totals, not event history.
//! Add each row once and remove the same row once; changing an event is an atomic
//! remove/add at the caller's commit boundary. Rebuild after a global policy or
//! pricing change. Saturation happens only when materializing a public view.

use crate::{
    accounting::ObservedCost,
    tokens::{TokenEvent, TokenSummary},
    usage_views::coverage_incomplete,
};
use std::collections::BTreeMap;

/// Neumaier compensation retains small prices when large prices are added and
/// subsequently removed. All prices have already passed the pricing policy.
#[derive(Debug, Clone, Copy, Default)]
struct CostSum {
    sum: f64,
    correction: f64,
}

impl CostSum {
    fn add(&mut self, value: f64) {
        let next = self.sum + value;
        self.correction += if self.sum.abs() >= value.abs() {
            (self.sum - next) + value
        } else {
            (value - next) + self.sum
        };
        self.sum = next;
    }

    fn value(&self) -> f64 {
        // True prices are nonnegative. Signed removal can leave a tiny negative
        // rounding residual, which must never become a negative display price.
        (self.sum + self.correction).max(0.0)
    }
}

fn tokens(value: u128) -> u64 {
    value.min(u64::MAX as u128) as u64
}

fn count(value: u128) -> usize {
    value.min(usize::MAX as u128) as usize
}

/// Exact, reversible token and status totals with a compensated known-cost sum.
///
/// No exclusion or timestamp filtering is performed here. This also supports an
/// explicit excluded-review audit view. Reasoning remains a subset of output.
/// A known zero price is priced; missing prices and coverage gaps are independent.
#[derive(Debug, Clone, Default)]
pub struct SummaryAccumulator {
    events: u128,
    input: u128,
    cached: u128,
    uncached: u128,
    output: u128,
    reasoning: u128,
    partial: u128,
    assumed: u128,
    unpriced: u128,
    nonzero_prices: u128,
    coverage: u128,
    cost: CostSum,
}

impl SummaryAccumulator {
    pub fn add(&mut self, event: &TokenEvent) {
        self.events += 1;
        self.input += event.input as u128;
        self.cached += event.cached as u128;
        self.uncached += event.uncached as u128;
        self.output += event.output as u128;
        self.reasoning += event.reasoning as u128;
        self.partial += u128::from(event.is_partial());
        self.assumed += u128::from(event.model_assumed);
        self.coverage += u128::from(coverage_incomplete(event));
        match event.cost_usd {
            Some(value) => {
                self.nonzero_prices += u128::from(value != 0.0);
                self.cost.add(value);
            }
            None => self.unpriced += 1,
        }
    }

    /// The supplied row must have previously been added, without an intervening
    /// removal. An updated row must be removed using its old field values.
    pub fn remove(&mut self, event: &TokenEvent) {
        self.events = self.events.checked_sub(1).expect("remove absent event");
        self.input = self
            .input
            .checked_sub(event.input as u128)
            .expect("old input");
        self.cached = self
            .cached
            .checked_sub(event.cached as u128)
            .expect("old cached");
        self.uncached = self
            .uncached
            .checked_sub(event.uncached as u128)
            .expect("old uncached");
        self.output = self
            .output
            .checked_sub(event.output as u128)
            .expect("old output");
        self.reasoning = self
            .reasoning
            .checked_sub(event.reasoning as u128)
            .expect("old reasoning");
        self.partial = self
            .partial
            .checked_sub(u128::from(event.is_partial()))
            .expect("old partial");
        self.assumed = self
            .assumed
            .checked_sub(u128::from(event.model_assumed))
            .expect("old assumed");
        self.coverage = self
            .coverage
            .checked_sub(u128::from(coverage_incomplete(event)))
            .expect("old coverage");
        match event.cost_usd {
            Some(value) => {
                self.nonzero_prices = self
                    .nonzero_prices
                    .checked_sub(u128::from(value != 0.0))
                    .expect("old nonzero price");
                self.cost.add(-value);
            }
            None => self.unpriced = self.unpriced.checked_sub(1).expect("old unpriced"),
        }
        // All-zero known-price partitions are exactly zero, even after long runs
        // of fractional additions/removals. Unknown-only scopes stay distinct.
        if self.nonzero_prices == 0 {
            self.cost = CostSum::default();
        }
    }

    pub fn adjust(&mut self, old: Option<&TokenEvent>, new: Option<&TokenEvent>) {
        if let Some(event) = old {
            self.remove(event);
        }
        if let Some(event) = new {
            self.add(event);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.events == 0
    }

    pub fn coverage_events(&self) -> usize {
        count(self.coverage)
    }

    pub fn summary(&self) -> TokenSummary {
        let known = self.cost.value();
        TokenSummary {
            event_count: count(self.events),
            input: tokens(self.input),
            cached: tokens(self.cached),
            uncached: tokens(self.uncached),
            output: tokens(self.output),
            reasoning: tokens(self.reasoning),
            total_tokens: tokens(self.input.saturating_add(self.output)),
            partial_events: count(self.partial),
            assumed_model_events: count(self.assumed),
            unpriced_events: count(self.unpriced),
            cost_usd: (self.unpriced == 0 && self.coverage == 0).then_some(known),
            priced_cost_usd: known,
        }
    }

    /// `extra_partial` covers evidence outside the selected events, such as a
    /// missing quota boundary or history that begins after that boundary.
    pub fn observed(&self, extra_partial: bool) -> ObservedCost {
        ObservedCost {
            known_usd: self.cost.value(),
            unknown_events: count(self.unpriced),
            events: count(self.events),
            partial: extra_partial || self.unpriced > 0 || self.partial > 0 || self.coverage > 0,
        }
    }
}

/// `boundary` distinguishes a confirmed boundary from the same fallback start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeKey {
    pub start: i64,
    pub end: i64,
    pub boundary: Option<i64>,
}

/// Current main-widget costs, retaining one aggregate per included logical task.
#[derive(Debug, Clone)]
pub struct RangeCache {
    key: RangeKey,
    total: SummaryAccumulator,
    tasks: BTreeMap<String, SummaryAccumulator>,
}

impl RangeCache {
    pub fn new(key: RangeKey) -> Self {
        Self {
            key,
            total: SummaryAccumulator::default(),
            tasks: BTreeMap::new(),
        }
    }

    pub fn key(&self) -> RangeKey {
        self.key
    }

    fn includes(&self, event: &TokenEvent) -> bool {
        !event.is_excluded_auto_review()
            && event.timestamp > self.key.start
            && event.timestamp <= self.key.end
    }

    pub fn add(&mut self, event: &TokenEvent) {
        if self.includes(event) {
            self.total.add(event);
            self.tasks
                .entry(event.session.clone())
                .or_default()
                .add(event);
        }
    }

    pub fn remove(&mut self, event: &TokenEvent) {
        if self.includes(event) {
            self.total.remove(event);
            let task = self
                .tasks
                .get_mut(&event.session)
                .expect("remove absent task");
            task.remove(event);
            if task.is_empty() {
                self.tasks.remove(&event.session);
            }
        }
    }

    pub fn adjust(&mut self, old: Option<&TokenEvent>, new: Option<&TokenEvent>) {
        if let Some(event) = old {
            self.remove(event);
        }
        if let Some(event) = new {
            self.add(event);
        }
    }

    /// Advances only the key; the caller must add newly admitted (old_end, end]
    /// rows. Apply commit changes against the old key before extending and query
    /// the new tail afterward, so a newly committed tail row is not added twice.
    /// A shrinking end, changed start, or changed boundary requires a rebuild.
    pub fn extend_end(&mut self, end: i64) -> bool {
        if end < self.key.end {
            return false;
        }
        self.key.end = end;
        true
    }

    pub fn observed(&self, extra_partial: bool) -> ObservedCost {
        self.total.observed(extra_partial)
    }

    pub fn task_observed(&self, task: &str) -> ObservedCost {
        self.tasks
            .get(task)
            .map_or_else(ObservedCost::default, |sum| sum.observed(false))
    }

    pub fn task_count(&self) -> usize {
        self.tasks.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DayKey {
    pub start: i64,
    pub end: i64,
    /// False selects included rows; true selects only excluded review rows.
    pub excluded: bool,
}

/// Whole-day totals independent of grouping, row pagination, and database revision.
/// Response pages can be fetched with bounded SQL without re-reading the day.
#[derive(Debug, Clone)]
pub struct DayCache {
    key: DayKey,
    total: SummaryAccumulator,
}

impl DayCache {
    pub fn new(key: DayKey) -> Self {
        Self {
            key,
            total: SummaryAccumulator::default(),
        }
    }

    pub fn key(&self) -> DayKey {
        self.key
    }

    fn includes(&self, event: &TokenEvent) -> bool {
        event.is_excluded_auto_review() == self.key.excluded
            && event.timestamp >= self.key.start
            && event.timestamp < self.key.end
    }

    pub fn add(&mut self, event: &TokenEvent) {
        if self.includes(event) {
            self.total.add(event);
        }
    }

    pub fn remove(&mut self, event: &TokenEvent) {
        if self.includes(event) {
            self.total.remove(event);
        }
    }

    pub fn adjust(&mut self, old: Option<&TokenEvent>, new: Option<&TokenEvent>) {
        if let Some(event) = old {
            self.remove(event);
        }
        if let Some(event) = new {
            self.add(event);
        }
    }

    pub fn summary(&self) -> TokenSummary {
        self.total.summary()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: usize) -> TokenEvent {
        TokenEvent {
            id: format!("event-{id}"),
            timestamp: id as i64,
            model: "synthetic".into(),
            model_assumed: false,
            reasoning_effort: None,
            session: format!("task-{}", id % 3),
            response_id: Some(format!("response-{id}")),
            turn_id: Some(format!("turn-{}", id / 4)),
            source_kind: Some("token_usage_record".into()),
            input: 11,
            cached: 3,
            uncached: 8,
            output: 7,
            reasoning: 5,
            cost_usd: Some(0.125),
            coverage_start_timestamp_ms: None,
            partial_flags: vec![],
        }
    }

    fn full_summary<'a>(events: impl Iterator<Item = &'a TokenEvent>) -> TokenSummary {
        let mut result = TokenSummary::default();
        let mut gap = false;
        for event in events {
            result.event_count += 1;
            result.input = result.input.saturating_add(event.input);
            result.cached = result.cached.saturating_add(event.cached);
            result.uncached = result.uncached.saturating_add(event.uncached);
            result.output = result.output.saturating_add(event.output);
            result.reasoning = result.reasoning.saturating_add(event.reasoning);
            result.partial_events += usize::from(event.is_partial());
            result.assumed_model_events += usize::from(event.model_assumed);
            result.unpriced_events += usize::from(event.cost_usd.is_none());
            result.priced_cost_usd += event.cost_usd.unwrap_or(0.0);
            gap |= coverage_incomplete(event);
        }
        result.total_tokens = result.input.saturating_add(result.output);
        result.cost_usd = (result.unpriced_events == 0 && !gap).then_some(result.priced_cost_usd);
        result
    }

    fn close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() <= 1e-10 * expected.abs().max(1.0),
            "{actual} != {expected}"
        );
    }

    fn assert_summary(actual: TokenSummary, expected: TokenSummary) {
        close(actual.priced_cost_usd, expected.priced_cost_usd);
        assert_eq!(actual.cost_usd.is_some(), expected.cost_usd.is_some());
        if let (Some(a), Some(b)) = (actual.cost_usd, expected.cost_usd) {
            close(a, b);
        }
        assert_eq!(actual.event_count, expected.event_count);
        assert_eq!(actual.input, expected.input);
        assert_eq!(actual.cached, expected.cached);
        assert_eq!(actual.uncached, expected.uncached);
        assert_eq!(actual.output, expected.output);
        assert_eq!(actual.reasoning, expected.reasoning);
        assert_eq!(actual.total_tokens, expected.total_tokens);
        assert_eq!(actual.partial_events, expected.partial_events);
        assert_eq!(actual.assumed_model_events, expected.assumed_model_events);
        assert_eq!(actual.unpriced_events, expected.unpriced_events);
    }

    fn assert_observed(actual: ObservedCost, expected: TokenSummary, extra_partial: bool) {
        close(actual.known_usd, expected.priced_cost_usd);
        assert_eq!(actual.events, expected.event_count);
        assert_eq!(actual.unknown_events, expected.unpriced_events);
        assert_eq!(
            actual.partial,
            extra_partial || expected.unpriced_events > 0 || expected.partial_events > 0
        );
    }

    #[test]
    fn saturated_tokens_and_counts_are_reversible() {
        let mut a = event(1);
        a.input = u64::MAX;
        a.cached = u64::MAX;
        a.uncached = u64::MAX;
        a.output = u64::MAX;
        a.reasoning = u64::MAX;
        let b = event(2);
        let mut sum = SummaryAccumulator::default();
        sum.add(&a);
        sum.add(&b);
        assert_eq!(sum.summary().input, u64::MAX);
        sum.remove(&a);
        assert_summary(sum.summary(), full_summary(std::iter::once(&b)));
        assert_eq!(sum.summary().total_tokens, 18); // reasoning is not added again.

        // Simulate a count beyond the public type without allocating usize::MAX
        // events. All affected counters must recover after a saturated display.
        let mut e = event(3);
        e.input = 0;
        e.cached = 0;
        e.uncached = 0;
        e.output = 0;
        e.reasoning = 0;
        e.cost_usd = None;
        e.model_assumed = true;
        e.partial_flags = vec!["response_coverage_gap".into()];
        let n = usize::MAX as u128 + 1;
        let mut huge = SummaryAccumulator {
            events: n,
            partial: n,
            assumed: n,
            unpriced: n,
            coverage: n,
            ..Default::default()
        };
        assert_eq!(huge.summary().event_count, usize::MAX);
        huge.remove(&e);
        huge.remove(&e);
        let shown = huge.summary();
        assert_eq!(shown.event_count, usize::MAX - 1);
        assert_eq!(shown.partial_events, usize::MAX - 1);
        assert_eq!(shown.assumed_model_events, usize::MAX - 1);
        assert_eq!(shown.unpriced_events, usize::MAX - 1);
        assert_eq!(huge.coverage_events(), usize::MAX - 1);
    }

    #[test]
    fn known_zero_unknown_and_coverage_are_independent_and_removable() {
        let mut zero = event(1);
        zero.cost_usd = Some(0.0);
        let mut unknown = event(2);
        unknown.cost_usd = None;
        let mut gap = event(3);
        gap.partial_flags.push("response_coverage_gap".into());
        let mut sum = SummaryAccumulator::default();
        sum.add(&zero);
        assert_eq!(sum.summary().cost_usd, Some(0.0));
        assert!(!sum.observed(false).partial);
        sum.add(&unknown);
        assert_eq!(sum.summary().cost_usd, None);
        assert_eq!(sum.summary().priced_cost_usd, 0.0);
        sum.remove(&unknown);
        sum.add(&gap);
        assert_eq!(sum.summary().cost_usd, None);
        assert_eq!(sum.summary().unpriced_events, 0);
        assert_eq!(sum.coverage_events(), 1);
        sum.remove(&gap);
        assert_eq!(sum.summary().cost_usd, Some(0.0));
        sum.remove(&zero);
        assert!(sum.is_empty());
        assert_eq!(sum.summary().cost_usd, Some(0.0));
    }

    #[test]
    fn prices_preserve_small_known_values_after_large_removal() {
        let mut large = event(1);
        large.cost_usd = Some(1e16);
        let mut small = event(2);
        small.cost_usd = Some(0.1);
        let mut sum = SummaryAccumulator::default();
        sum.add(&large);
        sum.add(&small);
        sum.remove(&large);
        close(sum.summary().priced_cost_usd, 0.1);
        sum.remove(&small);
        assert_eq!(sum.summary().priced_cost_usd, 0.0);

        let mut zero = event(3);
        zero.cost_usd = Some(0.0);
        sum.add(&zero);
        for id in 0..1000 {
            let mut fraction = event(id);
            fraction.cost_usd = Some((id % 13) as f64 / 1000.0);
            sum.add(&fraction);
            sum.add(&small);
            sum.remove(&fraction);
            sum.remove(&small);
        }
        assert_eq!(sum.summary().cost_usd, Some(0.0));
    }

    #[test]
    fn range_extension_boundary_and_partition_transitions() {
        let mut range = RangeCache::new(RangeKey {
            start: 10,
            end: 20,
            boundary: Some(10),
        });
        let mut day = DayCache::new(DayKey {
            start: 10,
            end: 20,
            excluded: false,
        });
        let mut excluded = DayCache::new(DayKey {
            start: 10,
            end: 20,
            excluded: true,
        });
        let mut rows: Vec<_> = (9..=21).map(event).collect();
        for e in &rows {
            range.add(e);
            day.add(e);
            excluded.add(e);
        }
        assert_eq!(range.observed(false).events, 10);
        assert_eq!(day.summary().event_count, 10);
        assert_eq!(excluded.summary().event_count, 0);
        let old = rows[6].clone(); // timestamp 15, moves from included to audit.
        rows[6]
            .partial_flags
            .push("excluded_recorded_codex_auto_review".into());
        rows[6].session = "changed-task".into();
        range.adjust(Some(&old), Some(&rows[6]));
        day.adjust(Some(&old), Some(&rows[6]));
        excluded.adjust(Some(&old), Some(&rows[6]));
        assert_eq!(range.observed(false).events, 9);
        assert_eq!(range.task_observed("changed-task").events, 0);
        assert_eq!(day.summary().event_count, 9);
        assert_eq!(excluded.summary().event_count, 1);
        assert_eq!(excluded.summary().partial_events, 0);
        assert!(range.extend_end(21));
        range.add(rows.last().unwrap());
        assert_eq!(range.observed(false).events, 10);
        assert!(!range.extend_end(20));
        assert_eq!(range.key().end, 21);
        assert!(range.observed(true).partial);
        for e in &rows {
            range.remove(e);
        }
        assert_eq!(range.task_count(), 0);
        assert_eq!(range.observed(false), ObservedCost::default());
    }

    #[test]
    fn randomized_commits_match_independent_full_reducers() {
        // Deterministic PRNG keeps this test dependency-free and reproducible.
        fn random(state: &mut u64) -> u64 {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        }
        fn random_event(id: usize, seed: &mut u64) -> TokenEvent {
            let mut e = event(id);
            e.timestamp = (random(seed) % 180) as i64 - 30;
            e.session = format!("task-{}", random(seed) % 8);
            e.model = format!("synthetic-{}", random(seed) % 4);
            e.model_assumed = random(seed).is_multiple_of(3);
            e.input = if random(seed).is_multiple_of(7) {
                u64::MAX
            } else {
                random(seed) % 5000
            };
            e.cached = e.input / 3;
            e.uncached = e.input - e.cached;
            e.output = if random(seed).is_multiple_of(9) {
                u64::MAX
            } else {
                random(seed) % 500
            };
            e.reasoning = e.output / 2;
            e.cost_usd = match random(seed) % 5 {
                0 => None,
                1 => Some(0.0),
                _ => Some((random(seed) % 1000) as f64 / 10000.0),
            };
            for flag in [
                "response_coverage_gap",
                "mixed_schema_legacy_overlap_suppressed",
                "synthetic_partial",
                "excluded_recorded_codex_auto_review",
            ] {
                if random(seed).is_multiple_of(8) {
                    e.partial_flags.push(flag.into());
                }
            }
            e
        }
        let mut seed = 0xa71f_ee83_12c4_9081;
        let mut rows = BTreeMap::<usize, TokenEvent>::new();
        let mut total = SummaryAccumulator::default();
        let key = RangeKey {
            start: 10,
            end: 90,
            boundary: Some(10),
        };
        let mut range = RangeCache::new(key);
        let mut day = DayCache::new(DayKey {
            start: 0,
            end: 100,
            excluded: false,
        });
        let mut audit = DayCache::new(DayKey {
            start: 0,
            end: 100,
            excluded: true,
        });
        for _ in 0..6000 {
            let id = (random(&mut seed) % 96) as usize;
            let old = rows.remove(&id);
            let new = (!random(&mut seed).is_multiple_of(4)).then(|| random_event(id, &mut seed));
            total.adjust(old.as_ref(), new.as_ref());
            range.adjust(old.as_ref(), new.as_ref());
            day.adjust(old.as_ref(), new.as_ref());
            audit.adjust(old.as_ref(), new.as_ref());
            if let Some(e) = new {
                rows.insert(id, e);
            }
            assert_summary(total.summary(), full_summary(rows.values()));
            assert_summary(
                day.summary(),
                full_summary(rows.values().filter(|e| {
                    e.timestamp >= 0 && e.timestamp < 100 && !e.is_excluded_auto_review()
                })),
            );
            assert_summary(
                audit.summary(),
                full_summary(rows.values().filter(|e| {
                    e.timestamp >= 0 && e.timestamp < 100 && e.is_excluded_auto_review()
                })),
            );
            let included = |e: &&TokenEvent| {
                e.timestamp > key.start && e.timestamp <= key.end && !e.is_excluded_auto_review()
            };
            assert_observed(
                range.observed(false),
                full_summary(rows.values().filter(included)),
                false,
            );
            assert_observed(
                range.observed(true),
                full_summary(rows.values().filter(included)),
                true,
            );
            for task in 0..9 {
                let task = format!("task-{task}");
                assert_observed(
                    range.task_observed(&task),
                    full_summary(rows.values().filter(included).filter(|e| e.session == task)),
                    false,
                );
            }
        }
        for e in rows.values() {
            total.remove(e);
            range.remove(e);
            day.remove(e);
            audit.remove(e);
        }
        assert!(total.is_empty());
        assert_eq!(range.task_count(), 0);
        assert_eq!(total.summary().priced_cost_usd, 0.0);
        assert_eq!(day.summary().event_count, 0);
        assert_eq!(audit.summary().event_count, 0);
    }
}
