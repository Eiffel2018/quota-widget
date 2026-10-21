//! Selected-day grouping without retaining raw historical events.
//!
//! A first build groups the requested day. Later commits reload only affected
//! logical tasks or exact recorded turns. Missing turns remain event singletons
//! and are updated directly from the projected commit rows. Global policy and
//! pricing changes invalidate this cache in the database owner.

use crate::{
    display_cache::DayKey,
    token_db::TokenDb,
    tokens::TokenEvent,
    usage_views::{self, GroupBy, UsageGroup},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Identity {
    Task,
    Turn(String),
    UnknownTurn(String),
    Response(String),
}

type GroupKey = (String, Identity);

#[derive(Debug, Clone)]
pub struct GroupedDayCache {
    day: DayKey,
    by: GroupBy,
    groups: BTreeMap<GroupKey, UsageGroup>,
}

impl GroupedDayCache {
    pub fn new(db: &TokenDb, day: DayKey, by: GroupBy) -> io::Result<Self> {
        let _snapshot = db.read_snapshot()?;
        let rows = db.query_events_all(Some(day.start), Some(day.end), true)?;
        let mut cache = Self {
            day,
            by,
            groups: BTreeMap::new(),
        };
        let mut grouped = BTreeMap::<GroupKey, Vec<&TokenEvent>>::new();
        for row in rows.iter().filter(|row| cache.includes(row)) {
            grouped.entry(cache.group_key(row)).or_default().push(row);
        }
        for (key, rows) in grouped {
            if let Some(group) = cache.reduce(&rows)? {
                cache.groups.insert(key, group);
            }
        }
        Ok(cache)
    }

    pub fn key(&self) -> (DayKey, GroupBy) {
        (self.day, self.by)
    }

    fn includes(&self, row: &TokenEvent) -> bool {
        row.timestamp >= self.day.start
            && row.timestamp < self.day.end
            && row.is_excluded_auto_review() == self.day.excluded
    }

    fn group_key(&self, row: &TokenEvent) -> GroupKey {
        let identity = match self.by {
            GroupBy::Task => Identity::Task,
            GroupBy::Turn => row.turn_id.as_ref().map_or_else(
                || Identity::UnknownTurn(row.id.clone()),
                |turn| Identity::Turn(turn.clone()),
            ),
            GroupBy::Response => Identity::Response(row.id.clone()),
        };
        (row.session.clone(), identity)
    }

    fn reduce(&self, rows: &[&TokenEvent]) -> io::Result<Option<UsageGroup>> {
        let mut groups = usage_views::group_with_excluded(rows, self.by, self.day.excluded);
        if groups.len() > 1 {
            return Err(io::Error::other(
                "Scoped day query returned multiple grouping identities",
            ));
        }
        Ok(groups.pop())
    }

    /// Changes must be from a committed transaction and already projected with
    /// the same current policy/prices as `db`. Each old/new event ID occurs once
    /// per commit. Cache mutation is deferred until every scoped query succeeds.
    pub fn apply_changes(
        &mut self,
        db: &TokenDb,
        changes: &[(Option<TokenEvent>, Option<TokenEvent>)],
    ) -> io::Result<()> {
        let _snapshot = db.read_snapshot()?;
        let mut affected = BTreeSet::new();
        let mut replacements = BTreeMap::<GroupKey, Option<UsageGroup>>::new();
        for (old, new) in changes {
            for row in old
                .iter()
                .chain(new.iter())
                .filter(|row| self.includes(row))
            {
                let key = self.group_key(row);
                match key.1 {
                    Identity::Task | Identity::Turn(_) => {
                        affected.insert(key);
                    }
                    Identity::UnknownTurn(_) | Identity::Response(_) => {
                        replacements.insert(key, None);
                    }
                }
            }
        }
        // Singletons need no database read. Do this after all removals so moved
        // IDs cannot leave an old group or accidentally erase a new singleton.
        for row in changes
            .iter()
            .filter_map(|(_, new)| new.as_ref())
            .filter(|row| self.includes(row))
        {
            let key = self.group_key(row);
            if matches!(key.1, Identity::UnknownTurn(_) | Identity::Response(_)) {
                replacements.insert(key, self.reduce(&[row])?);
            }
        }
        for key in affected {
            let rows = match &key.1 {
                Identity::Task => db.query_task_events(
                    &key.0,
                    Some(self.day.start),
                    Some(self.day.end),
                    self.day.excluded,
                )?,
                Identity::Turn(turn) => db.query_turn_events(
                    &key.0,
                    Some(turn),
                    Some(self.day.start),
                    Some(self.day.end),
                    self.day.excluded,
                )?,
                Identity::UnknownTurn(_) | Identity::Response(_) => unreachable!(),
            };
            let rows: Vec<_> = rows.iter().filter(|row| self.includes(row)).collect();
            replacements.insert(key, self.reduce(&rows)?);
        }
        for (key, replacement) in replacements {
            match replacement {
                Some(group) => {
                    self.groups.insert(key, group);
                }
                None => {
                    self.groups.remove(&key);
                }
            }
        }
        Ok(())
    }

    /// Offset counts from the newest group; rows within a page retain ascending
    /// display order. Only the requested page is cloned. Ties match the stable
    /// `usage_views` ordering, including distinct unknown-turn singleton IDs.
    pub fn page(&self, offset: usize, limit: usize) -> (Vec<UsageGroup>, usize) {
        let count = self.groups.len();
        if limit == 0 || offset >= count {
            return (Vec::new(), count);
        }
        let mut rows: Vec<_> = self.groups.values().collect();
        rows.sort_by(|a, b| {
            (a.last_timestamp, &a.session, &a.turn_id, &a.response_id).cmp(&(
                b.last_timestamp,
                &b.session,
                &b.turn_id,
                &b.response_id,
            ))
        });
        let end = count - offset;
        let start = end.saturating_sub(limit);
        (
            rows[start..end].iter().map(|row| (*row).clone()).collect(),
            count,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::{self, DirtyExport, ModelPrice, Pricing, ThreadShard, TokenStore};

    fn pricing() -> Pricing {
        let mut pricing = Pricing::demo();
        pricing.rates.insert(
            "synthetic-free".into(),
            ModelPrice {
                uncached_input_per_million: 0.0,
                cached_input_per_million: 0.0,
                output_per_million: 0.0,
            },
        );
        pricing
    }

    fn event(id: usize) -> TokenEvent {
        TokenEvent {
            id: format!("event-{id:03}"),
            timestamp: 10,
            model: "gpt-6.1-sol".into(),
            model_assumed: false,
            reasoning_effort: None,
            session: "task-0".into(),
            response_id: None,
            turn_id: None,
            source_kind: Some("token_usage_record".into()),
            input: 100,
            cached: 40,
            uncached: 60,
            output: 20,
            reasoning: 10,
            cost_usd: None,
            coverage_start_timestamp_ms: None,
            partial_flags: vec![],
        }
    }

    fn commit(
        db: &mut TokenDb,
        rows: &BTreeMap<usize, TokenEvent>,
        revision: u64,
    ) -> Vec<(Option<TokenEvent>, Option<TokenEvent>)> {
        let mut header = TokenStore::default().header();
        header.revision = revision;
        header.pricing = Some(pricing());
        let mut threads = BTreeMap::<String, ThreadShard>::new();
        // Include empty prior tasks too, so removals and moves really remove the
        // persisted old rows rather than leaving a stale shard in the fixture.
        for task in db
            .thread_ids()
            .unwrap()
            .into_iter()
            .chain(rows.values().map(|e| e.session.clone()))
        {
            threads.entry(task.clone()).or_insert_with(|| {
                let mut shard = ThreadShard::default();
                shard.thread_id = task;
                shard
            });
        }
        for row in rows.values() {
            threads
                .get_mut(&row.session)
                .unwrap()
                .events
                .push(row.clone());
        }
        let mut changes = db
            .commit(
                &DirtyExport {
                    header,
                    threads: threads.into_values().collect(),
                },
                revision - 1,
            )
            .unwrap()
            .event_changes;
        let header = db.header().unwrap();
        for (old, new) in &mut changes {
            for row in old.iter_mut().chain(new.iter_mut()) {
                assert!(header.apply_event_policy(row));
                row.cost_usd = tokens::event_cost(row, &pricing());
            }
        }
        changes
    }

    fn assert_oracle(cache: &GroupedDayCache, db: &TokenDb) {
        let (key, by) = cache.key();
        let rows = db
            .query_events_all(Some(key.start), Some(key.end), true)
            .unwrap();
        let selected: Vec<_> = rows
            .iter()
            .filter(|e| e.is_excluded_auto_review() == key.excluded)
            .collect();
        let expected = usage_views::group_with_excluded(&selected, by, key.excluded);
        for (offset, limit) in [(0, usize::MAX), (0, 0), (0, 3), (2, 4), (usize::MAX, 10)] {
            let (actual, count) = cache.page(offset, limit);
            assert_eq!(count, expected.len());
            let end = expected.len().saturating_sub(offset);
            let start = end.saturating_sub(limit);
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(&expected[start..end]).unwrap(),
                "group {by:?}, excluded {}, offset {offset}, limit {limit}",
                key.excluded
            );
        }
    }

    fn caches(db: &TokenDb) -> Vec<GroupedDayCache> {
        [false, true]
            .into_iter()
            .flat_map(|excluded| {
                [GroupBy::Turn, GroupBy::Task, GroupBy::Response]
                    .into_iter()
                    .map(move |by| {
                        GroupedDayCache::new(
                            db,
                            DayKey {
                                start: 0,
                                end: 100,
                                excluded,
                            },
                            by,
                        )
                        .unwrap()
                    })
            })
            .collect()
    }

    #[test]
    fn regrouping_moves_and_removes_exact_turns_and_unknown_singletons() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = TokenDb::open(&temp.path().join("synthetic.sqlite3"), true).unwrap();
        let mut rows: BTreeMap<_, _> = (0..7).map(|id| (id, event(id))).collect();
        for id in [2, 3] {
            rows.get_mut(&id).unwrap().turn_id = Some("known-turn".into());
        }
        rows.get_mut(&4)
            .unwrap()
            .partial_flags
            .push("excluded_recorded_codex_auto_review".into());
        rows.get_mut(&5).unwrap().timestamp = 101;
        rows.get_mut(&6).unwrap().timestamp = 99;
        rows.get_mut(&6).unwrap().turn_id = Some("turn:unknown:event-000".into());
        commit(&mut db, &rows, 1);
        let mut caches = caches(&db);
        for cache in &caches {
            assert_oracle(cache, &db);
        }
        let turn_rows = caches[0].page(0, usize::MAX).0;
        let unknown: Vec<_> = turn_rows
            .iter()
            .filter(|row| row.turn_id.is_none())
            .collect();
        assert_eq!(unknown.len(), 2);
        assert!(unknown.iter().all(|row| row.event_count == 1));

        let moved = rows.get_mut(&0).unwrap();
        moved.turn_id = Some("known-turn".into());
        moved.model = "gpt-6-luna".into();
        rows.get_mut(&1).unwrap().timestamp = 100;
        let changed = rows.get_mut(&2).unwrap();
        changed.session = "task-1".into();
        changed.turn_id = Some("other-turn".into());
        changed.model = "synthetic-free".into();
        changed.input = u64::MAX;
        changed.partial_flags = vec![
            "response_coverage_gap".into(),
            "excluded_recorded_codex_auto_review".into(),
        ];
        rows.remove(&3);
        rows.get_mut(&4).unwrap().partial_flags.clear();
        rows.get_mut(&4).unwrap().model = "synthetic-unpriced".into();
        rows.get_mut(&5).unwrap().timestamp = 0;
        rows.get_mut(&5).unwrap().turn_id = Some("known-turn".into());
        let changes = commit(&mut db, &rows, 2);
        for cache in &mut caches {
            cache.apply_changes(&db, &changes).unwrap();
            assert_oracle(cache, &db);
        }

        rows.clear();
        let changes = commit(&mut db, &rows, 3);
        for cache in &mut caches {
            cache.apply_changes(&db, &changes).unwrap();
            assert_oracle(cache, &db);
            assert_eq!(cache.page(0, 10).1, 0);
        }
    }

    #[test]
    fn randomized_database_commits_match_full_grouping_and_pagination() {
        fn random(seed: &mut u64) -> u64 {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            *seed
        }
        fn randomized_event(id: usize, seed: &mut u64) -> TokenEvent {
            let mut e = event(id);
            e.timestamp = (random(seed) % 120) as i64 - 10;
            e.session = format!("task-{}", random(seed) % 4);
            // Normalized identities are thread-scoped. Task changes therefore
            // retire the old identity and create the new task's identity.
            e.id = format!("{}/event-{id:03}", e.session);
            e.turn_id =
                (!random(seed).is_multiple_of(3)).then(|| format!("turn-{}", random(seed) % 5));
            e.response_id = (!random(seed).is_multiple_of(3)).then(|| format!("response-{id}"));
            e.model = [
                "gpt-6.1-sol",
                "gpt-6-luna",
                "synthetic-free",
                "synthetic-unpriced",
            ][(random(seed) % 4) as usize]
                .into();
            e.model_assumed = random(seed).is_multiple_of(3);
            e.reasoning_effort = (!random(seed).is_multiple_of(3)).then(|| "synthetic-high".into());
            e.source_kind = (!random(seed).is_multiple_of(3)).then(|| "token_usage_record".into());
            e.input = if random(seed).is_multiple_of(10) {
                u64::MAX
            } else {
                random(seed) % 1000
            };
            e.cached = e.input / 2;
            e.uncached = e.input - e.cached;
            e.output = random(seed) % 100;
            e.reasoning = e.output / 2;
            for flag in [
                "response_coverage_gap",
                "synthetic_partial",
                "excluded_recorded_codex_auto_review",
            ] {
                if random(seed).is_multiple_of(5) {
                    e.partial_flags.push(flag.into());
                }
            }
            e
        }
        let temp = tempfile::tempdir().unwrap();
        let mut db = TokenDb::open(&temp.path().join("synthetic.sqlite3"), true).unwrap();
        let mut seed = 0x3d70_d841_c233_6f83;
        let mut rows: BTreeMap<_, _> = (0..40)
            .map(|id| (id, randomized_event(id, &mut seed)))
            .collect();
        commit(&mut db, &rows, 1);
        let mut caches = caches(&db);
        for cache in &caches {
            assert_oracle(cache, &db);
        }
        for revision in 2..=240 {
            // Multiple changes sometimes share a known turn/task, exercising
            // deduplication and replacing those aggregates once per commit.
            for _ in 0..3 {
                let id = (random(&mut seed) % 48) as usize;
                if random(&mut seed).is_multiple_of(4) {
                    rows.remove(&id);
                } else {
                    rows.insert(id, randomized_event(id, &mut seed));
                }
            }
            let changes = commit(&mut db, &rows, revision);
            for cache in &mut caches {
                cache.apply_changes(&db, &changes).unwrap();
                assert_oracle(cache, &db);
            }
        }
    }
}
