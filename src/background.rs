//! Background ownership of token import and state-file serialization.
//!
//! The GUI owns the process's `DataLock` and decides which commands may write.
//! Passing a save path authorizes that state file and its sibling, rebuildable
//! `quota-intervals.json` snapshot; read-only paths never authorize writes. Keep
//! the lock alive until `shutdown` (or `Drop`) has joined the worker, so queued
//! atomic replacements cannot race a subsequently started instance.
use crate::{
    accounting, history,
    intervals::{CompletedInterval, IntervalIndex},
    model::HistoryEvent,
    storage::{self, Settings},
    tokens::{ImportReport, Pricing, TaskTurnUsage, TokenEvent, TokenStore},
    usage_views,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{mpsc, Arc},
    thread::{self, JoinHandle},
};

/// The GUI sees display data only, never source cursors or retained raw records.
/// Cloning the owning `Arc` is constant-time; construction happens on the worker.
#[derive(Debug, Default)]
pub struct Snapshot {
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

#[derive(Debug)]
pub enum Event {
    Loaded {
        snapshot: Arc<Snapshot>,
        normalized_history: Arc<Vec<HistoryEvent>>,
        history_error: Option<String>,
        error: Option<String>,
    },
    Updated {
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
}
impl Worker {
    pub fn spawn() -> Self {
        let (tx, commands) = mpsc::channel();
        let (events, rx) = mpsc::channel();
        let join = thread::spawn(move || run(commands, events));
        Self {
            tx: Some(tx),
            rx,
            join: Some(join),
        }
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
    fn save_if_changed(&mut self, path: Option<PathBuf>, events: &mpsc::Sender<Event>) {
        let Some(path) = path else { return };
        if self.token_writes_blocked || self.history_writes_blocked {
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
        match self.store.save(&path) {
            Ok(()) => self.saved_revision = Some((path, revision)),
            Err(error) => send_save_error(events, path, error),
        }
    }
}

fn run(commands: mpsc::Receiver<Command>, events: mpsc::Sender<Event>) {
    let mut state = State::new();
    // A second worker-side guard against callers enqueueing unchanged settings.
    // No serialization occurs in send().
    let mut saved_settings: Option<(PathBuf, Vec<u8>)> = None;
    while let Ok(command) = commands.recv() {
        state.collect_retired_snapshots();
        match command {
            Command::Load {
                token_path,
                history_path,
                pricing,
            } => {
                // A new load is a replacement, including failed loads. Never
                // carry old account data forward after switching data paths.
                state.store = TokenStore::default();
                state.saved_revision = None;
                state.interval_inputs = None;
                state.interval_index = None;
                state.saved_interval_revision = None;
                let existed = token_path.is_file();
                let error = match TokenStore::load(&token_path) {
                    Ok(store) => {
                        state.store = store;
                        state.token_writes_blocked = false;
                        state.saved_revision =
                            existed.then(|| (token_path, state.store.revision()));
                        None
                    }
                    Err(error) => {
                        state.token_writes_blocked = true;
                        Some(format!("tokens.json: {error}"))
                    }
                };
                state.raw_history.clear();
                state.history_revision = 0;
                state.saved_history_revision = None;
                state.history_writes_blocked = false;
                state.history_backup_pending = None;
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
                let imported = history::replay(state.store.quota_observations());
                state.merge_history(imported);
                let normalized_history = state.normalized_history();
                state.store.reprice(&pricing);
                let snapshot = state.publish_snapshot();
                let intervals = state.intervals_if_changed();
                let _ = events.send(Event::Loaded {
                    snapshot,
                    normalized_history,
                    history_error,
                    error,
                });
                if let Some(intervals) = intervals {
                    let _ = events.send(Event::IntervalsUpdated { intervals });
                }
            }
            Command::Import {
                root,
                metadata,
                pricing,
                save_path,
                history_save_path,
            } => {
                let previous_quota_count = state.store.quota_observations().len();
                let (report, error) =
                    match state
                        .store
                        .import_dir_with_metadata(&root, metadata.as_deref(), &pricing)
                    {
                        Ok(report) => (Some(report), None),
                        Err(error) => (None, Some(error.to_string())),
                    };
                let snapshot = state.snapshot_if_changed();
                let quota_changed = state.store.quota_observations().len() != previous_quota_count;
                let normalized_history = if quota_changed {
                    let imported = history::replay(state.store.quota_observations());
                    state
                        .merge_history(imported)
                        .then(|| state.normalized_history())
                } else {
                    None
                };
                let intervals = state.intervals_if_changed();
                state.save_if_changed(save_path.clone(), &events);
                state.save_history_if_changed(history_save_path.clone(), &events);
                state.save_intervals_if_changed(
                    history_save_path.as_deref().or(save_path.as_deref()),
                    &events,
                );
                let _ = events.send(Event::Updated {
                    snapshot,
                    normalized_history,
                    report,
                    error,
                });
                if let Some(intervals) = intervals {
                    let _ = events.send(Event::IntervalsUpdated { intervals });
                }
            }
            Command::Reprice { pricing, save_path } => {
                state.store.reprice(&pricing);
                let snapshot = state.snapshot_if_changed();
                let intervals = state.intervals_if_changed();
                state.save_if_changed(save_path.clone(), &events);
                state.save_intervals_if_changed(save_path.as_deref(), &events);
                let _ = events.send(Event::Updated {
                    snapshot,
                    normalized_history: None,
                    report: None,
                    error: None,
                });
                if let Some(intervals) = intervals {
                    let _ = events.send(Event::IntervalsUpdated { intervals });
                }
            }
            Command::MergeHistory {
                events: incoming,
                save_path,
            } => {
                let normalized = state
                    .merge_history(incoming)
                    .then(|| state.normalized_history());
                let intervals = state.intervals_if_changed();
                state.save_history_if_changed(save_path.clone(), &events);
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
            if !matches!(event, Event::IntervalsUpdated { .. }) {
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
}
