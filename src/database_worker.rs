//! Single-owner incremental database orchestration. The GUI receives bounded views.
use crate::{
    background::{
        flush_cancelled_command, Command, Event, Operation, Progress, ProgressStage, Snapshot,
    },
    cancellation,
    cycle_db::CycleDb,
    db_view::{DailyView, DatabaseView, ViewRequest},
    display_cache::{DayCache, DayKey, RangeCache, RangeKey},
    display_time,
    grouped_day_cache::GroupedDayCache,
    history,
    intervals::{self, CompletedInterval, ModelIntervalUsage},
    model::HistoryEvent,
    storage,
    token_db::{CommitStats, TokenDb},
    tokens::{self, ImportReport, Pricing, TokenEvent, TokenStore},
    usage_views::{self, GroupBy, TaskRow},
};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

struct State {
    db: Option<TokenDb>,
    cycle_db: Option<CycleDb>,
    cycle_start: Option<i64>,
    store: TokenStore,
    pricing: Pricing,
    raw_history: Vec<HistoryEvent>,
    loaded_history_path: Option<PathBuf>,
    normalized: Arc<Vec<HistoryEvent>>,
    history_blocked: bool,
    history_dirty: bool,
    request: ViewRequest,
    ranges: BTreeMap<(i64, i64), Vec<ModelIntervalUsage>>,
    intervals: Arc<Vec<CompletedInterval>>,
    snapshot: Option<Arc<Snapshot>>,
    retired: Vec<Arc<Snapshot>>,
    daily_cache: Option<(u64, ViewRequest, DailyView)>,
    day_totals: Option<DayCache>,
    grouped_day: Option<GroupedDayCache>,
    range_totals: Option<RangeCache>,
    blocked: bool,
    write_authorized: bool,
}
fn reset_cycle_starts(events: &[HistoryEvent]) -> Vec<i64> {
    history::reset_cycle_starts(events)
}

#[cfg(test)]
mod reset_cycle_tests {
    use super::*;
    use crate::model::{HistoryEvidence, HistoryKind, HistoryReconciliation};

    fn reset(at: i64, before: &str, after: &str, duration: i64) -> HistoryEvent {
        HistoryEvent {
            session: "test".into(),
            observed_at_unix: at,
            source_key: "codex".into(),
            epoch: 1,
            kind: HistoryKind::QuotaReset,
            window_label: "weekly".into(),
            before: before.into(),
            after: after.into(),
            delta: "1".into(),
            partial: false,
            before_observed_at_unix: None,
            window_key: None,
            evidence: Some(HistoryEvidence {
                window_duration_mins: Some(duration),
                ..HistoryEvidence::default()
            }),
            reconciliation: None,
        }
    }

    #[test]
    fn replayed_pre_reset_snapshot_does_not_open_another_cycle() {
        let mut first = reset(1000, "0", "100", 10080);
        first.evidence.as_mut().unwrap().before_reset_at = Some(900);
        let mut replay = first.clone();
        replay.observed_at_unix = 1790;
        replay.evidence.as_mut().unwrap().reset_at = Some(9999);
        assert_eq!(reset_cycle_starts(&[replay, first]), vec![1000]);
    }

    #[test]
    fn reset_split_uses_weekly_returns_below_ninety_nine_to_one_hundred() {
        let mut conflict = reset(3_000, "30", "100", 10_080);
        conflict.reconciliation = Some(HistoryReconciliation::ConflictingObservation);
        let mut legacy = reset(4_000, "65", "100", 10_080);
        legacy.partial = true;
        legacy.reconciliation = Some(HistoryReconciliation::UncertainRecovery);
        assert_eq!(
            reset_cycle_starts(&[
                reset(1_000, "2", "100", 10_080),
                reset(1_025, "99", "100", 10_080),
                reset(1_050, "99", "100", 10_080),
                reset(2_000, "20", "100", 10_080),
                reset(2_500, "20", "21", 10_080),
                reset(2_800, "20", "100", 300),
                conflict,
                legacy,
            ]),
            vec![1_000, 2_000, 4_000]
        );
    }
}
impl State {
    fn new(write_authorized: bool) -> Self {
        let now = chrono::Utc::now().timestamp();
        let (day_start, day_end) = display_time::day_bounds(display_time::now_day()).unwrap();
        Self {
            db: None,
            cycle_db: None,
            cycle_start: None,
            store: TokenStore::default(),
            pricing: Pricing::default(),
            raw_history: vec![],
            loaded_history_path: None,
            normalized: Arc::new(vec![]),
            history_blocked: false,
            history_dirty: false,
            request: ViewRequest {
                day_start,
                day_end,
                group: GroupBy::Response,
                excluded: false,
                page: 0,
                page_size: 500,
                boundary: None,
                fallback_start: now,
                now,
            },
            ranges: BTreeMap::new(),
            intervals: Arc::new(vec![]),
            snapshot: None,
            retired: vec![],
            daily_cache: None,
            day_totals: None,
            grouped_day: None,
            range_totals: None,
            blocked: false,
            write_authorized,
        }
    }
    fn merge(&mut self, incoming: Vec<HistoryEvent>) -> bool {
        let previous = self.raw_history.len();
        self.raw_history = history::merge(std::mem::take(&mut self.raw_history), incoming);
        if self.raw_history.len() == previous {
            return false;
        }
        self.history_dirty = true;
        self.normalized = Arc::new(history::normalized(&self.raw_history));
        true
    }
    fn invalidate(&mut self, stats: &CommitStats) {
        if cancellation::is_cancelled() {
            return;
        }
        if stats.global_policy_changed {
            self.ranges.clear();
            self.daily_cache = None;
            self.day_totals = None;
            self.grouped_day = None;
            self.range_totals = None;
            return;
        }
        // Commit diffs are canonical raw rows. Reapply the unchanged projection
        // policy and current prices before adjusting the displayed aggregates.
        if let Some(header) = self.db.as_ref().and_then(|db| db.header_shared().ok()) {
            let policy = header.event_policy();
            let project = |event: &Option<TokenEvent>| {
                event.as_ref().and_then(|e| {
                    let mut e = e.clone();
                    if !policy.apply(&mut e) {
                        return None;
                    }
                    e.cost_usd = tokens::event_cost(&e, &self.pricing);
                    Some(e)
                })
            };
            let changes: Vec<_> = stats
                .event_changes
                .iter()
                .take_while(|_| !cancellation::is_cancelled())
                .map(|(old, new)| (project(old), project(new)))
                .collect();
            if let Some(cache) = &mut self.grouped_day {
                if cache
                    .apply_changes(self.db.as_ref().unwrap(), &changes)
                    .is_err()
                {
                    self.grouped_day = None;
                }
            }
            for (old, new) in &changes {
                if cancellation::is_cancelled() {
                    return;
                }
                if let Some(cache) = &mut self.day_totals {
                    cache.adjust(old.as_ref(), new.as_ref());
                }
                if let Some(cache) = &mut self.range_totals {
                    cache.adjust(old.as_ref(), new.as_ref());
                }
            }
        } else {
            self.day_totals = None;
            self.grouped_day = None;
            self.range_totals = None;
        }
        for (old, new) in &stats.event_changes {
            if cancellation::is_cancelled() {
                return;
            }
            for e in old.iter().chain(new.iter()) {
                self.ranges
                    .retain(|&(start, end), _| !(e.timestamp > start && e.timestamp <= end));
                if self.daily_cache.as_ref().is_some_and(|(_, request, _)| {
                    e.timestamp >= request.day_start && e.timestamp < request.day_end
                }) {
                    self.daily_cache = None;
                }
            }
        }
        // Revision changes outside the selected day retain its prepared totals.
        if let Some((revision, _, _)) = &mut self.daily_cache {
            *revision = stats.revision;
        }
    }
    fn reprice(&mut self, pricing: &Pricing) -> io::Result<()> {
        cancellation::check()?;
        let Some(db) = self.db.as_ref() else {
            self.pricing = pricing.clone();
            return Ok(());
        };
        let header = db.header_shared()?;
        if header.pricing.as_ref() == Some(pricing) {
            self.pricing = pricing.clone();
            return Ok(());
        }
        if header.requires_attribution_rebuild(pricing) {
            for id in db.thread_ids()? {
                cancellation::check()?;
                if let Some(shard) = db.load_thread(&id)? {
                    self.store.hydrate_thread(shard)?;
                }
            }
        }
        self.store.reprice_hydrated_checked(pricing)?;
        cancellation::check()?;
        self.pricing = pricing.clone();
        self.ranges.clear();
        self.daily_cache = None;
        self.day_totals = None;
        self.grouped_day = None;
        self.range_totals = None;
        Ok(())
    }
    fn commit(&mut self) -> io::Result<Option<CommitStats>> {
        cancellation::check()?;
        let Some(db) = self.db.as_mut() else {
            return Ok(None);
        };
        let revision = db.revision()?.unwrap_or(0);
        if self.store.revision() == revision {
            return Ok(None);
        }
        if !self.write_authorized || self.blocked || self.history_blocked {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Read-only state: changes were not persisted",
            ));
        }
        let export = self.store.dirty_export_checked()?;
        cancellation::check()?;
        let stats = db.commit(&export, revision)?;
        self.store.mark_committed();
        if let Some(cycle_db) = self.cycle_db.as_mut() {
            cycle_db.apply_changes(&stats.event_changes)?;
        }
        self.invalidate(&stats);
        Ok(Some(stats))
    }
    fn sync_cycle_archive(&mut self) -> io::Result<()> {
        if !self.write_authorized || self.blocked || self.history_blocked {
            return Ok(());
        }
        let starts = reset_cycle_starts(&self.normalized);
        if let Some(db) = self.db.as_ref() {
            db.index_quota_cycles(&starts, &self.normalized)?;
        }
        let Some(&start) = starts.last() else {
            return Ok(());
        };
        let directory = self
            .db
            .as_ref()
            .and_then(|db| db.path().parent())
            .ok_or_else(|| io::Error::other("Token database has no parent directory"))?
            .to_owned();
        for (index, &cycle_start) in starts.iter().enumerate() {
            let cycle_end = starts.get(index + 1).copied();
            let mut archive = CycleDb::open(&directory, cycle_start)?;
            if archive.needs_backfill()? {
                let db = self
                    .db
                    .as_ref()
                    .ok_or_else(|| io::Error::other("Token database is unavailable"))?;
                let events = db.query_events(Some(cycle_start), cycle_end, true)?;
                archive.backfill(&events)?;
            }
        }
        if self.cycle_start != Some(start) {
            self.cycle_db = Some(CycleDb::open(&directory, start)?);
            self.cycle_start = Some(start);
        }
        let Some(cycle_db) = self.cycle_db.as_mut() else {
            return Ok(());
        };
        if cycle_db.needs_backfill()? {
            let Some(db) = self.db.as_ref() else {
                return Ok(());
            };
            let events = db.query_events(Some(start), None, true)?;
            cycle_db.backfill(&events)?;
        }
        let db = self
            .db
            .as_mut()
            .ok_or_else(|| io::Error::other("Token database is unavailable"))?;
        if db.has_events_before(start)? {
            let timestamp = chrono::DateTime::from_timestamp(start, 0)
                .ok_or_else(|| io::Error::other("Invalid quota reset timestamp"))?
                .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
                .format("%Y%m%d-%H%M%S");
            let backup_path =
                directory.join(format!("tokens-before-cycle-split-{timestamp}.sqlite3"));
            if !backup_path.exists() {
                db.backup_to(&backup_path)?;
            }
            let backup = TokenDb::open_readonly(&backup_path)?;
            backup.integrity_check()?;
            let revision = db.revision()?.unwrap_or(0);
            let store = db.export_store()?;
            let export = store.cycle_compaction_export(start)?;
            let stats = db.commit(&export, revision)?;
            db.checkpoint()?;
            db.vacuum()?;
            self.store = TokenStore::from_header(db.header()?);
            if let Some(cycle_db) = self.cycle_db.as_mut() {
                cycle_db.apply_changes(&stats.event_changes)?;
            }
            self.ranges.clear();
            self.daily_cache = None;
            self.day_totals = None;
            self.grouped_day = None;
            self.range_totals = None;
        }
        Ok(())
    }
    fn publish(&mut self) -> io::Result<Arc<Snapshot>> {
        cancellation::check()?;
        let db = self.db.as_ref().ok_or_else(|| {
            io::Error::other("Token database unavailable; original data preserved")
        })?;
        let _read_snapshot = db.read_snapshot()?;
        let header = db.header_shared()?;
        let counts = db.counts()?;
        let mut latest: Vec<TokenEvent> = db
            .latest_per_thread_before(self.request.now, false)?
            .into_values()
            .collect();
        let start = self.request.boundary.unwrap_or(self.request.fallback_start);
        latest.retain(|e| {
            e.timestamp <= self.request.now
                && (e.timestamp >= self.request.now.saturating_sub(7200) || e.timestamp > start)
        });
        latest.sort_by(|a, b| {
            (&a.session, a.timestamp, &a.id).cmp(&(&b.session, b.timestamp, &b.id))
        });
        for event in &mut latest {
            cancellation::check()?;
            event.cost_usd = tokens::event_cost(event, &self.pricing);
        }
        let range_key = RangeKey {
            start,
            end: self.request.now,
            boundary: self.request.boundary,
        };
        let mut range = self
            .range_totals
            .take()
            .filter(|cache| {
                let k = cache.key();
                k.start == range_key.start
                    && k.boundary == range_key.boundary
                    && k.end <= range_key.end
            })
            .unwrap_or_else(|| {
                RangeCache::new(RangeKey {
                    end: start,
                    ..range_key
                })
            });
        let tail_start = range.key().end;
        range.extend_end(range_key.end);
        db.for_each_event(
            Some(tail_start.saturating_add(1)),
            Some(range_key.end.saturating_add(1)),
            |e| {
                cancellation::check()?;
                range.add(&e);
                Ok(true)
            },
        )?;
        let main_cost = range.observed(
            self.request.boundary.is_none()
                || db.earliest_included_timestamp()?.is_none_or(|t| t > start),
        );
        let mut main_tasks: Vec<TaskRow> = latest
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                if e.timestamp < self.request.now.saturating_sub(7200) && e.timestamp <= start {
                    return None;
                }
                let own = range.task_observed(&e.session);
                Some(TaskRow {
                    task_id: e.session.clone(),
                    latest_event_index: i,
                    latest_timestamp: e.timestamp,
                    latest_cost_usd: e.cost_usd,
                    interval_cost_usd: Some(own.known_usd),
                    interval_events: own.events,
                    partial: self.request.boundary.is_none() || own.partial,
                })
            })
            .collect();
        main_tasks.sort_by(|a, b| {
            (b.latest_timestamp, &b.task_id).cmp(&(a.latest_timestamp, &a.task_id))
        });
        self.range_totals = Some(range);
        let day_key = DayKey {
            start: self.request.day_start,
            end: self.request.day_end,
            excluded: self.request.excluded,
        };
        if self
            .day_totals
            .as_ref()
            .is_none_or(|cache| cache.key() != day_key)
        {
            let mut cache = DayCache::new(day_key);
            db.for_each_event(Some(day_key.start), Some(day_key.end), |e| {
                cancellation::check()?;
                cache.add(&e);
                Ok(true)
            })?;
            self.day_totals = Some(cache);
        }
        let summary = self.day_totals.as_ref().unwrap().summary();
        if self.request.group != GroupBy::Response
            && self
                .grouped_day
                .as_ref()
                .is_none_or(|cache| cache.key() != (day_key, self.request.group))
        {
            self.grouped_day = Some(GroupedDayCache::new(db, day_key, self.request.group)?);
        }
        let daily = if let Some((revision, request, daily)) = &self.daily_cache {
            if *revision == header.revision && same_daily(request, &self.request) {
                daily.clone()
            } else {
                daily_view(
                    db,
                    &self.request,
                    summary.clone(),
                    self.grouped_day.as_ref(),
                )?
            }
        } else {
            daily_view(
                db,
                &self.request,
                summary.clone(),
                self.grouped_day.as_ref(),
            )?
        };
        self.daily_cache = Some((header.revision, self.request.clone(), daily.clone()));
        let database = DatabaseView {
            request: self.request.clone(),
            database_path: db.path().to_owned(),
            total_included: counts.included_events,
            total_excluded: counts.excluded_events,
            day_starts: db.days()?.into_iter().collect(),
            models: db.models()?.into_iter().collect(),
            main_tasks,
            main_cost,
            daily,
        };
        let snapshot = Arc::new(Snapshot {
            database: Some(database),
            revision: header.revision,
            pricing: self.pricing.clone(),
            events: latest,
            excluded_tasks: header.excluded_task_ids().clone(),
            task_names: header.task_names(),
            task_parents: header.task_parents(),
            task_turn_usage: db.turn_usage()?,
            ..Snapshot::default()
        });
        cancellation::check()?;
        if let Some(old) = self.snapshot.replace(snapshot.clone()) {
            self.retired.push(old);
        }
        self.retired.retain(|old| Arc::strong_count(old) > 1);
        Ok(snapshot)
    }
    fn rebuild_intervals(&mut self) -> io::Result<bool> {
        cancellation::check()?;
        let Some(db) = self.db.as_ref() else {
            return Ok(false);
        };
        let earliest = db.earliest_included_timestamp()?;
        let ranges = &mut self.ranges;
        let pricing = &self.pricing;
        let values = intervals::completed_intervals_with_provider(
            &self.normalized,
            earliest,
            |start, end| {
                cancellation::check()?;
                if let Some(cached) = ranges.get(&(start, end)) {
                    return Ok(cached.clone());
                }
                let mut events = db.query_events(
                    Some(start.saturating_add(1)),
                    Some(end.saturating_add(1)),
                    false,
                )?;
                for e in &mut events {
                    cancellation::check()?;
                    e.cost_usd = tokens::event_cost(e, pricing);
                }
                let values = intervals::model_usage(events.iter());
                ranges.insert((start, end), values.clone());
                Ok(values)
            },
        )?;
        cancellation::check()?;
        if *self.intervals == values {
            return Ok(false);
        }
        self.intervals = Arc::new(values);
        Ok(true)
    }
    fn save_history(&mut self, _path: Option<&Path>, events: &mpsc::Sender<Event>) {
        if !self.write_authorized || self.blocked || self.history_blocked || !self.history_dirty {
            return;
        }
        let Some(db) = self.db.as_ref() else { return };
        let path = db.path().to_owned();
        let result = db.save_history_events(&self.raw_history);
        match result {
            Ok(()) => self.history_dirty = false,
            Err(error) => save_error(events, &path, error),
        }
    }
    fn emit_intervals(&mut self, events: &mpsc::Sender<Event>, _save: Option<&Path>) {
        if cancellation::is_cancelled() {
            return;
        }
        match self.rebuild_intervals() {
            Ok(true) => {
                let _ = events.send(Event::IntervalsUpdated {
                    intervals: self.intervals.clone(),
                });
            }
            Ok(false) => {}
            Err(error) => {
                let _ = events.send(Event::Updated {
                    snapshot: None,
                    normalized_history: None,
                    report: None,
                    error: Some(format!("Interval query: {error}")),
                });
            }
        }
    }
}
fn same_daily(a: &ViewRequest, b: &ViewRequest) -> bool {
    a.day_start == b.day_start
        && a.day_end == b.day_end
        && a.group == b.group
        && a.excluded == b.excluded
        && a.page == b.page
        && a.page_size == b.page_size
}
fn daily_view(
    db: &TokenDb,
    request: &ViewRequest,
    summary: tokens::TokenSummary,
    grouped: Option<&GroupedDayCache>,
) -> io::Result<DailyView> {
    if request.group == GroupBy::Response {
        let selected = db.query_events_filtered(
            Some(request.day_start),
            Some(request.day_end),
            request.excluded,
            request.page.saturating_mul(request.page_size),
            request.page_size,
        )?;
        let rows = usage_views::group_with_excluded(
            &selected.iter().collect::<Vec<_>>(),
            GroupBy::Response,
            request.excluded,
        );
        return Ok(DailyView {
            rows,
            group_count: summary.event_count,
            summary,
        });
    }
    let grouped = grouped.ok_or_else(|| io::Error::other("Grouped day cache is not ready"))?;
    let (rows, group_count) = grouped.page(
        request.page.saturating_mul(request.page_size),
        request.page_size,
    );
    Ok(DailyView {
        rows,
        summary,
        group_count,
    })
}

fn save_error(events: &mpsc::Sender<Event>, path: &Path, error: io::Error) {
    let _ = events.send(Event::SaveFailed {
        path: path.to_owned(),
        error: error.to_string(),
    });
}
fn progress(
    events: &mpsc::Sender<Event>,
    operation: Operation,
    stage: ProgressStage,
    started: Instant,
    report: Option<tokens::ImportProgress>,
) {
    let r = report;
    let _ = events.send(Event::Progress(Progress {
        operation,
        stage,
        elapsed: started.elapsed(),
        files_done: r.map_or(0, |r| r.files_done),
        files_total: r.map_or(0, |r| r.files_total),
        lines_read: r.map_or(0, |r| r.lines_read),
        observations: r.map_or(0, |r| r.observations),
        events: r.map_or(0, |r| r.events),
    }));
}

pub(crate) fn run(
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    mut write_authorized: bool,
) {
    let mut state = State::new(write_authorized);
    let mut saved_settings: Option<(PathBuf, Vec<u8>)> = None;
    while let Ok(command) = commands.recv() {
        if cancellation::is_cancelled() {
            if let Command::Load { history_path, .. } = &command {
                // A skipped account switch is still a raw-history boundary.
                // Never carry the previous account's unsaved evidence into it.
                state.raw_history.clear();
                state.loaded_history_path = history_path.clone();
            }
            if !flush_cancelled_command(
                command,
                &events,
                write_authorized,
                &state.raw_history,
                state.loaded_history_path.as_deref(),
                &mut saved_settings,
            ) {
                // A committed SQLite transaction is already durable. Avoid a
                // potentially slow WAL checkpoint during interactive close.
                break;
            }
            continue;
        }
        match command {
            Command::AcquireWriter { dir } => {
                if !write_authorized
                    && state.db.as_ref().is_some_and(|db| {
                        db.path() == TokenDb::database_path(&dir.join("tokens.json"))
                    })
                {
                    let path = state.db.as_ref().unwrap().path().to_owned();
                    // An immutable reader may itself own a shared directory
                    // lock. Release it before requesting exclusive ownership.
                    state.db = None;
                    let lock = storage::DataLock::acquire(&dir).ok();
                    match TokenDb::open(&path, lock.is_some()) {
                        Ok(db) => state.db = Some(db),
                        Err(e) => {
                            let _ = events.send(Event::Updated {
                                snapshot: None,
                                normalized_history: None,
                                report: None,
                                error: Some(e.to_string()),
                            });
                            let _ = events.send(Event::WriterLock { lock: None });
                            continue;
                        }
                    }
                    if lock.is_some() {
                        write_authorized = true;
                        state.write_authorized = true;
                    }
                    let _ = events.send(Event::WriterLock { lock });
                } else {
                    let _ = events.send(Event::WriterLock { lock: None });
                }
            }
            Command::Load {
                token_path,
                history_path,
                pricing,
            } => {
                state = State::new(write_authorized);
                state.pricing = pricing.clone();
                state.loaded_history_path = history_path.clone();
                let started = Instant::now();
                progress(
                    &events,
                    Operation::Load,
                    ProgressStage::Loading,
                    started,
                    None,
                );
                let result =
                    TokenDb::open_current(&TokenDb::database_path(&token_path), write_authorized);
                let mut error = None;
                match result {
                    Ok(Some(db)) => match db.header() {
                        Ok(header) => {
                            state.store = TokenStore::from_header(header);
                            state.db = Some(db);
                        }
                        Err(e) => error = Some(e.to_string()),
                    },
                    Ok(None) => error = Some("SQLite database needs the application's writer lock. Close the other instance and reopen.".into()),
                    Err(e) => error = Some(e.to_string()),
                }
                state.blocked = error.is_some();
                if cancellation::is_cancelled() {
                    continue;
                }
                let history_error = state.db.as_ref().and_then(|db| match db.history_events() {
                    Ok(raw_history) => {
                        state.raw_history = raw_history;
                        None
                    }
                    Err(e) => {
                        state.history_blocked = true;
                        Some(format!("SQLite quota history: {e}"))
                    }
                });
                if cancellation::is_cancelled() {
                    continue;
                }
                state.normalized = Arc::new(history::normalized(&state.raw_history));
                state.merge(history::replay(state.store.quota_observations()));
                if error.is_none() {
                    if let Err(e) = state.sync_cycle_archive() {
                        error = Some(format!("Quota cycle archive: {e}"));
                    }
                }
                if error.is_none() && write_authorized {
                    if let Err(e) = state
                        .reprice(&pricing)
                        .and_then(|_| state.commit().map(|_| ()))
                    {
                        error = Some(e.to_string());
                        state.blocked = true;
                    }
                }
                if cancellation::is_cancelled() {
                    continue;
                }
                progress(
                    &events,
                    Operation::Load,
                    ProgressStage::BuildingViews,
                    started,
                    None,
                );
                let snapshot = match state.publish() {
                    Ok(s) => s,
                    Err(e) => {
                        error.get_or_insert(e.to_string());
                        Arc::new(Snapshot::default())
                    }
                };
                if cancellation::is_cancelled() {
                    continue;
                }
                let _ = events.send(Event::Loaded {
                    snapshot,
                    normalized_history: state.normalized.clone(),
                    history_error,
                    error,
                });
                state.emit_intervals(&events, None);
                progress(
                    &events,
                    Operation::Load,
                    ProgressStage::Complete,
                    started,
                    None,
                );
            }
            Command::View(request) => {
                state.request = request;
                let result = state.publish();
                if cancellation::is_cancelled() {
                    continue;
                }
                let (snapshot, error) = match result {
                    Ok(s) => (Some(s), None),
                    Err(e) => (None, Some(e.to_string())),
                };
                let _ = events.send(Event::Updated {
                    snapshot,
                    normalized_history: None,
                    report: None,
                    error,
                });
            }
            Command::Import {
                root,
                metadata,
                pricing,
                save_path,
                history_save_path,
            } => {
                let started = Instant::now();
                progress(
                    &events,
                    Operation::Import,
                    ProgressStage::Loading,
                    started,
                    None,
                );
                let old_revision = state.store.revision();
                let old_quota = state.store.quota_observations().len();
                let result = (|| -> io::Result<ImportReport> {
                    if state.blocked || state.history_blocked {
                        return Err(io::Error::other(
                            "Import paused to preserve unreadable state",
                        ));
                    }
                    if !state.write_authorized || save_path.is_none() {
                        return Err(io::Error::new(io::ErrorKind::PermissionDenied,"Read-only database: import is paused until the writer lock is available"));
                    }
                    if state.db.as_ref().is_some_and(|db| {
                        save_path
                            .as_ref()
                            .is_none_or(|path| TokenDb::database_path(path) != db.path())
                    }) {
                        return Err(io::Error::other(
                            "Import save path differs from the loaded database; no data written",
                        ));
                    }
                    state.reprice(&pricing)?;
                    let db = state
                        .db
                        .as_ref()
                        .ok_or_else(|| io::Error::other("Token database not loaded"))?;
                    let mut last = Instant::now() - Duration::from_secs(1);
                    let report = state.store.import_dir_hydrated(
                        &root,
                        metadata.as_deref(),
                        &pricing,
                        &mut |id| db.load_thread(id),
                        &mut |r| {
                            if last.elapsed() >= Duration::from_millis(200) {
                                progress(
                                    &events,
                                    Operation::Import,
                                    ProgressStage::Import(r.phase),
                                    started,
                                    Some(r),
                                );
                                last = Instant::now();
                            }
                        },
                    )?;
                    state.commit()?;
                    Ok(report)
                })();
                if cancellation::is_cancelled() {
                    // The token importer may contain a partially rebuilt shard.
                    // It is abandoned here and cannot reach any later commit.
                    continue;
                }
                let mut history_changed = false;
                if result.is_ok() && state.store.quota_observations().len() != old_quota {
                    history_changed =
                        state.merge(history::replay(state.store.quota_observations()));
                }
                if result.is_ok() && history_changed {
                    if let Err(e) = state.sync_cycle_archive() {
                        save_error(
                            &events,
                            state
                                .db
                                .as_ref()
                                .map(TokenDb::path)
                                .unwrap_or(Path::new("quota-cycle")),
                            e,
                        );
                    }
                }
                let (report, mut error) = match result {
                    Ok(r) => (Some(r), None),
                    Err(e) => {
                        // Failed parsing/transactions never advance durable cursors. Retry
                        // from the committed header rather than a partially mutated importer.
                        if let Some(db) = state.db.as_ref() {
                            if let Ok(header) = db.header() {
                                state.store = TokenStore::from_header(header);
                            }
                        }
                        (None, Some(e.to_string()))
                    }
                };
                let snapshot = if error.is_none() && state.store.revision() != old_revision {
                    match state.publish() {
                        Ok(s) => Some(s),
                        Err(e) => {
                            error = Some(e.to_string());
                            None
                        }
                    }
                } else {
                    None
                };
                let _ = events.send(Event::Updated {
                    snapshot,
                    normalized_history: history_changed.then(|| state.normalized.clone()),
                    report,
                    error,
                });
                state.save_history(history_save_path.as_deref(), &events);
                if history_changed || state.store.revision() != old_revision {
                    state.emit_intervals(
                        &events,
                        history_save_path.as_deref().or(save_path.as_deref()),
                    );
                }
                progress(
                    &events,
                    Operation::Import,
                    ProgressStage::Complete,
                    started,
                    None,
                );
            }
            Command::Reprice { pricing, save_path } => {
                let authorized = state.write_authorized
                    && state.db.as_ref().is_some_and(|db| {
                        save_path
                            .as_ref()
                            .is_some_and(|path| TokenDb::database_path(path) == db.path())
                    });
                let result = if authorized {
                    state
                        .reprice(&pricing)
                        .and_then(|_| state.commit())
                        .and_then(|_| state.publish())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Repricing requires the matching writable database path",
                    ))
                };
                if cancellation::is_cancelled() {
                    continue;
                }
                let (snapshot, error) = match result {
                    Ok(s) => (Some(s), None),
                    Err(e) => (None, Some(e.to_string())),
                };
                let _ = events.send(Event::Updated {
                    snapshot,
                    normalized_history: None,
                    report: None,
                    error,
                });
                state.emit_intervals(&events, save_path.as_deref());
            }
            Command::MergeHistory {
                events: incoming,
                save_path,
            } => {
                let changed = state.merge(incoming);
                if changed {
                    if let Err(e) = state.sync_cycle_archive() {
                        save_error(
                            &events,
                            state
                                .db
                                .as_ref()
                                .map(TokenDb::path)
                                .unwrap_or(Path::new("quota-cycle")),
                            e,
                        );
                    }
                }
                let _ = events.send(Event::HistoryUpdated {
                    normalized: changed.then(|| state.normalized.clone()),
                });
                if changed {
                    state.save_history(save_path.as_deref(), &events);
                    if !cancellation::is_cancelled() {
                        state.emit_intervals(&events, save_path.as_deref());
                    }
                }
            }
            Command::SaveSettings { path, settings } => {
                if !write_authorized || state.blocked || state.history_blocked {
                    continue;
                }
                match serde_json::to_vec(&settings).map_err(io::Error::other) {
                    Ok(bytes)
                        if saved_settings
                            .as_ref()
                            .is_none_or(|(p, b)| p != &path || b != &bytes) =>
                    {
                        match storage::save_settings(&path, &settings) {
                            Ok(()) => saved_settings = Some((path, bytes)),
                            Err(e) => save_error(&events, &path, e),
                        }
                    }
                    Ok(_) => {}
                    Err(e) => save_error(&events, &path, e),
                }
            }
            Command::Shutdown => {
                if let Some(db) = state.db.as_ref() {
                    if write_authorized {
                        let _ = db.checkpoint();
                    }
                }
                break;
            }
        }
    }
}
