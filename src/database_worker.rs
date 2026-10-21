//! Single-owner incremental database orchestration. The GUI receives bounded views.
use crate::{
    background::{Command, Event, Operation, Progress, ProgressStage, Snapshot},
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
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

#[derive(Deserialize)]
struct HistoryFile {
    version: u32,
    events: Vec<HistoryEvent>,
}
#[derive(Serialize)]
struct HistoryOutput<'a> {
    version: u32,
    events: &'a [HistoryEvent],
}
#[derive(Serialize)]
struct IntervalOutput<'a> {
    version: u32,
    intervals: &'a [CompletedInterval],
}

struct State {
    db: Option<TokenDb>,
    store: TokenStore,
    pricing: Pricing,
    raw_history: Vec<HistoryEvent>,
    normalized: Arc<Vec<HistoryEvent>>,
    history_blocked: bool,
    history_backup: Option<PathBuf>,
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
impl State {
    fn new(write_authorized: bool) -> Self {
        let now = chrono::Utc::now().timestamp();
        let (day_start, day_end) = display_time::day_bounds(display_time::now_day()).unwrap();
        Self {
            db: None,
            store: TokenStore::default(),
            pricing: Pricing::default(),
            raw_history: vec![],
            normalized: Arc::new(vec![]),
            history_blocked: false,
            history_backup: None,
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
                if let Some(shard) = db.load_thread(&id)? {
                    self.store.hydrate_thread(shard)?;
                }
            }
        }
        self.store.reprice_hydrated(pricing);
        self.pricing = pricing.clone();
        self.ranges.clear();
        self.daily_cache = None;
        self.day_totals = None;
        self.grouped_day = None;
        self.range_totals = None;
        Ok(())
    }
    fn commit(&mut self) -> io::Result<Option<CommitStats>> {
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
        let stats = db.commit(&self.store.dirty_export(), revision)?;
        self.store.mark_committed();
        self.invalidate(&stats);
        Ok(Some(stats))
    }
    fn publish(&mut self) -> io::Result<Arc<Snapshot>> {
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
        if let Some(old) = self.snapshot.replace(snapshot.clone()) {
            self.retired.push(old);
        }
        self.retired.retain(|old| Arc::strong_count(old) > 1);
        Ok(snapshot)
    }
    fn rebuild_intervals(&mut self) -> io::Result<bool> {
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
                if let Some(cached) = ranges.get(&(start, end)) {
                    return Ok(cached.clone());
                }
                let mut events = db.query_events(
                    Some(start.saturating_add(1)),
                    Some(end.saturating_add(1)),
                    false,
                )?;
                for e in &mut events {
                    e.cost_usd = tokens::event_cost(e, pricing);
                }
                let values = intervals::model_usage(events.iter());
                ranges.insert((start, end), values.clone());
                Ok(values)
            },
        )?;
        if *self.intervals == values {
            return Ok(false);
        }
        self.intervals = Arc::new(values);
        Ok(true)
    }
    fn save_history(&mut self, path: Option<&Path>, events: &mpsc::Sender<Event>) {
        let Some(path) = path else { return };
        if !self.write_authorized || self.blocked || self.history_blocked || !self.history_dirty {
            return;
        }
        let result = (|| {
            if self.history_backup.as_deref() == Some(path) {
                storage::backup_legacy_history(path)?;
                self.history_backup = None;
            }
            storage::save_json(
                path,
                &HistoryOutput {
                    version: 2,
                    events: &self.raw_history,
                },
            )
        })();
        match result {
            Ok(()) => self.history_dirty = false,
            Err(error) => save_error(events, path, error),
        }
    }
    fn emit_intervals(&mut self, events: &mpsc::Sender<Event>, save: Option<&Path>) {
        match self.rebuild_intervals() {
            Ok(true) => {
                let _ = events.send(Event::IntervalsUpdated {
                    intervals: self.intervals.clone(),
                });
                if self.write_authorized && !self.blocked && !self.history_blocked {
                    if let Some(parent) = save.and_then(Path::parent) {
                        let path = parent.join("quota-intervals.json");
                        if let Err(error) = storage::save_json(
                            &path,
                            &IntervalOutput {
                                version: 2,
                                intervals: &self.intervals,
                            },
                        ) {
                            save_error(events, &path, error)
                        }
                    }
                }
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
    write_authorized: bool,
) {
    let mut state = State::new(write_authorized);
    let mut saved_settings: Option<(PathBuf, Vec<u8>)> = None;
    while let Ok(command) = commands.recv() {
        match command {
            Command::Load {
                token_path,
                history_path,
                pricing,
            } => {
                state = State::new(write_authorized);
                state.pricing = pricing.clone();
                let started = Instant::now();
                progress(
                    &events,
                    Operation::Load,
                    ProgressStage::Loading,
                    started,
                    None,
                );
                let result = TokenDb::open_or_migrate(&token_path, write_authorized, &mut |p| {
                    let _ = events.send(Event::Progress(Progress {
                        operation: Operation::Load,
                        stage: ProgressStage::Migrating,
                        elapsed: started.elapsed(),
                        files_done: p.done,
                        files_total: p.total,
                        lines_read: 0,
                        observations: 0,
                        events: 0,
                    }));
                });
                let mut error = None;
                match result {Ok(Some(db))=>match db.header(){Ok(header)=>{state.store=TokenStore::from_header(header);state.db=Some(db)},Err(e)=>error=Some(e.to_string())},Ok(None)=>error=Some("SQLite migration needs the application's writer lock; original JSON was preserved. Close the other instance and reopen.".into()),Err(e)=>error=Some(e.to_string())}
                state.blocked = error.is_some();
                let history_error =
                    history_path.as_ref().and_then(|path| {
                        match storage::load_json::<HistoryFile>(path) {
                            Ok(file) if matches!(file.version, 1 | 2) => {
                                if file.version == 1 {
                                    state.history_backup = Some(path.clone())
                                };
                                state.raw_history = file.events;
                                None
                            }
                            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                            Ok(_) => {
                                state.history_blocked = true;
                                Some("Unsupported history version; original preserved".into())
                            }
                            Err(e) => {
                                state.history_blocked = true;
                                Some(format!("history.json: {e}"))
                            }
                        }
                    });
                state.normalized = Arc::new(history::normalized(&state.raw_history));
                state.merge(history::replay(state.store.quota_observations()));
                if error.is_none() && write_authorized {
                    if let Err(e) = state
                        .reprice(&pricing)
                        .and_then(|_| state.commit().map(|_| ()))
                    {
                        error = Some(e.to_string());
                        state.blocked = true;
                    }
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
                let mut history_changed = false;
                if result.is_ok() && state.store.quota_observations().len() != old_quota {
                    history_changed =
                        state.merge(history::replay(state.store.quota_observations()));
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
                let _ = events.send(Event::HistoryUpdated {
                    normalized: changed.then(|| state.normalized.clone()),
                });
                if changed {
                    state.save_history(save_path.as_deref(), &events);
                    state.emit_intervals(&events, save_path.as_deref());
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
