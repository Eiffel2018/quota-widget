//! Background ownership of token import and state-file serialization.
//!
//! The GUI owns the process's `DataLock` and decides which commands may write.
//! Passing a save path authorizes that state file and its sibling, rebuildable
//! `quota-intervals.json` snapshot; read-only paths never authorize writes. Keep
//! the lock alive until `shutdown` (or `Drop`) has joined the worker, so queued
//! atomic replacements cannot race a subsequently started instance.
use crate::{
    accounting,
    cancellation::{self, CancellationToken},
    history,
    intervals::{CompletedInterval, IntervalIndex},
    model::HistoryEvent,
    storage::{self, Settings},
    tokens::{
        ImportPhase, ImportProgress, ImportReport, Pricing, TaskTurnUsage, TokenEvent, TokenStore,
    },
    usage_views,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// The GUI sees display data only, never source cursors or retained raw records.
/// Cloning the owning `Arc` is constant-time; construction happens on the worker.
#[derive(Debug, Default)]
pub struct Snapshot {
    pub database: Option<crate::db_view::DatabaseView>,
    pub revision: u64,
    pub pricing: Pricing,
    /// Default usage surface: normalized events with automatic review excluded.
    pub events: Vec<TokenEvent>,
    /// Retained normalized review records for a clearly separate audit surface.
    pub excluded_events: Vec<TokenEvent>,
    /// Review-only task identities, including identified metadata-only tasks.
    pub excluded_tasks: BTreeSet<String>,
    pub task_names: BTreeMap<String, String>,
    pub task_parents: BTreeMap<String, String>,
    pub task_turn_usage: BTreeMap<String, TaskTurnUsage>,
    pub usage_index: usage_views::UsageIndex,
    pub cost_index: accounting::CostIndex,
}
impl Snapshot {
    pub fn from_store(store: &TokenStore) -> Self {
        let (excluded_events, events): (Vec<_>, Vec<_>) = store
            .events()
            .iter()
            .cloned()
            .partition(TokenEvent::is_excluded_auto_review);
        let excluded_tasks = store.excluded_auto_review_tasks();
        let mut task_parents = store.task_parents();
        // Metadata-only ancestors and inactive descendants participate in tree
        // counts. Remove both endpoints of excluded edges so review-only tasks
        // cannot return through that path. A surviving child is detached, never
        // guessed to belong to the excluded parent's parent.
        task_parents.retain(|child, parent| {
            !excluded_tasks.contains(child) && !excluded_tasks.contains(parent)
        });
        Self {
            database: None,
            revision: store.revision(),
            pricing: store.pricing().cloned().unwrap_or_default(),
            usage_index: usage_views::UsageIndex::new(&events),
            cost_index: accounting::CostIndex::new(&events),
            events,
            excluded_events,
            excluded_tasks,
            task_names: store.task_names(),
            task_parents,
            task_turn_usage: store.included_task_turn_usage(),
        }
    }
    pub fn events(&self) -> &[TokenEvent] {
        &self.events
    }
    pub fn task_name(&self, id: &str) -> Option<&str> {
        self.task_names.get(id).map(String::as_str)
    }
}

#[derive(Debug)]
pub enum Command {
    /// Release the reader connection and try exclusive ownership; return the lock.
    AcquireWriter {
        dir: PathBuf,
    },
    View(crate::db_view::ViewRequest),
    Load {
        token_path: PathBuf,
        history_path: Option<PathBuf>,
        pricing: Pricing,
    },
    Import {
        root: PathBuf,
        metadata: Option<PathBuf>,
        pricing: Pricing,
        /// None means read-only for tokens, including when another process owns
        /// the lock. Either save path also authorizes the sibling interval file.
        save_path: Option<PathBuf>,
        history_save_path: Option<PathBuf>,
    },
    Reprice {
        pricing: Pricing,
        save_path: Option<PathBuf>,
    },
    MergeHistory {
        events: Vec<HistoryEvent>,
        save_path: Option<PathBuf>,
    },
    SaveSettings {
        path: PathBuf,
        settings: Settings,
    },
    /// FIFO: all commands sent before this one finish before the thread exits.
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Load,
    Import,
    Reprice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressStage {
    Loading,
    Migrating,
    Import(ImportPhase),
    BuildingViews,
    BuildingIntervals,
    Saving,
    Complete,
}

/// Small status messages only: progress never publishes provisional accounting
/// or clones the growing token/history data. Counts describe this operation.
#[derive(Debug, Clone)]
pub struct Progress {
    pub operation: Operation,
    pub stage: ProgressStage,
    pub elapsed: Duration,
    pub files_done: usize,
    pub files_total: usize,
    pub lines_read: usize,
    pub observations: usize,
    pub events: usize,
}

#[derive(Debug)]
pub enum Event {
    WriterLock {
        lock: Option<storage::DataLock>,
    },
    /// Opt-in status stream for interactive clients; existing display data stays
    /// valid throughout every stage. Complete follows any authorized writes.
    Progress(Progress),
    Loaded {
        snapshot: Arc<Snapshot>,
        normalized_history: Arc<Vec<HistoryEvent>>,
        history_error: Option<String>,
        error: Option<String>,
    },
    Updated {
        /// Display-ready, not a durability acknowledgement. Persistence follows
        /// this event; SaveFailed reports failure and shutdown flushes the FIFO.
        /// None on a no-op poll, so old snapshots and view caches stay valid.
        snapshot: Option<Arc<Snapshot>>,
        normalized_history: Option<Arc<Vec<HistoryEvent>>>,
        /// Some for an import, None for a reprice.
        report: Option<ImportReport>,
        error: Option<String>,
    },
    HistoryUpdated {
        normalized: Option<Arc<Vec<HistoryEvent>>>,
    },
    /// A worker-built, immutable view of completed quota-decrement intervals.
    /// Sent after the corresponding load/import/history/reprice response, and
    /// only when the derived contents change. Even an empty replacement is sent
    /// when necessary, so a reload cannot leave stale intervals on screen.
    IntervalsUpdated {
        intervals: Arc<Vec<CompletedInterval>>,
    },
    SaveFailed {
        path: PathBuf,
        error: String,
    },
}

pub struct Worker {
    tx: Option<mpsc::Sender<Command>>,
    rx: mpsc::Receiver<Event>,
    join: Option<JoinHandle<()>>,
    cancellation: CancellationToken,
}
impl Worker {
    pub fn spawn_database_with_progress(write_authorized: bool) -> Self {
        Self::spawn_task(move |commands, events| {
            crate::database_worker::run(commands, events, write_authorized)
        })
    }
    fn spawn_task(
        task: impl FnOnce(mpsc::Receiver<Command>, mpsc::Sender<Event>) + Send + 'static,
    ) -> Self {
        let (tx, commands) = mpsc::channel();
        let (events, rx) = mpsc::channel();
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let join = thread::spawn(move || {
            worker_cancellation.scope(|| task(commands, events));
        });
        Self {
            tx: Some(tx),
            rx,
            join: Some(join),
            cancellation,
        }
    }
    pub fn spawn() -> Self {
        Self::spawn_with_sink(false, |store, path| store.save(path))
    }
    /// Include lightweight stage/count events in addition to display snapshots.
    pub fn spawn_with_progress() -> Self {
        Self::spawn_with_sink(true, |store, path| store.save(path))
    }
    fn spawn_with_sink(
        emit_progress: bool,
        save_tokens: impl FnMut(&TokenStore, &Path) -> std::io::Result<()> + Send + 'static,
    ) -> Self {
        Self::spawn_task(move |commands, events| run(commands, events, emit_progress, save_tokens))
    }
    pub fn send(&self, command: Command) -> Result<(), Box<mpsc::SendError<Command>>> {
        match &self.tx {
            Some(tx) => tx.send(command).map_err(Box::new),
            None => Err(Box::new(mpsc::SendError(command))),
        }
    }
    pub fn try_recv(&self) -> Result<Event, mpsc::TryRecvError> {
        self.rx.try_recv()
    }
    /// Request a cooperative, nonblocking close. Current expensive work stops
    /// at a checkpoint; queued scans/views/reprices are skipped. Already accepted
    /// settings and raw-history commands still flush before the thread exits.
    /// Keep the GUI's writer lock alive until `is_finished()` and `shutdown()`.
    pub fn request_cancel_shutdown(&mut self) {
        self.cancellation.cancel();
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Command::Shutdown);
        }
    }

    /// Poll from the UI while it continues repainting during close. Once true,
    /// `shutdown()` only reaps the completed thread and does not wait for work.
    pub fn is_finished(&self) -> bool {
        self.join.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Flush queued work and wait before releasing the GUI's writer lock.
    /// Idempotent; after this returns, no background file writes remain.
    pub fn shutdown(&mut self) -> thread::Result<()> {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Command::Shutdown);
        }
        self.join.take().map_or(Ok(()), JoinHandle::join)
    }
}
impl Default for Worker {
    fn default() -> Self {
        Self::spawn()
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[derive(Deserialize)]
struct LoadedHistory {
    version: u32,
    events: Vec<HistoryEvent>,
}

#[derive(Serialize)]
struct HistoryFile<'a> {
    version: u32,
    events: &'a [HistoryEvent],
}

/// A disposable derived snapshot. Raw token observations and history remain the
/// authoritative persisted inputs; this file is never used to seed accounting.
#[derive(Serialize)]
struct IntervalFile<'a> {
    version: u32,
    intervals: &'a [CompletedInterval],
}

fn send_save_error(events: &mpsc::Sender<Event>, path: PathBuf, error: std::io::Error) {
    let _ = events.send(Event::SaveFailed {
        path,
        error: error.to_string(),
    });
}

/// Drain only accepted control writes during cooperative close. Never consult
/// the canceled importer or derive views. Read the destination history before
/// merging so cancellation of an initial load cannot erase existing evidence.
pub(crate) fn flush_cancelled_command(
    command: Command,
    events: &mpsc::Sender<Event>,
    write_authorized: bool,
    raw_history: &[HistoryEvent],
    loaded_history_path: Option<&Path>,
    saved_settings: &mut Option<(PathBuf, Vec<u8>)>,
) -> bool {
    match command {
        Command::Shutdown => return false,
        Command::SaveSettings { path, settings } if write_authorized => {
            let result = (|| {
                let bytes = serde_json::to_vec(&settings).map_err(std::io::Error::other)?;
                if saved_settings
                    .as_ref()
                    .is_some_and(|(p, b)| p == &path && b == &bytes)
                {
                    return Ok(());
                }
                storage::save_settings(&path, &settings)?;
                *saved_settings = Some((path.clone(), bytes));
                Ok(())
            })();
            if let Err(error) = result {
                send_save_error(events, path, error);
            }
        }
        Command::MergeHistory {
            events: incoming,
            save_path: Some(path),
        } if write_authorized => {
            let result = (|| {
                let (existing, legacy, missing) = match storage::load_json::<LoadedHistory>(&path) {
                    Ok(file) if matches!(file.version, 1 | 2) => {
                        (file.events, file.version == 1, false)
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        (vec![], false, true)
                    }
                    Ok(_) => {
                        return Err(std::io::Error::other(
                            "Unsupported history-file version; original preserved",
                        ))
                    }
                    Err(error) => return Err(error),
                };
                let mut merged = existing.clone();
                if loaded_history_path.is_none_or(|loaded| loaded == path) {
                    merged = history::merge(merged, raw_history.to_vec());
                }
                merged = history::merge(merged, incoming);
                if !missing && !legacy && merged == existing {
                    return Ok(());
                }
                if legacy {
                    storage::backup_legacy_history(&path)?;
                }
                storage::save_json(
                    &path,
                    &HistoryFile {
                        version: 2,
                        events: &merged,
                    },
                )
            })();
            if let Err(error) = result {
                send_save_error(events, path, error);
            }
            let _ = events.send(Event::HistoryUpdated { normalized: None });
        }
        // No accepted save path means there is no authorized write to flush.
        // Remaining heavy commands and display-only history can be abandoned.
        _ => {}
    }
    true
}

struct ProgressReporter<'a> {
    enabled: bool,
    sender: &'a mpsc::Sender<Event>,
    started: Instant,
    progress: Progress,
}
impl<'a> ProgressReporter<'a> {
    fn new(enabled: bool, sender: &'a mpsc::Sender<Event>, operation: Operation) -> Self {
        Self {
            enabled,
            sender,
            started: Instant::now(),
            progress: Progress {
                operation,
                stage: ProgressStage::Loading,
                elapsed: Duration::ZERO,
                files_done: 0,
                files_total: 0,
                lines_read: 0,
                observations: 0,
                events: 0,
            },
        }
    }
    fn stage(&mut self, stage: ProgressStage) {
        self.progress.stage = stage;
        self.progress.elapsed = self.started.elapsed();
        if self.enabled {
            let _ = self.sender.send(Event::Progress(self.progress.clone()));
        }
    }
    fn import(&mut self, progress: ImportProgress) {
        self.progress.files_done = progress.files_done;
        self.progress.files_total = progress.files_total;
        self.progress.lines_read = progress.lines_read;
        self.progress.observations = progress.observations;
        self.progress.events = progress.events;
        self.stage(ProgressStage::Import(progress.phase));
    }
}

/// A failed/corrupt load must never turn an empty replacement store into an
/// overwrite of the original. The GUI also disables all persistence on error.
struct State {
    store: TokenStore,
    published_revision: Option<u64>,
    current_snapshot: Option<Arc<Snapshot>>,
    retired_snapshots: Vec<Arc<Snapshot>>,
    current_history_snapshot: Option<Arc<Vec<HistoryEvent>>>,
    retired_history_snapshots: Vec<Arc<Vec<HistoryEvent>>>,
    interval_inputs: Option<(u64, u64)>,
    interval_index: Option<(u64, IntervalIndex)>,
    current_interval_snapshot: Option<Arc<Vec<CompletedInterval>>>,
    retired_interval_snapshots: Vec<Arc<Vec<CompletedInterval>>>,
    interval_revision: u64,
    saved_interval_revision: Option<(PathBuf, u64)>,
    saved_revision: Option<(PathBuf, u64)>,
    token_writes_blocked: bool,
    raw_history: Vec<HistoryEvent>,
    loaded_history_path: Option<PathBuf>,
    history_revision: u64,
    saved_history_revision: Option<(PathBuf, u64)>,
    history_writes_blocked: bool,
    history_backup_pending: Option<PathBuf>,
}
impl State {
    fn new() -> Self {
        Self {
            store: TokenStore::default(),
            published_revision: None,
            current_snapshot: None,
            retired_snapshots: Vec::new(),
            current_history_snapshot: None,
            retired_history_snapshots: Vec::new(),
            interval_inputs: None,
            interval_index: None,
            current_interval_snapshot: None,
            retired_interval_snapshots: Vec::new(),
            interval_revision: 0,
            saved_interval_revision: None,
            saved_revision: None,
            token_writes_blocked: false,
            raw_history: Vec::new(),
            loaded_history_path: None,
            history_revision: 0,
            saved_history_revision: None,
            history_writes_blocked: false,
            history_backup_pending: None,
        }
    }
    fn snapshot_if_changed(&mut self) -> Option<Arc<Snapshot>> {
        let revision = self.store.revision();
        if self.published_revision == Some(revision) {
            return None;
        }
        Some(self.publish_snapshot())
    }
    fn publish_snapshot(&mut self) -> Arc<Snapshot> {
        let snapshot = Arc::new(Snapshot::from_store(&self.store));
        self.published_revision = Some(snapshot.revision);
        if let Some(old) = self.current_snapshot.replace(Arc::clone(&snapshot)) {
            self.retired_snapshots.push(old);
        }
        snapshot
    }
    fn collect_retired_snapshots(&mut self) {
        // Keep the final owning Arc here, so releasing an old GUI snapshot only
        // decrements a refcount. String/vector/index destruction stays off UI.
        self.retired_snapshots
            .retain(|snapshot| Arc::strong_count(snapshot) > 1);
        self.retired_history_snapshots
            .retain(|snapshot| Arc::strong_count(snapshot) > 1);
        self.retired_interval_snapshots
            .retain(|snapshot| Arc::strong_count(snapshot) > 1);
    }
    fn merge_history(&mut self, incoming: Vec<HistoryEvent>) -> bool {
        if incoming.is_empty() {
            return false;
        }
        let old_count = self.raw_history.len();
        self.raw_history = history::merge(std::mem::take(&mut self.raw_history), incoming);
        let changed = self.raw_history.len() != old_count;
        if changed {
            self.history_revision = self.history_revision.wrapping_add(1);
        }
        changed
    }
    fn normalized_history(&mut self) -> Arc<Vec<HistoryEvent>> {
        let snapshot = Arc::new(history::normalized(&self.raw_history));
        if let Some(old) = self.current_history_snapshot.replace(Arc::clone(&snapshot)) {
            self.retired_history_snapshots.push(old);
        }
        snapshot
    }
    fn intervals_if_changed(&mut self) -> Option<Arc<Vec<CompletedInterval>>> {
        let inputs = (self.store.revision(), self.history_revision);
        if self.interval_inputs == Some(inputs) {
            return None;
        }
        self.interval_inputs = Some(inputs);
        // Both filtered token data and normalized history are already built on
        // this thread. Borrow them; never clone raw token/history vectors into
        // the GUI or compute interval accounting inside a paint callback.
        let token_events = self
            .current_snapshot
            .as_ref()
            .map_or(&[][..], |snapshot| snapshot.events());
        let normalized = self
            .current_history_snapshot
            .as_ref()
            .map_or(&[][..], |snapshot| snapshot.as_slice());
        if self
            .interval_index
            .as_ref()
            .is_none_or(|(revision, _)| *revision != inputs.0)
        {
            self.interval_index = Some((inputs.0, IntervalIndex::new(token_events)));
        }
        let intervals = self
            .interval_index
            .as_ref()
            .expect("interval index was built above")
            .1
            .completed_intervals(normalized);
        if self
            .current_interval_snapshot
            .as_ref()
            .is_some_and(|old| old.as_slice() == intervals.as_slice())
        {
            return None;
        }
        let snapshot = Arc::new(intervals);
        if let Some(old) = self
            .current_interval_snapshot
            .replace(Arc::clone(&snapshot))
        {
            self.retired_interval_snapshots.push(old);
        }
        self.interval_revision = self.interval_revision.wrapping_add(1);
        Some(snapshot)
    }
    fn save_intervals_if_changed(
        &mut self,
        authorized_path: Option<&std::path::Path>,
        events: &mpsc::Sender<Event>,
    ) {
        if cancellation::is_cancelled() {
            return;
        }
        let Some(parent) = authorized_path.and_then(std::path::Path::parent) else {
            return;
        };
        if self.token_writes_blocked || self.history_writes_blocked {
            return;
        }
        let Some(intervals) = self.current_interval_snapshot.as_deref() else {
            return;
        };
        let path = parent.join("quota-intervals.json");
        if self
            .saved_interval_revision
            .as_ref()
            .is_some_and(|(saved_path, revision)| {
                saved_path == &path && *revision == self.interval_revision
            })
        {
            return;
        }
        let file = IntervalFile {
            version: 1,
            intervals,
        };
        match storage::save_json(&path, &file) {
            Ok(()) => {
                self.saved_interval_revision = Some((path, self.interval_revision));
            }
            Err(error) => send_save_error(events, path, error),
        }
    }
    fn save_history_if_changed(&mut self, path: Option<PathBuf>, events: &mpsc::Sender<Event>) {
        let Some(path) = path else { return };
        if self.token_writes_blocked || self.history_writes_blocked {
            return;
        }
        if self
            .saved_history_revision
            .as_ref()
            .is_some_and(|(old_path, revision)| {
                old_path == &path && *revision == self.history_revision
            })
        {
            return;
        }
        if self.history_backup_pending.as_ref() == Some(&path) {
            if let Err(error) = storage::backup_legacy_history(&path) {
                self.history_writes_blocked = true;
                send_save_error(
                    events,
                    path,
                    std::io::Error::other(format!(
                        "Legacy history backup failed; original preserved: {error}"
                    )),
                );
                return;
            }
            self.history_backup_pending = None;
        }
        let file = HistoryFile {
            version: 2,
            events: &self.raw_history,
        };
        match storage::save_json(&path, &file) {
            Ok(()) => self.saved_history_revision = Some((path, self.history_revision)),
            Err(error) => send_save_error(events, path, error),
        }
    }
    fn save_if_changed(
        &mut self,
        path: Option<PathBuf>,
        events: &mpsc::Sender<Event>,
        save_tokens: &mut dyn FnMut(&TokenStore, &Path) -> std::io::Result<()>,
    ) {
        let Some(path) = path else { return };
        if cancellation::is_cancelled() || self.token_writes_blocked || self.history_writes_blocked
        {
            return;
        }
        let revision = self.store.revision();
        if self
            .saved_revision
            .as_ref()
            .is_some_and(|(old_path, saved)| old_path == &path && *saved == revision)
        {
            return;
        }
        match save_tokens(&self.store, &path) {
            Ok(()) => self.saved_revision = Some((path, revision)),
            Err(error) => send_save_error(events, path, error),
        }
    }
}

fn run(
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    emit_progress: bool,
    mut save_tokens: impl FnMut(&TokenStore, &Path) -> std::io::Result<()>,
) {
    let mut state = State::new();
    // A second worker-side guard against callers enqueueing unchanged settings.
    // No serialization occurs in send().
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
                true,
                &state.raw_history,
                state.loaded_history_path.as_deref(),
                &mut saved_settings,
            ) {
                break;
            }
            continue;
        }
        state.collect_retired_snapshots();
        match command {
            Command::Load {
                token_path,
                history_path,
                pricing,
            } => {
                let mut progress = ProgressReporter::new(emit_progress, &events, Operation::Load);
                progress.stage(ProgressStage::Loading);
                // A new load is a replacement, including failed loads. Never
                // carry old account data forward after switching data paths.
                state.store = TokenStore::default();
                state.saved_revision = None;
                state.interval_inputs = None;
                state.interval_index = None;
                state.saved_interval_revision = None;
                let error = match TokenStore::load(&token_path) {
                    Ok(store) => {
                        state.store = store;
                        state.token_writes_blocked = false;
                        state.saved_revision = state
                            .store
                            .persisted_revision_at_load()
                            .map(|revision| (token_path, revision));
                        None
                    }
                    Err(error) => {
                        state.token_writes_blocked = true;
                        Some(format!("tokens.json: {error}"))
                    }
                };
                state.raw_history.clear();
                state.loaded_history_path = history_path.clone();
                state.history_revision = 0;
                state.saved_history_revision = None;
                state.history_writes_blocked = false;
                state.history_backup_pending = None;
                if cancellation::is_cancelled() {
                    continue;
                }
                let history_error = history_path.and_then(|path| {
                    match storage::load_json::<LoadedHistory>(&path) {
                        Ok(file) if file.version == 1 || file.version == 2 => {
                            if file.version == 1 {
                                state.history_backup_pending = Some(path.clone());
                            }
                            state.raw_history = file.events;
                            state.saved_history_revision = Some((path, 0));
                            None
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Ok(_) => {
                            state.history_writes_blocked = true;
                            Some("history.json: Unsupported history-file version; original preserved".into())
                        }
                        Err(error) => {
                            state.history_writes_blocked = true;
                            Some(format!("history.json: {error}"))
                        }
                    }
                });
                if cancellation::is_cancelled() {
                    continue;
                }
                progress.progress.events = state.store.events().len();
                progress.stage(ProgressStage::BuildingViews);
                let imported = history::replay(state.store.quota_observations());
                state.merge_history(imported);
                let normalized_history = state.normalized_history();
                if state.store.reprice_checked(&pricing).is_err() {
                    continue;
                }
                if cancellation::is_cancelled() {
                    continue;
                }
                let snapshot = state.publish_snapshot();
                let _ = events.send(Event::Loaded {
                    snapshot,
                    normalized_history,
                    history_error,
                    error,
                });
                // Make cached records usable before calculating derived quota
                // intervals, which can be expensive on a large history.
                progress.progress.events = state.store.events().len();
                if cancellation::is_cancelled() {
                    continue;
                }
                progress.stage(ProgressStage::BuildingIntervals);
                if let Some(intervals) = state.intervals_if_changed() {
                    let _ = events.send(Event::IntervalsUpdated { intervals });
                }
                progress.stage(ProgressStage::Complete);
            }
            Command::Import {
                root,
                metadata,
                pricing,
                save_path,
                history_save_path,
            } => {
                let mut progress = ProgressReporter::new(emit_progress, &events, Operation::Import);
                let previous_quota_count = state.store.quota_observations().len();
                let (report, error) = match state.store.import_dir_with_metadata_progress(
                    &root,
                    metadata.as_deref(),
                    &pricing,
                    &mut |update| progress.import(update),
                ) {
                    Ok(report) => (Some(report), None),
                    Err(error) => (None, Some(error.to_string())),
                };
                if cancellation::is_cancelled() {
                    continue;
                }
                progress.progress.events = state.store.events().len();
                progress.stage(ProgressStage::BuildingViews);
                let snapshot = state.snapshot_if_changed();
                if cancellation::is_cancelled() {
                    continue;
                }
                let quota_changed = state.store.quota_observations().len() != previous_quota_count;
                let normalized_history = if quota_changed {
                    let imported = history::replay(state.store.quota_observations());
                    state
                        .merge_history(imported)
                        .then(|| state.normalized_history())
                } else {
                    None
                };
                let _ = events.send(Event::Updated {
                    snapshot,
                    normalized_history,
                    report,
                    error,
                });
                if cancellation::is_cancelled() {
                    continue;
                }
                progress.stage(ProgressStage::BuildingIntervals);
                let intervals = state.intervals_if_changed();
                if save_path.is_some() || history_save_path.is_some() {
                    progress.stage(ProgressStage::Saving);
                }
                state.save_if_changed(save_path.clone(), &events, &mut save_tokens);
                state.save_history_if_changed(history_save_path.clone(), &events);
                state.save_intervals_if_changed(
                    history_save_path.as_deref().or(save_path.as_deref()),
                    &events,
                );
                if let Some(intervals) = intervals {
                    let _ = events.send(Event::IntervalsUpdated { intervals });
                }
                progress.stage(ProgressStage::Complete);
            }
            Command::Reprice { pricing, save_path } => {
                let mut progress =
                    ProgressReporter::new(emit_progress, &events, Operation::Reprice);
                progress.progress.events = state.store.events().len();
                progress.stage(ProgressStage::BuildingViews);
                if state.store.reprice_checked(&pricing).is_err() {
                    continue;
                }
                if cancellation::is_cancelled() {
                    continue;
                }
                let snapshot = state.snapshot_if_changed();
                let _ = events.send(Event::Updated {
                    snapshot,
                    normalized_history: None,
                    report: None,
                    error: None,
                });
                if cancellation::is_cancelled() {
                    continue;
                }
                progress.stage(ProgressStage::BuildingIntervals);
                let intervals = state.intervals_if_changed();
                if save_path.is_some() {
                    progress.stage(ProgressStage::Saving);
                }
                state.save_if_changed(save_path.clone(), &events, &mut save_tokens);
                state.save_intervals_if_changed(save_path.as_deref(), &events);
                if let Some(intervals) = intervals {
                    let _ = events.send(Event::IntervalsUpdated { intervals });
                }
                progress.stage(ProgressStage::Complete);
            }
            Command::MergeHistory {
                events: incoming,
                save_path,
            } => {
                let normalized = state
                    .merge_history(incoming)
                    .then(|| state.normalized_history());
                state.save_history_if_changed(save_path.clone(), &events);
                let intervals = if cancellation::is_cancelled() {
                    None
                } else {
                    state.intervals_if_changed()
                };
                state.save_intervals_if_changed(save_path.as_deref(), &events);
                let _ = events.send(Event::HistoryUpdated { normalized });
                if let Some(intervals) = intervals {
                    let _ = events.send(Event::IntervalsUpdated { intervals });
                }
            }
            Command::SaveSettings { path, settings } => {
                if state.token_writes_blocked || state.history_writes_blocked {
                    continue;
                }
                match serde_json::to_vec(&settings).map_err(std::io::Error::other) {
                    Ok(bytes) => {
                        if saved_settings
                            .as_ref()
                            .is_some_and(|(old_path, old)| old_path == &path && old == &bytes)
                        {
                            continue;
                        }
                        match storage::save_settings(&path, &settings) {
                            Ok(()) => saved_settings = Some((path, bytes)),
                            Err(error) => send_save_error(&events, path, error),
                        }
                    }
                    Err(error) => send_save_error(&events, path, error),
                }
            }
            Command::View(_) | Command::AcquireWriter { .. } => {}
            Command::Shutdown => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::Duration};

    fn next(worker: &Worker) -> Event {
        loop {
            let event = worker.rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if !matches!(event, Event::IntervalsUpdated { .. } | Event::Progress(_)) {
                return event;
            }
        }
    }
    fn load(worker: &Worker, path: PathBuf, pricing: Pricing) -> Arc<Snapshot> {
        worker
            .send(Command::Load {
                token_path: path,
                history_path: None,
                pricing,
            })
            .unwrap();
        match next(worker) {
            Event::Loaded {
                snapshot,
                error: None,
                ..
            } => snapshot,
            other => panic!("unexpected load result: {other:?}"),
        }
    }

    fn wait_until_finished(worker: &Worker) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !worker.is_finished() {
            assert!(
                Instant::now() < deadline,
                "worker did not complete cooperative close"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn cancelled_shutdown_skips_queued_heavy_work_but_flushes_controls_for_both_workers() {
        for database in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let token_path = temp.path().join("tokens.json");
            let history_path = temp.path().join("history.json");
            let settings_path = temp.path().join("settings.json");
            fs::write(&token_path, b"synthetic original must remain untouched").unwrap();
            let first = history_event();
            storage::save_json(
                &history_path,
                &HistoryFile {
                    version: 1,
                    events: std::slice::from_ref(&first),
                },
            )
            .unwrap();
            let original_history = fs::read(&history_path).unwrap();
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let mut worker = Worker::spawn_task(move |commands, events| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                if database {
                    crate::database_worker::run(commands, events, true);
                } else {
                    run(commands, events, false, |store, path| store.save(path));
                }
            });
            entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            worker
                .send(Command::Load {
                    token_path: token_path.clone(),
                    history_path: Some(history_path.clone()),
                    pricing: Pricing::default(),
                })
                .unwrap();
            worker
                .send(Command::Import {
                    root: temp.path().join("nonexistent-synthetic-source"),
                    metadata: None,
                    pricing: Pricing::default(),
                    save_path: Some(token_path.clone()),
                    history_save_path: None,
                })
                .unwrap();
            worker
                .send(Command::Reprice {
                    pricing: Pricing::demo(),
                    save_path: Some(token_path.clone()),
                })
                .unwrap();
            worker
                .send(Command::View(crate::db_view::ViewRequest {
                    day_start: 0,
                    day_end: 100,
                    group: usage_views::GroupBy::Response,
                    excluded: false,
                    page: 0,
                    page_size: 10,
                    boundary: None,
                    fallback_start: 0,
                    now: 100,
                }))
                .unwrap();
            for dark in [true, false] {
                worker
                    .send(Command::SaveSettings {
                        path: settings_path.clone(),
                        settings: Settings {
                            dark,
                            sessions_path: temp
                                .path()
                                .join("synthetic-sessions")
                                .to_string_lossy()
                                .into(),
                            ..Settings::default()
                        },
                    })
                    .unwrap();
            }
            let mut second = first.clone();
            second.observed_at_unix += 10;
            let mut third = first.clone();
            third.observed_at_unix += 20;
            for event in [second.clone(), third.clone(), third.clone()] {
                worker
                    .send(Command::MergeHistory {
                        events: vec![event],
                        save_path: Some(history_path.clone()),
                    })
                    .unwrap();
            }
            worker.request_cancel_shutdown();
            worker.request_cancel_shutdown();
            assert!(
                !worker.is_finished(),
                "cancel request must not wait for the gated worker"
            );
            assert!(worker.send(Command::Shutdown).is_err());
            release_tx.send(()).unwrap();
            wait_until_finished(&worker);
            worker.shutdown().unwrap();
            worker.shutdown().unwrap();
            assert!(worker.is_finished());
            assert_eq!(
                fs::read(&token_path).unwrap(),
                b"synthetic original must remain untouched"
            );
            assert!(!crate::token_db::TokenDb::database_path(&token_path).exists());
            assert!(!storage::load_json::<Settings>(&settings_path).unwrap().dark);
            let stored: LoadedHistory = storage::load_json(&history_path).unwrap();
            assert_eq!(stored.version, 2);
            assert_eq!(
                stored.events,
                history::merge(vec![first, second], vec![third])
            );
            let backups: Vec<_> = fs::read_dir(temp.path())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("history.json.pre-0.3.4-")
                })
                .collect();
            assert_eq!(backups.len(), 1);
            assert_eq!(fs::read(&backups[0]).unwrap(), original_history);
            assert!(!temp.path().join("quota-intervals.json").exists());
            assert!(!worker
                .rx
                .try_iter()
                .any(|event| matches!(event, Event::Loaded { .. } | Event::Updated { .. })));
        }
    }

    #[test]
    fn healthy_shutdown_flushes_heavy_work_and_controls_in_fifo_for_both_workers() {
        for database in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let tokens = temp.path().join("tokens.json");
            let history_path = temp.path().join("history.json");
            let settings_path = temp.path().join("settings.json");
            let mut worker = if database {
                Worker::spawn_database_with_progress(true)
            } else {
                Worker::spawn()
            };
            worker
                .send(Command::Load {
                    token_path: tokens.clone(),
                    history_path: None,
                    pricing: Pricing::default(),
                })
                .unwrap();
            worker
                .send(synthetic_import(
                    &temp.path().join("synthetic-source"),
                    Some(tokens.clone()),
                ))
                .unwrap();
            worker
                .send(Command::SaveSettings {
                    path: settings_path.clone(),
                    settings: Settings {
                        dark: false,
                        sessions_path: temp
                            .path()
                            .join("synthetic-source")
                            .to_string_lossy()
                            .into(),
                        ..Settings::default()
                    },
                })
                .unwrap();
            worker
                .send(Command::MergeHistory {
                    events: vec![history_event()],
                    save_path: Some(history_path.clone()),
                })
                .unwrap();
            worker.shutdown().unwrap();
            assert!(worker.is_finished());
            if database {
                let db = crate::token_db::TokenDb::open_readonly(
                    &crate::token_db::TokenDb::database_path(&tokens),
                )
                .unwrap();
                assert_eq!(db.query_events_all(None, None, true).unwrap().len(), 1);
            } else {
                assert_eq!(TokenStore::load(&tokens).unwrap().events().len(), 1);
            }
            assert!(!storage::load_json::<Settings>(&settings_path).unwrap().dark);
            assert_eq!(
                storage::load_json::<LoadedHistory>(&history_path)
                    .unwrap()
                    .events,
                vec![history_event()]
            );
            assert!(worker.rx.try_iter().any(|event| matches!(
                event,
                Event::Updated {
                    report: Some(_),
                    error: None,
                    ..
                }
            )));
        }
    }

    #[test]
    fn cancel_during_save_is_nonblocking_and_never_persists_canceled_tokens() {
        let temp = tempfile::tempdir().unwrap();
        let token_path = temp.path().join("tokens.json");
        let settings_path = temp.path().join("settings.json");
        TokenStore::default().save(&token_path).unwrap();
        let original = fs::read(&token_path).unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut worker = Worker::spawn_with_sink(false, move |store, path| {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            cancellation::check()?;
            store.save(path)
        });
        load(&worker, token_path.clone(), Pricing::default());
        let source = temp.path().join("synthetic-source");
        worker
            .send(synthetic_import(&source, Some(token_path.clone())))
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let source_before: Vec<_> = fs::read_dir(&source)
            .unwrap()
            .map(|e| {
                let path = e.unwrap().path();
                let bytes = fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect();
        worker
            .send(Command::SaveSettings {
                path: settings_path.clone(),
                settings: Settings {
                    dark: false,
                    sessions_path: source.to_string_lossy().into(),
                    ..Settings::default()
                },
            })
            .unwrap();
        worker
            .send(Command::Reprice {
                pricing: Pricing::demo(),
                save_path: Some(token_path.clone()),
            })
            .unwrap();
        worker.request_cancel_shutdown();
        assert!(!worker.is_finished());
        release_tx.send(()).unwrap();
        wait_until_finished(&worker);
        worker.shutdown().unwrap();
        assert_eq!(fs::read(token_path).unwrap(), original);
        assert!(!storage::load_json::<Settings>(&settings_path).unwrap().dark);
        for (path, before) in source_before {
            assert_eq!(fs::read(path).unwrap(), before);
        }
    }

    #[test]
    fn canceled_history_drain_preserves_corrupt_input_but_saves_requested_settings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.json");
        let settings_path = temp.path().join("settings.json");
        fs::write(&path, b"unreadable original raw history").unwrap();
        let (tx, rx) = mpsc::channel();
        let mut saved = None;
        assert!(flush_cancelled_command(
            Command::MergeHistory {
                events: vec![history_event()],
                save_path: Some(path.clone())
            },
            &tx,
            true,
            &[],
            None,
            &mut saved
        ));
        assert!(matches!(rx.try_recv().unwrap(), Event::SaveFailed { .. }));
        assert_eq!(fs::read(path).unwrap(), b"unreadable original raw history");
        flush_cancelled_command(
            Command::SaveSettings {
                path: settings_path.clone(),
                settings: Settings {
                    sessions_path: temp
                        .path()
                        .join("synthetic-sessions")
                        .to_string_lossy()
                        .into(),
                    ..Settings::default()
                },
            },
            &tx,
            true,
            &[],
            None,
            &mut saved,
        );
        assert!(settings_path.is_file());
    }

    #[test]
    fn canceled_history_drain_does_not_merge_evidence_from_another_account() {
        let temp = tempfile::tempdir().unwrap();
        let old_path = temp.path().join("old-account-history.json");
        let new_path = temp.path().join("new-account-history.json");
        let old = history_event();
        let mut incoming = old.clone();
        incoming.session = "synthetic-new-account".into();
        let (events, _) = mpsc::channel();
        flush_cancelled_command(
            Command::MergeHistory {
                events: vec![incoming.clone()],
                save_path: Some(new_path.clone()),
            },
            &events,
            true,
            &[old],
            Some(&old_path),
            &mut None,
        );
        let saved: LoadedHistory = storage::load_json(&new_path).unwrap();
        assert_eq!(saved.events, vec![incoming]);
        assert!(!old_path.exists());
    }

    #[test]
    fn canceled_readonly_database_never_flushes_unauthorized_writes() {
        let temp = tempfile::tempdir().unwrap();
        let (commands, receiver) = mpsc::channel();
        let (events, _) = mpsc::channel();
        let settings = temp.path().join("settings.json");
        let history = temp.path().join("history.json");
        commands
            .send(Command::SaveSettings {
                path: settings.clone(),
                settings: Settings {
                    sessions_path: temp
                        .path()
                        .join("synthetic-sessions")
                        .to_string_lossy()
                        .into(),
                    ..Settings::default()
                },
            })
            .unwrap();
        commands
            .send(Command::MergeHistory {
                events: vec![history_event()],
                save_path: Some(history.clone()),
            })
            .unwrap();
        commands.send(Command::Shutdown).unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        cancellation.scope(|| crate::database_worker::run(receiver, events, false));
        assert!(!settings.exists());
        assert!(!history.exists());
    }

    #[test]
    fn background_noop_import_reuses_snapshot_and_does_not_rewrite_tokens() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("synthetic-sessions");
        fs::create_dir(&root).unwrap();
        let path = temp.path().join("tokens.json");
        let mut worker = Worker::spawn();
        load(&worker, path.clone(), Pricing::default());
        let import = || Command::Import {
            root: root.clone(),
            metadata: None,
            pricing: Pricing::default(),
            save_path: Some(path.clone()),
            history_save_path: None,
        };
        worker.send(import()).unwrap();
        match next(&worker) {
            Event::Updated { error: None, .. } => {}
            other => panic!("unexpected import result: {other:?}"),
        }
        // Updated means display-ready. An empty FIFO command is a barrier for
        // the preceding save without changing data or ending the worker.
        worker
            .send(Command::MergeHistory {
                events: vec![],
                save_path: None,
            })
            .unwrap();
        assert!(matches!(
            next(&worker),
            Event::HistoryUpdated { normalized: None }
        ));
        assert!(path.is_file());
        // If the idle poll tried a write it would recreate this file. Removing
        // only synthetic output avoids relying on filesystem clock precision.
        fs::remove_file(&path).unwrap();
        worker.send(import()).unwrap();
        match next(&worker) {
            Event::Updated {
                snapshot: None,
                error: None,
                report: Some(report),
                ..
            } => assert_eq!(report.lines_read, 0),
            other => panic!("unexpected no-op result: {other:?}"),
        }
        worker.shutdown().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn background_readonly_import_never_creates_token_file() {
        let temp = tempfile::tempdir().unwrap();
        let mut worker = Worker::spawn();
        let path = temp.path().join("tokens.json");
        load(&worker, path.clone(), Pricing::default());
        worker
            .send(Command::Import {
                root: temp.path().to_path_buf(),
                metadata: None,
                pricing: Pricing::demo(),
                save_path: None,
                history_save_path: None,
            })
            .unwrap();
        assert!(matches!(next(&worker), Event::Updated { error: None, .. }));
        worker.shutdown().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn background_shutdown_flushes_queued_settings_and_history() {
        let temp = tempfile::tempdir().unwrap();
        let settings_path = temp.path().join("settings.json");
        let history_path = temp.path().join("history.json");
        let mut worker = Worker::spawn();
        let settings = Settings {
            dark: false,
            ..Settings::default()
        };
        worker
            .send(Command::SaveSettings {
                path: settings_path.clone(),
                settings,
            })
            .unwrap();
        worker
            .send(Command::MergeHistory {
                events: Vec::new(),
                save_path: Some(history_path.clone()),
            })
            .unwrap();
        worker.shutdown().unwrap();
        assert!(!storage::load_json::<Settings>(&settings_path).unwrap().dark);
        let history: serde_json::Value = storage::load_json(&history_path).unwrap();
        assert_eq!(history["version"], 2);
        assert_eq!(history["events"], serde_json::json!([]));
        assert!(worker.send(Command::Shutdown).is_err());
        worker.shutdown().unwrap();
    }

    #[test]
    fn background_corrupt_load_blocks_token_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tokens.json");
        fs::write(&path, "keep the damaged original").unwrap();
        let mut worker = Worker::spawn();
        worker
            .send(Command::Load {
                token_path: path.clone(),
                history_path: None,
                pricing: Pricing::default(),
            })
            .unwrap();
        assert!(matches!(
            next(&worker),
            Event::Loaded { error: Some(_), .. }
        ));
        worker
            .send(Command::Reprice {
                pricing: Pricing::demo(),
                save_path: Some(path.clone()),
            })
            .unwrap();
        assert!(matches!(next(&worker), Event::Updated { .. }));
        worker.shutdown().unwrap();
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "keep the damaged original"
        );
    }

    #[test]
    fn background_identical_reprice_does_not_replace_gui_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let mut worker = Worker::spawn();
        load(&worker, temp.path().join("tokens.json"), Pricing::demo());
        worker
            .send(Command::Reprice {
                pricing: Pricing::demo(),
                save_path: None,
            })
            .unwrap();
        assert!(matches!(
            next(&worker),
            Event::Updated { snapshot: None, .. }
        ));
        worker.shutdown().unwrap();
    }

    fn history_event() -> HistoryEvent {
        serde_json::from_value(serde_json::json!({
            "session": "synthetic-live-session",
            "observed_at_unix": 1000,
            "source_key": "codex",
            "epoch": 0,
            "kind": "QuotaDecrease",
            "window_label": "week",
            "window_key": "primary",
            "before": "92",
            "after": "91",
            "delta": "1",
            "partial": false,
            "before_observed_at_unix": 995
        }))
        .unwrap()
    }

    #[test]
    fn background_history_keeps_raw_evidence_and_skips_duplicate_write() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.json");
        let mut worker = Worker::spawn();
        worker
            .send(Command::MergeHistory {
                events: vec![history_event()],
                save_path: Some(path.clone()),
            })
            .unwrap();
        assert!(matches!(
            next(&worker),
            Event::HistoryUpdated {
                normalized: Some(_)
            }
        ));
        let stored: LoadedHistory = storage::load_json(&path).unwrap();
        assert_eq!(stored.events, vec![history_event()]);
        fs::remove_file(&path).unwrap();
        worker
            .send(Command::MergeHistory {
                events: vec![history_event()],
                save_path: Some(path.clone()),
            })
            .unwrap();
        assert!(matches!(
            next(&worker),
            Event::HistoryUpdated { normalized: None }
        ));
        worker.shutdown().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn background_invalid_history_version_preserves_original_and_blocks_writes() {
        let temp = tempfile::tempdir().unwrap();
        let history_path = temp.path().join("history.json");
        let token_path = temp.path().join("tokens.json");
        let settings_path = temp.path().join("settings.json");
        let original = r#"{"version":999,"events":[]}"#;
        fs::write(&history_path, original).unwrap();
        let mut worker = Worker::spawn();
        worker
            .send(Command::Load {
                token_path: token_path.clone(),
                history_path: Some(history_path.clone()),
                pricing: Pricing::default(),
            })
            .unwrap();
        assert!(matches!(
            next(&worker),
            Event::Loaded {
                history_error: Some(_),
                ..
            }
        ));
        worker
            .send(Command::MergeHistory {
                events: vec![history_event()],
                save_path: Some(history_path.clone()),
            })
            .unwrap();
        worker
            .send(Command::Reprice {
                pricing: Pricing::demo(),
                save_path: Some(token_path.clone()),
            })
            .unwrap();
        worker
            .send(Command::SaveSettings {
                path: settings_path.clone(),
                settings: Settings::default(),
            })
            .unwrap();
        worker.shutdown().unwrap();
        assert_eq!(fs::read_to_string(history_path).unwrap(), original);
        assert!(!token_path.exists());
        assert!(!settings_path.exists());
    }

    #[test]
    fn background_drop_flushes_without_an_explicit_shutdown() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        {
            let worker = Worker::spawn();
            worker
                .send(Command::SaveSettings {
                    path: path.clone(),
                    settings: Settings::default(),
                })
                .unwrap();
        }
        assert!(storage::load_json::<Settings>(&path).is_ok());
    }

    #[test]
    fn background_retains_old_snapshots_until_gui_releases_them() {
        let mut state = State::new();
        let old = state.publish_snapshot();
        state.store.reprice(&Pricing::demo());
        let current = state.snapshot_if_changed().unwrap();
        assert_eq!(Arc::strong_count(&old), 2);
        assert_eq!(Arc::strong_count(&current), 2);
        state.collect_retired_snapshots();
        assert_eq!(state.retired_snapshots.len(), 1);
        drop(old);
        state.collect_retired_snapshots();
        assert!(state.retired_snapshots.is_empty());
        assert_eq!(Arc::strong_count(&current), 2);
    }

    #[test]
    fn background_retains_retired_intervals_and_skips_unchanged_input_work() {
        let mut state = State::new();
        let initial = state.intervals_if_changed().unwrap();
        assert!(initial.is_empty());
        assert!(state.intervals_if_changed().is_none());
        assert!(Arc::ptr_eq(
            &initial,
            state.current_interval_snapshot.as_ref().unwrap()
        ));
        state.merge_history(vec![history_event()]);
        state.normalized_history();
        let current = state.intervals_if_changed().unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(Arc::strong_count(&initial), 2);
        assert_eq!(Arc::strong_count(&current), 2);
        state.collect_retired_snapshots();
        assert_eq!(state.retired_interval_snapshots.len(), 1);
        drop(initial);
        state.collect_retired_snapshots();
        assert!(state.retired_interval_snapshots.is_empty());
        assert_eq!(Arc::strong_count(&current), 2);
    }

    fn synthetic_import(root: &Path, save_path: Option<PathBuf>) -> Command {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("response.jsonl"),
            concat!(
                "{\"type\":\"token_usage_record\",\"timestamp\":1791158400,\"payload\":{",
                "\"thread_id\":\"synthetic-thread\",\"response_id\":\"synthetic-response\",",
                "\"usage\":{\"input_tokens\":1000,\"output_tokens\":10}}}\n"
            ),
        )
        .unwrap();
        Command::Import {
            root: root.to_owned(),
            metadata: None,
            pricing: Pricing::default(),
            save_path,
            history_save_path: None,
        }
    }

    #[test]
    fn repaired_load_persists_once_on_authorized_unchanged_import() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("logs");
        let path = temp.path().join("tokens.json");
        // Build a synthetic EOF cursor, then remove only its derived cache.
        let _ = synthetic_import(&root, None);
        let pricing = Pricing::default();
        let mut original = TokenStore::default();
        original.import_dir(&root, &pricing).unwrap();
        assert_eq!(original.events().len(), 1);
        let mut payload = serde_json::to_value(&original).unwrap();
        payload["events"] = serde_json::json!([]);
        let original_bytes = serde_json::to_vec(&payload).unwrap();
        fs::write(&path, &original_bytes).unwrap();
        let writes = Arc::new(AtomicUsize::new(0));

        // Read-only recovery preserves the original. The next authorized scan
        // persists the repair once; a healthy restart needs no further write.
        for (save_path, expected_writes) in
            [(None, 0), (Some(path.clone()), 1), (Some(path.clone()), 1)]
        {
            let count = Arc::clone(&writes);
            let mut worker = Worker::spawn_with_sink(false, move |store, path| {
                count.fetch_add(1, Ordering::SeqCst);
                store.save(path)
            });
            worker
                .send(Command::Load {
                    token_path: path.clone(),
                    history_path: None,
                    pricing: pricing.clone(),
                })
                .unwrap();
            for _ in 0..2 {
                worker
                    .send(Command::Import {
                        root: root.clone(),
                        metadata: None,
                        pricing: pricing.clone(),
                        save_path: save_path.clone(),
                        history_save_path: None,
                    })
                    .unwrap();
            }
            worker.shutdown().unwrap();
            match next(&worker) {
                Event::Loaded {
                    snapshot,
                    error: None,
                    ..
                } => assert_eq!(snapshot.events.len(), 1),
                other => panic!("unexpected repaired load: {other:?}"),
            }
            for _ in 0..2 {
                match next(&worker) {
                    Event::Updated {
                        report: Some(report),
                        error: None,
                        ..
                    } => {
                        assert_eq!(report.lines_read, 0);
                        assert_eq!(report.files_unchanged, 1);
                    }
                    other => panic!("unexpected unchanged import: {other:?}"),
                }
            }
            assert_eq!(writes.load(Ordering::SeqCst), expected_writes);
            if save_path.is_none() {
                assert_eq!(fs::read(&path).unwrap(), original_bytes);
            } else {
                let saved: serde_json::Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                assert_eq!(saved["events"].as_array().unwrap().len(), 1);
                assert!(saved.get("persisted_revision_at_load").is_none());
                assert_eq!(saved["revision"], original.revision() + 1);
            }
        }
    }

    #[test]
    fn display_snapshot_precedes_slow_save_and_shutdown_flushes_queued_commands() {
        let temp = tempfile::tempdir().unwrap();
        let token_path = temp.path().join("state/tokens.json");
        let settings_path = temp.path().join("state/settings.json");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut worker = Worker::spawn_with_sink(false, move |store, path| {
            entered_tx.send(()).unwrap();
            // A deterministic slow sink, not an unreliable large-file timing test.
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            store.save(path)
        });
        worker
            .send(synthetic_import(
                &temp.path().join("logs"),
                Some(token_path.clone()),
            ))
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        match worker.try_recv().unwrap() {
            Event::Updated {
                snapshot: Some(snapshot),
                report: Some(report),
                error: None,
                ..
            } => {
                assert_eq!(snapshot.events.len(), 1);
                assert_eq!(report.events_added, 1);
            }
            other => panic!("display was not published before save: {other:?}"),
        }
        assert!(!token_path.exists());
        worker
            .send(Command::SaveSettings {
                path: settings_path.clone(),
                settings: Settings {
                    dark: false,
                    ..Settings::default()
                },
            })
            .unwrap();
        let (closed_tx, closed_rx) = mpsc::channel();
        let joining = thread::spawn(move || {
            worker.shutdown().unwrap();
            closed_tx.send(()).unwrap();
        });
        assert!(matches!(
            closed_rx.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        release_tx.send(()).unwrap();
        closed_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        joining.join().unwrap();
        assert_eq!(TokenStore::load(token_path).unwrap().events().len(), 1);
        assert!(!storage::load_json::<Settings>(&settings_path).unwrap().dark);
    }

    #[test]
    fn save_failure_is_separate_and_follows_successful_display_publication() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tokens.json");
        let mut worker = Worker::spawn_with_sink(false, |_, _| {
            Err(std::io::Error::other("synthetic disk unavailable"))
        });
        worker
            .send(synthetic_import(
                &temp.path().join("logs"),
                Some(path.clone()),
            ))
            .unwrap();
        assert!(matches!(
            next(&worker),
            Event::Updated {
                snapshot: Some(_),
                report: Some(_),
                error: None,
                ..
            }
        ));
        match next(&worker) {
            Event::SaveFailed {
                path: failed,
                error,
            } => {
                assert_eq!(failed, path);
                assert!(error.contains("synthetic disk unavailable"));
            }
            other => panic!("missing separate save error: {other:?}"),
        }
        worker.shutdown().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn cached_load_is_published_before_interval_stage_and_queued_scan() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tokens.json");
        let cached = TokenStore::demo(1_791_158_400);
        let expected = Snapshot::from_store(&cached).events.len();
        cached.save(&path).unwrap();
        let mut worker = Worker::spawn_with_progress();
        worker
            .send(Command::Load {
                token_path: path,
                history_path: None,
                pricing: Pricing::demo(),
            })
            .unwrap();
        worker
            .send(synthetic_import(&temp.path().join("logs"), None))
            .unwrap();
        worker.shutdown().unwrap();
        let events: Vec<_> = worker.rx.try_iter().collect();
        let loaded = events
            .iter()
            .position(|e| matches!(e, Event::Loaded { .. }))
            .unwrap();
        match &events[loaded] {
            Event::Loaded {
                snapshot,
                error: None,
                ..
            } => assert_eq!(snapshot.events.len(), expected),
            other => panic!("failed synthetic cache load: {other:?}"),
        }
        let interval_stage = events
            .iter()
            .position(|e| {
                matches!(
                    e,
                    Event::Progress(Progress {
                        operation: Operation::Load,
                        stage: ProgressStage::BuildingIntervals,
                        ..
                    })
                )
            })
            .unwrap();
        let scan = events
            .iter()
            .position(|e| {
                matches!(
                    e,
                    Event::Progress(Progress {
                        operation: Operation::Import,
                        ..
                    })
                )
            })
            .unwrap();
        assert!(loaded < interval_stage && interval_stage < scan);
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Progress(Progress {
                operation: Operation::Import,
                stage: ProgressStage::Complete,
                files_done: 1,
                lines_read: 1,
                ..
            })
        )));
    }
}
