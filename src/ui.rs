use chrono::{NaiveDate, Utc};
use eframe::egui::{
    self, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2, ViewportBuilder,
    ViewportCommand, ViewportId, WindowLevel,
};
use quota_widget::i18n::{set_language, tr, Language};
use quota_widget::{
    accessibility, accounting,
    background::{
        Command as WorkCommand, Event as WorkEvent, Operation, Progress as WorkProgress,
        ProgressStage, Snapshot, Worker,
    },
    bridge::{Bridge, BridgeCommand, BridgeEvent},
    cancellation::{self, CancellationToken},
    db_view::ViewRequest,
    display_time,
    interval_views::{self, IntervalPresentation},
    intervals::{self, CompletedInterval},
    model::{HistoryEvent, QuotaModel, WindowView},
    quota_estimate::{self, QuotaEstimate},
    storage::{self, DataLock, Settings},
    tokens::{ImportPhase, ModelPrice, Pricing, TokenEvent, TokenStore},
    usage_views::{self, GroupBy},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

const COLLAPSED_HEIGHT: f32 = 158.;

#[cfg(test)]
fn reset_reminder_at(value: &str) -> Option<i64> {
    display_time::parse_local(value, 480)
}

fn reset_reminder_countdown(target: i64, current: i64) -> String {
    let seconds = target.abs_diff(current);
    format!(
        "{}{}:{:02}:{:02}",
        if current > target { ">" } else { "" },
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

fn live_reset_alert(events: &[HistoryEvent], view: &WindowView) -> bool {
    events.iter().any(|event| {
        event.kind == quota_widget::model::HistoryKind::QuotaReset
            && event.source_key == view.source_key
            && event.window_key.as_deref() == Some(view.window_key.as_str())
            && event.reconciliation.is_none()
            && event.before.parse::<f64>().is_ok_and(|value| value < 100.)
            && event.after.parse::<f64>() == Ok(100.)
    })
}

#[cfg(windows)]
fn play_reset_chime() -> bool {
    #[link(name = "winmm")]
    unsafe extern "system" {
        fn PlaySoundW(sound: *const u16, module: *mut std::ffi::c_void, flags: u32) -> i32;
    }
    // Static storage keeps the embedded WAV alive for asynchronous playback.
    static CHIME: &[u8] = include_bytes!("../assets/reset-chime.wav");
    unsafe {
        PlaySoundW(
            CHIME.as_ptr().cast(),
            std::ptr::null_mut(),
            0x0004 | 0x0001 | 0x0002,
        ) != 0
    }
}

#[cfg(not(windows))]
fn play_reset_chime() -> bool {
    false
}

fn widget_target_height(width: f32, details: f32, monitor: Option<f32>) -> f32 {
    let desired = COLLAPSED_HEIGHT * width / 310. + details;
    let maximum = monitor
        .filter(|h| h.is_finite() && *h > 0.)
        .map(|h| (h - 60.).max(COLLAPSED_HEIGHT * width / 310.))
        .unwrap_or(2000.);
    desired.min(maximum)
}

fn prepare_widget_settings(settings: &mut Settings) {
    if settings.compact_layout_version == 0 {
        settings.size[0] *= 2. / 3.;
        settings.compact_layout_version = 1;
    }
    settings.size[0] = settings.size[0].clamp(280., 1000.);
    settings.size[1] = COLLAPSED_HEIGHT * settings.size[0] / 310.;
}

#[derive(Clone, Copy)]
enum WidgetIcon {
    History,
    Tokens,
    Tasks,
    Recent,
    Prediction,
    Settings,
    Close,
}

fn widget_icon(
    ui: &mut egui::Ui,
    rect: Rect,
    icon: WidgetIcon,
    expanded: Option<bool>,
    help: &str,
    foreground: Color32,
    fill: Color32,
) -> egui::Response {
    let response = ui
        .scope(|ui| {
            ui.spacing_mut().interact_size = Vec2::ZERO;
            ui.spacing_mut().button_padding = Vec2::ZERO;
            ui.put(
                rect,
                egui::Button::new(tr(""))
                    .selected(expanded == Some(true))
                    .fill(if expanded == Some(true) {
                        ui.visuals().selection.bg_fill
                    } else {
                        fill
                    })
                    .min_size(rect.size()),
            )
        })
        .inner
        .on_hover_text(tr(help));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, help));
    let side = rect.height() * 0.55;
    let disclosure = matches!(icon, WidgetIcon::Tasks | WidgetIcon::Recent);
    let center = if disclosure && expanded.is_some() {
        Pos2::new(rect.left() + rect.height() * 0.65, rect.center().y)
    } else {
        rect.center()
    };
    let r = Rect::from_center_size(center, Vec2::splat(side));
    let painter = ui.painter();
    let stroke = Stroke::new(1.4_f32, foreground);
    let line = |a: Pos2, b: Pos2| {
        painter.line_segment([a, b], stroke);
    };
    match icon {
        WidgetIcon::History => {
            painter.rect_stroke(r, 1., stroke, egui::StrokeKind::Inside);
            for y in [0.28, 0.5, 0.72] {
                line(
                    Pos2::new(r.left() + side * 0.2, r.top() + side * y),
                    Pos2::new(r.right() - side * 0.2, r.top() + side * y),
                );
            }
        }
        WidgetIcon::Tokens => {
            for (x, h) in [(0.15, 0.4), (0.5, 0.7), (0.85, 1.)] {
                line(
                    Pos2::new(r.left() + side * x, r.bottom()),
                    Pos2::new(r.left() + side * x, r.bottom() - side * h),
                );
            }
        }
        WidgetIcon::Tasks | WidgetIcon::Settings => {
            for y in [0.15, 0.5, 0.85] {
                line(
                    Pos2::new(r.left(), r.top() + side * y),
                    Pos2::new(r.right(), r.top() + side * y),
                );
            }
        }
        WidgetIcon::Recent => {
            painter.circle_stroke(r.center(), side * 0.5, stroke);
            line(r.center(), r.center() - Vec2::new(0., side * 0.3));
            line(r.center(), r.center() + Vec2::new(side * 0.25, 0.));
        }
        WidgetIcon::Prediction => {
            // Embolden only this compact label; retain its requested 80% height.
            let font = FontId::proportional(rect.height() * 0.7 * 0.8);
            for dx in [-0.25, 0.25] {
                painter.text(
                    rect.center() + Vec2::new(dx, 0.),
                    Align2::CENTER_CENTER,
                    "Reset",
                    font.clone(),
                    foreground,
                );
            }
        }
        WidgetIcon::Close => {
            line(r.left_top(), r.right_bottom());
            line(r.right_top(), r.left_bottom());
        }
    }
    if let Some(open) = expanded.filter(|_| disclosure) {
        let c = Pos2::new(rect.right() - rect.height() * 0.5, rect.center().y);
        let d = rect.height() * 0.15;
        let sign = if open { -1. } else { 1. };
        line(c + Vec2::new(-d, -d * sign), c + Vec2::new(0., d * sign));
        line(c + Vec2::new(0., d * sign), c + Vec2::new(d, -d * sign));
    }
    response
}

#[derive(PartialEq)]
struct LiveSelectionKey {
    source: String,
    window: String,
    label: String,
    duration: Option<i64>,
    reset_at: Option<i64>,
    remaining_bits: Option<u64>,
    epoch: u64,
    account: Option<String>,
}
#[derive(PartialEq)]
struct MainCacheKey {
    token_generation: u64,
    history_generation: u64,
    bucket: String,
    window: String,
    minute: i64,
    tree_generation: u64,
    boundary_after: Option<i64>,
    live_selection: Option<LiveSelectionKey>,
}
#[derive(Default)]
struct MainCache {
    key: Option<MainCacheKey>,
    tree: Vec<usage_views::TaskTreeRow>,
    tasks: Vec<usage_views::TaskRow>,
    boundary: Option<HistoryEvent>,
    cost: accounting::ObservedCost,
    loading: bool,
    recent_events: Vec<HistoryEvent>,
    recent_tooltip: String,
    historical_rate: Option<f64>,
}
/// Native window managers may apply a stale minimum height to the first
/// resize request. Constraints are sent first; height converges separately,
/// after geometry/input has gone quiet, with bounded exponential retries.
#[derive(Default)]
struct LayoutConvergence {
    key: Option<(usize, u32, u32)>,
    observed_size: Option<Vec2>,
    quiet_since: Option<Instant>,
    next_retry: Option<Instant>,
    attempts: u8,
    converged: bool,
    pointer_was_down: bool,
}
#[derive(Default)]
struct LayoutPlan {
    minimum: Option<Vec2>,
    size: Option<Vec2>,
    repaint_after: Option<Duration>,
}
impl LayoutConvergence {
    const QUIET: Duration = Duration::from_millis(150);
    const MAX_ATTEMPTS: u8 = 6;
    fn plan(
        &mut self,
        key: (usize, u32, u32),
        actual: Vec2,
        height: f32,
        pointer_down: bool,
        t: Instant,
    ) -> LayoutPlan {
        let mut plan = LayoutPlan::default();
        if self.key != Some(key) {
            self.key = Some(key);
            self.attempts = 0;
            self.next_retry = None;
            self.converged = false;
            self.quiet_since = Some(t);
            plan.minimum = Some(Vec2::new(280., COLLAPSED_HEIGHT * actual.x / 310.));
        }
        if self
            .observed_size
            .is_none_or(|old| (old - actual).abs().max_elem() > 0.5)
        {
            self.observed_size = Some(actual);
            self.quiet_since = Some(t);
        }
        if pointer_down {
            self.quiet_since = Some(t);
            if !self.pointer_was_down {
                self.attempts = 0;
                self.next_retry = None;
            }
        }
        self.pointer_was_down = pointer_down;
        if (actual.y - height).abs() <= 1.5 {
            self.converged = true;
            self.attempts = 0;
            self.next_retry = None;
            return plan;
        }
        if self.converged {
            // Once settled, respect a manual height resize; overflowing details scroll.
            return plan;
        }
        if self.attempts >= Self::MAX_ATTEMPTS {
            return plan;
        }
        let quiet_until = self.quiet_since.unwrap_or(t) + Self::QUIET;
        let ready_at = self
            .next_retry
            .map_or(quiet_until, |retry| retry.max(quiet_until));
        if pointer_down || t < ready_at {
            plan.repaint_after = Some(
                ready_at
                    .saturating_duration_since(t)
                    .max(Duration::from_millis(16)),
            );
            return plan;
        }
        // Preserve the current user-selected width, adjusting only the derived
        // height. Never repeatedly resize in every repaint while dragging.
        plan.size = Some(Vec2::new(actual.x, height));
        self.attempts += 1;
        let delay = Duration::from_millis(200 * (1_u64 << (self.attempts - 1).min(3)));
        self.next_retry = Some(t + delay);
        if self.attempts < Self::MAX_ATTEMPTS {
            plan.repaint_after = Some(delay);
        }
        plan
    }
}
#[derive(Default)]
struct TokenCache {
    key: Option<(u64, NaiveDate, GroupBy, bool, usize)>,
    loading: bool,
    group_count: usize,
    rows: Vec<usage_views::UsageGroup>,
    count: usize,
    input: u64,
    cached: u64,
    output: u64,
    reasoning: u64,
    missing: usize,
    cost: f64,
}
#[derive(Default)]
struct HistoryCache {
    sources_generation: Option<u64>,
    sources: Vec<(String, Option<String>, String)>,
    has_legacy: bool,
    key: Option<HistoryCacheKey>,
    work_key: Option<HistoryCacheKey>,
    rows: Vec<(HistoryEvent, Option<f64>, bool)>,
    weighted: Option<f64>,
    interval_indices: Vec<usize>,
    table_rows: Vec<TableIntervalRow>,
    pending_segments: Vec<DailyQuotaSegment>,
    day_token_indices: Vec<usize>,
    usage_receiver: Option<mpsc::Receiver<std::io::Result<CachedSegmentUsage>>>,
    next_segment: usize,
    weighted_usd: f64,
    weighted_points: f64,
    weighted_samples: usize,
    has_lower_bound: bool,
    calculation_error: Option<String>,
}
type HistoryCacheKey = (NaiveDate, String, bool, bool);
#[derive(Clone)]
struct DailyQuotaSegment {
    event: Option<HistoryEvent>,
    change: String,
    start: i64,
    end: i64,
    points: f64,
}
struct TableIntervalRow {
    interval_index: Option<usize>,
    presentation: IntervalPresentation,
    models: Vec<quota_widget::intervals::ModelIntervalUsage>,
    task_counts: [usize; 4],
}
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedSegmentUsage {
    models: Vec<intervals::ModelIntervalUsage>,
    counts: [usize; 4],
    cost: accounting::ObservedCost,
}
// Cache identity includes calculation version, exact boundaries and source files.
// Old-day summaries do not expire merely because the active cycle receives new usage.
fn segment_cache_path(
    database: &std::path::Path,
    start: i64,
    end: i64,
) -> std::io::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let dir = database
        .parent()
        .ok_or_else(|| std::io::Error::other("Missing data directory"))?;
    let mut cycles = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if let Some(dt) = name
            .strip_prefix("quota-cycle-")
            .and_then(|n| n.strip_suffix(".sqlite3"))
            .and_then(|v| chrono::NaiveDateTime::parse_from_str(v, "%Y%m%d-%H%M%S").ok())
        {
            cycles.push((dt.and_utc().timestamp() - 28800, path));
        }
    }
    cycles.sort();
    let mut sources = Vec::new();
    for (i, (at, path)) in cycles.iter().enumerate() {
        if *at <= end && cycles.get(i + 1).is_none_or(|(next, _)| *next > start) {
            sources.push(path.clone());
        }
    }
    if cycles.last().is_none_or(|(latest, _)| end >= *latest) {
        sources.push(database.to_owned());
        let wal = PathBuf::from(format!("{}-wal", database.display()));
        if wal.exists() {
            sources.push(wal);
        }
    }
    let mut hash = Sha256::new();
    hash.update(format!("segment-response-v1:{start}:{end}"));
    for path in sources {
        let meta = std::fs::metadata(&path)?;
        hash.update(format!("{:?}:{}:{:?}", path, meta.len(), meta.modified()?));
    }
    Ok(dir
        .join("history-cache")
        .join(format!("{:x}.json", hash.finalize())))
}

// One bounded worker for the selected day. Dropping its receiver when changing
// days stops the old calculation after its current query, with no stale UI rows.
fn start_history_usage(
    path: PathBuf,
    segments: Vec<DailyQuotaSegment>,
    writable_cache: bool,
    ctx: egui::Context,
) -> mpsc::Receiver<std::io::Result<CachedSegmentUsage>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut database = None;
        for segment in segments {
            let result = (|| {
                let cache = segment_cache_path(&path, segment.start, segment.end).ok();
                if let Some(usage) = cache
                    .as_ref()
                    .and_then(|cache| storage::load_json::<CachedSegmentUsage>(cache).ok())
                {
                    return Ok(usage);
                }
                if database.is_none() {
                    database = Some(quota_widget::token_db::TokenDb::open_readonly(&path)?);
                }
                let start = segment.start.saturating_add(1);
                let end = segment.end.saturating_add(1);
                let mut events: BTreeMap<String, TokenEvent> =
                    quota_widget::cycle_db::query_archived_events(
                        path.parent().unwrap_or(std::path::Path::new(".")),
                        start,
                        end,
                    )?
                    .into_iter()
                    .map(|event| (event.id.clone(), event))
                    .collect();
                for event in
                    database
                        .as_ref()
                        .unwrap()
                        .query_events(Some(start), Some(end), false)?
                {
                    events.insert(event.id.clone(), event);
                }
                let mut events: Vec<_> = events.into_values().collect();
                events.sort_by(|a, b| (a.timestamp, &a.id).cmp(&(b.timestamp, &b.id)));
                let refs: Vec<_> = events.iter().collect();
                let usage = CachedSegmentUsage {
                    models: intervals::model_usage(refs.iter().copied()),
                    counts: interval_task_counts(&refs),
                    cost: accounting::CostIndex::new(&events).observed(segment.start, segment.end),
                };
                if let Some(cache) = cache.as_ref().filter(|_| writable_cache) {
                    let _ = storage::save_json(cache, &usage);
                }
                Ok(usage)
            })();
            let failed = result.is_err();
            if sender.send(result).is_err() {
                break;
            }
            ctx.request_repaint();
            if failed {
                break;
            }
        }
    });
    receiver
}

struct WidgetApp {
    settings: Settings,
    dir: PathBuf,
    worker: Worker,
    database_mode: bool,
    requested_view: Option<ViewRequest>,
    view_pending: bool,
    _lock: Option<DataLock>,
    persistent: bool,
    read_state: bool,
    demo: bool,
    offline: bool,
    quota: QuotaModel,
    tokens: Arc<Snapshot>,
    history: Arc<Vec<HistoryEvent>>,
    intervals: Arc<Vec<CompletedInterval>>,
    interval_lookup: BTreeMap<String, usize>,
    interval_presentations: Vec<IntervalPresentation>,
    interval_estimates: Vec<Option<QuotaEstimate>>,
    history_intervals: bool,
    selected_interval: Option<String>,
    token_generation: u64,
    history_generation: u64,
    expanded_tasks: BTreeSet<String>,
    tree_generation: u64,
    boundary_after: Option<i64>,
    main_cache: MainCache,
    token_cache: TokenCache,
    history_cache: HistoryCache,
    dates_key: Option<(u64, u64, NaiveDate, u32)>,
    dates_cache: BTreeSet<NaiveDate>,
    layout: LayoutConvergence,
    bridge: Option<Bridge>,
    quota_retry_at: Option<Instant>,
    status: String,
    quota_error: Option<String>,
    tasks_expanded: bool,
    recent_expanded: bool,
    history_open: bool,
    history_selection: Option<(String, Option<String>, String)>,
    tokens_open: bool,
    settings_open: bool,
    settings_presented: bool,
    prediction_open: bool,
    prediction_presented: bool,
    prediction_input: String,
    prediction_pst: bool,
    prediction_history_checked: Option<(u64, i64, i64)>,
    date: NaiveDate,
    token_date: NaiveDate,
    token_group: GroupBy,
    token_page: usize,
    show_excluded_records: bool,
    import_pending: bool,
    load_pending: bool,
    worker_failed: bool,
    work_progress: Option<(WorkProgress, Instant)>,
    last_import_elapsed: Option<Duration>,
    load_error: Option<String>,
    import_error: Option<String>,
    import_warnings: Vec<String>,
    export_rx: Option<mpsc::Receiver<String>>,
    export_join: Option<std::thread::JoinHandle<()>>,
    export_cancel: Option<CancellationToken>,
    closing_since: Option<Instant>,
    close_ready: bool,
    lock_retry_pending: bool,
    last_lock_retry: Instant,
    last_import: Instant,
    last_save: Instant,
    last_topmost: Instant,
    notice: String,
    import_note: String,
    price_model: String,
    price_models_generation: Option<u64>,
    recorded_price_models: BTreeSet<String>,
    price_values: [f64; 3],
    price_configured: bool,
    last_focus: bool,
    last_minimized: bool,
    dirty: bool,
    demo_start: i64,
}
fn main_view_matches(
    request: &ViewRequest,
    boundary: Option<i64>,
    fallback_start: i64,
    t: i64,
) -> bool {
    request.boundary == boundary
        && request.fallback_start == fallback_start
        && request.now / 60 == t / 60
}
fn daily_view_matches(
    request: &ViewRequest,
    day: NaiveDate,
    group: GroupBy,
    excluded: bool,
    page: usize,
) -> bool {
    display_time::day_bounds(day)
        .is_some_and(|(start, end)| request.day_start == start && request.day_end == end)
        && request.group == group
        && request.excluded == excluded
        && request.page == page
        && request.page_size == 500
}
fn canonical_interval_cost(interval: Option<&CompletedInterval>) -> (Option<f64>, bool) {
    interval.map_or((None, true), |row| {
        (
            (row.events > row.unknown_cost_events || row.unknown_cost_events == 0)
                .then_some(row.known_usd),
            row.partial,
        )
    })
}
fn recovered_history_presentations(
    history: &[HistoryEvent],
    costs: &accounting::CostIndex,
    token_events: &[TokenEvent],
    wanted: Option<&BTreeSet<String>>,
) -> BTreeMap<
    String,
    (
        i64,
        IntervalPresentation,
        Vec<quota_widget::intervals::ModelIntervalUsage>,
    ),
> {
    let mut visible = quota_widget::history::visible_observations(history);
    visible.sort_by_key(|event| {
        (
            event.observed_at_unix,
            event
                .evidence
                .as_ref()
                .and_then(|e| e.observed_at_ms)
                .unwrap_or_else(|| event.observed_at_unix.saturating_mul(1000)),
        )
    });
    let mut previous: BTreeMap<(String, String, Option<String>, String), Vec<(i64, i64)>> =
        BTreeMap::new();
    let mut recovered = BTreeMap::new();
    // TokenStore normalizes the snapshot by (timestamp, id), so each interval
    // can use a binary range instead of rescanning all historical token rows.
    let tokens = token_events;
    for event in visible {
        if event.kind != quota_widget::model::HistoryKind::QuotaDecrease {
            continue;
        }
        let reset = event.evidence.as_ref().and_then(|e| e.reset_at);
        let window = event.window_key.clone();
        if let (Some(reset), Some(window)) = (reset, window) {
            let account_id = event
                .evidence
                .as_ref()
                .and_then(|evidence| evidence.account_id.clone());
            let scope = (
                event.source_key.clone(),
                window,
                account_id.clone(),
                event.before.clone(),
            );
            let start = previous.get(&scope).and_then(|rows| {
                rows.iter()
                    .filter(|(at, prior_reset)| {
                        *at < event.observed_at_unix
                            && event.observed_at_unix.saturating_sub(*at) <= 6 * 60 * 60
                            && reset.abs_diff(*prior_reset) <= 90
                    })
                    .max_by_key(|(at, _)| *at)
                    .map(|(at, _)| *at)
            });
            if let Some(start) = start {
                let key = intervals::history_event_key(&event);
                if wanted.is_none_or(|wanted| wanted.contains(&key)) {
                    let lo = tokens.partition_point(|token| token.timestamp <= start);
                    let hi =
                        tokens.partition_point(|token| token.timestamp <= event.observed_at_unix);
                    let cost = costs.observed(start, event.observed_at_unix);
                    let models = quota_widget::intervals::model_usage(&tokens[lo..hi]);
                    recovered.insert(
                        key,
                        (
                            start,
                            interval_views::recovered_observation(&event, start, cost),
                            models,
                        ),
                    );
                }
            }
            previous
                .entry((
                    event.source_key.clone(),
                    scope.1,
                    account_id,
                    event.after.clone(),
                ))
                .or_default()
                .push((event.observed_at_unix, reset));
        }
    }
    recovered
}
fn daily_quota_segments(
    history: &[HistoryEvent],
    day_start: i64,
    day_end: i64,
    future_remaining: Option<f64>,
    now: i64,
) -> Vec<DailyQuotaSegment> {
    let mut events = quota_widget::history::visible_observations(history);
    events.sort_by_key(|event| {
        (
            event.observed_at_unix,
            event
                .evidence
                .as_ref()
                .and_then(|evidence| evidence.observed_at_ms)
                .unwrap_or_else(|| event.observed_at_unix.saturating_mul(1000)),
        )
    });
    let resets = quota_widget::history::reset_cycle_starts(&events);
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    let mut start = day_start;
    for event in events.into_iter().filter(|event| {
        (event.kind == quota_widget::model::HistoryKind::QuotaDecrease
            || resets.contains(&event.observed_at_unix))
            && event.observed_at_unix >= day_start
            && event.observed_at_unix < day_end
    }) {
        let Some(before) = event
            .before
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
        else {
            continue;
        };
        let Some(after) = event
            .after
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
        else {
            continue;
        };
        if event.kind == quota_widget::model::HistoryKind::QuotaReset {
            seen.clear();
            start = event.observed_at_unix;
            result.push(DailyQuotaSegment {
                change: format!("{}% → {}%", event.before, event.after),
                event: Some(event),
                start,
                end: start,
                points: 0.0,
            });
            continue;
        }
        if before <= after || event.observed_at_unix <= start {
            continue;
        }
        let account = event
            .evidence
            .as_ref()
            .and_then(|evidence| evidence.account_id.as_deref())
            .unwrap_or("");
        if !seen.insert((account.to_owned(), before.to_bits(), after.to_bits())) {
            continue;
        }
        let end = event.observed_at_unix;
        result.push(DailyQuotaSegment {
            change: format!("{}% → {}%", event.before, event.after),
            event: Some(event),
            start,
            end,
            points: before - after,
        });
        start = end;
    }
    if let Some(remaining) = future_remaining.filter(|value| value.is_finite() && *value > 0.0) {
        let before = remaining.round() as i64;
        if (1..=100).contains(&before) && now >= start {
            result.push(DailyQuotaSegment {
                event: None,
                change: format!("{}% → {}%", before, before - 1),
                start,
                end: now,
                points: 1.0,
            });
        }
    }
    result.reverse();
    result
}
fn interval_task_counts(events: &[&TokenEvent]) -> [usize; 4] {
    let mut sol = BTreeSet::new();
    let mut luna = BTreeSet::new();
    let mut astra = BTreeSet::new();
    let mut total = BTreeSet::new();
    for event in events
        .iter()
        .filter(|event| !event.is_excluded_auto_review())
    {
        // Count usage responses, not conversations or user turns. One turn may
        // produce many billable responses while the agent runs tools.
        let response = match event.response_id.as_deref().filter(|id| !id.is_empty()) {
            Some(id) => (true, id),
            None => (false, event.id.as_str()),
        };
        total.insert(response);
        let model = event.model.to_ascii_lowercase();
        if model.ends_with("-sol") {
            sol.insert(response);
        } else if model.ends_with("-luna") {
            luna.insert(response);
        } else if model.ends_with("-astra") {
            astra.insert(response);
        }
    }
    [sol.len(), luna.len(), astra.len(), total.len()]
}
fn active_segment_presentation(
    change: String,
    start: i64,
    cost: accounting::ObservedCost,
) -> IntervalPresentation {
    let amount = if cost.events == 0 {
        "$0.0000".into()
    } else {
        format!(
            "{}{}",
            if cost.partial { "≥ " } else { "≈ " },
            money(Some(cost.known_usd))
        )
    };
    IntervalPresentation {
        change,
        start: display_time::datetime(start).unwrap_or_else(|| "未知".into()),
        end: "未知".into(),
        duration: "進行中".into(),
        total: amount.clone(),
        observed_total: if cost.events == 0 { "0 筆 Token 記錄".into() } else { amount },
        per_point: money(Some(cost.known_usd)),
        status: "尚未觀察到下一次下降".into(),
        tooltip: "今日目前仍在此額度；以午夜或上一個首次下降時間作為起點，統計至現在。下一個百分比變化尚未發生，因此時間未知。".into(),
    }
}
fn progress_label(stage: ProgressStage) -> &'static str {
    match stage {
        ProgressStage::Loading => "正在載入已儲存紀錄",
        ProgressStage::Migrating => "正在轉換舊資料",
        ProgressStage::Import(ImportPhase::Discovering) => "正在列出來源檔案",
        ProgressStage::Import(ImportPhase::Reading) => "正在掃描 Token",
        ProgressStage::Import(ImportPhase::Reconciling) => "正在整理用量",
        ProgressStage::Import(ImportPhase::Metadata) => "正在讀取 Task 名稱",
        ProgressStage::BuildingViews => "正在建立顯示資料",
        ProgressStage::BuildingIntervals => "正在計算扣額區間",
        ProgressStage::Saving => "正在儲存本機紀錄",
        ProgressStage::Complete => "本機資料處理完成",
    }
}
fn migration_progress_label(progress: &WorkProgress) -> Option<String> {
    if progress.stage == ProgressStage::Migrating && progress.files_total > 0 {
        Some(format!(
            "正在轉換舊資料：task {}/{}（原檔保留）",
            progress.files_done, progress.files_total
        ))
    } else if progress.stage == ProgressStage::Migrating
        || (progress.operation == Operation::Load && progress.stage == ProgressStage::Loading)
    {
        // Fingerprinting/parsing precedes a known task total; never claim 0/0
        // completed tasks, or relabel task counters as session files.
        Some("首次載入／驗證舊資料（原檔保留）".into())
    } else {
        None
    }
}
fn interval_model_cost(model: &intervals::ModelIntervalUsage) -> String {
    if model.events > 0 && model.unknown_cost_events == model.events {
        "未知（價格未設定）".into()
    } else {
        format!(
            "{}{}",
            money(Some(model.known_usd)),
            if model.unknown_cost_events > 0 {
                " *"
            } else {
                ""
            }
        )
    }
}
fn model_family_usage(
    models: &[intervals::ModelIntervalUsage],
    family: &str,
) -> Option<intervals::ModelIntervalUsage> {
    let mut rows = models.iter().filter(|model| {
        model
            .model
            .to_ascii_lowercase()
            .ends_with(&format!("-{family}"))
    });
    let first = rows.next()?;
    let mut total = first.clone();
    total.model = family.into();
    for model in rows {
        total.input = total.input.saturating_add(model.input);
        total.cached = total.cached.saturating_add(model.cached);
        total.uncached = total.uncached.saturating_add(model.uncached);
        total.output = total.output.saturating_add(model.output);
        total.known_usd += model.known_usd;
        total.unknown_cost_events += model.unknown_cost_events;
        total.events += model.events;
        total.assumed_model_events += model.assumed_model_events;
        total.model_assumed |= model.model_assumed;
        total.partial_events += model.partial_events;
        total.coverage_gap_events += model.coverage_gap_events;
    }
    total.reasoning = 0;
    Some(total)
}
fn total_model_usage(models: &[intervals::ModelIntervalUsage]) -> intervals::ModelIntervalUsage {
    let mut total = intervals::ModelIntervalUsage {
        model: "total".into(),
        input: 0,
        cached: 0,
        uncached: 0,
        output: 0,
        reasoning: 0,
        known_usd: 0.0,
        unknown_cost_events: 0,
        events: 0,
        assumed_model_events: 0,
        model_assumed: false,
        partial_events: 0,
        coverage_gap_events: 0,
    };
    for model in models {
        total.input = total.input.saturating_add(model.input);
        total.cached = total.cached.saturating_add(model.cached);
        total.uncached = total.uncached.saturating_add(model.uncached);
        total.output = total.output.saturating_add(model.output);
        total.known_usd += model.known_usd;
        total.unknown_cost_events += model.unknown_cost_events;
        total.events += model.events;
        total.assumed_model_events += model.assumed_model_events;
        total.model_assumed |= model.model_assumed;
        total.partial_events += model.partial_events;
        total.coverage_gap_events += model.coverage_gap_events;
    }
    total
}
fn model_usage_tooltip(
    label: &str,
    model: Option<&intervals::ModelIntervalUsage>,
    turn_count: usize,
) -> String {
    let Some(model) = model else {
        return format!("{label}\nResponse：{turn_count} 筆\n0 筆 · $0.0000");
    };
    format!(
        "{label}\nResponse：{turn_count} 筆\nToken 記錄：{} 筆\n估算：{}\nInput：{}\nCached：{}\nOutput：{}{}",
        model.events,
        interval_model_cost(model),
        model.input,
        model.cached,
        model.output,
        if model.unknown_cost_events > 0 {
            format!("\n價格未設定：{} 筆", model.unknown_cost_events)
        } else {
            String::new()
        }
    )
}
fn now() -> i64 {
    Utc::now().timestamp()
}
fn local_time(t: i64) -> String {
    display_time::time(t)
}
fn local_day(t: i64) -> Option<NaiveDate> {
    display_time::day(t)
}
fn effort_label(group: &usage_views::UsageGroup) -> String {
    let mut values = group.reasoning_efforts.clone();
    if group.unknown_effort_events > 0 {
        values.push("未知".into());
    }
    if values.is_empty() {
        "未知".into()
    } else {
        values.join(" / ")
    }
}
/// Whether an actual priced request used the long-context multiplier. This
/// describes exported response evidence, never the global toggle or a Turn sum.
fn exported_long_context_applied(event: &serde_json::Value, pricing: &Pricing) -> Option<bool> {
    let input = event.get("input")?.as_u64()?;
    if event
        .get("partial_flags")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|flags| {
            flags
                .iter()
                .filter_map(serde_json::Value::as_str)
                .any(|flag| {
                    matches!(
                        flag,
                        "input_count_unavailable"
                            | "invalid_token_counts"
                            | "cached_exceeded_input"
                    )
                })
        })
    {
        return None;
    }
    if !pricing.apply_long_context {
        return Some(false);
    }
    let model = event.get("model")?.as_str()?;
    if !matches!(model, "gpt-6.1-sol" | "gpt-6-luna" | "gpt-6-astra") {
        return Some(false);
    }
    let response_scoped = event.get("source_kind").and_then(serde_json::Value::as_str)
        == Some("token_usage_record")
        || event
            .get("response_id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| !id.is_empty());
    if !response_scoped {
        return None;
    }
    if input <= 272_000 {
        return Some(false);
    }
    // A request without an estimated price does not establish that any
    // multiplier was actually applied. Keep that distinction explicit.
    event.get("cost_usd")?.as_f64()?;
    Some(true)
}
fn annotate_long_context_export(event: &mut serde_json::Value, pricing: &Pricing) {
    let applied = exported_long_context_applied(event, pricing);
    event["long_context_adjustment_enabled"] = serde_json::json!(pricing.apply_long_context);
    event["long_context_adjustment_applied"] = serde_json::json!(applied);
    // Keep the old boolean for consumers of schema version 1, with an explicit
    // deprecation note so it cannot be confused with the per-request result.
    event["long_context_adjustment"] = serde_json::json!(pricing.apply_long_context);
    event["long_context_adjustment_legacy_semantics"] =
        serde_json::json!("deprecated_alias_of_long_context_adjustment_enabled");
}
fn money(v: Option<f64>) -> String {
    v.map(|x| {
        if x > 0. && x < 0.0001 {
            format!("${x:.6}")
        } else {
            format!("${x:.4}")
        }
    })
    .unwrap_or_else(|| "$—".into())
}
const PARTIAL_INTERVAL_WARNING: &str = "狀態：部分";
fn history_description(
    event: &HistoryEvent,
    cost: Option<f64>,
    partial: bool,
    interval: Option<&CompletedInterval>,
) -> String {
    if event.reconciliation.is_some() {
        format!("{}% → {}% · 狀態：差異", event.before, event.after)
    } else if event.kind == quota_widget::model::HistoryKind::QuotaDecrease {
        let cost_text = match interval {
            Some(v) if !v.comparable => format!("小計 {} * · 部分", money(Some(v.known_usd))),
            Some(v) => format!(
                "{} · {}/百分點",
                money(Some(v.known_usd)),
                money(v.cost_per_point)
            ),
            None if partial => format!("小計 {} * · 部分", money(cost)),
            None => money(cost),
        };
        format!(
            "{}% → {}% · 扣除 {} 百分點 · {}",
            event.before, event.after, event.delta, cost_text
        )
    } else if event.kind == quota_widget::model::HistoryKind::QuotaReset {
        format!("{}% → {}% · 額度補回 / 新週期", event.before, event.after)
    } else {
        format!(
            "{} → {} · {}",
            event.before,
            event.after,
            event.kind.label()
        )
    }
}
fn readable_tooltip(ui: &mut egui::Ui, text: &str, font_size: f32) {
    // Tooltips are painted inside the native viewport, not a new OS window.
    // Keep both the popup frame and its scroll bar inside the smallest root.
    let screen = ui.ctx().screen_rect();
    let width = (screen.width() - 40.).clamp(120., 680.);
    ui.set_width(width);
    egui::ScrollArea::vertical()
        .max_width(width)
        .max_height((screen.height() - 40.).max(120.))
        .min_scrolled_height(0.)
        .show(ui, |ui| {
            ui.set_width(width - 20.);
            ui.add(egui::Label::new(RichText::new(tr(text)).size(font_size)).wrap());
        });
}

fn history_tooltip(description: &str, interval: Option<&CompletedInterval>) -> String {
    let Some(v) = interval else {
        return description.to_owned();
    };
    let mut text = format!(
        "{}\n時段：{} → {}",
        description,
        v.start
            .and_then(display_time::datetime)
            .unwrap_or_else(|| "開始時間未知".into()),
        display_time::datetime(v.end).unwrap_or_else(|| "結束時間未知".into())
    );
    if !v.comparable {
        text.push('\n');
        text.push_str(PARTIAL_INTERVAL_WARNING);
    }
    text
}
fn duration(s: i64) -> String {
    let s = s.max(0);
    if s >= 86400 {
        format!(
            "{}d {:02}:{:02}:{:02}",
            s / 86400,
            s / 3600 % 24,
            s / 60 % 60,
            s % 60
        )
    } else {
        format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    }
}
fn kind(e: &HistoryEvent) -> String {
    serde_json::to_value(e.kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
        .to_lowercase()
}
fn drop_event(e: &HistoryEvent) -> bool {
    kind(e).contains("quotadecrease") || kind(e).contains("quota_decrease")
}
fn reset_event(e: &HistoryEvent) -> bool {
    kind(e).contains("quotareset") || kind(e).contains("quota_reset")
}
fn unchanged(e: &HistoryEvent) -> bool {
    e.before == e.after || e.delta.parse::<f64>().map(|v| v == 0.).unwrap_or(false)
}
fn boundary_matches_live(
    event: &HistoryEvent,
    live: Option<&WindowView>,
    account: Option<&str>,
) -> bool {
    let Some(live) = live else {
        return true;
    };
    let evidence = event.evidence.as_ref();
    if evidence
        .and_then(|e| e.account_id.as_deref())
        .zip(account)
        .is_some_and(|(recorded, current)| recorded != current)
    {
        return false;
    }
    // Reject a demonstrably expired cycle, allowing the existing 90-second
    // deadline jitter tolerance. Run-local epoch counters are not comparable.
    !evidence
        .and_then(|e| e.reset_at)
        .zip(live.reset_at)
        .is_some_and(|(old, current)| {
            old <= live.observed_at
                && current > live.observed_at
                && current.saturating_sub(old) > 90
        })
}
fn quota_endpoint_mismatch(boundary: Option<&HistoryEvent>, live: Option<&WindowView>) -> bool {
    let endpoints = boundary
        .and_then(|e| e.after.parse::<f64>().ok())
        .zip(live.and_then(|v| v.remaining_percent));
    // Match the live model's percentage tolerance; tiny numeric jitter is not
    // evidence of a missing observation, and missing numbers prove nothing.
    endpoints.is_some_and(|(recorded, current)| {
        recorded.is_finite() && current.is_finite() && (recorded - current).abs() > 0.000001
    })
}

pub fn launch(args: Vec<String>) -> eframe::Result<()> {
    let demo = args.iter().any(|s| s == "--demo");
    let offline = args.iter().any(|s| s == "--offline");
    let persistent = !args.iter().any(|s| s == "--no-settings") && !demo;
    let dir = args
        .iter()
        .position(|s| s == "--data-dir")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(storage::data_dir);
    let lock_result = if persistent {
        Some(DataLock::acquire(&dir))
    } else {
        None
    };
    let lock_notice = lock_result
        .as_ref()
        .and_then(|r| r.as_ref().err())
        .map(|e| format!("資料目錄被另一個視窗使用；目前只讀。{e}"));
    let lock = lock_result.and_then(Result::ok);
    let settings_read = if persistent {
        storage::load_json::<Settings>(&dir.join("settings.json"))
    } else {
        Err(std::io::Error::from(std::io::ErrorKind::NotFound))
    };
    let corrupt_settings = settings_read
        .as_ref()
        .is_err_and(|e| e.kind() != std::io::ErrorKind::NotFound);
    let mut settings = settings_read.unwrap_or_default();
    let compact_layout_migrated = settings.compact_layout_version == 0;
    prepare_widget_settings(&mut settings);
    let mut viewport = ViewportBuilder::default()
        .with_inner_size(settings.size)
        .with_min_inner_size([280., COLLAPSED_HEIGHT * 280. / 310.])
        .with_max_inner_size([1000., 2000.])
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(true)
        .with_window_level(if settings.on_top {
            WindowLevel::AlwaysOnTop
        } else {
            WindowLevel::Normal
        });
    if let Some(p) = settings.position {
        viewport = viewport.with_position(p);
    }
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "Quota Widget",
        options,
        Box::new(move |cc| {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "quota-cjk".into(),
                egui::FontData::from_static(include_bytes!("../assets/NotoSansCJKtc-full.otf"))
                    .into(),
            );
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push("quota-cjk".into());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("quota-cjk".into());
            fonts.font_data.insert(
                "quota-original-cjk".into(),
                egui::FontData::from_static(include_bytes!("../assets/NotoSansCJKtc-subset.otf"))
                    .into(),
            );
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                let list = fonts.families.entry(family).or_default();
                let insert = list.len().saturating_sub(1);
                list.insert(insert, "quota-original-cjk".into());
            }
            cc.egui_ctx.set_fonts(fonts);
            let mut app = WidgetApp::new(settings, dir, persistent, demo, offline, args);
            app.dirty = compact_layout_migrated;
            app._lock = lock;
            app.database_mode = app.read_state && !demo;
            if app._lock.is_none() {
                app.persistent = false;
            }
            if let Some(note) = lock_notice {
                app.notice = note;
            }
            app.apply_theme(&cc.egui_ctx);
            if corrupt_settings {
                app.persistent = false;
                app.notice = "設定檔無法讀取；已停用寫入，原檔保留。".into();
            }
            app.initialize();
            Ok(Box::new(app))
        }),
    )
}
impl WidgetApp {
    fn new(
        settings: Settings,
        dir: PathBuf,
        persistent: bool,
        demo: bool,
        offline: bool,
        args: Vec<String>,
    ) -> Self {
        let _zone = display_time::scope(settings.utc_offset_minutes);
        Self {
            settings,
            dir,
            worker: Worker::spawn_with_progress(),
            database_mode: false,
            requested_view: None,
            view_pending: false,
            _lock: None,
            persistent,
            read_state: persistent,
            demo,
            offline,
            quota: QuotaModel::new(format!("local-{}", now())),
            tokens: Arc::new(Snapshot::default()),
            history: Arc::new(Vec::new()),
            intervals: Arc::new(Vec::new()),
            interval_lookup: BTreeMap::new(),
            interval_presentations: Vec::new(),
            interval_estimates: Vec::new(),
            history_intervals: true,
            selected_interval: None,
            token_generation: 0,
            history_generation: 0,
            expanded_tasks: BTreeSet::new(),
            tree_generation: 0,
            boundary_after: None,
            main_cache: MainCache::default(),
            token_cache: TokenCache::default(),
            history_cache: HistoryCache::default(),
            dates_key: None,
            dates_cache: BTreeSet::new(),
            layout: LayoutConvergence::default(),
            bridge: None,
            quota_retry_at: None,
            status: "OFFLINE".into(),
            quota_error: None,
            history_selection: None,
            tasks_expanded: false,
            recent_expanded: false,
            history_open: args.iter().any(|s| s == "--show-history"),
            tokens_open: args.iter().any(|s| s == "--show-tokens"),
            settings_open: args.iter().any(|s| s == "--show-settings"),
            settings_presented: false,
            prediction_open: false,
            prediction_presented: false,
            prediction_input: String::new(),
            prediction_pst: false,
            prediction_history_checked: None,
            date: display_time::now_day(),
            token_date: display_time::now_day(),
            token_group: GroupBy::Response,
            token_page: 0,
            show_excluded_records: false,
            import_pending: false,
            load_pending: false,
            worker_failed: false,
            work_progress: None,
            last_import_elapsed: None,
            load_error: None,
            import_error: None,
            import_warnings: Vec::new(),
            export_rx: None,
            export_join: None,
            export_cancel: None,
            closing_since: None,
            close_ready: false,
            lock_retry_pending: false,
            last_lock_retry: Instant::now(),
            last_import: Instant::now() - Duration::from_secs(60),
            last_save: Instant::now(),
            last_topmost: Instant::now(),
            notice: String::new(),
            import_note: String::new(),
            price_model: "gpt-6.1-sol".into(),
            price_models_generation: None,
            recorded_price_models: BTreeSet::new(),
            price_values: [0.; 3],
            price_configured: false,
            last_focus: false,
            last_minimized: false,
            dirty: false,
            demo_start: now(),
        }
    }
    fn initialize(&mut self) {
        if let Some(p) = self.settings.pricing.rates.get(&self.price_model) {
            self.price_values = [
                p.uncached_input_per_million,
                p.cached_input_per_million,
                p.output_per_million,
            ];
            self.price_configured = true;
        }
        if self.demo {
            self.make_demo();
            return;
        }
        if self.database_mode {
            self.worker =
                Worker::spawn_database_with_progress(self.persistent && self._lock.is_some());
        }
        if self.read_state {
            self.load_pending = self.send_work(WorkCommand::Load {
                token_path: self.dir.join("tokens.json"),
                history_path: None,
                pricing: self.settings.pricing.clone(),
            });
        }
        if !self.offline {
            self.connect();
        }
        if self.settings.import_enabled {
            self.start_import();
        }
    }
    fn included_count(&self) -> usize {
        self.tokens
            .database
            .as_ref()
            .map_or(self.tokens.events.len(), |view| view.total_included)
    }
    fn excluded_count(&self) -> usize {
        self.tokens
            .database
            .as_ref()
            .map_or(self.tokens.excluded_events.len(), |view| {
                view.total_excluded
            })
    }
    fn database_request(&self, boundary: Option<i64>, t: i64) -> Option<ViewRequest> {
        let (day_start, day_end) = display_time::day_bounds(self.token_date)?;
        Some(ViewRequest {
            day_start,
            day_end,
            group: self.token_group,
            excluded: self.show_excluded_records,
            page: self.token_page,
            page_size: 500,
            boundary,
            fallback_start: self.demo_start,
            now: t,
        })
    }
    fn request_database_view(&mut self, boundary: Option<i64>, t: i64) {
        if self.tokens.database.is_none() {
            return;
        }
        let Some(request) = self.database_request(boundary, t) else {
            return;
        };
        if self
            .requested_view
            .as_ref()
            .is_some_and(|sent| sent.same_selection(&request))
        {
            return;
        }
        if self.send_work(WorkCommand::View(request.clone())) {
            self.requested_view = Some(request);
            self.view_pending = true;
        }
    }
    fn accept_database_view(&mut self) {
        if self.tokens.database.as_ref().is_some_and(|view| {
            self.requested_view
                .as_ref()
                .is_some_and(|request| request.same_selection(&view.request))
        }) {
            self.view_pending = false;
        }
    }
    fn save_path(&self, name: &str) -> Option<PathBuf> {
        self.persistent.then(|| self.dir.join(name))
    }
    fn request_reprice(&mut self) {
        if self.demo {
            return;
        }
        self.send_work(WorkCommand::Reprice {
            pricing: self.settings.pricing.clone(),
            save_path: self.save_path("tokens.json"),
        });
    }
    fn apply_theme(&self, ctx: &egui::Context) {
        // Pin the saved application preference; set_visuals alone modifies the
        // currently selected style but leaves System free to switch it on frame 1.
        ctx.set_theme(egui::Theme::from_dark_mode(self.settings.dark));
        let mut style = (*ctx.style()).clone();
        accessibility::configure_style(
            &mut style,
            self.settings.dark,
            accessibility::scaled_minimum_font(
                self.settings.size[0],
                self.settings.font_scale_percent,
            ),
        );
        ctx.set_style(style);
    }
    fn connect(&mut self) {
        self.quota_retry_at = None;
        self.bridge = None;
        self.quota.reset_continuity();
        self.main_cache.key = None;
        self.history_cache.key = None;
        self.history_cache.work_key = None;
        self.bridge = Some(Bridge::spawn(
            (!self.settings.codex_path.trim().is_empty())
                .then(|| PathBuf::from(self.settings.codex_path.trim())),
        ));
        self.status = "CONNECTING".into();
        self.quota_error = None;
    }
    /// A completed bridge leaves a disconnected sender. A user refresh must
    /// reconnect that source instead of silently sending into a dead channel.
    fn refresh_quota(&mut self) {
        if self.demo {
            return;
        }
        self.offline = false;
        if self
            .bridge
            .as_ref()
            .is_none_or(|bridge| bridge.commands.send(BridgeCommand::Refresh).is_err())
        {
            self.connect();
        }
    }
    fn retry_quota_connection(&mut self, t: Instant) {
        if self.demo || self.offline || self.closing_since.is_some() || self.bridge.is_some() {
            return;
        }
        if self.quota_retry_at.is_some_and(|deadline| t >= deadline) {
            self.connect();
        }
    }
    fn quota_diagnostic_text(&self) -> String {
        let mut text = format!("額度來源狀態：{}", self.status);
        if let Some(error) = &self.quota_error {
            text.push_str(&format!("\n{error}"));
        }
        text
    }
    fn compact_status_text(&self) -> Option<String> {
        if !self.demo && self.quota_error.is_some() {
            Some("額度連線有問題 · 點此檢查".into())
        } else if !self.demo && self.selected().is_none() {
            Some(
                match self.status.as_str() {
                    "CONNECTING" => "正在連接額度來源…",
                    "CONNECTED · 等待額度" => "已連線 · 等待額度資料…",
                    "OFFLINE" => "額度未連線 · 點此設定",
                    _ => "尚未取得額度 · 點此檢查",
                }
                .into(),
            )
        } else {
            self.compact_progress_text()
        }
    }
    fn start_import(&mut self) {
        if self.demo
            || (self.database_mode && !self.persistent)
            || self.import_pending
            || self.worker_failed
            || !self.settings.import_enabled
            || self
                .work_progress
                .as_ref()
                .is_some_and(|(progress, _)| progress.operation == Operation::Import)
        {
            return;
        }
        let root = PathBuf::from(self.settings.sessions_path.trim());
        // Only the explicitly selected sessions directory's adjacent official index.
        let metadata = (root.file_name().is_some_and(|n| n == "sessions"))
            .then(|| root.parent().map(|p| p.join("session_index.jsonl")))
            .flatten();
        if self.send_work(WorkCommand::Import {
            root,
            metadata,
            pricing: self.settings.pricing.clone(),
            save_path: self.save_path("tokens.json"),
            history_save_path: None,
        }) {
            self.import_pending = true;
        }
        self.last_import = Instant::now();
    }
    fn send_work(&mut self, command: WorkCommand) -> bool {
        if self.worker_failed || self.worker.send(command).is_err() {
            self.worker_disconnected();
            false
        } else {
            true
        }
    }
    fn worker_disconnected(&mut self) {
        self.worker_failed = true;
        self.view_pending = false;
        self.load_pending = false;
        self.import_pending = false;
        self.work_progress = None;
        self.persistent = false;
        // Do not expose a panic payload or imply that live quota stopped too.
        self.import_error =
            Some("本機歷史背景處理已中斷；請重新啟動程式。畫面保留已載入資料。".into());
    }
    #[cfg(test)]
    fn account_changed(&mut self) {
        self.quota.account_changed();
        self.boundary_after = Some(now());
        self.main_cache.key = None;
        self.history_cache.key = None;
        self.history_cache.work_key = None;
        self.status = "帳戶已變更".into();
    }
    fn retain_import_warning(&mut self, warning: String) {
        // Keep diagnostics across no-op polls, with bounded memory and no raw JSONL.
        if self.import_warnings.len() < 20 && !self.import_warnings.contains(&warning) {
            self.import_warnings.push(warning);
        }
    }
    fn import_status_text(&self) -> String {
        if self.worker_failed {
            return self.import_error.clone().unwrap_or_default();
        }
        if let Some((progress, received)) = &self.work_progress {
            if let Some(label) = migration_progress_label(progress) {
                return format!(
                    "{} · {} 秒",
                    label,
                    progress
                        .elapsed
                        .saturating_add(received.elapsed())
                        .as_secs()
                );
            }
            return format!(
                "{} · {} 秒 · 檔案 {}/{} · 行 {} · 觀察 {} · 用量 {}",
                progress_label(progress.stage),
                progress
                    .elapsed
                    .saturating_add(received.elapsed())
                    .as_secs(),
                progress.files_done,
                progress.files_total,
                progress.lines_read,
                progress.observations,
                progress.events,
            );
        }
        if self.load_pending {
            return "正在載入已儲存的本機紀錄…".into();
        }
        if self.import_pending {
            return "正在掃描所選本機 Token 來源…".into();
        }
        if let Some(error) = &self.import_error {
            return format!("背景匯入失敗：{error}");
        }
        if !self.settings.import_enabled {
            return format!("本機匯入已停用。{}", self.import_note);
        }
        if self.import_note.is_empty() {
            "等待本機 Token 掃描。".into()
        } else {
            match self.last_import_elapsed {
                Some(elapsed) => {
                    format!("{} 耗時 {:.1} 秒", self.import_note, elapsed.as_secs_f64())
                }
                None => self.import_note.clone(),
            }
        }
    }
    fn compact_progress_text(&self) -> Option<String> {
        let (progress, received) = self.work_progress.as_ref()?;
        let seconds = progress
            .elapsed
            .saturating_add(received.elapsed())
            .as_secs();
        if let Some(label) = migration_progress_label(progress) {
            return Some(format!("{label} · {seconds} 秒"));
        }
        Some(if progress.files_total > 0 {
            format!(
                "{} · {}/{} 檔 · {} 秒",
                progress_label(progress.stage),
                progress.files_done,
                progress.files_total,
                seconds
            )
        } else {
            format!("{} · {} 秒", progress_label(progress.stage), seconds)
        })
    }
    fn empty_task_status(&self) -> String {
        let status = if self.worker_failed {
            "背景處理中斷 · 點此檢查"
        } else if self.load_pending {
            return self
                .compact_progress_text()
                .unwrap_or_else(|| "正在載入本機紀錄…".into());
        } else if self.import_error.is_some() || self.load_error.is_some() {
            "本機匯入有錯誤 · 點此檢查"
        } else if !self.import_warnings.is_empty() {
            "本機匯入有警告 · 點此檢查"
        } else if let Some(progress) = self.compact_progress_text() {
            return progress;
        } else if self.import_pending {
            "正在掃描本機 Token…"
        } else if !self.settings.import_enabled && self.included_count() == 0 {
            "本機匯入已停用 · 點此設定"
        } else if self.included_count() == 0 && self.excluded_count() > 0 {
            "只有排除用量 · 點此檢查"
        } else if self.included_count() == 0 {
            "尚未匯入用量 · 點此設定"
        } else {
            "近期範圍無用量 · 點此檢查"
        };
        status.into()
    }
    fn import_diagnostic_text(&self) -> String {
        format!(
            "{}\n來源：{}\n用量：{} · 排除：{}\n{}",
            self.import_status_text(),
            self.settings.sessions_path,
            self.included_count(),
            self.excluded_count(),
            self.notice
        )
    }
    fn import_diagnostics_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(tr(self.import_status_text()));
        if let Some(error) = &self.load_error {
            ui.colored_label(ui.visuals().warn_fg_color, tr(error));
        }
        if !self.import_warnings.is_empty() {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                tr(format!("匯入警告：{}", self.import_warnings.len())),
            );
        }
        ui.collapsing(tr("匯入診斷／來源"), |ui| {
            ui.label(tr(format!("用量來源：{}", self.settings.sessions_path)));
            ui.label(tr(format!("儲存目錄：{}", self.dir.display())));
            ui.label(tr(format!(
                "用量：{} · 排除：{} · 額度：{}",
                self.included_count(),
                self.excluded_count(),
                self.history.len()
            )));
            for warning in &self.import_warnings {
                ui.colored_label(ui.visuals().warn_fg_color, tr(warning));
            }
            if !self.import_warnings.is_empty() && ui.button(tr("清除已讀警告")).clicked() {
                self.import_warnings.clear();
            }
        });
    }
    fn handle_bridge_event(&mut self, event: BridgeEvent) {
        match event {
            BridgeEvent::Connecting => {
                self.status = "CONNECTING".into();
                self.quota_error = None;
            }
            BridgeEvent::Connected => self.status = "CONNECTED · 等待額度".into(),
            BridgeEvent::RateLimits(v) => {
                let previous_account = self.quota.account_id().map(str::to_owned);
                match self.quota.ingest_json(&v, now()) {
                    Ok(_) => {
                        let account_changed = previous_account
                            .as_ref()
                            .zip(self.quota.account_id())
                            .is_some_and(|(previous, current)| previous != current);
                        if account_changed {
                            self.boundary_after = Some(now());
                            self.main_cache.key = None;
                            self.history_cache.key = None;
                            self.history_cache.work_key = None;
                            self.status = "帳戶已變更".into();
                        }
                        self.status = "LIVE".into();
                        self.quota_error = None;
                        if !self.quota.history.is_empty() {
                            let events = std::mem::take(&mut self.quota.history);
                            if !self.demo
                                && !account_changed
                                && self
                                    .selected()
                                    .is_some_and(|view| live_reset_alert(&events, &view))
                            {
                                self.settings.clear_prediction();
                                self.dirty = true;
                                if self.settings.reset_sound_enabled && !play_reset_chime() {
                                    self.notice = "額度已重設至 100%；提示音播放失敗。".into();
                                }
                            }
                            self.send_work(WorkCommand::MergeHistory {
                                events,
                                save_path: None,
                            });
                        }
                    }
                    Err(_) => {
                        self.status = "資料格式不支援".into();
                        self.quota_error =
                            Some("CLI 回傳的額度格式不支援；請在設定檢查來源。".into());
                    }
                }
            }
            BridgeEvent::Error(e) => {
                self.quota_retry_at = matches!(
                    e,
                    quota_widget::bridge::BridgeError::Transport
                        | quota_widget::bridge::BridgeError::Timeout
                        | quota_widget::bridge::BridgeError::StartFailed
                        | quota_widget::bridge::BridgeError::Unavailable
                )
                .then(|| Instant::now() + Duration::from_secs(30));
                self.status = "OFFLINE".into();
                self.quota_error = Some(format!("CLI：{e}"));
                self.notice = self.quota_error.clone().unwrap();
                self.quota.reset_continuity();
            }
            BridgeEvent::Stopped => {
                self.status = "OFFLINE".into();
                self.bridge = None;
            }
        }
    }
    fn retry_write_lock(&mut self) {
        if !self.database_mode
            || !self.read_state
            || self._lock.is_some()
            || self.lock_retry_pending
            || self.load_pending
            || self.import_pending
            || self.worker_failed
            || self.load_error.is_some()
            || self.closing_since.is_some()
            || self.last_lock_retry.elapsed() < Duration::from_secs(5)
        {
            return;
        }
        self.last_lock_retry = Instant::now();
        self.lock_retry_pending = self.send_work(WorkCommand::AcquireWriter {
            dir: self.dir.clone(),
        });
    }
    fn pump(&mut self) {
        let messages: Vec<_> = self
            .bridge
            .as_ref()
            .map(|b| b.events.try_iter().collect())
            .unwrap_or_default();
        for event in messages {
            self.handle_bridge_event(event);
        }
        loop {
            match self.worker.try_recv() {
                Ok(event) => self.handle_worker_event(event),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.worker_disconnected();
                    break;
                }
            }
        }
        match self.export_rx.as_ref().map(|rx| rx.try_recv()) {
            Some(Ok(message)) => {
                self.notice = message;
                self.export_rx = None;
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.notice = "背景匯出中斷；請重新匯出。".into();
                self.export_rx = None;
            }
            Some(Err(mpsc::TryRecvError::Empty)) | None => {}
        }
        self.retry_quota_connection(Instant::now());
        self.retry_write_lock();
        if !self.demo && !self.import_pending && self.last_import.elapsed() > Duration::from_secs(5)
        {
            self.start_import();
        }
        if self.dirty && self.last_save.elapsed() > Duration::from_secs(5) {
            self.save_state();
        }
    }
    fn handle_worker_event(&mut self, event: WorkEvent) {
        match event {
            WorkEvent::WriterLock { lock } => {
                self.lock_retry_pending = false;
                if let Some(lock) = lock {
                    self._lock = Some(lock);
                    self.persistent = true;
                    self.load_pending = self.send_work(WorkCommand::Load {
                        token_path: self.dir.join("tokens.json"),
                        history_path: None,
                        pricing: self.settings.pricing.clone(),
                    });
                    self.send_work(WorkCommand::MergeHistory {
                        events: self.history.as_ref().clone(),
                        save_path: None,
                    });
                    self.import_error = None;
                    self.notice = "已重新取得寫入權，正在更新本機用量。".into();
                    self.last_import = Instant::now() - Duration::from_secs(60);
                }
            }
            WorkEvent::Progress(progress) => {
                if self.worker_failed {
                    return;
                }
                if progress.stage == ProgressStage::Complete {
                    if progress.operation == Operation::Import {
                        self.last_import_elapsed = Some(progress.elapsed);
                    }
                    self.work_progress = None;
                } else {
                    // Status only. In particular, scanning a selected directory
                    // must not blank the cache or invalidate any rendered view.
                    self.work_progress = Some((progress, Instant::now()));
                }
            }
            WorkEvent::Loaded {
                snapshot,
                normalized_history,
                history_error,
                error,
            } => {
                self.load_pending = false;
                self.tokens = snapshot;
                self.accept_database_view();
                self.token_generation += 1;
                self.history = normalized_history;
                self.history_generation += 1;
                self.import_note = format!(
                    "已載入 {} 筆用量、{} 筆額度觀察。",
                    self.included_count(),
                    self.history.len()
                );
                let errors: Vec<_> = error.into_iter().chain(history_error).collect();
                self.load_error = (!errors.is_empty()).then(|| errors.join("；"));
                if let Some(error) = &self.load_error {
                    self.persistent = false;
                    self.notice = format!("儲存檔無法讀取；已停用寫入並保留原檔。{error}");
                }
            }
            WorkEvent::Updated {
                snapshot,
                normalized_history,
                report,
                error,
            } => {
                if let Some(snapshot) = snapshot {
                    self.tokens = snapshot;
                    self.accept_database_view();
                    self.token_generation += 1;
                }
                if let Some(history) = normalized_history {
                    self.history = history;
                    self.history_generation += 1;
                }
                if let Some(report) = report {
                    self.import_pending = false;
                    self.import_error = None;
                    if report.files_scanned == 0 {
                        self.retain_import_warning(
                            "所選來源未找到 JSONL 檔案；請檢查來源路徑及檔案存取狀態。".into(),
                        );
                    }
                    if report.malformed_lines > 0 {
                        self.retain_import_warning(format!(
                            "曾略過 {} 行格式不支援或無效的用量記錄。",
                            report.malformed_lines
                        ));
                    }
                    for warning in report.warnings {
                        self.retain_import_warning(warning);
                    }
                    self.import_note = format!(
                            "已掃描 {} 檔（未變更 {}），讀取 {} 行，本輪新增 {} 筆；格式問題 {}，未完成行 {}。目前載入 {} 筆用量。",
                            report.files_scanned, report.files_unchanged, report.lines_read,
                            report.events_added, report.malformed_lines, report.partial_lines,
                            self.included_count()
                        );
                }
                if let Some(error) = error {
                    self.view_pending = false;
                    self.import_pending = false;
                    self.retain_import_warning(format!("匯入曾失敗：{error}"));
                    self.import_error = Some(error);
                }
            }
            WorkEvent::HistoryUpdated { normalized } => {
                if let Some(history) = normalized {
                    self.history = history;
                    self.history_generation += 1;
                }
            }
            WorkEvent::IntervalsUpdated { intervals } => {
                self.set_intervals(intervals);
            }
            WorkEvent::SaveFailed { path: _, error } => {
                self.notice = format!("儲存失敗，畫面仍保留資料：{error}");
            }
        }
    }
    fn save_state(&mut self) {
        if self.persistent
            && self.dirty
            && self.send_work(WorkCommand::SaveSettings {
                path: self.dir.join("settings.json"),
                settings: self.settings.clone(),
            })
        {
            self.dirty = false;
        }
        self.last_save = Instant::now();
    }
    fn set_intervals(&mut self, intervals: Arc<Vec<CompletedInterval>>) {
        self.interval_lookup = intervals
            .iter()
            .enumerate()
            .map(|(i, row)| (row.history_event_key.clone(), i))
            .collect();
        // Reformat only when canonical data changes, never on every repaint.
        self.interval_estimates = quota_estimate::interval_estimates(&intervals);
        self.interval_presentations = intervals
            .iter()
            .zip(&self.interval_estimates)
            .map(|(row, estimate)| interval_views::present_with_estimate(row, estimate.as_ref()))
            .collect();
        self.intervals = intervals;
        self.main_cache.key = None;
    }
    fn selected(&self) -> Option<WindowView> {
        let views = self.quota.views(now());
        views
            .iter()
            .find(|v| {
                v.source_key == self.settings.bucket
                    && (v.window_key == self.settings.window
                        || (self.settings.window == "weekly" && v.window_label.contains("週")))
            })
            .cloned()
            .or_else(|| {
                views
                    .iter()
                    .find(|v| v.window_label.contains("週"))
                    .cloned()
            })
            .or_else(|| views.into_iter().next())
    }
    fn matching_history(&self) -> Vec<HistoryEvent> {
        let view = self.selected();
        let fallback = self
            .history
            .iter()
            .filter(|e| drop_event(e) || reset_event(e))
            .filter(|e| {
                e.source_key == self.settings.bucket
                    && (e.window_label.contains("週") || e.window_label.contains("week"))
            })
            .max_by_key(|e| e.observed_at_unix)
            .or_else(|| {
                self.history
                    .iter()
                    .filter(|e| drop_event(e) || reset_event(e))
                    .max_by_key(|e| e.observed_at_unix)
            });
        let selection = view
            .as_ref()
            .map(|v| {
                (
                    v.source_key.as_str(),
                    Some(v.window_key.as_str()),
                    v.window_label.as_str(),
                )
            })
            .or_else(|| {
                fallback.map(|e| {
                    (
                        e.source_key.as_str(),
                        e.window_key.as_deref(),
                        e.window_label.as_str(),
                    )
                })
            });
        let mut list: Vec<_> = self
            .history
            .iter()
            .filter(|e| {
                selection.is_some_and(|(s, k, w)| {
                    e.source_key == s
                        && match (e.window_key.as_deref(), k) {
                            (Some(a), Some(b)) => a == b,
                            _ => e.window_label == w,
                        }
                })
            })
            .cloned()
            .collect();
        list.sort_by_key(|e| e.observed_at_unix);
        list
    }
    fn browsed_history_on_day(&self, day: NaiveDate) -> Vec<HistoryEvent> {
        let Some((day_start, day_end)) = display_time::day_bounds(day) else {
            return Vec::new();
        };
        let live = self.selected();
        let fallback = self
            .history
            .iter()
            .filter(|event| drop_event(event) || reset_event(event))
            .filter(|event| {
                event.source_key == self.settings.bucket
                    && (event.window_label.contains("週")
                        || event.window_label.to_ascii_lowercase().contains("week"))
            })
            .max_by_key(|event| event.observed_at_unix)
            .or_else(|| {
                self.history
                    .iter()
                    .filter(|event| drop_event(event) || reset_event(event))
                    .max_by_key(|event| event.observed_at_unix)
            });
        let selected = self
            .history_selection
            .as_ref()
            .map(|(source, key, label)| (source.as_str(), key.as_deref(), label.as_str()))
            .or_else(|| {
                live.as_ref().map(|view| {
                    (
                        view.source_key.as_str(),
                        Some(view.window_key.as_str()),
                        view.window_label.as_str(),
                    )
                })
            })
            .or_else(|| {
                fallback.map(|event| {
                    (
                        event.source_key.as_str(),
                        event.window_key.as_deref(),
                        event.window_label.as_str(),
                    )
                })
            });
        let mut events: Vec<_> = self
            .history
            .iter()
            .filter(|event| {
                event.observed_at_unix >= day_start
                    && event.observed_at_unix < day_end
                    && selected.is_some_and(|(source, key, label)| {
                        event.source_key == source
                            && match (event.window_key.as_deref(), key) {
                                (Some(event_key), Some(selected_key)) => event_key == selected_key,
                                _ => event.window_label == label,
                            }
                    })
            })
            .cloned()
            .collect();
        events.sort_by_key(|event| {
            (
                event.observed_at_unix,
                event
                    .evidence
                    .as_ref()
                    .and_then(|evidence| evidence.observed_at_ms)
                    .unwrap_or_else(|| event.observed_at_unix.saturating_mul(1000)),
            )
        });
        events
    }
    fn refresh_main_cache(&mut self) {
        let t = now();
        let live = self.selected();
        let key = MainCacheKey {
            token_generation: self.token_generation,
            history_generation: self.history_generation,
            bucket: self.settings.bucket.clone(),
            window: self.settings.window.clone(),
            minute: t / 60,
            tree_generation: self.tree_generation,
            boundary_after: self.boundary_after,
            live_selection: live.as_ref().map(|v| LiveSelectionKey {
                source: v.source_key.clone(),
                window: v.window_key.clone(),
                label: v.window_label.clone(),
                duration: v.window_duration_mins,
                reset_at: v.reset_at,
                remaining_bits: v.remaining_percent.map(f64::to_bits),
                epoch: v.epoch,
                account: self.quota.account_id().map(str::to_owned),
            }),
        };
        if self.main_cache.key.as_ref() == Some(&key) {
            self.request_database_view(
                self.main_cache
                    .boundary
                    .as_ref()
                    .map(|e| e.observed_at_unix),
                t,
            );
            return;
        }
        let all = self.matching_history();
        let visible = quota_widget::history::visible_observations(&all);
        let recent_events: Vec<_> = visible
            .iter()
            .rev()
            .filter(|e| e.observed_at_unix <= t && drop_event(e))
            .filter(|e| {
                self.interval_lookup
                    .contains_key(&intervals::history_event_key(e))
                    || e.reconciliation.is_some()
            })
            .take(5)
            .cloned()
            .collect();
        let wanted: BTreeSet<_> = recent_events
            .iter()
            .map(intervals::history_event_key)
            .collect();
        let recovered_rows = recovered_history_presentations(
            &all,
            &self.tokens.cost_index,
            self.tokens.events(),
            Some(&wanted),
        );
        let mut recent_tooltip = format!("最近 5 次扣額 · {}\n", display_time::zone_label());
        if recent_events.is_empty() {
            recent_tooltip.push_str("已完成區間：0\n");
        }
        for event in &recent_events {
            let key = intervals::history_event_key(event);
            let canonical = self.interval_lookup.get(&key).copied();
            let recovered = recovered_rows.get(&key);
            let row = recovered
                .filter(|(start, _, _)| {
                    event.reconciliation.is_some()
                        || canonical.is_none_or(|i| self.intervals[i].start != Some(*start))
                })
                .map(|(_, row, _)| row.clone())
                .or_else(|| canonical.map(|i| self.interval_presentations[i].clone()))
                .unwrap_or_else(|| interval_views::unconfirmed_observation(event));
            recent_tooltip.push_str(&format!(
                "\n{} · {}\n估算 USD {} · 每% {} · {}\n區間 Token 小計 {}\n",
                row.change,
                row.end,
                row.total,
                row.per_point.replace(" / 百分點", ""),
                row.status,
                row.observed_total
            ));
        }
        let boundary = all
            .iter()
            .filter(|e| {
                e.observed_at_unix <= t
                    && quota_widget::history::is_confirmed_quota_change(e)
                    && !unchanged(e)
            })
            .max_by_key(|e| e.observed_at_unix)
            // Reject the latest incompatible boundary; never fall back to an
            // older metadata-less row that could revive the same wrong account.
            .filter(|e| {
                self.boundary_after
                    .is_none_or(|after| e.observed_at_unix > after)
                    && boundary_matches_live(e, live.as_ref(), self.quota.account_id())
            })
            .cloned();
        let start = boundary.as_ref().map(|e| e.observed_at_unix);
        let quota_coverage_gap = quota_endpoint_mismatch(boundary.as_ref(), live.as_ref());
        if quota_coverage_gap {
            recent_tooltip.push_str("\n覆蓋：部分（額度缺口）");
        }
        self.request_database_view(start, t);
        let (mut tasks, mut cost, loading) = if let Some(view) = &self.tokens.database {
            let matches = main_view_matches(&view.request, start, self.demo_start, t);
            let mut tasks = view.main_tasks.clone();
            if !matches {
                for task in &mut tasks {
                    // Keep recent task identities visible during work, but do not
                    // attach a previous boundary's totals to the new selection.
                    task.interval_cost_usd = None;
                    task.interval_events = 0;
                    task.partial = true;
                }
            }
            let cost = if matches {
                view.main_cost
            } else {
                accounting::ObservedCost {
                    partial: true,
                    ..Default::default()
                }
            };
            (tasks, cost, !matches)
        } else {
            (
                self.tokens
                    .usage_index
                    .task_rows(self.tokens.events(), start, self.demo_start, t),
                self.tokens
                    .cost_index
                    .observed(start.unwrap_or(self.demo_start), t),
                false,
            )
        };
        if quota_coverage_gap {
            for task in &mut tasks {
                task.partial = true;
            }
        }
        if loading {
            recent_tooltip.push_str("\n狀態：更新中");
        }
        cost.partial |= start.is_none() || quota_coverage_gap;
        let tree =
            usage_views::task_tree_rows(&tasks, &self.tokens.task_parents, &self.expanded_tasks);
        self.main_cache = MainCache {
            tree,
            key: Some(key),
            tasks,
            boundary,
            cost,
            loading,
            recent_events,
            recent_tooltip,
            historical_rate: live.as_ref().and_then(|v| {
                quota_widget::consumption::historical_rate(
                    &self.history,
                    v,
                    self.quota.account_id(),
                )
            }),
        };
    }
    fn event_pricing_tooltip(&self, e: &TokenEvent) -> String {
        let model = format!(
            "模型：{}{}",
            e.model,
            if e.model_assumed { "（假設）" } else { "" }
        );
        if e.is_excluded_auto_review() {
            return format!("{model}\n狀態：排除\n單價：未知");
        }
        if let Some(rate) = self.tokens.pricing.rates.get(&e.model) {
            let long = e.cost_usd.is_some()
                && self.tokens.pricing.apply_long_context
                && matches!(
                    e.model.as_str(),
                    "gpt-6.1-sol" | "gpt-6-luna" | "gpt-6-astra"
                )
                && e.input > 272000
                && !e
                    .partial_flags
                    .iter()
                    .any(|f| f == "input_count_unavailable");
            let im = if long { 2. } else { 1. };
            let om = if long { 1.5 } else { 1. };
            format!(
                "{model}\n{}；USD / 1M：輸入{}／快取{}／輸出{}{}",
                self.tokens.pricing.tier_label,
                rate.uncached_input_per_million * im,
                rate.cached_input_per_million * im,
                rate.output_per_million * om,
                if long { "（單次長上下文）" } else { "" }
            )
        } else {
            format!("{model}\n單價：未知")
        }
    }
    fn task_tooltip(&self, task: &usage_views::TaskRow) -> String {
        let mut text = format!(
            "{}\nTask ID: {}",
            self.tokens.task_name(&task.task_id).unwrap_or("名稱未記錄"),
            task.task_id
        );
        if let Some(e) = self.tokens.events().get(task.latest_event_index) {
            text.push_str(&format!(
                "\n{}\nReasoning effort: {}\n顯示時間: {}",
                self.event_pricing_tooltip(e),
                e.reasoning_effort.as_deref().unwrap_or("未知"),
                display_time::datetime(e.timestamp).unwrap_or_else(|| "未知".into())
            ));
            let number = |n: u64, flag: &str| {
                if e.partial_flags.iter().any(|f| f == flag) {
                    "未知".to_owned()
                } else {
                    n.to_string()
                }
            };
            let uncached = if e
                .partial_flags
                .iter()
                .any(|f| f == "input_count_unavailable" || f == "cached_count_unavailable")
            {
                "未知".into()
            } else {
                e.uncached.to_string()
            };
            text.push_str(&format!("\n最新 Response: {}\nInput: {}\nCached: {}\nUncached: {}\nOutput: {}\nReasoning: {}",e.response_id.as_deref().unwrap_or("舊版／未知"),number(e.input,"input_count_unavailable"),number(e.cached,"cached_count_unavailable"),uncached,number(e.output,"output_count_unavailable"),number(e.reasoning,"reasoning_count_unavailable")));
        }
        if let Some(turn) = self.tokens.task_turn_usage.get(&task.task_id) {
            text.push_str(&format!(
                "\nTurn: {}\nTurn Input: {}\nTurn Output: {}\n{}{}",
                turn.turn_id.as_deref().unwrap_or("未知"),
                turn.input
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "未知".into()),
                turn.output
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "未知".into()),
                if turn.recorded_snapshot {
                    "已記錄 Turn 累計快照"
                } else {
                    "可用 Response 合計"
                },
                if turn.partial {
                    "（資料不完整）"
                } else {
                    ""
                }
            ));
        } else {
            text.push_str("\nTurn Input: 未知\nTurn Output: 未知");
        }
        if task.partial {
            text.push_str("\n狀態：部分");
        }
        text
    }
    fn recent_detail_text(&self) -> String {
        let age = if let Some(event) = &self.main_cache.boundary {
            format!(
                "{} {} 分鐘",
                if reset_event(event) {
                    "距本次重設"
                } else {
                    "距上次扣額"
                },
                (now() - event.observed_at_unix).max(0) / 60
            )
        } else {
            format!("自啟動 {} 分鐘", (now() - self.demo_start).max(0) / 60)
        };
        let details = self.main_cache.recent_tooltip.replacen(
            "最近 5 次扣額",
            &format!("最近 {} 次扣額", self.main_cache.recent_events.len()),
            1,
        );
        format!("{age}\n\n{details}")
    }
    fn widget(&mut self, ctx: &egui::Context) {
        self.refresh_main_cache();
        let palette = accessibility::palette(self.settings.dark);
        let (bg, fg, muted, line, accent) = (
            palette.background,
            palette.text,
            palette.text,
            palette.widget,
            palette.text,
        );
        let root_width = ctx.screen_rect().width();
        let font_multiplier = self.settings.font_scale_percent as f32 / 100.;
        let font_size =
            accessibility::scaled_minimum_font(root_width, self.settings.font_scale_percent);
        if (ctx.style().text_styles[&egui::TextStyle::Body].size - font_size).abs() > 0.1 {
            let mut style = (*ctx.style()).clone();
            accessibility::configure_style(&mut style, self.settings.dark, font_size);
            ctx.set_style(style);
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                let sx = rect.width() / 310.;
                let sy = sx;
                let scale = sx.clamp(0.85, 2.5);
                let painter = ui.painter().clone();
                let pt = |x: f32, y: f32| Pos2::new(rect.left() + x * sx, rect.top() + y * sy);
                painter.rect(
                    rect.shrink(1.),
                    22.,
                    bg,
                    Stroke::new(1_f32, line),
                    egui::StrokeKind::Inside,
                );
                let monitor_height = ctx.input(|i| i.viewport().monitor_size.map(|size| size.y));
                let mut details = 0.;
                let recent_text = if self.recent_expanded {
                    self.recent_detail_text()
                } else {
                    String::new()
                };
                if self.tasks_expanded {
                    details += (24. * sy).max(font_size * 1.4)
                        + (22. * sy).max(font_size * 1.4)
                            * self.main_cache.tree.len().max(1) as f32
                        + 52. * sy;
                }
                if self.recent_expanded {
                    details += painter
                        .layout(
                            recent_text.clone(),
                            FontId::proportional(font_size),
                            fg,
                            268. * sx,
                        )
                        .size()
                        .y
                        + 16. * sy;
                }
                if self.tasks_expanded || self.recent_expanded {
                    details += 42. * sy;
                }
                let height = widget_target_height(rect.width(), details, monitor_height);
                let suspended = ctx.input(|i| {
                    i.viewport().minimized.unwrap_or(false)
                        || i.viewport().maximized.unwrap_or(false)
                        || i.viewport().fullscreen.unwrap_or(false)
                });
                if !suspended {
                    let key = (
                        usize::from(self.tasks_expanded) | usize::from(self.recent_expanded) << 1,
                        height.round() as u32,
                        rect.width().round() as u32,
                    );
                    let plan = self.layout.plan(
                        key,
                        rect.size(),
                        height,
                        ctx.input(|i| i.pointer.any_down()),
                        Instant::now(),
                    );
                    if let Some(minimum) = plan.minimum {
                        ctx.send_viewport_cmd(ViewportCommand::MaxInnerSize(Vec2::new(
                            1000.,
                            monitor_height
                                .map(|h| (h - 60.).max(minimum.y))
                                .unwrap_or(2000.),
                        )));
                        ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(minimum));
                    }
                    if let Some(size) = plan.size {
                        ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
                    }
                    if let Some(delay) = plan.repaint_after {
                        ctx.request_repaint_after(delay);
                    }
                }
                let drag = ui.interact(
                    Rect::from_min_max(pt(4., 38.), pt(306., 119.)),
                    ui.id().with("drag"),
                    Sense::click_and_drag(),
                );
                let title_drag = ui.interact(
                    Rect::from_min_max(pt(4., 4.), pt(180., 37.)),
                    ui.id().with("title-drag"),
                    Sense::click_and_drag(),
                );
                if drag.drag_started() || title_drag.drag_started() {
                    ctx.send_viewport_cmd(ViewportCommand::StartDrag);
                }
                drag.context_menu(|ui| {
                    if ui.button(tr("設定 / 連接")).clicked() {
                        self.toggle_settings();
                        ui.close();
                    }
                    if ui.button(tr("更新額度")).clicked() {
                        self.refresh_quota();
                        ui.close();
                    }
                    if ui
                        .checkbox(&mut self.settings.dark, tr("深色外觀"))
                        .changed()
                    {
                        self.apply_theme(ctx);
                        self.dirty = true;
                    }
                    if ui
                        .checkbox(&mut self.settings.on_top, tr("保持最上層"))
                        .changed()
                    {
                        self.topmost(ctx);
                        self.dirty = true;
                    }
                    if ui.button(tr("關閉小工具")).clicked() {
                        self.begin_close();
                        ctx.request_repaint();
                    }
                });
                let view = self.selected();
                let title = view
                    .as_ref()
                    .filter(|v| v.window_duration_mins != Some(10080))
                    .map(|v| v.window_label.clone())
                    .unwrap_or_else(|| "Codex 額度 v3.37".into());
                painter.circle_filled(pt(21., 22.), 3. * scale, accent);
                painter
                    .with_clip_rect(Rect::from_min_max(pt(32., 10.), pt(181., 35.)))
                    .text(
                        pt(33., 14.),
                        Align2::LEFT_TOP,
                        tr(title),
                        FontId::proportional(font_size),
                        fg,
                    );
                if widget_icon(
                    ui,
                    Rect::from_min_max(pt(181., 10.), pt(205., 34.)),
                    WidgetIcon::History,
                    None,
                    "扣額紀錄",
                    if self.history_open { bg } else { fg },
                    if self.history_open { accent } else { line },
                )
                .clicked()
                {
                    self.set_history_open(!self.history_open);
                }
                if widget_icon(
                    ui,
                    Rect::from_min_max(pt(209., 10.), pt(233., 34.)),
                    WidgetIcon::Tokens,
                    None,
                    "Token明細",
                    if self.tokens_open { bg } else { fg },
                    if self.tokens_open { accent } else { line },
                )
                .clicked()
                {
                    self.tokens_open = !self.tokens_open;
                }
                if widget_icon(
                    ui,
                    Rect::from_min_max(pt(265., 10.), pt(289., 34.)),
                    WidgetIcon::Close,
                    None,
                    "關閉小工具",
                    fg,
                    line,
                )
                .clicked()
                {
                    self.begin_close();
                    ctx.request_repaint();
                }
                let percent = view
                    .as_ref()
                    .and_then(|v| v.remaining_percent)
                    .map(|r| {
                        if r.fract().abs() < 1e-7 {
                            format!("{r:.0}")
                        } else {
                            format!("{r:.1}")
                        }
                    })
                    .unwrap_or_else(|| "—".into());
                let number_font = 60. * scale * font_multiplier.min(1.);
                let unit_font = 28. * scale * font_multiplier.min(1.);
                let number_width = painter
                    .layout_no_wrap(tr(percent.clone()), FontId::proportional(number_font), fg)
                    .size()
                    .x;
                let unit_width = painter
                    .layout_no_wrap(tr("%"), FontId::proportional(unit_font), fg)
                    .size()
                    .x;
                let number_fit = (115. * sx / (number_width + unit_width + 5. * sx)).min(1.);
                painter.text(
                    pt(20., 38.),
                    Align2::LEFT_TOP,
                    tr(percent.clone()),
                    FontId::proportional(number_font * number_fit),
                    fg,
                );
                let percent_width = painter
                    .layout_no_wrap(
                        tr(percent),
                        FontId::proportional(number_font * number_fit),
                        fg,
                    )
                    .size()
                    .x;
                painter.text(
                    pt(25. + percent_width / sx, 66.),
                    Align2::LEFT_TOP,
                    tr("%"),
                    FontId::proportional(unit_font * number_fit),
                    fg,
                );
                let bar = Rect::from_min_max(pt(145., 97.), pt(289., 100.));
                painter.rect_filled(bar, 3., line);
                if let Some(p) = view.as_ref().and_then(|v| v.remaining_percent) {
                    painter.rect_filled(
                        Rect::from_min_size(
                            bar.min,
                            Vec2::new(bar.width() * (p as f32 / 100.).clamp(0., 1.), bar.height()),
                        ),
                        3.,
                        accent,
                    );
                }
                let reset = view
                    .as_ref()
                    .and_then(|v| v.reset_at)
                    .map(|t| duration(t - now()))
                    .unwrap_or_else(|| "—".into());
                let historical_rate = self.main_cache.historical_rate;
                let measured_rate = view
                    .as_ref()
                    .and_then(|v| v.rate_per_hour)
                    .or(historical_rate);
                let rate = measured_rate
                    .map(|r| format!("{r:.2} 百分點 / 小時"))
                    .unwrap_or_else(|| "— 等待量測".into());
                let credits = self.quota.credits(now());
                let credit = credits
                    .iter()
                    .find(|c| view.as_ref().is_none_or(|v| c.source_key == v.source_key));
                let credit_text = credit
                    .map(|c| {
                        if c.unlimited {
                            "不限".into()
                        } else {
                            c.balance.clone().unwrap_or_else(|| "—".into())
                        }
                    })
                    .unwrap_or_else(|| "—".into());
                let short_credit = credit_text
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .map(|v| format!("{v:.2}"))
                    .unwrap_or_else(|| credit_text.clone());
                let short_rate = measured_rate
                    .map(|r| format!("{r:.2}%/時"))
                    .unwrap_or_else(|| "等待量測".into());
                let short_rate = if view.as_ref().is_some_and(|v| v.rate_per_hour.is_none())
                    && historical_rate.is_some()
                {
                    format!("{short_rate}（歷史）")
                } else {
                    short_rate
                };
                let short_rate = tr(short_rate);
                let rate = tr(rate);
                let short_credit = tr(short_credit);
                let reminder = self.settings.prediction_target();
                let summary = [
                    format!("重設 {reset}"),
                    format!("消耗 {short_rate}"),
                    format!("點數 {short_credit}"),
                ];
                for (index, text) in summary.into_iter().enumerate() {
                    let text = tr(text);
                    let y = 40. + index as f32 * 17.;
                    let available = 144. * sx;
                    let measured = painter
                        .layout_no_wrap(tr(text.clone()), FontId::proportional(font_size), fg)
                        .size()
                        .x;
                    let fitted = (font_size * (available / measured.max(1.)).min(1.)).min(16. * sx);
                    painter.text(
                        pt(145., y),
                        Align2::LEFT_TOP,
                        tr(text),
                        FontId::proportional(fitted),
                        fg,
                    );
                }
                ui.interact(
                    Rect::from_min_max(pt(145., 40.), pt(289., 104.)),
                    ui.id().with("quota-details"),
                    Sense::hover(),
                )
                .on_hover_text(tr(format!(
                    "重設：{reset}\n估計消耗：{rate}\n點數：{credit_text}{}",
                    reminder
                        .map(|_| format!(
                            "\n自訂提醒：{}",
                            reminder
                                .and_then(display_time::datetime)
                                .unwrap_or_default()
                        ))
                        .unwrap_or_default()
                )));
                if reminder.is_none() {
                    if widget_icon(
                        ui,
                        Rect::from_min_max(pt(21., 117.), pt(73., 141.)),
                        WidgetIcon::Prediction,
                        Some(self.prediction_open),
                        "Reset預測",
                        fg,
                        line,
                    )
                    .clicked()
                    {
                        self.open_prediction();
                    }
                } else {
                    let prediction_title = if reminder.is_some() {
                        "下次 reset 預測："
                    } else {
                        "Reset預測"
                    };
                    let title_width = painter
                        .layout_no_wrap(tr(prediction_title), FontId::proportional(font_size), fg)
                        .size()
                        .x;
                    painter.text(
                        pt(21., 105.),
                        Align2::LEFT_TOP,
                        tr(prediction_title),
                        FontId::proportional(font_size * (155. * sx / title_width.max(1.)).min(1.)),
                        fg,
                    );
                    let reminder_now = now();
                    let timer_color = if reminder.is_some_and(|target| reminder_now > target) {
                        if self.settings.dark {
                            Color32::from_rgb(255, 120, 110)
                        } else {
                            Color32::from_rgb(184, 35, 35)
                        }
                    } else {
                        fg
                    };
                    painter.text(
                        pt(21., 124.),
                        Align2::LEFT_TOP,
                        tr(reminder
                            .map(|target| reset_reminder_countdown(target, reminder_now))
                            .unwrap_or_else(|| "未設定".into())),
                        FontId::proportional((18. * scale * font_multiplier).max(font_size)),
                        timer_color,
                    );
                    if ui
                        .interact(
                            Rect::from_min_max(pt(21., 103.), pt(176., 150.)),
                            ui.id().with("reset-prediction"),
                            Sense::click(),
                        )
                        .on_hover_text(tr("設定或清除 Reset 預測"))
                        .clicked()
                    {
                        self.open_prediction();
                    }
                }
                if widget_icon(
                    ui,
                    Rect::from_min_max(pt(237., 10.), pt(261., 34.)),
                    WidgetIcon::Settings,
                    Some(self.settings_open),
                    "設定 / 連接",
                    fg,
                    line,
                )
                .clicked()
                {
                    self.toggle_settings();
                }
                if widget_icon(
                    ui,
                    Rect::from_min_max(pt(181., 117.), pt(233., 141.)),
                    WidgetIcon::Tasks,
                    Some(self.tasks_expanded),
                    "展開 / 收起對話列表",
                    fg,
                    line,
                )
                .clicked()
                {
                    self.tasks_expanded = !self.tasks_expanded;
                    ctx.request_repaint();
                }
                if widget_icon(
                    ui,
                    Rect::from_min_max(pt(237., 117.), pt(289., 141.)),
                    WidgetIcon::Recent,
                    Some(self.recent_expanded),
                    "展開 / 收起最近扣額",
                    fg,
                    line,
                )
                .clicked()
                {
                    self.recent_expanded = !self.recent_expanded;
                    ctx.request_repaint();
                }
                if self.tasks_expanded || self.recent_expanded {
                    let content = Rect::from_min_max(
                        pt(21., 158.),
                        Pos2::new(pt(289., 158.).x, rect.bottom() - 12. * sy),
                    );
                    ui.scope_builder(egui::UiBuilder::new().max_rect(content), |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("widget-details")
                            .max_height(content.height().max(0.))
                            .min_scrolled_height(0.)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if self.tasks_expanded {
                                    self.widget_task_rows(
                                        ui, ctx, sx, sy, font_size, fg, muted, accent,
                                    );
                                    let footer = if self.main_cache.loading {
                                        "更新中".into()
                                    } else {
                                        format!(
                                            "本%累計 {}{}",
                                            money(Some(self.main_cache.cost.known_usd)),
                                            if self.main_cache.cost.partial {
                                                " *"
                                            } else {
                                                ""
                                            }
                                        )
                                    };
                                    ui.add_space(6. * sy);
                                    ui.label(RichText::new(tr(footer)).size(font_size).color(fg));
                                }
                                if self.recent_expanded {
                                    if self.tasks_expanded {
                                        ui.separator();
                                    }
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(tr(&recent_text))
                                                .size(font_size)
                                                .color(fg),
                                        )
                                        .wrap(),
                                    );
                                }
                                if let Some(progress) = self.compact_status_text() {
                                    ui.separator();
                                    if ui
                                        .add(
                                            egui::Label::new(
                                                RichText::new(tr(progress))
                                                    .size(font_size)
                                                    .color(muted),
                                            )
                                            .wrap()
                                            .sense(Sense::click()),
                                        )
                                        .on_hover_text(tr(self.import_diagnostic_text()))
                                        .clicked()
                                    {
                                        self.open_settings();
                                    }
                                }
                            });
                    });
                }
                let resize = ui.interact(
                    Rect::from_min_max(
                        Pos2::new(rect.right() - 16. * sx, rect.bottom() - 16. * sy),
                        rect.max,
                    ),
                    ui.id().with("resize"),
                    Sense::drag(),
                );
                if resize.is_pointer_button_down_on()
                    && ui.input(|i| i.pointer.button_pressed(egui::PointerButton::Primary))
                {
                    ctx.send_viewport_cmd(ViewportCommand::BeginResize(
                        egui::ResizeDirection::SouthEast,
                    ));
                }
            });
    }
    fn widget_task_rows(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        sx: f32,
        sy: f32,
        font_size: f32,
        fg: Color32,
        muted: Color32,
        accent: Color32,
    ) {
        let row_height = (22. * sy).max(font_size * 1.4);
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        let (header, _) = ui.allocate_exact_size(
            Vec2::new(268. * sx, (24. * sy).max(font_size * 1.4)),
            Sense::hover(),
        );
        for (x, label) in [
            (0., "對話"),
            (84., "時間"),
            (142., "用度"),
            (204., "本%累計"),
        ] {
            ui.painter().text(
                header.min + Vec2::new(x * sx, 0.),
                Align2::LEFT_TOP,
                tr(label),
                FontId::proportional(font_size),
                fg,
            );
        }
        ui.interact(header, ui.id().with("task-column-help"), Sense::hover())
            .on_hover_text(tr("用度：最新 Response\n本%累計：自身估算 USD\n*：部分"));
        if self.main_cache.tree.is_empty() {
            ui.label(
                RichText::new(tr(self.empty_task_status()))
                    .size(font_size)
                    .color(muted),
            );
        }
        for i in 0..self.main_cache.tree.len() {
            let row = &self.main_cache.tree[i];
            let (r, response) =
                ui.allocate_exact_size(Vec2::new(268. * sx, row_height), Sense::hover());
            match row {
                usage_views::TaskTreeRow::SelfTask {
                    task_id,
                    task_index,
                    depth,
                    relationship,
                } => {
                    let task = task_index.and_then(|i| self.main_cache.tasks.get(i));
                    let mut tooltip=task.map(|t|self.task_tooltip(t)).unwrap_or_else(||format!("{}\nTask ID: {}\n狀態：無近期用量\nInput: 未知\nCached: 未知\nUncached: 未知\nOutput: 未知\nReasoning: 未知\nTurn Input: 未知\nTurn Output: 未知",self.tokens.task_name(task_id).unwrap_or("名稱未記錄"),task_id));
                    let relation = match relationship {
                        usage_views::TaskRelationship::Unlinked => "上層：未知".into(),
                        usage_views::TaskRelationship::Child { parent_id } => {
                            format!("上層: {parent_id}")
                        }
                        usage_views::TaskRelationship::Orphan { parent_id } => {
                            format!("上層（部分）: {}", parent_id.as_deref().unwrap_or("未知"))
                        }
                        usage_views::TaskRelationship::Cycle { parent_id } => {
                            format!("上層（循環）: {parent_id}")
                        }
                    };
                    tooltip.push_str(&format!("\n{relation}"));
                    response.on_hover_ui(|ui| {
                        readable_tooltip(ui, &tooltip, font_size);
                    });
                    let indent = (*depth as f32 * 8.).min(32.);
                    let name = self
                        .tokens
                        .task_name(task_id)
                        .map(str::to_owned)
                        .unwrap_or_else(|| task_id.chars().take(7).collect());
                    let rect = Rect::from_min_size(
                        r.min + Vec2::new(indent * sx, 0.),
                        Vec2::new((81. - indent) * sx, row_height),
                    );
                    let label = if *depth > 0 {
                        format!("└ {name}")
                    } else {
                        name
                    };
                    let font = FontId::proportional(font_size);
                    let mut short = label.clone();
                    while ui
                        .painter()
                        .layout_no_wrap(tr(short.clone()), font.clone(), fg)
                        .size()
                        .x
                        > rect.width() - 2. * sx
                        && short.chars().count() > 2
                    {
                        if short.ends_with('…') {
                            short.pop();
                        }
                        short.pop();
                        short.push('…');
                    }
                    ui.painter()
                        .with_clip_rect(rect.intersect(ui.clip_rect()))
                        .text(
                            rect.min + Vec2::new(0., 2. * sy),
                            Align2::LEFT_TOP,
                            tr(short),
                            font,
                            fg,
                        );
                    if ui
                        .interact(rect, ui.id().with(("task-name", task_id)), Sense::click())
                        .on_hover_ui(|ui| {
                            readable_tooltip(ui, &tooltip, font_size);
                        })
                        .clicked()
                    {
                        ctx.copy_text(task_id.clone());
                    }
                    let values = if let Some(task) = task {
                        [
                            (84., local_time(task.latest_timestamp)),
                            (142., money(task.latest_cost_usd)),
                            (
                                204.,
                                format!(
                                    "{}{}",
                                    money(task.interval_cost_usd),
                                    if task.partial { "*" } else { "" }
                                ),
                            ),
                        ]
                    } else {
                        [
                            (84., "—".into()),
                            (142., "—".into()),
                            (
                                204.,
                                if self.main_cache.loading {
                                    "$—*".into()
                                } else {
                                    "$0.0000*".into()
                                },
                            ),
                        ]
                    };
                    for (x, text) in values {
                        let cell = Rect::from_min_size(
                            r.min + Vec2::new(x * sx, 0.),
                            Vec2::new(if x == 84. { 56. } else { 62. } * sx, row_height),
                        );
                        ui.painter()
                            .with_clip_rect(cell.intersect(ui.clip_rect()))
                            .text(
                                cell.min + Vec2::new(0., 2. * sy),
                                Align2::LEFT_TOP,
                                tr(&text),
                                FontId::proportional(font_size),
                                fg,
                            );
                        ui.interact(
                            cell,
                            ui.id().with(("task-cell", task_id, x as i32)),
                            Sense::hover(),
                        )
                        .on_hover_ui(|ui| {
                            readable_tooltip(ui, &format!("{}\n{}", text, tooltip), font_size)
                        });
                    }
                }
                usage_views::TaskTreeRow::ChildrenSummary {
                    parent_id,
                    depth,
                    descendant_count,
                    direct_child_count,
                    known_cost_usd,
                    partial,
                    expanded,
                } => {
                    let indent = (*depth as f32 * 8.).min(32.);
                    let label = format!(
                        "[{}] 子代理 {}",
                        if *expanded { "−" } else { "+" },
                        descendant_count
                    );
                    let help=format!("顯示子代理：{descendant_count}\n直接分支：{direct_child_count}\n範圍：近2h／本%\n本%累計：{}{}",money(Some(*known_cost_usd)),if *partial {" *"} else {""});
                    response.on_hover_text(tr(&help));
                    let button_rect = Rect::from_min_size(
                        r.min + Vec2::new(indent * sx, 0.),
                        Vec2::new((200. - indent) * sx, row_height),
                    );
                    ui.painter().text(
                        button_rect.min + Vec2::new(0., 2. * sy),
                        Align2::LEFT_TOP,
                        tr(label),
                        FontId::proportional(font_size),
                        accent,
                    );
                    if ui
                        .interact(
                            button_rect,
                            ui.id().with(("children-toggle", parent_id)),
                            Sense::click(),
                        )
                        .on_hover_text(tr(&help))
                        .clicked()
                    {
                        if *expanded {
                            self.expanded_tasks.remove(parent_id);
                        } else {
                            self.expanded_tasks.insert(parent_id.clone());
                        }
                        self.tree_generation += 1;
                    }
                    ui.painter().text(
                        r.min + Vec2::new(204. * sx, 2. * sy),
                        Align2::LEFT_TOP,
                        tr(format!(
                            "{}{}",
                            money((!self.main_cache.loading).then_some(*known_cost_usd)),
                            if *partial { "*" } else { "" }
                        )),
                        FontId::proportional(font_size),
                        accent,
                    );
                }
            }
        }
    }
    fn topmost(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd_to(
            ViewportId::ROOT,
            ViewportCommand::WindowLevel(if self.settings.on_top {
                WindowLevel::AlwaysOnTop
            } else {
                WindowLevel::Normal
            }),
        );
        self.last_topmost = Instant::now();
    }
    fn date_controls(
        ui: &mut egui::Ui,
        date: &mut NaiveDate,
        dates: &BTreeSet<NaiveDate>,
        id: &str,
    ) {
        ui.horizontal_wrapped(|ui| {
            if ui.button(tr("◀ 前一天")).clicked() {
                *date = date.pred_opt().unwrap_or(*date);
            }
            egui::ComboBox::from_id_salt(id)
                .selected_text(tr(date.to_string()))
                .show_ui(ui, |ui| {
                    for d in dates.iter().rev() {
                        ui.selectable_value(date, *d, tr(d.to_string()));
                    }
                });
            if ui.button(tr("後一天 ▶")).clicked() {
                *date = date.succ_opt().unwrap_or(*date);
            }
            if ui.button(tr("今天")).clicked() {
                *date = display_time::now_day();
            }
        });
        if *date > display_time::now_day() {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                tr("所選日期晚於今天；可按「今天」查看當日紀錄。"),
            );
        }
    }
    fn days(&mut self) -> BTreeSet<NaiveDate> {
        let today = display_time::now_day();
        let key = (
            self.token_generation,
            self.history_generation,
            today,
            self.settings.history_days,
        );
        if self.dates_key != Some(key) {
            self.dates_cache = (0..self.settings.history_days.clamp(1, 3650))
                .filter_map(|i| today.checked_sub_days(chrono::Days::new(i as u64)))
                .collect();
            for e in self.history.iter() {
                if let Some(d) = local_day(e.observed_at_unix) {
                    self.dates_cache.insert(d);
                }
            }
            if let Some(view) = &self.tokens.database {
                self.dates_cache
                    .extend(view.day_starts.iter().filter_map(|&start| local_day(start)));
            } else {
                for e in self.tokens.events() {
                    if let Some(d) = local_day(e.timestamp) {
                        self.dates_cache.insert(d);
                    }
                }
            }
            self.dates_key = Some(key);
        }
        self.dates_cache.clone()
    }
    fn set_history_open(&mut self, open: bool) {
        self.history_open = open;
        if !open {
            // Reopening explicitly refreshes the selected day once.
            self.history_cache.key = None;
            self.history_cache.work_key = None;
            self.history_cache.usage_receiver = None;
        }
    }
    fn remember_detail_position(&mut self, ctx: &egui::Context, history: bool) {
        if ctx.viewport_id() == ViewportId::ROOT {
            return;
        }
        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
            let position = [rect.min.x, rect.min.y];
            let saved = if history {
                &mut self.settings.history_position
            } else {
                &mut self.settings.tokens_position
            };
            if *saved != Some(position) && position.iter().all(|v| v.is_finite()) {
                *saved = Some(position);
                self.dirty = true;
            }
        }
    }
    fn detail_windows(&mut self, ctx: &egui::Context) {
        if self.history_open {
            let mut viewport = ViewportBuilder::default()
                .with_title(tr("Quota Widget · 扣額紀錄"))
                .with_inner_size([1120., 760.])
                .with_min_inner_size([560., 400.]);
            if let Some(position) = self.settings.history_position {
                viewport = viewport.with_position(position);
            }
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("quota-history"),
                viewport,
                |ctx, _| {
                    self.remember_detail_position(ctx, true);
                    if ctx.input(|i| i.viewport().close_requested()) {
                        self.set_history_open(false);
                    } else {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            self.history_ui(ui);
                        });
                    }
                },
            );
        }
        if self.tokens_open {
            let mut viewport = ViewportBuilder::default()
                .with_title(tr("Quota Widget · Token 明細"))
                .with_inner_size([1120., 650.])
                .with_min_inner_size([680., 420.]);
            if let Some(position) = self.settings.tokens_position {
                viewport = viewport.with_position(position);
            }
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("token-history"),
                viewport,
                |ctx, _| {
                    self.remember_detail_position(ctx, false);
                    if ctx.input(|i| i.viewport().close_requested()) {
                        self.tokens_open = false;
                        return;
                    }
                    egui::CentralPanel::default().show(ctx, |ui| {
                        self.token_ui(ui);
                    });
                },
            );
        }
        if self.settings_open {
            let first = !self.settings_presented;
            self.settings_presented = true;
            let mut builder = ViewportBuilder::default()
                .with_title(tr("Quota Widget · 設定 / 連接"))
                .with_inner_size([760., 700.])
                .with_min_inner_size([520., 420.])
                .with_visible(true)
                .with_window_level(WindowLevel::AlwaysOnTop);
            if first {
                if let Some(position) = self.settings.settings_position {
                    builder = builder.with_position(position);
                } else if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                    builder = builder.with_position(rect.min);
                }
                builder = builder.with_active(true);
            }
            ctx.show_viewport_immediate(ViewportId::from_hash_of("settings"), builder, |ctx, _| {
                self.remember_settings_position(ctx);
                if first {
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                }
                if ctx.input(|i| i.viewport().close_requested()) {
                    self.settings_open = false;
                    self.settings_presented = false;
                    return;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    egui::ScrollArea::both().show(ui, |ui| self.settings_ui(ui, ctx));
                });
            });
        } else {
            self.settings_presented = false;
        }
        if self.prediction_open {
            let first = !self.prediction_presented;
            self.prediction_presented = true;
            let mut builder = ViewportBuilder::default()
                .with_title(tr("Reset預測"))
                .with_inner_size([350., 150.])
                .with_resizable(false)
                .with_visible(true)
                .with_window_level(WindowLevel::AlwaysOnTop);
            if first {
                builder = builder.with_active(true);
                if let Some(position) = self.settings.prediction_position {
                    builder = builder.with_position(position);
                } else if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                    builder = builder.with_position(rect.min);
                }
            }
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("reset-prediction-dialog"),
                builder,
                |ctx, _| {
                    if first {
                        ctx.send_viewport_cmd(ViewportCommand::Focus);
                    }
                    self.remember_prediction_position(ctx);
                    if ctx.input(|i| i.viewport().close_requested()) {
                        self.prediction_open = false;
                        return;
                    }
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let content = ui.vertical(|ui| self.prediction_ui(ui));
                        let size = content.response.rect.size() + Vec2::splat(16.);
                        ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
                    });
                },
            );
        }
    }
    fn history_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading(tr("扣額紀錄"));
        if self.demo {
            ui.colored_label(
                if self.settings.dark {
                    Color32::YELLOW
                } else {
                    Color32::from_rgb(130, 75, 0)
                },
                tr("DEMO"),
            );
        }
        ui.label(tr(format!(
            "時間 {} · Token 成本估算 USD",
            display_time::zone_label()
        )));
        if self.history_cache.sources_generation != Some(self.history_generation) {
            self.history_cache.sources = self
                .history
                .iter()
                .map(|e| {
                    (
                        e.source_key.clone(),
                        e.window_key.clone(),
                        e.window_label.clone(),
                    )
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            self.history_cache.has_legacy = self.history.iter().any(|e| {
                e.window_key.is_none()
                    && e.evidence.is_none()
                    && e.source_key
                        .split_once(':')
                        .is_some_and(|(prefix, _)| prefix.parse::<u64>().is_ok())
            });
            self.history_cache.sources_generation = Some(self.history_generation);
        }
        egui::ComboBox::from_label(tr("來源 / 額度視窗"))
            .selected_text(tr(self
                .history_selection
                .as_ref()
                .map(|(s, k, l)| format!("{s} · {l} · {}", k.as_deref().unwrap_or("舊版")))
                .unwrap_or_else(|| "目前小工具選擇".into())))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.history_selection, None, tr("目前小工具選擇"));
                for (s, k, l) in &self.history_cache.sources {
                    ui.selectable_value(
                        &mut self.history_selection,
                        Some((s.clone(), k.clone(), l.clone())),
                        tr(format!("{s} · {l} · {}", k.as_deref().unwrap_or("舊版"))),
                    );
                }
            });
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.history_intervals, true, tr("每%用度表"));
            ui.selectable_value(&mut self.history_intervals, false, tr("全部額度觀察"));
        });
        let dates = self.days();
        Self::date_controls(ui, &mut self.date, &dates, "quota-date");
        ui.horizontal_wrapped(|ui| {
            if ui
                .checkbox(&mut self.settings.hide_unchanged, tr("忽略相同百分比"))
                .changed()
            {
                self.dirty = true;
            }
            if ui.button(tr("重新整理")).clicked() {
                self.history_cache.key = None;
                self.history_cache.work_key = None;
            }
            if ui.button(tr("匯出當日 CSV")).clicked() {
                self.export_day(false, false);
            }
            if ui.button(tr("匯出當日 JSON")).clicked() {
                self.export_day(false, true);
            }
        });
        let current_view = self.selected();
        if let Some(v) = &current_view {
            ui.separator();
            ui.label(tr(&v.window_label));
            if let Some(t) = v.prediction.estimated_at {
                ui.label(tr(format!(
                    "下次下降約 {}（估算，{}）",
                    local_time(t),
                    v.prediction.confidence.label()
                )));
            }
        }
        let selection_key = serde_json::to_string(&(
            &self.history_selection,
            &self.settings.bucket,
            &self.settings.window,
            current_view
                .as_ref()
                .map(|v| (&v.source_key, &v.window_key, &v.window_label)),
        ))
        .unwrap_or_default();
        let key = (
            self.date,
            selection_key,
            self.settings.hide_unchanged,
            self.history_intervals,
        );
        if self.history_cache.key.as_ref() != Some(&key)
            && self.history_cache.work_key.as_ref() != Some(&key)
        {
            let all = self.browsed_history_on_day(self.date);
            // One batch pass, instead of one full-history scan for every row.
            let costs = if self.tokens.database.is_some() {
                all.iter()
                    .map(|event| {
                        canonical_interval_cost(
                            self.interval_lookup
                                .get(&intervals::history_event_key(event))
                                .and_then(|&index| self.intervals.get(index)),
                        )
                    })
                    .collect()
            } else {
                accounting::history_costs(&self.tokens.cost_index, &all)
            };
            self.history_cache.rows = all
                .iter()
                .cloned()
                .zip(costs)
                .filter_map(|(event, (cost, partial))| {
                    (local_day(event.observed_at_unix) == Some(self.date)
                        && (!self.settings.hide_unchanged || !unchanged(&event)))
                    .then_some((event, cost, partial))
                })
                .collect();
            self.history_cache.rows.reverse();
            self.history_cache.table_rows.clear();
            self.history_cache.interval_indices.clear();
            self.history_cache.pending_segments.clear();
            self.history_cache.day_token_indices.clear();
            self.history_cache.next_segment = 0;
            self.history_cache.usage_receiver = None;
            self.history_cache.weighted_usd = 0.0;
            self.history_cache.weighted_points = 0.0;
            self.history_cache.weighted_samples = 0;
            self.history_cache.has_lower_bound = false;
            self.history_cache.weighted = None;
            self.history_cache.calculation_error = None;
            if self.history_intervals {
                if let Some((day_start, day_end)) = display_time::day_bounds(self.date) {
                    let same_live_scope = current_view.as_ref().is_some_and(|view| {
                        self.history_selection
                            .as_ref()
                            .is_none_or(|(source, window, label)| {
                                view.source_key == *source
                                    && (window.as_deref() == Some(view.window_key.as_str())
                                        || (window.is_none() && view.window_label == *label))
                            })
                    });
                    let future_remaining = (self.date == display_time::now_day()
                        && same_live_scope)
                        .then(|| {
                            current_view
                                .as_ref()
                                .and_then(|view| view.remaining_percent)
                        })
                        .flatten();
                    self.history_cache.pending_segments =
                        daily_quota_segments(&all, day_start, day_end, future_remaining, now());
                    if self.tokens.database.is_none() {
                        let tokens = self.tokens.events();
                        // TokenStore normalizes events by (timestamp, id); binary
                        // bounds avoid scanning years of token rows before showing
                        // the first segment for the selected day.
                        let lo = tokens.partition_point(|event| event.timestamp < day_start);
                        let hi = tokens.partition_point(|event| event.timestamp < day_end);
                        self.history_cache.day_token_indices = (lo..hi).collect();
                    }
                }
            }
            if let Some(view) = self
                .tokens
                .database
                .as_ref()
                .filter(|_| self.history_intervals)
            {
                self.history_cache.usage_receiver = Some(start_history_usage(
                    view.database_path.clone(),
                    self.history_cache.pending_segments.clone(),
                    self._lock.is_some(),
                    ui.ctx().clone(),
                ));
            }
            self.history_cache.work_key = Some(key.clone());
        }
        let calculation_frame = Instant::now();
        while self.history_cache.work_key.as_ref() == Some(&key)
            && self.history_cache.next_segment < self.history_cache.pending_segments.len()
        {
            let segment =
                self.history_cache.pending_segments[self.history_cache.next_segment].clone();
            let usage = if let Some(receiver) = &self.history_cache.usage_receiver {
                match receiver.try_recv() {
                    Ok(usage) => usage,
                    Err(mpsc::TryRecvError::Empty) => {
                        ui.ctx().request_repaint_after(Duration::from_millis(50));
                        break;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        Err(std::io::Error::other("History worker stopped"))
                    }
                }
            } else {
                let tokens = self.tokens.events();
                let lo = self
                    .history_cache
                    .day_token_indices
                    .partition_point(|&index| tokens[index].timestamp <= segment.start);
                let hi = self
                    .history_cache
                    .day_token_indices
                    .partition_point(|&index| tokens[index].timestamp <= segment.end);
                let events: Vec<_> = self.history_cache.day_token_indices[lo..hi]
                    .iter()
                    .map(|&index| &tokens[index])
                    .collect();
                Ok(CachedSegmentUsage {
                    models: intervals::model_usage(events.iter().copied()),
                    counts: interval_task_counts(&events),
                    cost: self.tokens.cost_index.observed(segment.start, segment.end),
                })
            };
            let Ok(CachedSegmentUsage {
                models,
                counts: task_counts,
                cost,
            }) = usage
            else {
                self.history_cache.calculation_error =
                    Some("無法讀取此區間的 Token 資料；請重新開啟扣額記錄重試。".into());
                self.history_cache.key = Some(key.clone());
                self.history_cache.work_key = None;
                self.history_cache.usage_receiver = None;
                break;
            };
            let presentation = segment.event.as_ref().map_or_else(
                || active_segment_presentation(segment.change.clone(), segment.start, cost),
                |event| interval_views::recovered_observation(event, segment.start, cost),
            );
            let presentation = if segment.points == 0.0 {
                let mut marker = presentation;
                marker.status = "額度重設".into();
                marker.tooltip = "額度重設；此行只標示新週期開始，不計 Token 費用。".into();
                marker
            } else {
                presentation
            };
            if cost.events > 0 && segment.points > 0.0 {
                self.history_cache.weighted_usd += cost.known_usd;
                self.history_cache.weighted_points += segment.points;
                self.history_cache.weighted_samples += 1;
                self.history_cache.has_lower_bound |= cost.partial || cost.unknown_events > 0;
                self.history_cache.weighted =
                    Some(self.history_cache.weighted_usd / self.history_cache.weighted_points);
            }
            self.history_cache.table_rows.push(TableIntervalRow {
                interval_index: None,
                presentation,
                models,
                task_counts,
            });
            if let Some(index) = segment.event.as_ref().and_then(|event| {
                self.interval_lookup
                    .get(&intervals::history_event_key(event))
            }) {
                self.history_cache.interval_indices.push(*index);
            }
            self.history_cache.next_segment += 1;
            if self.history_cache.next_segment == self.history_cache.pending_segments.len() {
                self.history_cache.key = Some(key.clone());
                self.history_cache.work_key = None;
            } else {
                ui.ctx().request_repaint_after(Duration::from_millis(16));
            }
            // Drain already-computed database rows without forcing one frame per row.
            if self.history_cache.usage_receiver.is_none()
                || calculation_frame.elapsed() >= Duration::from_millis(8)
            {
                break;
            }
        }
        if self.history_cache.work_key.as_ref() == Some(&key)
            && self.history_cache.pending_segments.is_empty()
        {
            self.history_cache.key = Some(key.clone());
            self.history_cache.work_key = None;
        }
        ui.label(tr(format!(
            "紀錄：{} 筆",
            if self.history_intervals {
                self.history_cache.table_rows.len()
            } else {
                self.history_cache.rows.len()
            }
        )));
        ui.separator();
        ui.label(tr(format!(
            "所列區間加權平均：{}{} / 百分點",
            if self.history_cache.has_lower_bound {
                "≥ "
            } else {
                "≈ "
            },
            money(self.history_cache.weighted)
        )));
        if self.history_cache.weighted_samples > 0 {
            ui.label(tr(format!(
                "樣本：{} 個區間 · {} 百分點{}",
                self.history_cache.weighted_samples,
                self.history_cache.weighted_points,
                if self.history_cache.has_lower_bound {
                    " · Token 覆蓋不完整，僅供下限估算"
                } else {
                    ""
                }
            )));
        }
        if let Some(message) = self.empty_history_diagnostic() {
            ui.colored_label(ui.visuals().warn_fg_color, tr(message));
        }
        if self.history_intervals {
            self.completed_intervals_table(ui);
        } else if self.history_cache.rows.is_empty() {
            ui.label(tr("額度紀錄：0"));
        } else {
            let row_height = 58. * ui.style().text_styles[&egui::TextStyle::Body].size / 13.;
            egui::ScrollArea::vertical()
                .id_salt("quota-history-rows")
                .show_rows(
                    ui,
                    row_height,
                    self.history_cache.rows.len(),
                    |ui, range| {
                        for index in range {
                            let (event, cost, partial) = &self.history_cache.rows[index];
                            let (rect, _) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), row_height),
                                Sense::hover(),
                            );
                            let mut row = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                            row.set_clip_rect(rect.intersect(ui.clip_rect()));
                            let interval = self
                                .interval_lookup
                                .get(&intervals::history_event_key(event))
                                .and_then(|i| self.intervals.get(*i));
                            let presentation = self
                                .interval_lookup
                                .get(&intervals::history_event_key(event))
                                .map(|&i| &self.interval_presentations[i]);
                            let description = presentation
                                .map(|view| {
                                    format!(
                                        "{} · 統一扣額估算 {} · 區間 Token 小計 {}",
                                        view.change, view.total, view.observed_total
                                    )
                                })
                                .unwrap_or_else(|| {
                                    history_description(event, *cost, *partial, interval)
                                });
                            let tooltip = format!(
                                "{}\n來源：{} · 觀察工作階段：{}",
                                presentation
                                    .map(|view| view.tooltip.clone())
                                    .unwrap_or_else(|| history_tooltip(&description, interval)),
                                event.source_key,
                                event.session
                            );
                            row.horizontal(|ui| {
                                ui.strong(tr(local_time(event.observed_at_unix)));
                                if let Some(interval) = interval {
                                    if ui.small_button("模型分項").clicked() {
                                        self.selected_interval =
                                            Some(interval.history_event_key.clone());
                                    }
                                }
                                ui.add(egui::Label::new(tr(&description)).truncate())
                                    .on_hover_text(tr(&tooltip));
                            });
                            row.add(
                                egui::Label::new(
                                    RichText::new(tr(format!(
                                        "{} · {} · 週期 {}{}",
                                        event.window_label,
                                        event.source_key,
                                        event.epoch,
                                        if event.reconciliation.is_some() {
                                            " · 不作累計分界"
                                        } else {
                                            ""
                                        }
                                    )))
                                    .small(),
                                )
                                .truncate(),
                            );
                            row.separator();
                        }
                    },
                );
        }
        if let Some(interval) = self
            .selected_interval
            .as_ref()
            .and_then(|key| self.interval_lookup.get(key))
            .and_then(|i| self.intervals.get(*i))
        {
            let mut open = true;
            egui::Window::new(tr("扣額區間 · 模型分項"))
                .id(egui::Id::new("completed-interval-detail"))
                .open(&mut open)
                .default_width(710.)
                .show(ui.ctx(), |ui| {
                    ui.label(tr(format!(
                        "{} → {} · 實際下降 {} 百分點",
                        interval
                            .start
                            .and_then(display_time::datetime)
                            .unwrap_or_else(|| "開始時間未知".into()),
                        display_time::datetime(interval.end).unwrap_or_else(|| "未知".into()),
                        interval.percentage_points
                    )));
                    ui.strong(tr(format!(
                        "區間 Token 小計 {}",
                        self.interval_presentations
                            [self.interval_lookup[&interval.history_event_key]]
                            .observed_total
                    )));
                    if let Some(&index) = self.interval_lookup.get(&interval.history_event_key) {
                        let view = &self.interval_presentations[index];
                        ui.label(tr(format!(
                            "統一扣額估算 {} · {} · {}",
                            view.total, view.per_point, view.status
                        )))
                        .on_hover_text(tr(&view.tooltip));
                    }
                    if !interval.comparable {
                        ui.colored_label(ui.visuals().warn_fg_color, tr(PARTIAL_INTERVAL_WARNING));
                    }

                    egui::ScrollArea::both().max_height(320.).show(ui, |ui| {
                        egui::Grid::new("interval-model-breakdown")
                            .striped(true)
                            .show(ui, |ui| {
                                for label in
                                    ["模型家族", "筆數", "Input", "Cached", "Output", "USD"]
                                {
                                    ui.strong(tr(label));
                                }
                                ui.end_row();
                                let families =
                                    [("sol", "Sol"), ("luna", "Luna"), ("astra", "Astra")];
                                let mut grouped = Vec::new();
                                for (family, label) in families {
                                    if let Some(model) =
                                        model_family_usage(&interval.models, family)
                                    {
                                        grouped.push((label, model));
                                    }
                                }
                                grouped.push(("Total", total_model_usage(&interval.models)));
                                for (label, model) in grouped {
                                    ui.label(tr(label));
                                    ui.label(tr(model.events.to_string()));
                                    ui.label(tr(model.input.to_string()));
                                    ui.label(tr(model.cached.to_string()));
                                    ui.label(tr(model.output.to_string()));
                                    ui.label(tr(interval_model_cost(&model)));
                                    ui.end_row();
                                }
                            });
                    });
                    ui.small(tr(format!(
                        "事件：{} · 價格未知：{}",
                        interval.events, interval.unknown_cost_events
                    )));
                });
            if !open {
                self.selected_interval = None;
            }
        }
        if !self.notice.is_empty() {
            ui.label(tr(&self.notice));
        }
    }

    fn empty_history_diagnostic(&self) -> Option<String> {
        if !self.history_cache.rows.is_empty() || self.history.is_empty() {
            return None;
        }
        let day_count = self
            .history
            .iter()
            .filter(|e| local_day(e.observed_at_unix) == Some(self.date))
            .count();
        Some(format!(
            "目前篩選：0 · 已載入：{} · 當日：{}",
            self.history.len(),
            day_count
        ))
    }
    fn completed_intervals_table(&mut self, ui: &mut egui::Ui) {
        if self.history_cache.table_rows.is_empty() {
            if let Some(error) = &self.history_cache.calculation_error {
                ui.colored_label(ui.visuals().error_fg_color, tr(error));
            } else if self.history_cache.work_key.is_some() {
                ui.label(tr("正在計算今日分段"));
            } else {
                ui.label(tr("已完成區間：0"));
            }
            return;
        }
        if let Some(error) = &self.history_cache.calculation_error {
            ui.colored_label(ui.visuals().error_fg_color, tr(error));
        }
        let scale = ui.style().text_styles[&egui::TextStyle::Body].size / 12.;
        // Keep the model totals visible in the default window. The previous
        // fixed-width columns expanded with font scaling and pushed Total off-screen.
        let table_width = (ui.available_width() / scale).max(1.);
        let column_widths = [
            table_width * 0.09,
            table_width * 0.25,
            table_width * 0.165,
            table_width * 0.165,
            table_width * 0.165,
            table_width * 0.165,
        ];
        let mut column_x = [0.; 6];
        for i in 1..column_x.len() {
            column_x[i] = column_x[i - 1] + column_widths[i - 1];
        }
        let x = column_x;
        let widths = column_widths;
        ui.set_min_width(table_width * scale);
        let (header, _) =
            ui.allocate_exact_size(Vec2::new(table_width * scale, 26. * scale), Sense::hover());
        for (i, title) in [
            "額度變化",
            &format!("變化時間（{}）", display_time::zone_label()),
            "Luna",
            "Sol",
            "Astra",
            "Total",
        ]
        .iter()
        .enumerate()
        {
            ui.put(
                Rect::from_min_size(
                    header.min + Vec2::new(x[i] * scale, 0.),
                    Vec2::new(widths[i] * scale, 26. * scale),
                ),
                egui::Label::new(RichText::new(tr(*title)).strong()).truncate(),
            );
        }
        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt("interval-table-rows")
            .show_rows(
                ui,
                54. * scale,
                self.history_cache.table_rows.len(),
                |ui, range| {
                    for row_index in range {
                        let row = &self.history_cache.table_rows[row_index];
                        let view = &row.presentation;
                        let (rect, response) = ui.allocate_exact_size(
                            Vec2::new(table_width * scale, 54. * scale),
                            Sense::click(),
                        );
                        if response
                            .on_hover_text(tr(if row.interval_index.is_some() {
                                format!("{}\n點擊查看詳情", view.tooltip)
                            } else {
                                view.tooltip.clone()
                            }))
                            .clicked()
                        {
                            if let Some(index) = row.interval_index {
                                self.selected_interval =
                                    Some(self.intervals[index].history_event_key.clone());
                            }
                        }
                        let mut change_time = if view.end == "—" {
                            "—".into()
                        } else {
                            view.end.clone()
                        };
                        change_time =
                            change_time.replace(&format!(" {}", display_time::offset_label()), "");
                        let families = [
                            ("luna", "Luna", 1),
                            ("sol", "Sol", 0),
                            ("astra", "Astra", 2),
                        ];
                        let family_models: Vec<_> = families
                            .iter()
                            .map(|(family, _, _)| model_family_usage(&row.models, family))
                            .collect();
                        let total = total_model_usage(&row.models);
                        let family_cell =
                            |family_index: usize, usage: Option<&intervals::ModelIntervalUsage>| {
                                usage.map_or_else(
                                    || {
                                        format!(
                                            "{} 筆 Response\n$0.0000",
                                            row.task_counts[family_index]
                                        )
                                    },
                                    |model| {
                                        format!(
                                            "{} 筆 Response\n{}",
                                            row.task_counts[family_index],
                                            interval_model_cost(model)
                                        )
                                    },
                                )
                            };
                        let cells = [
                            view.change.clone(),
                            change_time,
                            family_cell(1, family_models[0].as_ref()),
                            family_cell(0, family_models[1].as_ref()),
                            family_cell(2, family_models[2].as_ref()),
                            format!(
                                "{} 筆 Response\n{}",
                                row.task_counts[3],
                                interval_model_cost(&total)
                            ),
                        ];
                        let hints = [
                            view.tooltip.clone(),
                            view.tooltip.clone(),
                            model_usage_tooltip(
                                "Luna（所有版本合併）",
                                family_models[0].as_ref(),
                                row.task_counts[1],
                            ),
                            model_usage_tooltip(
                                "Sol（所有版本合併）",
                                family_models[1].as_ref(),
                                row.task_counts[0],
                            ),
                            model_usage_tooltip(
                                "Astra（所有版本合併）",
                                family_models[2].as_ref(),
                                row.task_counts[2],
                            ),
                            model_usage_tooltip("所有模型總計", Some(&total), row.task_counts[3]),
                        ];
                        for i in 0..cells.len() {
                            ui.put(
                                Rect::from_min_size(
                                    rect.min + Vec2::new(x[i] * scale, 0.),
                                    Vec2::new(widths[i] * scale, 50. * scale),
                                ),
                                egui::Label::new(
                                    RichText::new(tr(&cells[i]))
                                        .size(ui.style().text_styles[&egui::TextStyle::Body].size),
                                ),
                            )
                            .on_hover_text(tr(&hints[i]));
                        }
                        ui.painter().hline(
                            rect.x_range(),
                            rect.bottom() - 1.,
                            Stroke::new(1_f32, ui.visuals().widgets.noninteractive.bg_stroke.color),
                        );
                    }
                },
            );
    }

    fn refresh_token_cache(&mut self) {
        self.request_database_view(
            self.main_cache
                .boundary
                .as_ref()
                .map(|event| event.observed_at_unix),
            now(),
        );
        let key = (
            self.token_generation,
            self.token_date,
            self.token_group,
            self.show_excluded_records,
            self.token_page,
        );
        if self.token_cache.key == Some(key) {
            return;
        }
        if let Some(view) = &self.tokens.database {
            if daily_view_matches(
                &view.request,
                self.token_date,
                self.token_group,
                self.show_excluded_records,
                self.token_page,
            ) {
                let summary = &view.daily.summary;
                self.token_cache = TokenCache {
                    key: Some(key),
                    rows: view.daily.rows.clone(),
                    group_count: view.daily.group_count,
                    count: summary.event_count,
                    input: summary.input,
                    cached: summary.cached,
                    output: summary.output,
                    reasoning: summary.reasoning,
                    missing: summary.unpriced_events,
                    cost: summary.priced_cost_usd,
                    loading: false,
                };
            } else {
                // A newly selected day/group/page never displays the old page
                // beneath its new label while the background query is pending.
                self.token_cache = TokenCache {
                    key: Some(key),
                    loading: true,
                    ..Default::default()
                };
            }
            return;
        }
        let events: Vec<_> = if self.show_excluded_records {
            &self.tokens.excluded_events[..]
        } else {
            self.tokens.events()
        }
        .iter()
        .filter(|event| local_day(event.timestamp) == Some(self.token_date))
        .collect();
        let mut cache = TokenCache {
            key: Some(key),
            count: events.len(),
            rows: usage_views::group_with_excluded(
                &events,
                self.token_group,
                self.show_excluded_records,
            ),
            ..Default::default()
        };
        cache.group_count = cache.rows.len();
        for event in events {
            cache.input = cache.input.saturating_add(event.input);
            cache.cached = cache.cached.saturating_add(event.cached);
            cache.output = cache.output.saturating_add(event.output);
            cache.reasoning = cache.reasoning.saturating_add(event.reasoning);
            cache.missing += usize::from(event.cost_usd.is_none());
            cache.cost += event.cost_usd.unwrap_or(0.);
        }
        self.token_cache = cache;
    }
    fn token_ui(&mut self, ui: &mut egui::Ui) {
        let old_selection = (
            self.token_date,
            self.token_group,
            self.show_excluded_records,
        );
        ui.heading(tr(format!("Token 明細 · {}", display_time::zone_label())));
        if self.demo {
            ui.colored_label(
                if self.settings.dark {
                    Color32::YELLOW
                } else {
                    Color32::from_rgb(130, 75, 0)
                },
                tr("DEMO"),
            );
        }
        ui.small(tr(format!("價格基準：{}", self.tokens.pricing.tier_label)));
        let dates = self.days();
        Self::date_controls(ui, &mut self.token_date, &dates, "token-date");
        ui.horizontal_wrapped(|ui| {
            if ui.button(tr("重新掃描")).clicked() {
                self.start_import();
            }
            if ui.button(tr("匯出當日 CSV")).clicked() {
                self.export_day(true, false);
            }
            if ui.button(tr("匯出當日 JSON")).clicked() {
                self.export_day(true, true);
            }
            if ui.button(tr("編輯價格")).clicked() {
                self.open_settings();
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(
                &mut self.token_group,
                GroupBy::Response,
                tr("Response／舊版事件"),
            );
            ui.selectable_value(&mut self.token_group, GroupBy::Turn, tr("Turn（當日）"));
            ui.selectable_value(
                &mut self.token_group,
                GroupBy::Task,
                tr("Task／Thread（當日）"),
            );
        });
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(
                &mut self.show_excluded_records,
                tr("查看排除紀錄（codex-auto-review）"),
            );
            ui.small(tr(format!("排除：{} 筆", self.excluded_count())));
        });
        if old_selection
            != (
                self.token_date,
                self.token_group,
                self.show_excluded_records,
            )
        {
            self.token_page = 0;
        }
        self.refresh_token_cache();
        if self.tokens.database.is_some() {
            let pages = self.token_cache.group_count.div_ceil(500).max(1);
            if !self.token_cache.loading && self.token_page >= pages {
                self.token_page = pages - 1;
                self.refresh_token_cache();
            }
            let mut page_changed = false;
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(self.token_page > 0, egui::Button::new(tr("上一頁")))
                    .clicked()
                {
                    self.token_page -= 1;
                    page_changed = true;
                }
                ui.label(tr(if self.token_cache.loading {
                    format!("第 {} 頁 · 正在載入…", self.token_page + 1)
                } else {
                    format!(
                        "第 {} / {} 頁 · {} 組 · 每頁最多 500 列",
                        self.token_page + 1,
                        pages,
                        self.token_cache.group_count
                    )
                }));
                if ui
                    .add_enabled(
                        !self.token_cache.loading && self.token_page + 1 < pages,
                        egui::Button::new(tr("下一頁")),
                    )
                    .clicked()
                {
                    self.token_page += 1;
                    page_changed = true;
                }
            });
            if page_changed {
                self.refresh_token_cache();
            }
        }
        self.import_diagnostics_ui(ui);
        let cache = &self.token_cache;
        if cache.loading {
            if self.view_pending {
                ui.horizontal_wrapped(|ui| {
                    ui.spinner();
                    ui.label(tr(format!("正在載入 {} 的明細…", self.token_date)));
                });
            } else {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    tr("所選明細尚未載入；請查看上方診斷。"),
                );
                if ui
                    .add_enabled(!self.worker_failed, egui::Button::new(tr("重試載入")))
                    .clicked()
                {
                    self.requested_view = None;
                    self.refresh_token_cache();
                }
            }
            return;
        }
        if self.show_excluded_records {
            ui.colored_label(ui.visuals().warn_fg_color, tr("狀態：已排除"));
        }
        ui.label(tr(format!(
            "{} 筆 · 已記錄輸入 {}（快取 {}／未快取 {}）· 輸出 {}（推理 {}）· 已可估部分 {}{}",
            cache.count,
            cache.input,
            cache.cached,
            cache.input.saturating_sub(cache.cached),
            cache.output,
            cache.reasoning,
            money((cache.count > cache.missing).then_some(cache.cost)),
            if cache.missing > 0 {
                format!("；{} 筆費用未知", cache.missing)
            } else {
                String::new()
            }
        )));
        ui.separator();
        let font_size = ui.style().text_styles[&egui::TextStyle::Body].size;
        let scale = font_size / 12.;
        let row_height = 24. * scale;
        let widths = [
            72., 150., 80., 115., 120., 45., 90., 90., 90., 80., 80., 110.,
        ]
        .map(|width| width * scale);
        let headers = [
            "時間",
            "模型",
            "推理強度",
            "Task / Session",
            "Turn / Response",
            "筆數",
            "輸入",
            "快取",
            "未快取",
            "輸出",
            "推理",
            "估算 USD",
        ];
        let total_width: f32 = widths.iter().sum();
        egui::ScrollArea::horizontal().show(ui,|ui| {
            ui.set_min_width(total_width);
            let (header,_) = ui.allocate_exact_size(Vec2::new(total_width,row_height),Sense::hover());
            let mut x=header.left();
            for (label,width) in headers.iter().zip(widths){ui.painter().text(Pos2::new(x+3.,header.top()+3.),Align2::LEFT_TOP,tr(label),FontId::proportional(font_size),ui.visuals().strong_text_color());x+=width;}
            egui::ScrollArea::vertical().id_salt("token-rows").show_rows(ui,row_height,cache.rows.len(),|ui,range| {
                for i in range {
                    let e=&cache.rows[cache.rows.len()-1-i];
                    let number=|n:u64,flag:&str|if e.partial_flags.iter().any(|f|f==flag){"未知".to_owned()}else{n.to_string()};
                    let uncached=if e.partial_flags.iter().any(|f|f=="input_count_unavailable"||f=="cached_count_unavailable"){"未知".into()}else{e.uncached.to_string()};
                    let identity=match self.token_group {GroupBy::Response=>e.response_id.as_deref().unwrap_or("舊版／未知"),GroupBy::Turn=>e.turn_id.as_deref().unwrap_or("未知 Turn"),GroupBy::Task=>"當日事件合計"};
                    let values=[local_time(e.last_timestamp),format!("{}{}",e.models.join(" / "),if e.model_assumed {"（假設）"}else{""}),effort_label(e),e.session.chars().take(12).collect::<String>(),identity.chars().take(12).collect(),e.event_count.to_string(),number(e.input,"input_count_unavailable"),number(e.cached,"cached_count_unavailable"),uncached,number(e.output,"output_count_unavailable"),number(e.reasoning,"reasoning_count_unavailable"),format!("{}{}",money(e.display_cost_usd()),if e.display_cost_is_partial(){" *"}else{""})];
                    let tooltip=format!("Reasoning effort: {}\n{}\nTask: {}\nTurn: {}\nResponse: {}\n{} 至 {}\n模型: {}\n已可估部分 {}；{} 筆費用未知\n{}",effort_label(e),self.tokens.task_name(&e.session).unwrap_or("名稱未記錄"),e.session,e.turn_id.as_deref().unwrap_or("未知"),e.response_id.as_deref().unwrap_or("未知"),display_time::datetime(e.first_timestamp).unwrap_or_else(||"未知".into()),display_time::datetime(e.last_timestamp).unwrap_or_else(||"未知".into()),e.models.join(" / "),money(e.display_cost_usd()),e.unknown_cost_events,if e.partial_flags.is_empty() {"狀態：完整"} else {"狀態：部分"});
                    let (rect,response)=ui.allocate_exact_size(Vec2::new(total_width,row_height),Sense::hover());response.on_hover_text(tr(tooltip));
                    if i%2==1 {ui.painter().rect_filled(rect,0.,ui.visuals().faint_bg_color);}
                    let mut x=rect.left();
                    for (value,width) in values.iter().zip(widths) {let cell=Rect::from_min_size(Pos2::new(x,rect.top()),Vec2::new(width,row_height));ui.painter().with_clip_rect(cell.intersect(ui.clip_rect())).text(Pos2::new(x+3.,rect.top()+3.),Align2::LEFT_TOP,tr(value),FontId::proportional(font_size),ui.visuals().text_color());x+=width;}
                }
            });
        });
        if cache.count == 0 {
            ui.label(tr("Token 紀錄：0"));
        }
        if !self.notice.is_empty() {
            ui.label(tr(&self.notice));
        }
    }
    fn price_model_choices(&mut self) -> BTreeSet<String> {
        // The model picker must not rescan all history on every settings repaint.
        if self.price_models_generation != Some(self.token_generation) {
            self.recorded_price_models = if let Some(view) = &self.tokens.database {
                view.models.clone()
            } else {
                self.tokens
                    .events
                    .iter()
                    .chain(&self.tokens.excluded_events)
                    .map(|event| event.model.clone())
                    .collect()
            };
            self.price_models_generation = Some(self.token_generation);
        }
        let mut choices = self.recorded_price_models.clone();
        choices.extend(self.settings.pricing.rates.keys().cloned());
        choices.extend(["gpt-6.1-sol", "gpt-6-luna", "gpt-6-astra"].map(String::from));
        choices
    }
    fn load_price_editor(&mut self) {
        if let Some(price) = self.settings.pricing.rates.get(self.price_model.trim()) {
            self.price_values = [
                price.uncached_input_per_million,
                price.cached_input_per_million,
                price.output_per_million,
            ];
            self.price_configured = true;
        } else {
            self.price_values = [0.; 3];
            self.price_configured = false;
        }
    }
    fn apply_price_editor(&mut self) {
        let model = self.price_model.trim();
        if model.is_empty() {
            return;
        }
        self.settings.pricing.tier_label = "自訂價格".into();
        self.settings.pricing.verified_at = None;
        if self.price_configured {
            self.settings.pricing.rates.insert(
                model.into(),
                ModelPrice {
                    uncached_input_per_million: self.price_values[0],
                    cached_input_per_million: self.price_values[1],
                    output_per_million: self.price_values[2],
                },
            );
        } else {
            self.settings.pricing.rates.remove(model);
        }
        self.notice = format!("價格已套用：{}", model);
        self.request_reprice();
        self.dirty = true;
    }
    fn remember_settings_position(&mut self, ctx: &egui::Context) {
        if ctx.viewport_id() != ViewportId::from_hash_of("settings") {
            return;
        }
        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
            let position = [rect.min.x, rect.min.y];
            if position.iter().all(|v| v.is_finite())
                && self.settings.settings_position != Some(position)
            {
                self.settings.settings_position = Some(position);
                self.dirty = true;
            }
        }
    }
    fn remember_prediction_position(&mut self, ctx: &egui::Context) {
        if ctx.viewport_id() != ViewportId::from_hash_of("reset-prediction-dialog") {
            return;
        }
        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
            let position = [rect.min.x, rect.min.y];
            if position.iter().all(|v| v.is_finite())
                && self.settings.prediction_position != Some(position)
            {
                self.settings.prediction_position = Some(position);
                self.dirty = true;
            }
        }
    }
    fn toggle_settings(&mut self) {
        if self.settings_open {
            self.settings_open = false;
            self.settings_presented = false;
        } else {
            self.open_settings();
        }
    }
    fn open_settings(&mut self) {
        self.settings_open = true;
        self.settings_presented = false;
    }
    fn open_prediction(&mut self) {
        if self.prediction_open {
            self.prediction_open = false;
            self.prediction_presented = false;
            return;
        }
        self.prediction_pst = false;
        self.prediction_input = self
            .settings
            .prediction_target()
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|t| {
                t.with_timezone(&display_time::timezone())
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        self.prediction_open = true;
        self.prediction_presented = false;
    }
    fn clear_completed_prediction(&mut self) {
        let Some(target) = self.settings.prediction_target() else {
            return;
        };
        let Some(view) = self.selected() else {
            return;
        };
        let since = self.settings.reset_prediction_set_at.unwrap_or(target);
        let key = (self.history_generation, target, since);
        if self.prediction_history_checked == Some(key) {
            return;
        }
        self.prediction_history_checked = Some(key);
        if self.history.iter().any(|event| {
            event.observed_at_unix >= since
                && live_reset_alert(std::slice::from_ref(event), &view)
                && event
                    .evidence
                    .as_ref()
                    .and_then(|e| e.account_id.as_deref())
                    == self.quota.account_id()
        }) {
            self.settings.clear_prediction();
            self.dirty = true;
        }
    }
    fn prediction_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading(tr("Tibo 已公告了Reset時間嗎?"));
        ui.label(tr("請輸入時間（YYYY-MM-DD HH:MM）"));
        ui.text_edit_singleline(&mut self.prediction_input);
        ui.horizontal(|ui| {
            ui.radio_value(&mut self.prediction_pst, true, "PST (UTC-08:00)");
            ui.radio_value(&mut self.prediction_pst, false, display_time::zone_label());
        });
        let offset = if self.prediction_pst {
            -480
        } else {
            self.settings.utc_offset_minutes
        };
        let target = display_time::parse_local(&self.prediction_input, offset);
        if target.is_none() && !self.prediction_input.is_empty() {
            ui.label(tr("日期或時間格式不正確"));
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(target.is_some(), egui::Button::new(tr("儲存設定")))
                .clicked()
            {
                self.settings.clear_prediction();
                self.settings.reset_prediction_utc = target;
                self.settings.reset_prediction_set_at = Some(now());
                self.dirty = true;
                self.prediction_open = false;
            }
            if ui.button(tr("清除提醒")).clicked() {
                self.settings.clear_prediction();
                self.dirty = true;
                self.prediction_open = false;
            }
            if ui.button(tr("取消")).clicked() {
                self.prediction_open = false;
            }
        });
    }
    fn settings_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.label(tr("語言 / Language"));
            let previous = self.settings.language;
            egui::ComboBox::from_id_salt("interface-language")
                .selected_text(tr(self.settings.language.label()))
                .show_ui(ui, |ui| {
                    for language in Language::ALL {
                        ui.selectable_value(
                            &mut self.settings.language,
                            language,
                            tr(language.label()),
                        );
                    }
                });
            if previous != self.settings.language {
                set_language(self.settings.language);
                self.dirty = true;
                ctx.request_repaint();
            }
        });
        ui.heading(tr("設定 / 連接"));
        ui.label(tr(format!("狀態：{}", tr(&self.status))));
        if self.demo {
            ui.colored_label(
                if self.settings.dark {
                    Color32::YELLOW
                } else {
                    Color32::from_rgb(130, 75, 0)
                },
                tr("DEMO"),
            );
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .checkbox(&mut self.settings.dark, tr("深色外觀"))
                .changed()
            {
                self.apply_theme(ctx);
                self.dirty = true;
            }
            if ui
                .checkbox(&mut self.settings.on_top, tr("保持最上層"))
                .changed()
            {
                self.topmost(ctx);
                self.dirty = true;
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(tr("介面字體大小"));
            if ui
                .add(
                    egui::Slider::new(
                        &mut self.settings.font_scale_percent,
                        accessibility::MIN_FONT_SCALE_PERCENT
                            ..=accessibility::MAX_FONT_SCALE_PERCENT,
                    )
                    .step_by(5.0)
                    .suffix("%"),
                )
                .changed()
            {
                self.settings.font_scale_percent = self.settings.font_scale_percent.clamp(
                    accessibility::MIN_FONT_SCALE_PERCENT,
                    accessibility::MAX_FONT_SCALE_PERCENT,
                );
                self.apply_theme(ctx);
                self.dirty = true;
            }
            ui.label(tr("Aa 中文預覽"));
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(tr("顯示時區"));
            let before = self.settings.utc_offset_minutes;
            let mut hours = before as f64 / 60.;
            if ui
                .add(
                    egui::DragValue::new(&mut hours)
                        .range(-12.0..=14.0)
                        .speed(0.25)
                        .prefix("UTC ")
                        .max_decimals(2),
                )
                .changed()
            {
                self.settings.utc_offset_minutes =
                    ((hours * 4.).round() as i32 * 15).clamp(-720, 840);
                self.dates_key = None;
                self.history_cache = HistoryCache::default();
                self.token_cache = TokenCache::default();
                self.main_cache.key = None;
                self.requested_view = None;
                let _zone = display_time::scope(self.settings.utc_offset_minutes);
                self.set_intervals(Arc::clone(&self.intervals));
                self.dirty = true;
                ctx.request_repaint();
            }
        });
        if ui.button(tr("Reset預測")).clicked() {
            self.open_prediction();
        }
        if let Some(target) = self.settings.prediction_target() {
            ui.label(display_time::datetime(target).unwrap_or_default());
        }
        if ui
            .checkbox(
                &mut self.settings.reset_sound_enabled,
                tr("真正重設至 100% 時播放提示音"),
            )
            .changed()
        {
            self.dirty = true;
        }
        if ui.button(tr("試播提示音")).clicked() && !play_reset_chime() {
            self.notice = "提示音播放失敗，請檢查系統音效設定。".into();
        }
        ui.separator();
        ui.strong(tr("官方 Codex CLI（唯讀）"));
        ui.label(tr(self.quota_diagnostic_text()));
        if ui
            .text_edit_singleline(&mut self.settings.codex_path)
            .changed()
        {
            self.dirty = true;
        }
        ui.add_enabled_ui(!self.demo, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button(tr("重新連線")).clicked() {
                    self.offline = false;
                    self.connect();
                }
                if ui.button(tr("停止連線")).clicked() {
                    self.bridge = None;
                    self.offline = true;
                    self.status = "OFFLINE".into();
                    self.quota_error = None;
                    self.quota.reset_continuity();
                }
                if ui.button(tr("更新額度")).clicked() {
                    self.refresh_quota();
                }
            });
        });
        let views = self.quota.views(now());
        egui::ComboBox::from_label(tr("額度來源 / 視窗"))
            .selected_text(tr(self
                .selected()
                .map(|v| format!("{} · {}", v.source_label, v.window_label))
                .unwrap_or_else(|| "等待來源".into())))
            .show_ui(ui, |ui| {
                for v in views {
                    if ui
                        .selectable_label(
                            self.settings.bucket == v.source_key
                                && self.settings.window == v.window_key,
                            tr(format!("{} · {}", v.source_label, v.window_label)),
                        )
                        .clicked()
                    {
                        self.settings.bucket = v.source_key;
                        self.settings.window = v.window_key;
                        self.dirty = true;
                    }
                }
            });
        ui.separator();
        ui.strong(tr("本機 Token 歷史"));
        if ui
            .checkbox(&mut self.settings.import_enabled, tr("啟用唯讀增量匯入"))
            .changed()
        {
            self.dirty = true;
        }
        if ui
            .text_edit_singleline(&mut self.settings.sessions_path)
            .changed()
        {
            self.dirty = true;
        }
        ui.horizontal_wrapped(|ui| {
            if ui.button(tr("掃描所有可用歷史")).clicked() {
                self.start_import();
            }
            ui.label(tr("預設日期範圍"));
            if ui
                .add(
                    egui::DragValue::new(&mut self.settings.history_days)
                        .range(1..=3650)
                        .suffix(" 天"),
                )
                .changed()
            {
                self.dirty = true;
            }
        });
        self.import_diagnostics_ui(ui);
        ui.separator();
        ui.strong(tr("估算單價 · USD / 1M tokens"));
        ui.horizontal_wrapped(|ui| {
            for (label, preset) in [
                ("Standard（重設全部）", Pricing::official_standard()),
                ("Fast（重設全部）", Pricing::official_fast()),
            ] {
                if ui.button(tr(label)).clicked() {
                    let current_override = self.settings.pricing.model_override.clone();
                    self.settings.pricing = preset;
                    self.settings.pricing.model_override = current_override;
                    self.request_reprice();
                    self.load_price_editor();
                    self.dirty = true;
                }
            }
        });
        ui.label(tr(format!(
            "目前：{} · 核對 {}",
            self.settings.pricing.tier_label,
            self.settings
                .pricing
                .verified_at
                .as_deref()
                .unwrap_or("未提供")
        )));
        ui.hyperlink_to(
            tr("官方價格與服務等級"),
            "https://developers.openai.com/api/docs/pricing",
        );
        if ui
            .checkbox(
                &mut self.settings.pricing.apply_long_context,
                tr("長上下文 >272K：輸入／快取 ×2 · 輸出 ×1.5"),
            )
            .changed()
        {
            self.request_reprice();
            self.dirty = true;
        }
        let models = self.price_model_choices();
        let previous_model = self.price_model.clone();
        egui::ComboBox::from_label(tr("模型"))
            .selected_text(tr(&self.price_model))
            .width(240.)
            .show_ui(ui, |ui| {
                for model in models {
                    let label = if self.settings.pricing.rates.contains_key(&model) {
                        model.clone()
                    } else {
                        format!("{model}（未設定價格）")
                    };
                    ui.selectable_value(&mut self.price_model, model, tr(label));
                }
            });
        if self.price_model != previous_model {
            self.load_price_editor();
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(tr("Model ID"));
            if ui.text_edit_singleline(&mut self.price_model).changed() {
                self.load_price_editor();
            }
        });
        egui::CollapsingHeader::new("模型單價")
            .default_open(true)
            .show(ui, |ui| {
                let mut selected = None;
                egui::Grid::new("price-overview")
                    .striped(true)
                    .show(ui, |ui| {
                        for label in ["模型", "未快取輸入", "快取輸入", "輸出"] {
                            ui.strong(tr(label));
                        }
                        ui.end_row();
                        for (model, price) in &self.settings.pricing.rates {
                            if ui
                                .selectable_label(*model == self.price_model, tr(model))
                                .clicked()
                            {
                                selected = Some(model.clone());
                            }
                            ui.label(tr(price.uncached_input_per_million.to_string()));
                            ui.label(tr(price.cached_input_per_million.to_string()));
                            ui.label(tr(price.output_per_million.to_string()));
                            ui.end_row();
                        }
                    });
                if let Some(model) = selected {
                    self.price_model = model;
                    self.load_price_editor();
                }
            });
        ui.checkbox(&mut self.price_configured, tr("已確認此模型價格"));
        egui::Grid::new("prices").show(ui, |ui| {
            for (i, label) in ["未快取輸入", "快取輸入", "輸出（已含推理）"]
                .iter()
                .enumerate()
            {
                ui.label(tr(*label));
                ui.add(
                    egui::DragValue::new(&mut self.price_values[i])
                        .range(0.0..=100000.0)
                        .speed(0.05)
                        .max_decimals(6),
                );
                ui.end_row();
            }
        });
        if ui
            .add_enabled(
                !self.price_model.trim().is_empty(),
                egui::Button::new(tr("套用模型價格")),
            )
            .clicked()
        {
            self.apply_price_editor();
        }
        let mut override_model = self
            .settings
            .pricing
            .model_override
            .clone()
            .unwrap_or_default();
        ui.label(tr("模型覆寫"));
        if ui.text_edit_singleline(&mut override_model).changed() {
            self.settings.pricing.model_override =
                (!override_model.trim().is_empty()).then(|| override_model.trim().to_string());
            self.request_reprice();
            self.dirty = true;
        }
        ui.separator();
        if !self.notice.is_empty() {
            ui.label(tr(&self.notice));
        }
        if ui.button(tr("儲存設定")).clicked() {
            self.dirty = true;
            self.save_state();
        }
    }
    fn export_day(&mut self, tokens: bool, json: bool) {
        if self.demo || !self.read_state {
            self.notice = "示範／暫存模式不寫入檔案。請在一般模式匯出。".into();
            return;
        }
        if self.export_rx.is_some() {
            self.notice = "正在匯出，請稍候。".into();
            return;
        }
        // GUI work is limited to small configuration copies and Arc refcounts.
        // Filtering, attribution, serialization, and filesystem work stay here.
        let audit_excluded = tokens && self.show_excluded_records;
        let snapshot = Arc::clone(&self.tokens);
        let history = Arc::clone(&self.history);
        let canonical_intervals = Arc::clone(&self.intervals);
        let day = if tokens { self.token_date } else { self.date };
        let folder = self.dir.join("exports");
        let pricing = snapshot.pricing.clone();
        let hide_unchanged = self.settings.hide_unchanged;
        let utc_offset_minutes = self.settings.utc_offset_minutes;
        let bucket = self.settings.bucket.clone();
        let selection = self.history_selection.clone().or_else(|| {
            self.selected()
                .map(|view| (view.source_key, Some(view.window_key), view.window_label))
        });
        let (tx, rx) = mpsc::channel();
        self.export_rx = Some(rx);
        self.notice = "正在背景匯出…".into();
        let export_cancel = CancellationToken::new();
        self.export_cancel = Some(export_cancel.clone());
        let spawned = std::thread::Builder::new()
            .name("quota-export".into())
            .spawn(move || export_cancel.scope(|| {
                let _zone = display_time::scope(utc_offset_minutes);
                if cancellation::check().is_err() { return; }
                let kind = if audit_excluded {"tokens-excluded"}else if tokens { "tokens" } else { "quota" };
                // Export the entire selected day from a separate read-only
                // connection. The GUI snapshot contains only a bounded page
                // and latest-task tooltip events, never full export history.
                let mut database_events = None;
                let mut excluded_count = snapshot.excluded_events.iter().filter(|event| local_day(event.timestamp) == Some(day)).count();
                if let Some(view) = &snapshot.database {
                    let result = (|| -> Result<Vec<TokenEvent>, String> {
                        let (start, end) = display_time::day_bounds(day).ok_or("所選日期無效")?;
                        let db = quota_widget::token_db::TokenDb::open_readonly(&view.database_path).map_err(|error| error.to_string())?;
                        db.query_events_all(Some(start), Some(end), true).map_err(|error| error.to_string())
                    })();
                    match result {
                        Ok(mut events) => {
                            excluded_count = events.iter().filter(|event| event.is_excluded_auto_review()).count();
                            events.retain(|event| event.is_excluded_auto_review() == audit_excluded);
                            database_events = Some(events);
                        }
                        Err(error) => {
                            let _ = tx.send(format!("匯出失敗；未產生不完整檔案：{error}"));
                            return;
                        }
                    }
                }
                let usage_events = database_events.as_deref().unwrap_or_else(|| {
                    if audit_excluded { &snapshot.excluded_events[..] } else { snapshot.events() }
                });
                let filename = format!(
                    "{kind}-{day}-{}.{}",
                    Utc::now().with_timezone(&display_time::timezone()).format("%H%M%S%3f"),
                    if json { "json" } else { "csv" }
                );
                let path = folder.join(filename);
                let mut completed_export: Vec<CompletedInterval> = Vec::new();
                let mut estimate_by_key: BTreeMap<String, Option<QuotaEstimate>> = canonical_intervals.iter()
                    .zip(quota_estimate::interval_estimates(&canonical_intervals))
                    .map(|(row, estimate)| (row.history_event_key.clone(), estimate)).collect();
                let mut events: Vec<serde_json::Value> = if tokens {
                    usage_events
                        .iter()
                        .filter(|e| local_day(e.timestamp) == Some(day))
                        .filter_map(|e| serde_json::to_value(e).ok())
                        .collect()
                } else {
                    let selection = selection.or_else(|| {
                        history
                            .iter()
                            .filter(|e| drop_event(e) || reset_event(e))
                            .find(|e| {
                                e.source_key == bucket
                                    && (e.window_label.contains("週")
                                        || e.window_label.contains("week"))
                            })
                            .or_else(|| history.iter().find(|e| drop_event(e) || reset_event(e)))
                            .map(|e| {
                                (
                                    e.source_key.clone(),
                                    e.window_key.clone(),
                                    e.window_label.clone(),
                                )
                            })
                    });
                    let mut all: Vec<HistoryEvent> = history
                        .iter()
                        .filter(|event| {
                            selection.as_ref().is_some_and(|(source, key, label)| {
                                event.source_key == *source
                                    && match (&event.window_key, key) {
                                        (Some(actual), Some(selected)) => actual == selected,
                                        _ => event.window_label == *label,
                                    }
                            })
                        })
                        .cloned()
                        .collect();
                    all.sort_by_key(|event| event.observed_at_unix);
                    let costs = if snapshot.database.is_some() {
                        let selected_keys: BTreeSet<_> = all.iter().map(intervals::history_event_key).collect();
                        completed_export = canonical_intervals.iter().filter(|interval| {
                            local_day(interval.end) == Some(day) && selected_keys.contains(&interval.history_event_key)
                        }).cloned().collect();
                        let canonical: BTreeMap<_, _> = canonical_intervals.iter().map(|interval| (&interval.history_event_key, interval)).collect();
                        all.iter().map(|event| canonical_interval_cost(canonical.get(&intervals::history_event_key(event)).copied())).collect()
                    } else {
                        let calculated = intervals::completed_intervals(snapshot.events(), &all);
                        estimate_by_key = calculated.iter().zip(quota_estimate::interval_estimates(&calculated))
                            .map(|(row, estimate)| (row.history_event_key.clone(), estimate)).collect();
                        completed_export = calculated.into_iter().filter(|interval|local_day(interval.end)==Some(day)).collect();
                        accounting::history_costs(&snapshot.cost_index, &all)
                    };
                    let interval_map: BTreeMap<_,_> = completed_export.iter().map(|interval|(interval.history_event_key.as_str(),interval)).collect();
                    all.iter()
                        .zip(costs)
                        .filter(|(e, _)| {
                            local_day(e.observed_at_unix) == Some(day)
                                && (!hide_unchanged || !unchanged(e))
                        })
                        .map(|(e, (cost, partial))| {
                            let mut v = serde_json::to_value(e).unwrap_or_default();
                            v["estimated_cost_usd"] =
                                serde_json::json!(if drop_event(e) { cost } else { None });
                            v["partial"] = serde_json::json!(partial);
                            if let Some(interval) = interval_map.get(intervals::history_event_key(e).as_str()) {
                                v["completed_interval"] = serde_json::json!(interval);
                                v["partial"] = serde_json::json!(interval.partial);
                                v["interval_start"] = serde_json::json!(interval.start);
                                v["interval_end"] = serde_json::json!(interval.end);
                                v["interval_percentage_points"] = serde_json::json!(interval.percentage_points);
                                v["interval_known_usd"] = serde_json::json!(interval.known_usd);
                                v["interval_cost_per_point"] = serde_json::json!(interval.cost_per_point);
                                v["interval_comparable"] = serde_json::json!(interval.comparable);
                                v["interval_reasons"] = serde_json::json!(interval.reasons);
                                v["interval_models"] = serde_json::json!(interval.models);
                                let estimate = estimate_by_key.get(&interval.history_event_key).and_then(Option::as_ref);
                                quota_estimate::annotate_export(&mut v, interval, estimate);
                            }
                            v
                        })
                        .collect()
                };
                for event in &mut events {
                    if cancellation::check().is_err() { return; }
                    let timestamp = event[if tokens {
                        "timestamp"
                    } else {
                        "observed_at_unix"
                    }]
                    .as_i64();
                    event["local_time"] = serde_json::json!(timestamp.map(local_time));
                    event["local_datetime"] = serde_json::json!(timestamp.and_then(display_time::datetime));
                    event["display_timezone"] = serde_json::json!(display_time::offset_label());
                    event["display_date"] = serde_json::json!(timestamp.and_then(display_time::day).map(|d|d.to_string()));
                    display_time::annotate_timestamps(event);
                    event["excluded_from_default_totals"] = serde_json::json!(audit_excluded);
                    event["pricing_basis"] = serde_json::json!(pricing.tier_label);
                    event["pricing_verified_at"] = serde_json::json!(pricing.verified_at);
                    annotate_long_context_export(event, &pricing);
                }
                let result = (|| -> std::io::Result<()> {
                    cancellation::check()?;
                    std::fs::create_dir_all(&folder)?;
                    if json {
                        let mut exported = serde_json::json!({
                                "schema_version": 1,
                                "date": day.to_string(),
                                "timezone": display_time::offset_label(),
                                "display_timezone": display_time::zone_label(),
                                "timestamp_basis": "UTC Unix seconds; *_hk preserves +08:00; *_local and local_* use display_timezone",
                                "kind": kind,
                                "cost_basis": "estimated_model_equivalent_not_subscription_charge",
                                "view_scope":if audit_excluded {"excluded_auto_review_audit"}else{"included_default"},
                                "excluded_auto_review_records":excluded_count,
                                "exclusion_is_not_free_billing_claim":true,
                                "pricing": pricing,
                                "long_context_adjustment_enabled": pricing.apply_long_context,
                                "events": events,
                                "completed_intervals": if tokens {None}else{Some(&completed_export)},
                                "turn_summaries": if tokens {
                                    Some(usage_views::group_with_excluded(&usage_events.iter()
                                        .filter(|e| local_day(e.timestamp) == Some(day))
                                        .collect::<Vec<_>>(), GroupBy::Turn,audit_excluded))
                                } else { None },
                                "task_summaries": if tokens {
                                    Some(usage_views::group_with_excluded(&usage_events.iter()
                                        .filter(|e| local_day(e.timestamp) == Some(day))
                                        .collect::<Vec<_>>(), GroupBy::Task,audit_excluded))
                                } else { None }
                            });
                        display_time::annotate_timestamps(&mut exported);
                        save_export(&path, |writer| serde_json::to_writer_pretty(writer, &exported).map_err(std::io::Error::other))
                    } else {
                        let columns: Vec<String> = if tokens {
                            vec![
                                "timestamp",
                                "timestamp_utc_unix",
                                "timestamp_hk",
                                "display_timezone",
                                "display_date",
                                "local_time",
                                "local_datetime",
                                "model",
                                "model_assumed",
                                "reasoning_effort",
                                "session",
                                "turn_id",
                                "response_id",
                                "source_kind",
                                "input",
                                "cached",
                                "uncached",
                                "output",
                                "reasoning",
                                "cost_usd",
                                "partial_flags",
                                "excluded_from_default_totals",
                                "pricing_basis",
                                "pricing_verified_at",
                                "long_context_adjustment",
                                "long_context_adjustment_enabled",
                                "long_context_adjustment_applied",
                                "long_context_adjustment_legacy_semantics",
                            ]
                        } else {
                            vec![
                                "observed_at_unix",
                                "observed_at_unix_utc_unix",
                                "observed_at_unix_hk",
                                "display_timezone",
                                "display_date",
                                "local_time",
                                "local_datetime",
                                "source_key",
                                "window_label",
                                "kind",
                                "before",
                                "after",
                                "delta",
                                "estimated_cost_usd",
                                "partial",
                                "interval_start",
                                "interval_start_utc_unix",
                                "interval_start_hk",
                                "interval_end",
                                "interval_end_utc_unix",
                                "interval_end_hk",
                                "interval_percentage_points",
                                "interval_known_usd",
                                "interval_cost_per_point",
                                "interval_comparable",
                                "interval_reasons",
                                "interval_models",
                                "quota_estimated_usd",
                                "quota_usd_per_point",
                                "quota_estimate",
                                "quota_estimate_method",
                                "pricing_basis",
                                "pricing_verified_at",
                                "long_context_adjustment",
                                "long_context_adjustment_enabled",
                                "long_context_adjustment_applied",
                                "long_context_adjustment_legacy_semantics",
                            ]
                        }
                        .into_iter()
                        .map(str::to_owned)
                        .collect();
                        let mut out = columns.join(",");
                        out.push('\n');
                        for e in &events {
                            cancellation::check()?;
                            let row: Vec<_> = columns
                                .iter()
                                .map(|k| {
                                    let v = &e[k];
                                    let mut text =
                                        v.as_str().map(str::to_owned).unwrap_or_else(|| {
                                            if v.is_null() {
                                                String::new()
                                            } else {
                                                v.to_string()
                                            }
                                        });
                                    if v.is_string()
                                        && text.starts_with(['=', '+', '-', '@', '\t', '\r'])
                                    {
                                        text.insert(0, '\'');
                                    }
                                    format!("\"{}\"", text.replace('"', "\"\""))
                                })
                                .collect();
                            out.push_str(&row.join(","));
                            out.push('\n');
                        }
                        save_export(&path, |writer| std::io::Write::write_all(writer, out.as_bytes()))?;
                        if !tokens {
                            let model_path = path.with_file_name(format!("{}-models.csv", path.file_stem().unwrap_or_default().to_string_lossy()));
                            let mut models_csv=String::from("interval_key,start_utc_unix,end_utc_unix,start_local,end_local,display_timezone,percentage_points,interval_known_usd,interval_cost_per_point,comparable,partial,reasons,model,events,input,cached,uncached,output,reasoning,model_known_usd,unknown_cost_events,assumed_model_events\n");
                            for interval in &completed_export { cancellation::check()?; for model in &interval.models {
                                let values=vec![interval.history_event_key.clone(),interval.start.map(|v|v.to_string()).unwrap_or_default(),interval.end.to_string(),interval.start.and_then(display_time::datetime).unwrap_or_default(),display_time::datetime(interval.end).unwrap_or_default(),display_time::offset_label().into(),interval.percentage_points.to_string(),interval.known_usd.to_string(),interval.cost_per_point.map(|v|v.to_string()).unwrap_or_default(),interval.comparable.to_string(),interval.partial.to_string(),interval.reasons.iter().map(|r|r.label()).collect::<Vec<_>>().join("; "),model.model.clone(),model.events.to_string(),model.input.to_string(),model.cached.to_string(),model.uncached.to_string(),model.output.to_string(),model.reasoning.to_string(),model.known_usd.to_string(),model.unknown_cost_events.to_string(),model.assumed_model_events.to_string()];
                                models_csv.push_str(&values.into_iter().map(|mut value|{if value.starts_with(['=','+','-','@','\t','\r']){value.insert(0,'\'');}format!("\"{}\"",value.replace('"',"\"\""))}).collect::<Vec<_>>().join(",")); models_csv.push('\n');
                            }}
                            save_export(&model_path, |writer| std::io::Write::write_all(writer, models_csv.as_bytes()))?;
                        }
                        Ok(())
                    }
                })();
                let message = if result.is_ok() {
                    format!("已匯出：{}", path.display())
                } else {
                    "匯出失敗；請檢查資料夾權限。".into()
                };
                let _ = tx.send(message);
            }));
        match spawned {
            Ok(join) => self.export_join = Some(join),
            Err(error) => {
                self.export_rx = None;
                self.notice = format!("無法啟動背景匯出：{error}");
            }
        }
    }
    /// Request stop without joining or doing filesystem work on the GUI thread.
    /// The writer lock remains held until all workers are actually finished.
    fn begin_close(&mut self) {
        if self.closing_since.is_some() {
            return;
        }
        self.closing_since = Some(Instant::now());
        // Drain already-received quota observations before enqueuing the final
        // history/settings writes. No new polling/import is started from here.
        let pending: Vec<_> = self
            .bridge
            .as_ref()
            .map(|b| b.events.try_iter().collect())
            .unwrap_or_default();
        for event in pending {
            self.handle_bridge_event(event);
        }
        self.save_state();
        if let Some(bridge) = &self.bridge {
            bridge.request_stop();
        }
        if let Some(cancel) = &self.export_cancel {
            cancel.cancel();
        }
        self.worker.request_cancel_shutdown();
        self.history_open = false;
        self.tokens_open = false;
        self.settings_open = false;
    }
    fn close_finished(&self) -> bool {
        self.worker.is_finished()
            && self.bridge.as_ref().is_none_or(Bridge::is_finished)
            && self
                .export_join
                .as_ref()
                .is_none_or(std::thread::JoinHandle::is_finished)
    }
    fn make_demo(&mut self) {
        self.status = "DEMO".into();
        self.settings.sessions_path = "DEMO：不讀取任何真實歷史".into();
        self.settings.pricing = Pricing::demo();
        self.tokens = Arc::new(Snapshot::from_store(&TokenStore::demo(self.demo_start)));
        self.token_generation += 1;
        let base = self.demo_start - 1500;
        for i in 0..=50 {
            let used = [55., 56., 58., 59., 60.][(i / 10).min(4)];
            let t = base + i as i64 * 30;
            let _=self.quota.ingest_json(&serde_json::json!({"rateLimits":{"limitId":"codex","limitName":"Codex","primary":{"usedPercent":used,"windowDurationMins":10080,"resetsAt":self.demo_start+390000},"credits":{"hasCredits":true,"unlimited":false,"balance":"62315.77"}}}),t);
        }
        let _=self.quota.ingest_json(&serde_json::json!({"rateLimits":{"limitId":"codex","limitName":"Codex","primary":{"usedPercent":60,"windowDurationMins":10080,"resetsAt":self.demo_start+390000},"credits":{"hasCredits":true,"unlimited":false,"balance":"62315.77"}}}),now());
        self.history = Arc::new(quota_widget::history::normalized(&self.quota.history));
        self.quota.history.clear();
        self.history_generation += 1;
        self.set_intervals(Arc::new(intervals::completed_intervals(
            self.tokens.events(),
            &self.history,
        )));
    }
}
/// Commit a new user export only after successful, uncancelled serialization.
/// Writes use bounded chunks and an adjacent create-new temporary file. Existing
/// exports are never replaced, including a same-millisecond name collision.
fn save_export(
    path: &std::path::Path,
    write: impl FnOnce(&mut dyn std::io::Write) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    cancellation::check()?;
    let tmp = path.with_extension(format!(
        "export-tmp-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    struct Pending(std::path::PathBuf);
    impl Drop for Pending {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    struct Checked<'a>(&'a mut std::fs::File);
    impl Write for Checked<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            // write_all retries Interrupted; use Other internally, then restore
            // cancellation classification after serialization has unwound.
            cancellation::check().map_err(std::io::Error::other)?;
            self.0.write(&bytes[..bytes.len().min(64 * 1024)])
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush()
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    let _pending = Pending(tmp.clone());
    let result = write(&mut Checked(&mut file));
    cancellation::check()?;
    result?;
    file.sync_all()?;
    drop(file);
    cancellation::check()?;
    std::fs::hard_link(&tmp, path)?;
    Ok(())
}

impl eframe::App for WidgetApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        set_language(self.settings.language);
        let _zone = display_time::scope(self.settings.utc_offset_minutes);
        self.clear_completed_prediction();
        if ctx.input(|i| i.viewport().close_requested()) && !self.close_ready {
            self.begin_close();
        }
        if self.closing_since.is_some() && !self.close_ready {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.close_ready = self.close_finished();
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading(tr("正在關閉…"));
                ui.label(tr(format!(
                    "{} 秒",
                    self.closing_since.unwrap().elapsed().as_secs()
                )));
                ui.spinner();
            });
            if self.close_ready {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            ctx.request_repaint_after(Duration::from_millis(50));
            return;
        }
        if self.close_ready {
            return;
        }
        self.pump();
        let (focus, minimized) = ctx.input(|i| {
            (
                i.viewport().focused.unwrap_or(false),
                i.viewport().minimized.unwrap_or(false),
            )
        });
        if self.settings.on_top
            && ((focus && !self.last_focus)
                || (!minimized && self.last_minimized)
                || self.last_topmost.elapsed() > Duration::from_secs(15))
        {
            self.topmost(ctx);
        }
        self.last_focus = focus;
        self.last_minimized = minimized;
        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
            let p = [rect.min.x, rect.min.y];
            if self.settings.position != Some(p) {
                self.settings.position = Some(p);
                self.dirty = true;
            }
        }
        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
            let s = [rect.width(), rect.height()];
            if self.settings.size != s {
                self.settings.size = s;
                self.dirty = true;
            }
        }
        self.widget(ctx);
        self.detail_windows(ctx);
        ctx.request_repaint_after(
            if self.load_pending
                || self.import_pending
                || self.view_pending
                || self.work_progress.is_some()
            {
                Duration::from_millis(250)
            } else {
                Duration::from_secs(1)
            },
        );
    }
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        [0., 0., 0., 0.]
    }
    fn on_exit(&mut self, _: Option<&eframe::glow::Context>) {
        self.begin_close();
        self.bridge = None;
        if let Some(join) = self.export_join.take() {
            let _ = join.join();
        }
        let _ = self.worker.shutdown();
    }
}

impl Drop for WidgetApp {
    fn drop(&mut self) {
        self.begin_close();
        if let Some(join) = self.export_join.take() {
            let _ = join.join();
        }
        let _ = self.worker.shutdown();
    }
}

#[cfg(test)]
mod async_export_tests {
    use super::*;

    #[test]
    fn async_export_disconnect_clears_pending_state() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = synthetic_export_app(temp.path().to_path_buf());
        app.settings.import_enabled = false;
        let (tx, rx) = mpsc::channel();
        app.export_rx = Some(rx);
        drop(tx);
        app.pump();
        assert!(app.export_rx.is_none());
        assert!(app.notice.contains("匯出中斷"));
        assert!(!temp.path().join("exports").exists());
    }

    fn synthetic_export_app(directory: PathBuf) -> WidgetApp {
        let timestamp = 1_791_201_600; // Fixed synthetic date, never a real log.
        let mut app = WidgetApp::new(
            Settings {
                pricing: Pricing::official_fast(),
                ..Settings::default()
            },
            directory,
            true,
            false,
            true,
            Vec::new(),
        );
        app.tokens = Arc::new(Snapshot::from_store(&TokenStore::demo(timestamp)));
        app.token_date = local_day(timestamp).unwrap();
        app.date = app.token_date;
        app.history = Arc::new(vec![serde_json::from_value(serde_json::json!({
            "session": "synthetic-live-session",
            "observed_at_unix": timestamp,
            "source_key": "codex",
            "epoch": 0,
            "kind": "QuotaDecrease",
            "window_label": "week",
            "window_key": "primary",
            "before": "92",
            "after": "91",
            "delta": "1",
            "partial": false,
            "before_observed_at_unix": timestamp - 5
        }))
        .unwrap()]);
        app
    }

    #[test]
    fn async_export_keeps_all_formats_and_snapshot_pricing_metadata() {
        for tokens in [false, true] {
            for json in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let mut app = synthetic_export_app(temp.path().to_path_buf());
                app.export_day(tokens, json);
                app.export_join.take().unwrap().join().unwrap();
                let message = app.export_rx.take().unwrap().recv().unwrap();
                assert!(message.starts_with("已匯出："), "{message}");
                let files: Vec<_> = std::fs::read_dir(temp.path().join("exports"))
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .collect();
                assert_eq!(files.len(), if !tokens && !json { 2 } else { 1 });
                let primary = files
                    .iter()
                    .find(|path| {
                        !path
                            .file_name()
                            .unwrap()
                            .to_string_lossy()
                            .ends_with("-models.csv")
                    })
                    .unwrap();
                let text = std::fs::read_to_string(primary).unwrap();
                if json {
                    let data: serde_json::Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(data["kind"], if tokens { "tokens" } else { "quota" });
                    assert_eq!(data["pricing"]["tier_label"], "DEMO synthetic prices");
                    assert!(!data["events"].as_array().unwrap().is_empty());
                    assert_eq!(data["events"][0]["pricing_basis"], "DEMO synthetic prices");
                    assert!(data["events"][0]["local_datetime"].is_string());
                    if !tokens {
                        assert_eq!(data["completed_intervals"].as_array().unwrap().len(), 1);
                        assert!(data["events"][0]["completed_interval"].is_object());
                    }
                    if tokens {
                        assert!(!data["turn_summaries"].as_array().unwrap().is_empty());
                        assert!(!data["task_summaries"].as_array().unwrap().is_empty());
                    }
                } else {
                    assert!(text.lines().count() > 1);
                    assert!(text.contains("pricing_verified_at"));
                    assert!(text.contains("DEMO synthetic prices"));
                    assert!(text.contains(if tokens {
                        "response_id"
                    } else {
                        "estimated_cost_usd"
                    }));
                }
            }
        }
    }

    #[test]
    fn async_export_close_joins_and_leaves_only_a_complete_or_cancelled_export() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = synthetic_export_app(temp.path().to_path_buf());
        app.export_day(true, true);
        drop(app);
        let files: Vec<_> = std::fs::read_dir(temp.path().join("exports"))
            .into_iter()
            .flatten()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert!(files.len() <= 1);
        if let Some(path) = files.first() {
            assert_eq!(path.extension().unwrap(), "json");
            let data: serde_json::Value = storage::load_json(path).unwrap();
            assert_eq!(data["kind"], "tokens");
            assert!(!data["events"].as_array().unwrap().is_empty());
        }
    }
}

#[cfg(test)]
mod widget_revision_tests {
    use super::*;
    fn history_fixture() -> HistoryEvent {
        serde_json::from_value(serde_json::json!({"session":"synthetic","observed_at_unix":1792252800,"before_observed_at_unix":1792249200,"source_key":"codex","window_label":"week","window_key":"primary","epoch":0,"kind":"QuotaDecrease","before":"2","after":"1","delta":"1"})).unwrap()
    }
    #[test]
    fn uncertain_history_label_retains_observed_drop_without_claiming_refill() {
        let mut e = history_fixture();
        e.before = "100".into();
        e.after = "99".into();
        e.reconciliation = Some(quota_widget::model::HistoryReconciliation::UncertainRecovery);
        let text = history_description(&e, None, true, None);
        assert!(text.starts_with("100% → 99%"));
        assert!(text.contains("狀態：差異"));
        assert!(!text.contains("回補"));
        assert!(!text.contains('$'));
    }
    #[test]
    fn model_breakdown_unknown_price_is_distinct_from_known_zero_and_partial_zero() {
        let mut model = intervals::ModelIntervalUsage {
            model: "synthetic-model".into(),
            input: 0,
            cached: 0,
            uncached: 0,
            output: 0,
            reasoning: 0,
            known_usd: 0.,
            unknown_cost_events: 0,
            events: 1,
            assumed_model_events: 0,
            model_assumed: false,
            partial_events: 0,
            coverage_gap_events: 0,
        };
        assert_eq!(interval_model_cost(&model), "$0.0000");
        model.unknown_cost_events = 1;
        assert_eq!(interval_model_cost(&model), "未知（價格未設定）");
        model.events = 2;
        assert_eq!(interval_model_cost(&model), "$0.0000 *");
        model.known_usd = 0.0024;
        assert_eq!(interval_model_cost(&model), "$0.0024 *");
    }

    fn quota_drop(at: i64, before: &str, after: &str) -> HistoryEvent {
        serde_json::from_value(serde_json::json!({
            "session":"synthetic", "observed_at_unix":at, "source_key":"codex",
            "epoch":0, "kind":"QuotaDecrease", "window_label":"week",
            "window_key":"primary", "before":before, "after":after, "delta":"1"
        }))
        .unwrap()
    }

    #[test]
    fn daily_segments_start_at_midnight_and_ignore_repeated_first_decrements() {
        let day = NaiveDate::from_ymd_opt(2026, 10, 22).unwrap();
        let (start, end) = display_time::day_bounds(day).unwrap();
        let history = vec![
            quota_drop(start + 120, "11", "10"),
            quota_drop(start + 600, "10", "9"),
            quota_drop(start + 1200, "10", "9"),
            quota_drop(start + 1800, "9", "8"),
        ];
        let rows = daily_quota_segments(&history, start, end, Some(1.), start + 2400);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].change, "1% → 0%");
        assert!(rows[0].event.is_none());
        assert_eq!(rows[0].start, start + 1800);
        assert_eq!(rows[0].end, start + 2400);
        assert_eq!(rows[1].change, "9% → 8%");
        assert_eq!((rows[1].start, rows[1].end), (start + 600, start + 1800));
        assert_eq!(rows[2].change, "10% → 9%");
        assert_eq!((rows[2].start, rows[2].end), (start + 120, start + 600));
        assert_eq!(rows[3].change, "11% → 10%");
        assert_eq!((rows[3].start, rows[3].end), (start, start + 120));
    }

    #[test]
    fn daily_segments_show_reset_and_first_drop_without_charging_the_old_cycle() {
        let start = 1_792_684_800;
        let mut reset = quota_drop(start + 100, "0", "100");
        reset.kind = quota_widget::model::HistoryKind::QuotaReset;
        reset.evidence = Some(quota_widget::model::HistoryEvidence {
            window_duration_mins: Some(10080),
            before_reset_at: Some(start + 50),
            reset_at: Some(start + 604800),
            ..Default::default()
        });
        let mut replay = reset.clone();
        replay.observed_at_unix += 790;
        let rows = daily_quota_segments(
            &[
                quota_drop(start + 10, "100", "99"),
                reset,
                replay,
                quota_drop(start + 900, "100", "99"),
            ],
            start,
            start + 86400,
            None,
            start + 1000,
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].change, "100% → 99%");
        assert_eq!((rows[0].start, rows[0].end), (start + 100, start + 900));
        assert_eq!(rows[1].change, "0% → 100%");
        assert_eq!(
            (rows[1].start, rows[1].end, rows[1].points),
            (start + 100, start + 100, 0.0)
        );
    }

    #[test]
    fn historical_summary_cache_survives_active_cycle_writes_but_tracks_its_archive() {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("tokens.sqlite3");
        std::fs::write(&db, b"active").unwrap();
        let old = root.path().join("quota-cycle-20261020-000000.sqlite3");
        let new = root.path().join("quota-cycle-20261023-000000.sqlite3");
        std::fs::write(&old, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        let (a, b) =
            display_time::day_bounds(NaiveDate::from_ymd_opt(2026, 10, 21).unwrap()).unwrap();
        let key = segment_cache_path(&db, a, b).unwrap();
        std::fs::write(&db, b"active updated").unwrap();
        assert_eq!(key, segment_cache_path(&db, a, b).unwrap());
        std::fs::write(&old, b"old corrected data").unwrap();
        assert_ne!(key, segment_cache_path(&db, a, b).unwrap());
        let path = segment_cache_path(&db, a, b).unwrap();
        storage::save_json(
            &path,
            &CachedSegmentUsage {
                models: vec![],
                counts: [0, 169, 0, 169],
                cost: accounting::ObservedCost {
                    known_usd: 4.317,
                    events: 169,
                    ..Default::default()
                },
            },
        )
        .unwrap();
        let restored: CachedSegmentUsage = storage::load_json(&path).unwrap();
        assert_eq!(restored.counts[3], 169);
        assert_eq!(restored.cost.known_usd, 4.317);
    }

    #[test]
    fn interval_counts_usage_records_per_model_family() {
        let make =
            |id: &str, session: &str, turn: Option<&str>, model: &str, flags: Vec<String>| {
                TokenEvent {
                    id: id.into(),
                    timestamp: 100,
                    model: model.into(),
                    model_assumed: false,
                    reasoning_effort: None,
                    session: session.into(),
                    response_id: None,
                    turn_id: turn.map(str::to_owned),
                    source_kind: None,
                    input: 1,
                    cached: 0,
                    uncached: 1,
                    output: 0,
                    reasoning: 0,
                    cost_usd: Some(0.01),
                    coverage_start_timestamp_ms: None,
                    partial_flags: flags,
                }
            };
        let events = [
            make("1", "task-a", Some("turn-1"), "gpt-6.1-sol", vec![]),
            make("2", "task-a", Some("turn-1"), "gpt-6.1-sol", vec![]),
            make("3", "task-a", Some("turn-2"), "gpt-6.1-sol", vec![]),
            make("4", "task-a", Some("turn-1"), "gpt-6-luna", vec![]),
            make("5", "task-b", None, "gpt-6-luna", vec![]),
            make("6", "task-b", None, "gpt-6-luna", vec![]),
            make("7", "task-c", Some("turn-1"), "gpt-6-astra", vec![]),
            make(
                "8",
                "task-review",
                Some("review-turn"),
                "codex-auto-review",
                vec!["excluded_recorded_codex_auto_review".into()],
            ),
        ];
        let refs: Vec<_> = events.iter().collect();
        assert_eq!(interval_task_counts(&refs), [3, 3, 1, 7]);
        let responses: Vec<_> = (0..169)
            .map(|i| {
                let mut event = make(
                    &format!("event-{i}"),
                    "one-chat",
                    Some("one-turn"),
                    "gpt-6.1-sol",
                    vec![],
                );
                event.response_id = Some(format!("response-{i}"));
                event
            })
            .collect();
        let mut replay = responses[0].clone();
        replay.id = "replayed-response".into();
        let mut refs: Vec<_> = responses.iter().collect();
        refs.push(&replay);
        assert_eq!(interval_task_counts(&refs), [169, 0, 0, 169]);
    }

    #[test]
    fn partial_interval_label_keeps_known_subtotal_and_explains_full_time_span() {
        let e = history_fixture();
        let mut v = intervals::completed_intervals(&[], std::slice::from_ref(&e)).remove(0);
        v.known_usd = 1937.2468;
        assert!(v.partial && !v.comparable);
        let text = history_description(&e, None, true, Some(&v));
        assert!(text.contains("小計 $1937.2468 *"));
        assert!(text.contains("部分"));
        assert!(!text.contains("$—/百分點"));
        let tip = history_tooltip(&text, Some(&v));
        for time in [v.start.unwrap(), v.end] {
            assert!(tip.contains(&display_time::datetime(time).unwrap()));
        }
        assert!(tip.contains("狀態：部分"));
    }
    #[test]
    fn comparable_interval_keeps_its_cost_per_point_and_known_zero_stays_numeric() {
        let e = history_fixture();
        let mut v = intervals::completed_intervals(&[], std::slice::from_ref(&e)).remove(0);
        v.known_usd = 2.;
        v.partial = false;
        v.comparable = true;
        v.cost_per_point = Some(2.);
        v.reasons.clear();
        let text = history_description(&e, None, false, Some(&v));
        assert!(text.contains("$2.0000 · $2.0000/百分點"));
        assert!(!history_tooltip(&text, Some(&v)).contains("狀態：部分"));
        assert!(history_description(&e, Some(0.), true, None).contains("$0.0000 *"));
        assert!(history_description(&e, None, true, None).contains("$— *"));
    }
    #[test]
    fn recent_interval_tooltip_and_table_share_canonical_cached_rows() {
        let mut app = demo_app();
        let time = now() - 600;
        let events: Vec<_> = (0..8)
            .map(|i| {
                let mut e = history_fixture();
                e.observed_at_unix = time + i * 30;
                e.before_observed_at_unix = Some(e.observed_at_unix - 30);
                e.before = (90 - i).to_string();
                e.after = (89 - i).to_string();
                e
            })
            .collect();
        app.history = Arc::new(events);
        app.history_generation += 1;
        app.set_intervals(Arc::new(intervals::completed_intervals(&[], &app.history)));
        app.refresh_main_cache();
        assert_eq!(app.main_cache.recent_events.len(), 5);
        let recent: Vec<_> = app
            .main_cache
            .recent_events
            .iter()
            .map(|event| event.observed_at_unix)
            .collect();
        assert_eq!(
            recent,
            vec![time + 210, time + 180, time + 150, time + 120, time + 90]
        );
        for event in &app.main_cache.recent_events {
            let i = app.interval_lookup[&intervals::history_event_key(event)];
            assert!(app
                .main_cache
                .recent_tooltip
                .contains(&app.interval_presentations[i].change));
            assert!(app
                .main_cache
                .recent_tooltip
                .contains(&app.interval_presentations[i].total));
            assert!(app
                .main_cache
                .recent_tooltip
                .contains(&app.interval_presentations[i].status));
        }
        let text_ptr = app.main_cache.recent_tooltip.as_ptr();
        let views_ptr = app.interval_presentations.as_ptr();
        for _ in 0..20 {
            app.refresh_main_cache();
        }
        assert_eq!(text_ptr, app.main_cache.recent_tooltip.as_ptr());
        assert_eq!(views_ptr, app.interval_presentations.as_ptr());
        app.date = local_day(time + 210).unwrap();
        let ctx = egui::Context::default();
        for _ in 0..app.history.len() {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.history_ui(ui));
            });
        }
        let ids = app.history_cache.interval_indices.clone();
        assert!(!ids.is_empty());
        assert!(ids
            .iter()
            .all(|&i| local_day(app.intervals[i].end) == Some(app.date)));
        let ids_ptr = app.history_cache.interval_indices.as_ptr();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.history_ui(ui));
        });
        assert_eq!(ids_ptr, app.history_cache.interval_indices.as_ptr());
        let mut repriced = app.intervals.as_ref().clone();
        repriced.last_mut().unwrap().known_usd = 12.3456;
        app.set_intervals(Arc::new(repriced));
        app.refresh_main_cache();
        assert!(app.main_cache.recent_tooltip.contains("$12.3456"));
        assert!(app.history_cache.key.is_some());
    }
    #[test]
    fn empty_recent_interval_hover_has_no_fabricated_price() {
        let mut app = demo_app();
        app.set_intervals(Arc::new(Vec::new()));
        app.refresh_main_cache();
        assert!(app.main_cache.recent_events.is_empty());
        assert!(app.main_cache.recent_tooltip.contains("已完成區間：0"));
        assert!(!app.main_cache.recent_tooltip.contains("$0"));
    }
    #[test]
    fn demo_completed_intervals_populate_the_canonical_presentation_cache() {
        let app = demo_app();
        assert!(!app.intervals.is_empty());
        assert_eq!(
            app.intervals.as_slice(),
            intervals::completed_intervals(app.tokens.events(), &app.history)
        );
        assert_eq!(app.interval_presentations.len(), app.intervals.len());
        assert!(!app.main_cache.recent_events.is_empty());
        assert!(!app.main_cache.recent_tooltip.contains("已完成區間：0"));
    }
    pub(super) fn demo_app() -> WidgetApp {
        let mut app = WidgetApp::new(
            Settings::default(),
            PathBuf::new(),
            false,
            true,
            true,
            Vec::new(),
        );
        app.initialize();
        app.refresh_main_cache();
        app
    }
    #[test]
    fn task_tooltip_uses_actual_name_response_and_matching_turn_snapshot() {
        let app = demo_app();
        let row = app
            .main_cache
            .tasks
            .iter()
            .find(|r| r.task_id.starts_with("task-A"))
            .unwrap();
        let tip = app.task_tooltip(row);
        for text in [
            "DEMO：修復匯入效能",
            "Input: 18200",
            "Cached: 14100",
            "Uncached: 4100",
            "Output: 2320",
            "Reasoning: 1120",
            "Turn Input: 30600",
            "Turn Output: 4160",
            "已記錄 Turn 累計快照",
        ] {
            assert!(tip.contains(text), "{text}: {tip}");
        }
    }
    #[test]
    fn missing_tooltip_counters_show_unknown_not_zero() {
        let mut app = demo_app();
        let row = app.main_cache.tasks[0].clone();
        let snap = Arc::get_mut(&mut app.tokens).unwrap();
        let event = &mut snap.events[row.latest_event_index];
        event.partial_flags.extend([
            "cached_count_unavailable".into(),
            "reasoning_count_unavailable".into(),
        ]);
        let tip = app.task_tooltip(&row);
        assert!(tip.contains("Cached: 未知"));
        assert!(tip.contains("Uncached: 未知"));
        assert!(tip.contains("Reasoning: 未知"));
    }
    #[test]
    fn model_picker_and_editor_preserve_other_rates_and_unknowns() {
        let mut app = demo_app();
        app.settings.pricing = Pricing::official_standard();
        let original = app.settings.pricing.rates.clone();
        let choices = app.price_model_choices();
        for name in ["gpt-6.1-sol", "gpt-6-luna", "gpt-6-astra"] {
            assert!(choices.contains(name));
        }
        app.price_model = "gpt-6-luna".into();
        app.load_price_editor();
        assert_eq!(app.price_values, [0.1, 0.01, 0.5]);
        app.price_values = [0.3, 0.04, 0.8];
        app.apply_price_editor();
        assert_eq!(
            app.settings.pricing.rates["gpt-6-luna"].output_per_million,
            0.8
        );
        for name in ["gpt-6.1-sol", "gpt-6-astra"] {
            assert_eq!(app.settings.pricing.rates[name], original[name]);
        }
        app.price_model = "custom-recorded-model".into();
        app.load_price_editor();
        assert!(!app.price_configured);
        app.price_values = [1., 0.2, 3.];
        app.price_configured = true;
        app.apply_price_editor();
        assert!(app.price_model_choices().contains("custom-recorded-model"));
        app.price_model = "gpt-6-luna".into();
        app.load_price_editor();
        assert_eq!(app.price_values, [0.3, 0.04, 0.8]);
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        storage::save_json(&path, &app.settings).unwrap();
        let saved: Settings = storage::load_json(&path).unwrap();
        assert_eq!(saved.pricing, app.settings.pricing);
    }
    #[test]
    fn main_tree_cache_expansion_preserves_self_and_global_cost() {
        let mut app = demo_app();
        assert_eq!(app.main_cache.tree.len(), 2);
        let total = app.main_cache.cost;
        let ptr = app.main_cache.tasks.as_ptr();
        app.refresh_main_cache();
        assert_eq!(ptr, app.main_cache.tasks.as_ptr());
        app.expanded_tasks
            .insert("task-A: DEMO synthetic usage".into());
        app.tree_generation += 1;
        app.refresh_main_cache();
        assert_eq!(app.main_cache.tree.len(), 3);
        assert_eq!(app.main_cache.cost, total);
        assert!(app
            .main_cache
            .tasks
            .iter()
            .all(|t| t.interval_cost_usd.is_some()));
    }
    #[test]
    fn missing_history_starts_known_zero_without_blanks() {
        let mut app = demo_app();
        app.history = Arc::new(Vec::new());
        app.history_generation += 1;
        app.refresh_main_cache();
        assert_eq!(app.main_cache.cost.known_usd, 0.);
        assert!(app.main_cache.cost.partial);
        assert!(app
            .main_cache
            .tasks
            .iter()
            .all(|t| t.interval_cost_usd == Some(0.) && t.partial));
    }
    #[test]
    fn visible_children_summary_click_expands_and_collapses() {
        let mut app = demo_app();
        app.tasks_expanded = true;
        let ctx = egui::Context::default();
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(430., 511.));
        let pos = Pos2::new(60. * 430. / 310., 215. * 430. / 310.);
        let frame = |app: &mut WidgetApp, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(rect),
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| app.widget(ctx));
        };
        frame(&mut app, vec![]);
        frame(&mut app, vec![egui::Event::PointerMoved(pos)]);
        for expected in [3, 2] {
            frame(
                &mut app,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Default::default(),
                }],
            );
            frame(
                &mut app,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Default::default(),
                }],
            );
            app.refresh_main_cache();
            assert_eq!(app.main_cache.tree.len(), expected);
        }
    }
    #[test]
    fn audit_export_preserves_review_without_default_cost_contamination() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("synthetic.jsonl");
        let timestamp = now();
        let records=[("normal","gpt-6-luna"),("review","codex-auto-review")].into_iter().map(|(id,model)|serde_json::json!({"timestamp":timestamp,"type":"token_usage_record","payload":{"thread_id":id,"turn_id":"t","response_id":id,"model":model,"usage":{"input_tokens":10000,"cached_input_tokens":9000,"output_tokens":100,"reasoning_output_tokens":50}}}).to_string()).collect::<Vec<_>>().join("\n")+"\n";
        std::fs::write(source, records).unwrap();
        let mut store = TokenStore::default();
        store
            .import_dir(temp.path(), &Pricing::official_standard())
            .unwrap();
        let mut app = WidgetApp::new(
            Settings::default(),
            temp.path().to_owned(),
            true,
            false,
            true,
            Vec::new(),
        );
        app.tokens = Arc::new(Snapshot::from_store(&store));
        app.token_date = local_day(timestamp).unwrap();
        assert_eq!(app.tokens.events.len(), 1);
        assert_eq!(app.tokens.excluded_events.len(), 1);
        for audit in [false, true] {
            app.show_excluded_records = audit;
            app.export_day(true, true);
            app.export_join.take().unwrap().join().unwrap();
            let message = app.export_rx.take().unwrap().recv().unwrap();
            assert!(message.starts_with("已匯出"));
        }
        for entry in std::fs::read_dir(temp.path().join("exports")).unwrap() {
            let value: serde_json::Value = storage::load_json(&entry.unwrap().path()).unwrap();
            let audit = value["kind"] == "tokens-excluded";
            assert_eq!(value["events"].as_array().unwrap().len(), 1);
            assert_eq!(
                value["events"][0]["model"],
                if audit {
                    "codex-auto-review"
                } else {
                    "gpt-6-luna"
                }
            );
            assert_eq!(value["events"][0]["excluded_from_default_totals"], audit);
            if audit {
                assert!(value["events"][0]["cost_usd"].is_null());
            }
            assert_eq!(value["excluded_auto_review_records"], 1);
        }
    }
    #[test]
    fn saved_theme_pins_first_frame_and_icon_strokes_against_system_theme() {
        let temp = tempfile::tempdir().unwrap();
        for dark in [true, false, true] {
            let settings = Settings {
                dark,
                ..Default::default()
            };
            storage::save_json(&temp.path().join("settings.json"), &settings).unwrap();
            let restored: Settings =
                storage::load_json(&temp.path().join("settings.json")).unwrap();
            let mut app = WidgetApp::new(
                restored,
                temp.path().to_owned(),
                false,
                false,
                true,
                Vec::new(),
            );
            let ctx = egui::Context::default();
            app.apply_theme(&ctx);
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(400., 500.))),
                system_theme: Some(if dark {
                    egui::Theme::Light
                } else {
                    egui::Theme::Dark
                }),
                ..Default::default()
            };
            let output = ctx.run(input, |ctx| app.widget(ctx));
            assert_eq!(ctx.style().visuals.dark_mode, dark);
            assert_eq!(
                ctx.options(|o| o.theme_preference),
                if dark {
                    egui::ThemePreference::Dark
                } else {
                    egui::ThemePreference::Light
                }
            );
            let expected = if dark {
                Color32::from_rgb(220, 247, 229)
            } else {
                Color32::from_rgb(29, 61, 45)
            };
            let icon_strokes=output.shapes.iter().filter(|shape| {
                matches!(&shape.shape,egui::Shape::LineSegment{stroke,..} if stroke.color==expected)
            }).count();
            assert!(icon_strokes >= 10, "icons remain visible in either theme");
        }
    }
    #[test]
    fn hong_kong_day_filters_exports_intervals_and_restart_without_moving_epochs() {
        let temp = tempfile::tempdir().unwrap();
        let utc = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp();
        let before = utc("2026-10-17T15:59:59Z");
        let midnight = utc("2026-10-17T16:00:00Z");
        let late = utc("2026-10-17T23:51:00Z");
        let source = temp.path().join("synthetic.jsonl");
        let rows=[("prior",before),("midnight",midnight),("late",late)].into_iter().map(|(id,t)|serde_json::json!({"timestamp":t,"type":"token_usage_record","payload":{"thread_id":"synthetic-hk","turn_id":id,"response_id":id,"model":"gpt-6-luna","reasoning_effort":"max","usage":{"input_tokens":1000,"cached_input_tokens":900,"output_tokens":100,"reasoning_output_tokens":50}}}).to_string()).collect::<Vec<_>>().join("\n")+"\n";
        std::fs::write(&source, rows).unwrap();
        let mut store = TokenStore::default();
        store
            .import_dir(temp.path(), &Pricing::official_standard())
            .unwrap();
        let cache = temp.path().join("saved.json");
        store.save(&cache).unwrap();
        let reopened = TokenStore::load(&cache).unwrap();
        assert_eq!(
            reopened
                .events()
                .iter()
                .map(|e| e.timestamp)
                .collect::<Vec<_>>(),
            vec![before, midnight, late]
        );
        let mut app = WidgetApp::new(
            Settings::default(),
            temp.path().to_owned(),
            true,
            false,
            true,
            Vec::new(),
        );
        app.tokens = Arc::new(Snapshot::from_store(&reopened));
        let day = NaiveDate::from_ymd_opt(2026, 10, 18).unwrap();
        app.date = day;
        app.token_date = day;
        app.history=Arc::new([(midnight-30,"90","89"),(midnight+60,"89","88")].into_iter().map(|(timestamp,before,after)|serde_json::from_value(serde_json::json!({"session":"test","observed_at_unix":timestamp,"source_key":"codex","epoch":0,"kind":"QuotaDecrease","window_label":"week","window_key":"primary","before":before,"after":after,"delta":"1"})).unwrap()).collect());
        assert_eq!(local_time(late), "07:51:00");
        assert_eq!(local_day(before), Some(day.pred_opt().unwrap()));
        for tokens in [true, false] {
            for json in [true, false] {
                app.export_day(tokens, json);
                app.export_join.take().unwrap().join().unwrap();
                assert!(app
                    .export_rx
                    .take()
                    .unwrap()
                    .recv()
                    .unwrap()
                    .starts_with("已匯出"));
            }
        }
        for entry in std::fs::read_dir(temp.path().join("exports")).unwrap() {
            let path = entry.unwrap().path();
            let text = std::fs::read_to_string(&path).unwrap();
            if path.extension().is_some_and(|e| e == "json") {
                let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(value["timezone"], "+08:00");
                assert_eq!(value["date"], "2026-10-18");
                if value["kind"] == "tokens" {
                    let events = value["events"].as_array().unwrap();
                    assert_eq!(events.len(), 2);
                    assert!(events
                        .iter()
                        .all(|e| e["timestamp"] == e["timestamp_utc_unix"]));
                    let event = events.iter().find(|e| e["timestamp"] == late).unwrap();
                    assert_eq!(event["local_time"], "07:51:00");
                    assert_eq!(event["timestamp_hk"], "2026-10-18 07:51:00 +08:00");
                } else {
                    assert_eq!(value["events"].as_array().unwrap().len(), 1);
                    let interval = &value["completed_intervals"][0];
                    assert_eq!(interval["start"], midnight - 30);
                    assert_eq!(interval["start_hk"], "2026-10-17 23:59:30 +08:00");
                    assert_eq!(interval["end_hk"], "2026-10-18 00:01:00 +08:00");
                }
            } else {
                assert!(text.contains("+08:00"));
                assert!(text.contains("utc_unix"));
            }
        }
        assert_eq!(duration(3600), "01:00:00");
    }
}

#[cfg(test)]
mod import_diagnostics_tests {
    use super::*;
    use quota_widget::tokens::ImportReport;

    fn app(directory: &std::path::Path) -> WidgetApp {
        WidgetApp::new(
            Settings {
                import_enabled: false,
                sessions_path: directory.join("synthetic-logs").display().to_string(),
                ..Settings::default()
            },
            directory.to_owned(),
            false,
            false,
            true,
            vec![],
        )
    }
    fn loaded(error: Option<&str>, history_error: Option<&str>) -> WorkEvent {
        WorkEvent::Loaded {
            snapshot: Arc::new(Snapshot::default()),
            normalized_history: Arc::new(vec![]),
            error: error.map(str::to_owned),
            history_error: history_error.map(str::to_owned),
        }
    }
    fn updated(report: Option<ImportReport>, error: Option<&str>) -> WorkEvent {
        WorkEvent::Updated {
            snapshot: None,
            normalized_history: None,
            report,
            error: error.map(str::to_owned),
        }
    }
    fn progress(stage: ProgressStage) -> WorkEvent {
        WorkEvent::Progress(WorkProgress {
            operation: Operation::Import,
            stage,
            elapsed: Duration::from_secs(12),
            files_done: 17,
            files_total: 30,
            lines_read: 1234,
            observations: 567,
            events: 100,
        })
    }
    #[test]
    fn progress_keeps_cached_records_and_render_cache_generations() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        app.tokens = Arc::new(Snapshot::from_store(&TokenStore::demo(now())));
        app.history = Arc::new(vec![history("synthetic-source", now(), None, None)]);
        app.refresh_main_cache();
        app.token_cache.key = Some((
            app.token_generation,
            app.token_date,
            GroupBy::Response,
            false,
            0,
        ));
        let tokens = Arc::clone(&app.tokens);
        let history = Arc::clone(&app.history);
        let intervals = Arc::clone(&app.intervals);
        let token_key = app.token_cache.key;
        let generations = (app.token_generation, app.history_generation);
        app.status = "LIVE".into();
        for stage in [
            ProgressStage::Loading,
            ProgressStage::Import(ImportPhase::Discovering),
            ProgressStage::Import(ImportPhase::Reading),
            ProgressStage::Import(ImportPhase::Reconciling),
            ProgressStage::Import(ImportPhase::Metadata),
            ProgressStage::BuildingViews,
            ProgressStage::BuildingIntervals,
            ProgressStage::Saving,
        ] {
            app.handle_worker_event(progress(stage));
            assert!(Arc::ptr_eq(&tokens, &app.tokens));
            assert!(Arc::ptr_eq(&history, &app.history));
            assert!(Arc::ptr_eq(&intervals, &app.intervals));
            assert_eq!((app.token_generation, app.history_generation), generations);
            assert_eq!(app.token_cache.key, token_key);
            assert!(app.main_cache.key.is_some());
            assert_eq!(app.status, "LIVE");
            assert!(app.compact_progress_text().unwrap().contains("17/30"));
            assert!(app.import_status_text().contains("行 1234"));
            assert!(app.import_status_text().contains("觀察 567"));
            assert!(app.import_status_text().contains("12 秒"));
        }
        app.handle_worker_event(progress(ProgressStage::Complete));
        assert!(app.work_progress.is_none());
        assert_eq!(app.last_import_elapsed, Some(Duration::from_secs(12)));
        assert!(Arc::ptr_eq(&tokens, &app.tokens));
    }
    #[test]
    fn migration_progress_labels_tasks_and_preserves_cached_data() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        app.tokens = Arc::new(Snapshot::from_store(&TokenStore::demo(now())));
        app.refresh_main_cache();
        let snapshot = Arc::clone(&app.tokens);
        let generations = (app.token_generation, app.history_generation);
        app.load_pending = true;
        app.handle_worker_event(WorkEvent::Progress(WorkProgress {
            operation: Operation::Load,
            stage: ProgressStage::Migrating,
            elapsed: Duration::from_secs(17),
            files_done: 42,
            files_total: 150,
            lines_read: 0,
            observations: 0,
            events: 0,
        }));
        for text in [
            app.import_status_text(),
            app.compact_progress_text().unwrap(),
            app.empty_task_status(),
        ] {
            assert!(
                text.contains("正在轉換舊資料：task 42/150（原檔保留）"),
                "{text}"
            );
            assert!(text.contains("17 秒"), "{text}");
            assert!(!text.contains("150 檔"), "{text}");
            assert!(!text.contains("讀取 0 行"), "{text}");
        }
        assert!(Arc::ptr_eq(&snapshot, &app.tokens));
        assert_eq!((app.token_generation, app.history_generation), generations);
        assert!(app.main_cache.key.is_some());
    }
    #[test]
    fn migration_validation_with_unknown_total_does_not_claim_zero_of_zero() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        for stage in [ProgressStage::Loading, ProgressStage::Migrating] {
            app.handle_worker_event(WorkEvent::Progress(WorkProgress {
                operation: Operation::Load,
                stage,
                elapsed: Duration::from_secs(3),
                files_done: 0,
                files_total: 0,
                lines_read: 0,
                observations: 0,
                events: 0,
            }));
            for text in [
                app.import_status_text(),
                app.compact_progress_text().unwrap(),
            ] {
                assert!(text.contains("首次載入／驗證舊資料"), "{text}");
                assert!(text.contains("原檔保留"), "{text}");
                assert!(text.contains("3 秒"), "{text}");
                assert!(!text.contains("0/0"), "{text}");
            }
        }
        app.handle_worker_event(progress(ProgressStage::Import(ImportPhase::Reading)));
        assert!(app.compact_progress_text().unwrap().contains("17/30 檔"));
        assert!(app.import_status_text().contains("行 1234"));
    }
    #[test]
    fn displayed_import_remains_usable_when_later_save_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        app.import_pending = true;
        let snapshot = Arc::new(Snapshot::from_store(&TokenStore::demo(now())));
        app.handle_worker_event(WorkEvent::Updated {
            snapshot: Some(Arc::clone(&snapshot)),
            normalized_history: None,
            report: Some(ImportReport {
                files_scanned: 1,
                ..ImportReport::default()
            }),
            error: None,
        });
        app.handle_worker_event(progress(ProgressStage::Saving));
        assert!(!app.import_pending);
        assert!(app.import_status_text().contains("正在儲存"));
        app.handle_worker_event(WorkEvent::SaveFailed {
            path: dir.path().join("tokens.json"),
            error: "synthetic disk unavailable".into(),
        });
        app.handle_worker_event(progress(ProgressStage::Complete));
        assert!(Arc::ptr_eq(&snapshot, &app.tokens));
        assert!(app.import_error.is_none());
        assert!(!app.worker_failed);
        assert!(app.notice.contains("儲存失敗"));
        assert!(app.notice.contains("synthetic disk unavailable"));
    }
    #[test]
    fn interrupted_progress_cannot_leave_perpetual_busy_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        app.handle_worker_event(progress(ProgressStage::Saving));
        app.worker_disconnected();
        assert!(app.work_progress.is_none());
        assert!(app.import_status_text().contains("中斷"));
        // Even stale progress cannot overwrite the terminal failure status.
        app.handle_worker_event(progress(ProgressStage::Import(ImportPhase::Reading)));
        assert!(app.work_progress.is_none());
        assert!(app.import_status_text().contains("中斷"));
    }
    fn history(
        source: &str,
        timestamp: i64,
        account: Option<&str>,
        reset: Option<i64>,
    ) -> HistoryEvent {
        serde_json::from_value(serde_json::json!({
            "session":"synthetic-run", "observed_at_unix":timestamp, "source_key":source,
            "epoch":0,"kind":"QuotaDecrease","window_label":"每週額度","window_key":"primary",
            "before":"99","after":"98","delta":"1",
            "evidence":{"account_id":account,"before_reset_at":reset,"reset_at":reset,"window_duration_mins":10080}
        })).unwrap()
    }
    fn live(app: &mut WidgetApp, account: Option<&str>, timestamp: i64, reset: i64) {
        let mut value = serde_json::json!({"rateLimits":{"primary":{"usedPercent":13,"windowDurationMins":10080,"resetsAt":reset}}});
        if let Some(account) = account {
            value["accountId"] = serde_json::json!(account);
        }
        app.quota.ingest_json(&value, timestamp).unwrap();
    }
    #[test]
    fn completed_load_replaces_loading_status_when_import_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        app.read_state = true;
        app.initialize();
        assert!(app.load_pending);
        assert!(app.import_status_text().contains("正在載入"));
        for _ in 0..200 {
            app.pump();
            if !app.load_pending {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!app.load_pending);
        assert_eq!(app.token_generation, 1);
        assert!(app.import_status_text().contains("已載入 0 筆"));
        assert!(!app.import_status_text().contains("正在載入"));
    }
    #[test]
    fn completed_load_distinguishes_queued_scan_and_preserves_both_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        app.load_pending = true;
        app.import_pending = true;
        app.handle_worker_event(loaded(
            Some("tokens.json: bad cache"),
            Some("history.json: bad history"),
        ));
        assert!(!app.load_pending);
        assert!(app.import_pending);
        assert!(app.import_status_text().contains("正在掃描"));
        let error = app.load_error.as_ref().unwrap();
        assert!(error.contains("tokens.json"));
        assert!(error.contains("history.json"));
        assert!(app.empty_task_status().contains("錯誤"));
    }
    #[test]
    fn warnings_and_malformed_counts_survive_unchanged_polls_and_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        app.handle_worker_event(updated(
            Some(ImportReport {
                files_scanned: 2,
                malformed_lines: 1,
                warnings: vec!["Could not read synthetic file".into()],
                ..ImportReport::default()
            }),
            None,
        ));
        let original = app.import_warnings.clone();
        app.handle_worker_event(updated(
            Some(ImportReport {
                files_scanned: 2,
                files_unchanged: 2,
                ..ImportReport::default()
            }),
            None,
        ));
        assert_eq!(app.import_warnings, original);
        assert!(app.empty_task_status().contains("警告"));
        for i in 0..100 {
            app.retain_import_warning(format!("synthetic warning {i}"));
        }
        assert_eq!(app.import_warnings.len(), 20);
    }
    #[test]
    fn empty_source_and_import_failure_produce_actionable_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        app.import_pending = true;
        app.handle_worker_event(updated(None, Some("synthetic path unavailable")));
        assert!(!app.import_pending);
        assert!(app.import_status_text().contains("失敗"));
        assert!(app.empty_task_status().contains("錯誤"));
        app.handle_worker_event(updated(Some(ImportReport::default()), None));
        assert!(app.import_error.is_none());
        assert!(app
            .import_warnings
            .iter()
            .any(|w| w.contains("未找到 JSONL")));
        assert!(app.import_warnings.iter().any(|w| w.contains("曾失敗")));
        assert!(app.import_diagnostic_text().contains("synthetic-logs"));
    }
    #[test]
    fn disconnected_worker_clears_pending_flags_without_clearing_loaded_data_or_live_status() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        app.tokens = Arc::new(Snapshot::from_store(&TokenStore::demo(now())));
        let count = app.tokens.events.len();
        app.status = "LIVE".into();
        app.load_pending = true;
        app.import_pending = true;
        app.persistent = true;
        app.worker.shutdown().unwrap();
        app.pump();
        assert!(app.worker_failed);
        assert!(!app.load_pending);
        assert!(!app.import_pending);
        assert!(!app.persistent);
        assert_eq!(app.tokens.events.len(), count);
        assert_eq!(app.status, "LIVE");
        assert!(app.import_status_text().contains("中斷"));
        assert!(app.empty_task_status().contains("中斷"));
    }
    #[test]
    fn failed_load_or_import_send_cannot_leave_perpetual_pending_state() {
        for loading in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let mut app = app(dir.path());
            app.settings.import_enabled = true;
            app.read_state = loading;
            app.worker.shutdown().unwrap();
            if loading {
                app.initialize();
            } else {
                app.start_import();
            }
            assert!(app.worker_failed);
            assert!(!app.load_pending);
            assert!(!app.import_pending);
        }
    }
    #[test]
    fn empty_task_diagnostics_distinguish_missing_filtered_and_excluded_data() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        app.settings.import_enabled = true;
        assert!(app.empty_task_status().contains("尚未匯入"));
        app.tokens = Arc::new(Snapshot::from_store(&TokenStore::demo(now() - 86400)));
        app.refresh_main_cache();
        assert!(app.main_cache.tasks.is_empty());
        assert!(app.empty_task_status().contains("近期範圍"));
        let snapshot = Snapshot {
            excluded_events: app.tokens.events.clone(),
            ..Snapshot::default()
        };
        app.tokens = Arc::new(snapshot);
        assert!(app.empty_task_status().contains("排除用量"));
    }
    #[test]
    fn first_live_selection_invalidates_unrelated_fallback_within_same_minute() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let t = now();
        app.history = Arc::new(vec![history(
            "legacy-source",
            t - 321 * 60,
            None,
            Some(t + 86400),
        )]);
        app.refresh_main_cache();
        assert!(app.main_cache.boundary.is_some());
        live(&mut app, Some("current-account"), t, t + 86400);
        app.main_cache.key.as_mut().unwrap().minute = now() / 60;
        app.refresh_main_cache();
        assert!(app.main_cache.boundary.is_none());
        assert_eq!(app.history.len(), 1);
    }
    #[test]
    fn known_account_or_expired_cycle_conflicts_do_not_revive_older_unknown_boundaries() {
        let t = now();
        for (old_account, old_reset, new_account, new_reset) in [
            (
                Some("account-A"),
                Some(t + 86400),
                Some("account-B"),
                t + 86400,
            ),
            (
                Some("same-account"),
                Some(t - 3600),
                Some("same-account"),
                t + 7 * 86400,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut app = app(dir.path());
            app.history = Arc::new(vec![
                history("codex", t - 40000, None, None),
                history("codex", t - 321 * 60, old_account, old_reset),
            ]);
            live(&mut app, new_account, t, new_reset);
            app.refresh_main_cache();
            assert!(app.main_cache.boundary.is_none());
            assert_eq!(app.history.len(), 2);
            assert_eq!(
                app.matching_history().len(),
                2,
                "historical browsing is unchanged"
            );
        }
    }
    #[test]
    fn compatible_unknown_and_small_jitter_boundaries_remain_available() {
        let t = now();
        for (account, old_reset, new_reset) in [
            (Some("same-account"), Some(t + 86400), t + 86400),
            (Some("same-account"), Some(t - 30), t + 30),
            (None, None, t + 86400),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut app = app(dir.path());
            app.history = Arc::new(vec![history("codex", t - 600, account, old_reset)]);
            live(&mut app, Some("same-account"), t, new_reset);
            app.refresh_main_cache();
            assert!(app.main_cache.boundary.is_some());
        }
    }
    #[test]
    fn explicit_account_change_blocks_previous_boundary_until_a_new_observation() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let t = now();
        app.history = Arc::new(vec![history(
            "codex",
            t - 600,
            Some("old-account"),
            Some(t + 86400),
        )]);
        live(&mut app, Some("old-account"), t, t + 86400);
        app.refresh_main_cache();
        assert!(app.main_cache.boundary.is_some());
        app.account_changed();
        assert_eq!(app.quota.account_id(), None);
        live(&mut app, None, t + 1, t + 86400);
        app.refresh_main_cache();
        assert!(app.main_cache.boundary.is_none());
        // Put the synthetic floor in the past so a genuinely later observation
        // can be tested without altering the clock or waiting in this test.
        app.boundary_after = Some(t - 2);
        Arc::make_mut(&mut app.history).push(history("codex", t - 1, None, Some(t + 86400)));
        app.history_generation += 1;
        app.refresh_main_cache();
        assert_eq!(
            app.main_cache.boundary.as_ref().unwrap().observed_at_unix,
            t - 1
        );
        assert_eq!(app.history.len(), 2);
    }
    #[test]
    fn history_mismatch_diagnostics_count_loaded_data_without_remapping_sources() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let t = now();
        app.history = Arc::new(vec![history("legacy-source", t, None, None)]);
        app.date = local_day(t).unwrap();
        live(&mut app, None, t, t + 86400);
        assert!(app.matching_history().is_empty());
        let message = app.empty_history_diagnostic().unwrap();
        assert!(message.contains("已載入：1"));
        assert!(message.contains("當日：1"));
        assert!(message.contains("目前篩選：0"));
        assert_eq!(app.history[0].source_key, "legacy-source");
    }
    #[test]
    fn diagnostics_and_future_date_render_headlessly_without_changing_filters() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        app.token_date = display_time::now_day().succ_opt().unwrap();
        let selected_day = app.token_date;
        app.import_error = Some("synthetic failure".into());
        app.retain_import_warning("synthetic warning".into());
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.token_ui(ui));
        });
        assert_eq!(app.token_date, selected_day);
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.widget(ctx));
        assert!(app.main_cache.tree.is_empty());
        assert!(app.empty_task_status().contains("錯誤"));
    }
    #[test]
    fn endpoint_mismatch_marks_only_coverage_partial_and_invalidates_live_cache() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let t = now();
        let boundary = history("codex", t - 600, Some("same-account"), Some(t + 86400));
        app.history = Arc::new(vec![boundary]);
        let token = |id: &str, time, cost| TokenEvent {
            id: id.into(),
            timestamp: time,
            coverage_start_timestamp_ms: None,
            model: "synthetic".into(),
            model_assumed: false,
            reasoning_effort: None,
            session: "synthetic-task".into(),
            response_id: Some(id.into()),
            turn_id: None,
            source_kind: Some("token_usage_record".into()),
            input: 1,
            cached: 0,
            uncached: 1,
            output: 1,
            reasoning: 0,
            cost_usd: Some(cost),
            partial_flags: vec![],
        };
        let events = vec![token("before", t - 700, 2.), token("after", t - 500, 3.)];
        app.tokens = Arc::new(Snapshot {
            usage_index: usage_views::UsageIndex::new(&events),
            cost_index: accounting::CostIndex::new(&events),
            events,
            ..Snapshot::default()
        });
        app.quota.ingest_json(&serde_json::json!({"accountId":"same-account","rateLimits":{"primary":{"usedPercent":2,"windowDurationMins":10080,"resetsAt":t+86400}}}),t-1).unwrap();
        app.refresh_main_cache();
        assert_eq!(app.main_cache.cost.known_usd, 3.);
        assert!(!app.main_cache.cost.partial);
        assert_eq!(app.main_cache.tasks.len(), 1);
        assert!(!app.main_cache.tasks[0].partial);
        assert_eq!(app.main_cache.tasks[0].interval_cost_usd, Some(3.));
        live(&mut app, Some("same-account"), t, t + 86400);
        app.main_cache.key.as_mut().unwrap().minute = now() / 60;
        app.refresh_main_cache();
        assert_eq!(app.main_cache.cost.known_usd, 3.);
        assert!(app.main_cache.cost.partial);
        assert_eq!(app.main_cache.tasks[0].interval_cost_usd, Some(3.));
        assert!(app.main_cache.tasks[0].partial);
        assert!(app.main_cache.recent_tooltip.contains("額度缺口"));
        assert!(app.main_cache.recent_tooltip.contains("覆蓋：部分"));
        assert_eq!(
            app.history.len(),
            1,
            "no intermediate history was fabricated"
        );
        assert_eq!(app.history[0].after, "98");
    }
    #[test]
    fn endpoint_coverage_check_preserves_numeric_jitter_and_unknown_values() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let t = now();
        live(&mut app, None, t, t + 86400);
        let mut view = app.selected().unwrap();
        let mut event = history("codex", t - 600, None, None);
        for (end, current, expected) in [
            ("98", Some(98.), false),
            ("98", Some(98.0000005), false),
            ("98", Some(87.), true),
            ("unknown", Some(87.), false),
            ("NaN", Some(87.), false),
            ("98", Some(f64::INFINITY), false),
            ("98", None, false),
        ] {
            event.after = end.into();
            view.remaining_percent = current;
            assert_eq!(quota_endpoint_mismatch(Some(&event), Some(&view)), expected);
        }
        assert!(!quota_endpoint_mismatch(None, Some(&view)));
        assert!(!quota_endpoint_mismatch(Some(&event), None));
    }
}

#[cfg(test)]
mod database_display_tests {
    use super::*;
    use quota_widget::{
        db_view::{DailyView, DatabaseView},
        tokens::TokenSummary,
    };

    #[test]
    fn history_worker_preserves_boundaries_response_counts_costs_and_cached_results() {
        let dir = tempfile::tempdir().unwrap();
        let mut records = String::new();
        let base = now() - 300;
        for (id, timestamp) in [("a", 100), ("b", 101), ("c", 110), ("d", 111)] {
            records.push_str(
                &serde_json::json!({
                    "timestamp":base + timestamp, "type":"token_usage_record",
                    "payload":{"thread_id":"test", "response_id":id, "model":"gpt-6-luna",
                        "usage":{"input_tokens":1000,"cached_input_tokens":0,"output_tokens":100,"reasoning_output_tokens":0}}
                })
                .to_string(),
            );
            records.push('\n');
        }
        std::fs::write(dir.path().join("records.jsonl"), records).unwrap();
        let mut store = TokenStore::default();
        store
            .import_dir(dir.path(), &Pricing::official_standard())
            .unwrap();
        let legacy = dir.path().join("tokens.json");
        store.save(&legacy).unwrap();
        let path = quota_widget::token_db::TokenDb::open_or_migrate(&legacy, true, &mut |_| {})
            .unwrap()
            .unwrap()
            .path()
            .to_owned();
        let segments: Vec<_> = [(110, 120), (100, 110)]
            .into_iter()
            .map(|(start, end)| DailyQuotaSegment {
                start: base + start,
                end: base + end,
                event: None,
                change: String::new(),
                points: 1.,
            })
            .collect();
        let _reader = quota_widget::token_db::TokenDb::open_readonly(&path).unwrap();
        let mut previous = Vec::new();
        for pass in 0..2 {
            let receiver = start_history_usage(
                path.clone(),
                segments.clone(),
                true,
                egui::Context::default(),
            );
            let mut results = Vec::new();
            for (segment, expected) in segments.iter().zip([1, 2]) {
                let usage = receiver
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                assert_eq!(usage.counts[3], expected);
                assert_eq!(usage.cost.events, expected);
                let direct =
                    accounting::CostIndex::new(store.events()).observed(segment.start, segment.end);
                assert!((usage.cost.known_usd - direct.known_usd).abs() < 1e-10);
                assert!(usage.cost.known_usd > 0.);
                results.push(serde_json::to_string(&usage).unwrap());
            }
            if pass == 0 {
                previous = results;
            } else {
                assert_eq!(results, previous);
            }
        }
        assert_eq!(
            std::fs::read_dir(dir.path().join("history-cache"))
                .unwrap()
                .count(),
            2
        );
        // A cancelled page must not retain a worker blocked on its bounded channel.
        drop(start_history_usage(
            path,
            segments,
            false,
            egui::Context::default(),
        ));
    }

    // Opt-in, read-only measurement of the actual history-page calculation.
    #[test]
    #[ignore = "set QUOTA_HISTORY_BENCH_DB to a local database"]
    fn measure_real_history_page() {
        let path = PathBuf::from(std::env::var_os("QUOTA_HISTORY_BENCH_DB").unwrap());
        let db = quota_widget::token_db::TokenDb::open_readonly(&path).unwrap();
        let history = quota_widget::history::normalized(&db.history_events().unwrap());
        let mut app = app();
        Arc::get_mut(&mut app.tokens)
            .unwrap()
            .database
            .as_mut()
            .unwrap()
            .database_path = path;
        app.history = Arc::new(history);
        app.history_generation += 1;
        let mut dates: Vec<_> = app
            .history
            .iter()
            .filter_map(|event| local_day(event.observed_at_unix))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .rev()
            .take(3)
            .collect();
        dates.reverse();
        let ctx = egui::Context::default();
        for date in dates {
            app.date = date;
            let began = Instant::now();
            let mut maximum = Duration::ZERO;
            let mut frames = 0;
            loop {
                let frame = Instant::now();
                let _ = ctx.run(Default::default(), |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| app.history_ui(ui));
                });
                maximum = maximum.max(frame.elapsed());
                frames += 1;
                assert!(app.history_cache.calculation_error.is_none());
                if app.history_cache.work_key.is_none() {
                    break;
                }
                assert!(frames < 2000);
                std::thread::sleep(Duration::from_millis(1));
            }
            eprintln!(
                "history {date}: rows={} frames={frames} calculation={:?} max_frame={maximum:?}",
                app.history_cache.table_rows.len(),
                began.elapsed()
            );
        }
    }

    fn app() -> WidgetApp {
        let mut app = WidgetApp::new(
            Settings {
                import_enabled: false,
                ..Default::default()
            },
            PathBuf::new(),
            false,
            false,
            true,
            vec![],
        );
        let t = now();
        let latest = TokenStore::demo(t).events()[0].clone();
        let rows = usage_views::group_with_excluded(&[&latest], GroupBy::Response, false);
        let request = app.database_request(None, t).unwrap();
        app.tokens = Arc::new(Snapshot {
            database: Some(DatabaseView {
                request,
                database_path: PathBuf::from("synthetic.sqlite3"),
                total_included: 50_000,
                total_excluded: 23,
                day_starts: BTreeSet::from([display_time::day_bounds(
                    NaiveDate::from_ymd_opt(2001, 1, 2).unwrap(),
                )
                .unwrap()
                .0]),
                models: BTreeSet::from([
                    "historical-only-model".into(),
                    "codex-auto-review".into(),
                ]),
                main_tasks: vec![usage_views::TaskRow {
                    task_id: latest.session.clone(),
                    latest_event_index: 0,
                    latest_timestamp: latest.timestamp,
                    latest_cost_usd: Some(4.),
                    interval_cost_usd: Some(12.),
                    interval_events: 9,
                    partial: false,
                }],
                main_cost: accounting::ObservedCost {
                    known_usd: 12.,
                    events: 9,
                    ..Default::default()
                },
                daily: DailyView {
                    rows,
                    group_count: 1_001,
                    summary: TokenSummary {
                        event_count: 1_001,
                        input: 123_456,
                        cached: 100_000,
                        output: 456,
                        reasoning: 78,
                        unpriced_events: 2,
                        priced_cost_usd: 87.5,
                        ..Default::default()
                    },
                },
            }),
            events: vec![latest],
            ..Default::default()
        });
        app
    }

    #[test]
    fn main_database_totals_do_not_use_tooltip_event_index() {
        let mut app = app();
        app.refresh_main_cache();
        assert_eq!(app.main_cache.cost.known_usd, 12.);
        assert_eq!(app.main_cache.tasks[0].interval_cost_usd, Some(12.));
        assert!(!app.main_cache.loading);
        assert_eq!(app.tokens.cost_index.observed(0, now()).events, 0);
        assert_eq!(app.included_count(), 50_000);
        assert_eq!(app.excluded_count(), 23);
        assert_eq!(app.tokens.events.len(), 1);
        let tip = app.task_tooltip(&app.main_cache.tasks[0]);
        for field in [
            "最新 Response:",
            "Input:",
            "Cached:",
            "Uncached:",
            "Output:",
            "Reasoning:",
            "顯示時間:",
        ] {
            assert!(tip.contains(field), "missing {field}: {tip}");
        }
    }

    #[test]
    fn stale_boundary_never_relabels_old_cost_as_current() {
        let mut app = app();
        Arc::get_mut(&mut app.tokens)
            .unwrap()
            .database
            .as_mut()
            .unwrap()
            .request
            .boundary = Some(1);
        app.refresh_main_cache();
        assert!(app.main_cache.loading);
        assert!(app.main_cache.cost.partial);
        assert_eq!(app.main_cache.cost.known_usd, 0.);
        assert_eq!(
            app.main_cache.tasks.len(),
            1,
            "recent task identity stays visible"
        );
        assert_eq!(app.main_cache.tasks[0].interval_cost_usd, None);
        assert_eq!(app.main_cache.tasks[0].latest_cost_usd, Some(4.));
        assert!(app.main_cache.tasks[0].partial);
        assert!(app.main_cache.recent_tooltip.contains("狀態：更新中"));
    }

    #[test]
    fn daily_page_uses_whole_day_summary_and_hides_stale_selection() {
        let mut app = app();
        app.refresh_token_cache();
        assert!(!app.token_cache.loading);
        assert_eq!(app.token_cache.rows.len(), 1);
        assert_eq!(app.token_cache.count, 1_001);
        assert_eq!(app.token_cache.group_count, 1_001);
        assert_eq!(app.token_cache.input, 123_456);
        assert_eq!(app.token_cache.cost, 87.5);
        app.token_page = 1;
        app.refresh_token_cache();
        assert!(app.token_cache.loading);
        assert!(app.token_cache.rows.is_empty());
        assert_eq!(app.requested_view.as_ref().unwrap().page, 1);
        Arc::get_mut(&mut app.tokens)
            .unwrap()
            .database
            .as_mut()
            .unwrap()
            .request
            .page = 1;
        app.token_generation += 1;
        app.accept_database_view();
        app.refresh_token_cache();
        assert!(!app.view_pending);
        assert!(!app.token_cache.loading);
        assert_eq!(app.token_cache.count, 1_001);
        assert_eq!(app.token_cache.cost, 87.5);
        app.token_date = app.token_date.pred_opt().unwrap();
        app.refresh_token_cache();
        assert!(app.token_cache.loading);
        assert!(app.token_cache.rows.is_empty());
    }

    #[test]
    fn database_requests_dedupe_and_track_all_selectors() {
        let mut app = app();
        let t = now() / 60 * 60;
        app.request_database_view(None, t);
        app.request_database_view(None, t + 1);
        assert_eq!(app.requested_view.as_ref().unwrap().now, t);
        app.token_group = GroupBy::Turn;
        app.request_database_view(None, t + 1);
        assert_eq!(app.requested_view.as_ref().unwrap().group, GroupBy::Turn);
        app.show_excluded_records = true;
        app.token_page = 2;
        app.request_database_view(Some(123), t + 60);
        let request = app.requested_view.as_ref().unwrap();
        assert!(request.excluded);
        assert_eq!(request.page, 2);
        assert_eq!(request.page_size, 500);
        assert_eq!(request.boundary, Some(123));
        assert_eq!(request.now, t + 60);
    }

    #[test]
    fn database_metadata_supplies_historical_days_and_models() {
        let mut app = app();
        let day = NaiveDate::from_ymd_opt(2001, 1, 2).unwrap();
        assert!(app.days().contains(&day));
        let choices = app.price_model_choices();
        assert!(choices.contains("historical-only-model"));
        assert!(choices.contains("codex-auto-review"));
        assert!(app.import_diagnostic_text().contains("50000"));
        assert!(app.import_diagnostic_text().contains("23"));
    }

    #[test]
    fn main_scope_ignores_daily_page_but_requires_boundary_fallback_and_minute() {
        let app = app();
        let mut request = app.database_request(Some(100), 300).unwrap();
        request.page = 9;
        request.group = GroupBy::Task;
        assert!(main_view_matches(&request, Some(100), app.demo_start, 359));
        assert!(!main_view_matches(&request, Some(101), app.demo_start, 359));
        assert!(!main_view_matches(
            &request,
            Some(100),
            app.demo_start + 1,
            359
        ));
        assert!(!main_view_matches(&request, Some(100), app.demo_start, 360));
    }

    #[test]
    fn canonical_history_cost_keeps_partial_priced_subtotal() {
        let event: HistoryEvent = serde_json::from_value(serde_json::json!({
            "session":"synthetic", "observed_at_unix":1792252800,
            "before_observed_at_unix":1792249200, "source_key":"codex",
            "window_label":"week", "window_key":"primary", "epoch":0,
            "kind":"QuotaDecrease", "before":"2", "after":"1", "delta":"1"
        }))
        .unwrap();
        let mut interval = intervals::completed_intervals(&[], &[event]).remove(0);
        interval.known_usd = 1937.2468;
        interval.events = 2;
        interval.unknown_cost_events = 1;
        interval.partial = true;
        assert_eq!(
            canonical_interval_cost(Some(&interval)),
            (Some(1937.2468), true)
        );
        interval.unknown_cost_events = 2;
        assert_eq!(canonical_interval_cost(Some(&interval)), (None, true));
        assert_eq!(canonical_interval_cost(None), (None, true));
    }
    #[test]
    fn database_exports_read_whole_day_and_use_canonical_interval_subtotals() {
        let temp = tempfile::tempdir().unwrap();
        let day = display_time::now_day();
        let (start, end) = display_time::day_bounds(day).unwrap();
        let mut records = String::new();
        for i in 0..603 {
            let timestamp = if i == 601 {
                start - 1
            } else if i == 602 {
                end
            } else {
                start + 100 + i
            };
            let model = if i == 600 {
                "codex-auto-review"
            } else {
                "gpt-6-luna"
            };
            records.push_str(
                &serde_json::json!({
                    "timestamp":timestamp, "type":"token_usage_record",
                    "payload":{"thread_id":"synthetic-export", "turn_id":format!("t-{i}"),
                    "response_id":format!("r-{i}"), "model":model,
                    "usage":{"input_tokens":1000, "cached_input_tokens":900,
                    "output_tokens":100, "reasoning_output_tokens":50}}
                })
                .to_string(),
            );
            records.push('\n');
        }
        std::fs::write(temp.path().join("synthetic.jsonl"), records).unwrap();
        let mut store = TokenStore::default();
        store
            .import_dir(temp.path(), &Pricing::official_standard())
            .unwrap();
        let legacy = temp.path().join("tokens.json");
        store.save(&legacy).unwrap();
        let database_path = {
            let db = quota_widget::token_db::TokenDb::open_or_migrate(&legacy, true, &mut |_| {})
                .unwrap()
                .unwrap();
            db.path().to_owned()
        };
        let mut app = app();
        app.dir = temp.path().to_owned();
        app.read_state = true;
        app.token_date = day;
        app.date = day;
        Arc::get_mut(&mut app.tokens)
            .unwrap()
            .database
            .as_mut()
            .unwrap()
            .database_path = database_path;
        // The loaded GUI contains only one tooltip event; exports must not use it.
        assert_eq!(app.tokens.events.len(), 1);
        for audit in [false, true] {
            app.show_excluded_records = audit;
            app.export_day(true, true);
            app.export_join.take().unwrap().join().unwrap();
            assert!(app
                .export_rx
                .take()
                .unwrap()
                .recv()
                .unwrap()
                .starts_with("已匯出"));
        }
        let event: HistoryEvent = serde_json::from_value(serde_json::json!({
            "session":"synthetic", "observed_at_unix":start + 1000,
            "before_observed_at_unix":start + 1, "source_key":"codex",
            "window_label":"week", "window_key":"primary", "epoch":0,
            "kind":"QuotaDecrease", "before":"2", "after":"1", "delta":"1"
        }))
        .unwrap();
        let mut interval =
            intervals::completed_intervals(&[], std::slice::from_ref(&event)).remove(0);
        interval.known_usd = 1937.2468;
        interval.events = 2;
        interval.unknown_cost_events = 1;
        interval.partial = true;
        app.history = Arc::new(vec![event]);
        app.history_selection = Some(("codex".into(), Some("primary".into()), "week".into()));
        app.set_intervals(Arc::new(vec![interval]));
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.history_ui(ui));
        });
        assert_eq!(app.history_cache.rows[0].1, Some(1937.2468));
        assert!(app.history_cache.rows[0].2);
        app.export_day(false, true);
        app.export_join.take().unwrap().join().unwrap();
        assert!(app
            .export_rx
            .take()
            .unwrap()
            .recv()
            .unwrap()
            .starts_with("已匯出"));
        let mut files = 0;
        for entry in std::fs::read_dir(temp.path().join("exports")).unwrap() {
            let value: serde_json::Value = storage::load_json(&entry.unwrap().path()).unwrap();
            assert_eq!(value["excluded_auto_review_records"], 1);
            assert_eq!(value["date"], day.to_string());
            match value["kind"].as_str().unwrap() {
                "tokens" => {
                    assert_eq!(value["events"].as_array().unwrap().len(), 600);
                    assert!(value["events"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|event| event["model"] != "codex-auto-review"));
                }
                "tokens-excluded" => {
                    assert_eq!(value["events"].as_array().unwrap().len(), 1);
                    assert_eq!(value["events"][0]["model"], "codex-auto-review");
                }
                "quota" => {
                    assert_eq!(value["events"][0]["estimated_cost_usd"], 1937.2468);
                    assert_eq!(value["events"][0]["partial"], true);
                    assert_eq!(value["completed_intervals"][0]["known_usd"], 1937.2468);
                }
                kind => panic!("Unexpected export {kind}"),
            }
            files += 1;
        }
        assert_eq!(files, 3);
    }
    #[test]
    fn initialized_persistent_gui_uses_database_after_lock_decision() {
        let temp = tempfile::tempdir().unwrap();
        let make = || {
            WidgetApp::new(
                Settings {
                    import_enabled: false,
                    ..Default::default()
                },
                temp.path().to_owned(),
                true,
                false,
                true,
                vec![],
            )
        };
        let mut writer = make();
        writer._lock = Some(DataLock::acquire(temp.path()).unwrap());
        writer.database_mode = true;
        writer.initialize();
        for _ in 0..400 {
            writer.pump();
            if !writer.load_pending {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!writer.load_pending, "database load did not complete");
        assert!(
            writer.tokens.database.is_some(),
            "{}",
            writer.import_status_text()
        );
        assert!(writer.persistent);
        assert!(
            writer.bridge.is_none(),
            "offline synthetic test must not connect"
        );
        let mut reader = make();
        reader.persistent = false;
        reader.database_mode = true;
        reader.initialize();
        for _ in 0..400 {
            reader.pump();
            if !reader.load_pending {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!reader.load_pending);
        assert!(
            reader.tokens.database.is_some(),
            "{}",
            reader.import_status_text()
        );
        assert!(!reader.persistent);
        assert!(reader._lock.is_none());
        assert!(reader.bridge.is_none());
        // The remaining reader automatically becomes the writer once the old
        // owner closes, then reopens its SQLite connection with write access.
        let live: HistoryEvent = serde_json::from_value(serde_json::json!({
            "session":"read-only-observer", "observed_at_unix":1700000000,
            "source_key":"codex", "epoch":0, "kind":"QuotaDecrease",
            "window_label":"week", "window_key":"primary",
            "before":"50", "after":"49", "delta":"1", "partial":true
        }))
        .unwrap();
        writer.worker.shutdown().unwrap();
        drop(writer._lock.take());
        reader.load_pending = reader.send_work(WorkCommand::Load {
            token_path: temp.path().join("tokens.json"),
            history_path: Some(temp.path().join("history.json")),
            pricing: reader.settings.pricing.clone(),
        });
        for _ in 0..400 {
            reader.pump();
            if !reader.load_pending {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            DataLock::acquire(temp.path()).is_err(),
            "immutable reader holds a shared lock"
        );
        reader.history = Arc::new(vec![live.clone()]);
        reader.last_lock_retry = Instant::now() - Duration::from_secs(6);
        reader.retry_write_lock();
        for _ in 0..400 {
            reader.pump();
            if reader._lock.is_some() && !reader.load_pending {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(reader._lock.is_some());
        assert!(reader.persistent);
        assert!(!reader.load_pending);
        assert!(reader.load_error.is_none(), "{:?}", reader.load_error);
        assert!(reader.tokens.database.is_some());
        let logs = temp.path().join("logs");
        std::fs::create_dir(&logs).unwrap();
        std::fs::write(logs.join("synthetic.jsonl"), format!("{}\n", serde_json::json!({
            "timestamp": now(), "type":"token_usage_record", "payload": {
                "thread_id":"writer-recovery", "turn_id":"turn", "response_id":"response",
                "model":"gpt-6.1-sol", "usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10}
            }
        }))).unwrap();
        reader.settings.sessions_path = logs.to_string_lossy().into();
        reader.settings.import_enabled = true;
        reader.start_import();
        for _ in 0..400 {
            reader.pump();
            if !reader.import_pending {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(reader.import_error.is_none(), "{:?}", reader.import_error);
        assert_eq!(reader.included_count(), 1);
        reader.worker.shutdown().unwrap();
        let history_path = temp.path().join("history.json");
        assert!(!history_path.exists(), "quota history must stay in SQLite");
        let quota_db = quota_widget::token_db::TokenDb::open_readonly(
            &quota_widget::token_db::TokenDb::database_path(&temp.path().join("tokens.json")),
        )
        .unwrap();
        assert!(quota_db
            .history_events()
            .unwrap()
            .iter()
            .any(|event| event.observed_at_unix == live.observed_at_unix));
    }
    #[test]
    fn long_context_export_labels_thresholds_without_changing_costs() {
        let pricing = Pricing::official_standard();
        for (input, expected) in [(271_999, false), (272_000, false), (272_001, true)] {
            let cost = pricing.cost("gpt-6.1-sol", input - 200_000, 200_000, 100);
            let mut value = serde_json::json!({
                "model":"gpt-6.1-sol", "input":input, "cached":200_000,
                "uncached":input - 200_000, "output":100,
                "source_kind":"token_usage_record", "response_id":"r", "cost_usd":cost,
                "partial_flags":[], "turn_input":999_999
            });
            let original_cost = value["cost_usd"].clone();
            annotate_long_context_export(&mut value, &pricing);
            assert_eq!(value["long_context_adjustment_enabled"], true);
            assert_eq!(value["long_context_adjustment_applied"], expected);
            assert_eq!(
                value["long_context_adjustment"], true,
                "legacy field stays a configuration alias"
            );
            assert_eq!(
                value["long_context_adjustment_legacy_semantics"],
                "deprecated_alias_of_long_context_adjustment_enabled"
            );
            assert_eq!(value["cost_usd"], original_cost);
            let mut disabled = pricing.clone();
            disabled.apply_long_context = false;
            assert_eq!(
                exported_long_context_applied(&value, &disabled),
                Some(false)
            );
        }
        let mut unknown = serde_json::json!({
            "model":"gpt-6.1-sol", "input":300_000, "source_kind":"token_usage_record",
            "cost_usd":null, "partial_flags":["input_count_unavailable"]
        });
        annotate_long_context_export(&mut unknown, &pricing);
        assert!(unknown["long_context_adjustment_applied"].is_null());
        assert!(unknown["cost_usd"].is_null());
        let mut missing = unknown.clone();
        missing.as_object_mut().unwrap().remove("input");
        assert_eq!(exported_long_context_applied(&missing, &pricing), None);
        let legacy = serde_json::json!({"model":"gpt-6.1-sol", "input":500_000, "source_kind":"token_count", "cost_usd":2.0});
        assert_eq!(exported_long_context_applied(&legacy, &pricing), None);
        let unsupported = serde_json::json!({"model":"codex-auto-review", "input":500_000, "source_kind":"token_usage_record", "cost_usd":null});
        assert_eq!(
            exported_long_context_applied(&unsupported, &pricing),
            Some(false)
        );
    }

    #[test]
    fn actual_json_export_separates_enabled_and_applied_long_context_fields() {
        let temp = tempfile::tempdir().unwrap();
        let pricing = Pricing::official_standard();
        let t = now();
        let mut events = Vec::new();
        for (index, input) in [271_999, 272_000, 272_001, 0].into_iter().enumerate() {
            events.push(TokenEvent {
                id: format!("r-{index}"),
                timestamp: t,
                coverage_start_timestamp_ms: None,
                model: "gpt-6.1-sol".into(),
                model_assumed: false,
                reasoning_effort: None,
                session: "synthetic-export".into(),
                response_id: Some(format!("r-{index}")),
                turn_id: Some("same-turn".into()),
                source_kind: Some("token_usage_record".into()),
                input,
                cached: 0,
                uncached: input,
                output: 100,
                reasoning: 50,
                cost_usd: if index == 3 {
                    None
                } else {
                    pricing.cost("gpt-6.1-sol", input, 0, 100)
                },
                partial_flags: if index == 3 {
                    vec!["input_count_unavailable".into()]
                } else {
                    vec![]
                },
            });
        }
        let original_costs: Vec<_> = events.iter().map(|event| event.cost_usd).collect();
        let mut app = WidgetApp::new(
            Settings::default(),
            temp.path().to_owned(),
            true,
            false,
            true,
            vec![],
        );
        app.token_date = local_day(t).unwrap();
        app.tokens = Arc::new(Snapshot {
            events,
            pricing,
            ..Default::default()
        });
        app.export_day(true, true);
        app.export_join.take().unwrap().join().unwrap();
        assert!(app
            .export_rx
            .take()
            .unwrap()
            .recv()
            .unwrap()
            .starts_with("已匯出"));
        let path = std::fs::read_dir(temp.path().join("exports"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let value: serde_json::Value = storage::load_json(&path).unwrap();
        assert_eq!(value["long_context_adjustment_enabled"], true);
        for (index, applied) in [Some(false), Some(false), Some(true), None]
            .into_iter()
            .enumerate()
        {
            let event = &value["events"][index];
            assert_eq!(
                event["long_context_adjustment_applied"],
                serde_json::json!(applied)
            );
            assert_eq!(event["long_context_adjustment_enabled"], true);
            assert_eq!(event["cost_usd"], serde_json::json!(original_costs[index]));
        }
    }
    #[test]
    fn same_layout_key_retries_height_after_native_minimum_clamp() {
        let mut layout = LayoutConvergence::default();
        let t = Instant::now();
        let old = (4, 10, 1188);
        let new = (4, 10, 430);
        layout.plan(old, Vec2::new(1188., 812.), 1400., false, t);
        let first = layout.plan(
            new,
            Vec2::new(430., 812.),
            510.,
            false,
            t + Duration::from_secs(1),
        );
        assert_eq!(
            first.minimum,
            Some(Vec2::new(280., COLLAPSED_HEIGHT * 430. / 310.))
        );
        assert!(
            first.size.is_none(),
            "apply the new minimum before resizing"
        );
        let second = layout.plan(
            new,
            Vec2::new(430., 812.),
            510.,
            false,
            t + Duration::from_millis(1151),
        );
        assert!(
            second.minimum.is_none(),
            "same key does not resend the constraint"
        );
        assert_eq!(second.size, Some(Vec2::new(430., 510.)));
        let early = layout.plan(
            new,
            Vec2::new(430., 812.),
            510.,
            false,
            t + Duration::from_millis(1200),
        );
        assert!(early.size.is_none(), "backoff prevents resize storms");
        let retry = layout.plan(
            new,
            Vec2::new(430., 812.),
            510.,
            false,
            t + Duration::from_millis(1352),
        );
        assert_eq!(
            retry.size,
            Some(Vec2::new(430., 510.)),
            "retry despite unchanged rows/width key"
        );
        let settled = layout.plan(
            new,
            Vec2::new(430., 510.),
            510.,
            false,
            t + Duration::from_millis(1400),
        );
        assert!(settled.size.is_none());
        assert!(settled.repaint_after.is_none());
    }
    #[test]
    fn layout_waits_for_user_drag_and_changing_geometry_to_settle() {
        let mut layout = LayoutConvergence::default();
        let t = Instant::now();
        let key = (4, 10, 430);
        for millis in [0, 500, 1000] {
            assert!(layout
                .plan(
                    key,
                    Vec2::new(430., 812.),
                    510.,
                    true,
                    t + Duration::from_millis(millis)
                )
                .size
                .is_none());
        }
        assert!(layout
            .plan(
                key,
                Vec2::new(430., 812.),
                510.,
                false,
                t + Duration::from_millis(1100)
            )
            .size
            .is_none());
        assert_eq!(
            layout
                .plan(
                    key,
                    Vec2::new(430., 812.),
                    510.,
                    false,
                    t + Duration::from_millis(1151)
                )
                .size,
            Some(Vec2::new(430., 510.))
        );
        // Native keyboard/edge resizing can omit pointer events. Geometry alone
        // must postpone corrective requests until the resize stops.
        for (millis, height) in [(1200, 800.), (1300, 790.), (1400, 780.)] {
            assert!(layout
                .plan(
                    key,
                    Vec2::new(430., height),
                    510.,
                    false,
                    t + Duration::from_millis(millis)
                )
                .size
                .is_none());
        }
        assert_eq!(
            layout
                .plan(
                    key,
                    Vec2::new(430., 780.),
                    510.,
                    false,
                    t + Duration::from_millis(1551)
                )
                .size,
            Some(Vec2::new(430., 510.))
        );
    }
    #[test]
    fn layout_retries_are_bounded_and_new_user_size_restarts_convergence() {
        let mut layout = LayoutConvergence::default();
        let t = Instant::now();
        let key = (4, 10, 430);
        layout.plan(key, Vec2::new(430., 812.), 510., false, t);
        for attempt in 0..LayoutConvergence::MAX_ATTEMPTS {
            assert!(layout
                .plan(
                    key,
                    Vec2::new(430., 812.),
                    510.,
                    false,
                    t + Duration::from_secs(1 + u64::from(attempt) * 2)
                )
                .size
                .is_some());
        }
        let exhausted = layout.plan(
            key,
            Vec2::new(430., 812.),
            510.,
            false,
            t + Duration::from_secs(100),
        );
        assert!(exhausted.size.is_none());
        assert!(exhausted.repaint_after.is_none());
        let new = (4, 10, 450);
        assert!(layout
            .plan(
                new,
                Vec2::new(450., 812.),
                530.,
                false,
                t + Duration::from_secs(101)
            )
            .minimum
            .is_some());
        assert_eq!(
            layout
                .plan(
                    new,
                    Vec2::new(450., 812.),
                    530.,
                    false,
                    t + Duration::from_secs(102)
                )
                .size,
            Some(Vec2::new(450., 530.))
        );
        layout.plan(
            new,
            Vec2::new(450., 530.),
            530.,
            false,
            t + Duration::from_secs(103),
        );
        layout.plan(
            new,
            Vec2::new(450., 700.),
            530.,
            false,
            t + Duration::from_secs(104),
        );
        assert!(
            layout
                .plan(
                    new,
                    Vec2::new(450., 700.),
                    530.,
                    false,
                    t + Duration::from_secs(105)
                )
                .size
                .is_none(),
            "manual height is preserved after initial convergence"
        );
    }
}

#[cfg(test)]
mod quota_connection_recovery_tests {
    use super::*;
    use quota_widget::bridge::BridgeError;
    fn app(root: &std::path::Path) -> WidgetApp {
        let settings = Settings {
            codex_path: root
                .join("missing-synthetic-codex")
                .to_string_lossy()
                .into(),
            sessions_path: root.join("synthetic-logs").to_string_lossy().into(),
            import_enabled: false,
            ..Settings::default()
        };
        WidgetApp::new(settings, root.into(), false, false, true, vec![])
    }
    #[test]
    fn transient_quota_failure_retries_after_delay_but_manual_offline_does_not() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(root.path());
        app.offline = false;
        app.handle_bridge_event(BridgeEvent::Error(BridgeError::Transport));
        app.handle_bridge_event(BridgeEvent::Stopped);
        let due = app.quota_retry_at.unwrap();
        app.retry_quota_connection(due - Duration::from_secs(1));
        assert!(app.bridge.is_none());
        app.offline = true;
        app.retry_quota_connection(due);
        assert!(app.bridge.is_none());
        app.offline = false;
        app.retry_quota_connection(due);
        assert!(app.bridge.is_some());
        assert!(app.quota_retry_at.is_none());
        app.bridge = None;
        app.handle_bridge_event(BridgeEvent::Error(BridgeError::NotFound));
        assert!(app.quota_retry_at.is_none());
    }

    #[test]
    fn quota_refresh_reconnects_a_dead_sender_instead_of_silently_ignoring_it() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(root.path());
        app.connect();
        let deadline = Instant::now() + Duration::from_secs(3);
        while app
            .bridge
            .as_ref()
            .unwrap()
            .commands
            .send(BridgeCommand::Refresh)
            .is_ok()
        {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        app.status = "SYNTHETIC DEAD BRIDGE".into();
        app.quota_error = Some("synthetic failed connection".into());
        app.refresh_quota();
        assert_eq!(app.status, "CONNECTING");
        assert!(app.bridge.is_some());
        assert!(!app.offline);
        assert!(app.quota_error.is_none());
    }
    #[test]
    fn quota_stopped_clears_the_bridge_and_refresh_starts_a_new_attempt() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(root.path());
        app.connect();
        let deadline = Instant::now() + Duration::from_secs(3);
        while app.bridge.is_some() {
            app.pump();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(app.status, "OFFLINE");
        assert!(app.quota_error.is_some());
        app.refresh_quota();
        assert!(app.bridge.is_some());
        assert_eq!(app.status, "CONNECTING");
    }
    #[test]
    fn quota_failure_remains_visible_during_token_scan_and_unrelated_notice() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(root.path());
        app.handle_bridge_event(BridgeEvent::Error(BridgeError::NotFound));
        app.handle_bridge_event(BridgeEvent::Stopped);
        app.work_progress = Some((
            WorkProgress {
                operation: Operation::Import,
                stage: ProgressStage::Import(ImportPhase::Reading),
                elapsed: Duration::from_secs(9),
                files_done: 3237,
                files_total: 3257,
                lines_read: 10,
                observations: 1,
                events: 1,
            },
            Instant::now(),
        ));
        app.notice = "another storage notice".into();
        assert!(app.compact_progress_text().unwrap().contains("3237/3257"));
        assert!(app
            .compact_status_text()
            .unwrap()
            .contains("額度連線有問題"));
        assert!(app
            .quota_diagnostic_text()
            .contains("Standalone Codex CLI not found"));
        assert!(!app
            .quota_diagnostic_text()
            .contains("another storage notice"));
    }
    #[test]
    fn quota_reading_arrives_while_token_load_and_scan_are_pending() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(root.path());
        app.import_pending = true;
        app.load_pending = true;
        app.quota_error = Some("previous error".into());
        app.handle_bridge_event(BridgeEvent::RateLimits(serde_json::json!({
            "rateLimits": {"limitId":"codex", "primary": {
                "usedPercent":14, "windowDurationMins":10080, "resetsAt":now()+86400
            }}
        })));
        assert_eq!(app.status, "LIVE");
        assert_eq!(app.selected().unwrap().remaining_percent, Some(86.));
        assert!(app.quota_error.is_none());
        assert!(app.import_pending && app.load_pending);
    }
    #[test]
    fn demo_refresh_does_not_start_cli_or_change_offline_choice() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(root.path());
        app.demo = true;
        app.refresh_quota();
        assert!(app.bridge.is_none());
        assert!(app.offline);
    }
}

#[cfg(test)]
mod responsive_close_tests {
    use super::*;
    #[test]
    fn reset_alert_ignores_initial_snapshot_rebound_reconnect_and_repeated_full_quota() {
        let mut quota = QuotaModel::new("alert-test");
        for (at, used, expected) in [
            (100, 0., false),
            (130, 1., false),
            (160, 0., false),
            (190, 2., false),
            (220, 0., true),
            (250, 0., false),
        ] {
            quota.history.clear();
            quota.ingest_json(&serde_json::json!({"rateLimits":{"limitId":"codex","primary":{"usedPercent":used,"windowDurationMins":10080,"resetsAt":100000}}}), at).unwrap();
            let mut view = quota.views(at).into_iter().next().unwrap();
            assert_eq!(
                live_reset_alert(&quota.history, &view),
                expected,
                "used {used} at {at}"
            );
            view.source_key = "different-source".into();
            assert!(!live_reset_alert(&quota.history, &view));
        }
        quota.reset_continuity();
        quota.history.clear();
        quota.ingest_json(&serde_json::json!({"rateLimits":{"limitId":"codex","primary":{"usedPercent":0,"windowDurationMins":10080,"resetsAt":100000}}}), 280).unwrap();
        assert!(!live_reset_alert(&quota.history, &quota.views(280)[0]));
    }
    #[test]
    fn detail_window_positions_remain_independent_and_persist() {
        let mut app = super::widget_revision_tests::demo_app();
        let ctx = egui::Context::default();
        for (history, name, x) in [
            (true, "quota-history", 120.),
            (false, "token-history", 850.),
        ] {
            let id = ViewportId::from_hash_of(name);
            let mut input = egui::RawInput {
                viewport_id: id,
                ..Default::default()
            };
            input.viewports.insert(
                id,
                egui::ViewportInfo {
                    outer_rect: Some(Rect::from_min_size(
                        Pos2::new(x, 220.),
                        Vec2::new(1120., 760.),
                    )),
                    ..Default::default()
                },
            );
            let _ = ctx.run(input, |ctx| app.remember_detail_position(ctx, history));
        }
        assert!(app.dirty);
        let saved: Settings =
            serde_json::from_str(&serde_json::to_string(&app.settings).unwrap()).unwrap();
        assert_eq!(saved.history_position, Some([120., 220.]));
        assert_eq!(saved.tokens_position, Some([850., 220.]));
        assert_eq!(saved.position, app.settings.position);
    }
    #[test]
    fn reset_clears_prediction_even_with_sound_disabled_and_rebound_does_not() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(root.path());
        app.settings.reset_sound_enabled = false;
        app.settings.reset_prediction_utc = Some(now() + 1000);
        app.settings.reset_prediction_set_at = Some(now() - 10);
        let payload = |used| serde_json::json!({"rateLimits":{"limitId":"codex","primary":{"usedPercent":used,"windowDurationMins":10080,"resetsAt":now()+604800}}});
        app.quota.ingest_json(&payload(1.), now() - 30).unwrap();
        app.quota.history.clear();
        app.handle_bridge_event(BridgeEvent::RateLimits(payload(0.)));
        assert!(app.settings.prediction_target().is_some());
        app.quota = QuotaModel::new("second-cycle-test");
        app.quota.ingest_json(&payload(3.), now() - 1).unwrap();
        app.quota.history.clear();
        app.handle_bridge_event(BridgeEvent::RateLimits(payload(0.)));
        assert!(app.settings.prediction_target().is_none());
        assert!(app.dirty);
    }
    #[test]
    fn saved_prediction_clears_after_restart_from_confirmed_reset_history() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(root.path());
        app.settings.reset_prediction_utc = Some(now() + 500);
        app.settings.reset_prediction_set_at = Some(now() - 100);
        app.quota.ingest_json(&serde_json::json!({"rateLimits":{"limitId":"codex","primary":{"usedPercent":5,"windowDurationMins":10080,"resetsAt":now()+600000}}}), now()-50).unwrap();
        app.quota.ingest_json(&serde_json::json!({"rateLimits":{"limitId":"codex","primary":{"usedPercent":0,"windowDurationMins":10080,"resetsAt":now()+604800}}}), now()-10).unwrap();
        app.history = Arc::new(app.quota.history.clone());
        app.clear_completed_prediction();
        assert!(app.settings.prediction_target().is_none());
        app.settings.reset_prediction_utc = Some(now() + 1000);
        app.settings.reset_prediction_set_at = Some(now());
        app.clear_completed_prediction();
        assert!(
            app.settings.prediction_target().is_some(),
            "old reset must not delete a new prediction"
        );
    }
    #[test]
    fn settings_native_viewport_opens_front_and_reopens_with_content() {
        use std::cell::RefCell;
        use std::rc::Rc;
        let captured = Rc::new(RefCell::new(Vec::new()));
        let output = Rc::clone(&captured);
        egui::Context::set_immediate_viewport_renderer(move |ctx, mut viewport| {
            let mut input = egui::RawInput {
                viewport_id: viewport.ids.this,
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(760., 700.))),
                ..Default::default()
            };
            input.viewports.insert(
                viewport.ids.this,
                egui::ViewportInfo {
                    outer_rect: Some(Rect::from_min_size(
                        Pos2::new(430., 180.),
                        Vec2::new(760., 700.),
                    )),
                    ..Default::default()
                },
            );
            let rendered = ctx.run(input, |ctx| (viewport.viewport_ui_cb)(ctx));
            let text = rendered
                .shapes
                .iter()
                .filter_map(|s| {
                    if let egui::Shape::Text(t) = &s.shape {
                        Some(t.galley.job.text.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            output.borrow_mut().push((viewport.builder, text));
        });
        let root = tempfile::tempdir().unwrap();
        let mut app = app(root.path());
        let ctx = egui::Context::default();
        ctx.set_embed_viewports(false);
        app.apply_theme(&ctx);
        for attempt in 0..2 {
            app.toggle_settings();
            let _ = ctx.run(Default::default(), |ctx| app.detail_windows(ctx));
            assert!(app.settings_open);
            let rows = captured.borrow();
            let (builder, text) = rows.last().unwrap();
            assert_eq!(builder.visible, Some(true));
            assert_eq!(builder.active, Some(true));
            assert_eq!(builder.window_level, Some(WindowLevel::AlwaysOnTop));
            assert!(text.contains("顯示時區"), "{text}");
            assert!(text.contains("Reset預測"), "{text}");
            if attempt == 1 {
                assert_eq!(builder.position, Some(Pos2::new(430., 180.)));
            }
            assert_eq!(app.settings.settings_position, Some([430., 180.]));
            let saved: Settings =
                serde_json::from_str(&serde_json::to_string(&app.settings).unwrap()).unwrap();
            assert_eq!(saved.settings_position, Some([430., 180.]));
            app.toggle_settings();
            assert!(!app.settings_open);
        }
    }
    #[test]
    fn prediction_dialog_remembers_position_and_sizes_to_content() {
        use std::cell::RefCell;
        use std::rc::Rc;
        let captured = Rc::new(RefCell::new(Vec::new()));
        let output = Rc::clone(&captured);
        egui::Context::set_immediate_viewport_renderer(move |ctx, mut viewport| {
            let mut input = egui::RawInput {
                viewport_id: viewport.ids.this,
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(350., 230.))),
                ..Default::default()
            };
            input.viewports.insert(
                viewport.ids.this,
                egui::ViewportInfo {
                    outer_rect: Some(Rect::from_min_size(
                        Pos2::new(410., 210.),
                        Vec2::new(350., 230.),
                    )),
                    ..Default::default()
                },
            );
            let _ = ctx.run(input, |ctx| (viewport.viewport_ui_cb)(ctx));
            assert_eq!(viewport.builder.inner_size, Some(Vec2::new(350., 150.)));
            assert_eq!(viewport.builder.resizable, Some(false));
            output.borrow_mut().push(viewport.builder);
        });
        let root = tempfile::tempdir().unwrap();
        let mut app = app(root.path());
        let ctx = egui::Context::default();
        ctx.set_embed_viewports(false);
        app.apply_theme(&ctx);
        for attempt in 0..2 {
            app.open_prediction();
            let _ = ctx.run(Default::default(), |ctx| app.detail_windows(ctx));
            assert_eq!(app.settings.prediction_position, Some([410., 210.]));
            if attempt == 1 {
                assert_eq!(
                    captured.borrow().last().unwrap().position,
                    Some(Pos2::new(410., 210.))
                );
            }
            let saved: Settings =
                serde_json::from_str(&serde_json::to_string(&app.settings).unwrap()).unwrap();
            assert_eq!(saved.prediction_position, Some([410., 210.]));
            app.open_prediction();
            assert!(!app.prediction_open);
        }
    }
    #[test]
    fn reminder_uses_user_time_at_utc_plus_eight_and_counts_elapsed_after_zero() {
        let target = reset_reminder_at("2030-04-02 01:00").unwrap();
        assert_eq!(
            chrono::DateTime::from_timestamp(target, 0)
                .unwrap()
                .to_rfc3339(),
            "2030-04-01T17:00:00+00:00"
        );
        assert_eq!(reset_reminder_countdown(target, target - 3660), "1:01:00");
        assert_eq!(reset_reminder_countdown(target, target - 1), "0:00:01");
        assert_eq!(reset_reminder_countdown(target, target), "0:00:00");
        assert_eq!(reset_reminder_countdown(target, target + 90), ">0:01:30");
        assert_eq!(reset_reminder_countdown(target, target + 3661), ">1:01:01");
        assert!(reset_reminder_at("").is_none());
        assert!(reset_reminder_at("2030-02-30 01:00").is_none());
        assert!(Settings::default().reset_reminder_local.is_empty());
    }
    fn app(root: &std::path::Path) -> WidgetApp {
        WidgetApp::new(
            Settings {
                import_enabled: false,
                sessions_path: root.join("synthetic-logs").display().to_string(),
                codex_path: root.join("missing-synthetic-codex").display().to_string(),
                ..Settings::default()
            },
            root.into(),
            true,
            false,
            true,
            vec![],
        )
    }
    #[test]
    fn close_request_does_not_join_an_active_export_and_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(root.path());
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let token = CancellationToken::new();
        app.export_cancel = Some(token.clone());
        app.export_join = Some(std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
        ready_rx.recv().unwrap();
        let start = Instant::now();
        app.begin_close();
        app.begin_close();
        assert!(start.elapsed() < Duration::from_millis(200));
        assert!(token.is_cancelled());
        assert!(!app.close_finished());
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !app.close_finished() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(app.closing_since.is_some());
    }
    #[test]
    fn close_flushes_final_settings_and_keeps_writer_lock_until_finished() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(root.path());
        app._lock = Some(DataLock::acquire(root.path()).unwrap());
        app.settings.dark = false;
        app.dirty = true;
        app.begin_close();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !app.close_finished() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(DataLock::acquire(root.path()).is_err());
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.path().join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(saved["dark"], false);
        drop(app);
        assert!(DataLock::acquire(root.path()).is_ok());
    }
    #[test]
    fn cancelled_export_leaves_no_partial_target_or_temp_file() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("day.csv");
        let token = CancellationToken::new();
        let result = token.scope(|| {
            save_export(&target, |writer| {
                writer.write_all(b"first complete row\n")?;
                token.cancel();
                writer.write_all(b"must not publish\n")
            })
        });
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Interrupted);
        assert!(!target.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
    #[test]
    fn complete_export_never_replaces_an_existing_export() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("day.csv");
        std::fs::write(&target, b"original").unwrap();
        let result = save_export(&target, |writer| writer.write_all(b"replacement"));
        assert!(result.is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}

#[cfg(test)]
mod accessible_ui_tests {
    use super::*;

    #[test]
    fn languages_render_in_the_compact_widget_and_settings() {
        for (language, title, settings_label) in [
            (Language::Traditional, "Codex 額度 v3.37", "介面字體大小"),
            (Language::Simplified, "Codex 额度 v3.37", "界面字体大小"),
            (
                Language::English,
                "Codex Quota v3.37",
                "Interface font size",
            ),
        ] {
            let mut app = super::widget_revision_tests::demo_app();
            app.settings.language = language;
            set_language(language);
            let ctx = egui::Context::default();
            let input = || egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(540., 1600.))),
                ..Default::default()
            };
            let output = ctx.run(input(), |ctx| app.widget(ctx));
            let text = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some(text.galley.job.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains(title), "{language:?}: {text}");
            if language == Language::English {
                assert!(
                    !text.chars().any(|c| ('\u{3400}'..='\u{9fff}').contains(&c)),
                    "{text}"
                );
            }
            let output = ctx.run(input(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.settings_ui(ui, ctx));
            });
            let text = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some(text.galley.job.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains(settings_label), "{language:?}: {text}");
        }
        set_language(Language::Traditional);
    }

    #[test]
    fn expanded_content_uses_window_height_instead_of_row_threshold() {
        assert_eq!(widget_target_height(310., 0., Some(1080.)), 158.);
        assert_eq!(widget_target_height(310., 300., Some(1080.)), 458.);
        assert_eq!(widget_target_height(310., 2000., Some(768.)), 708.);
        let mut settings = Settings {
            size: [450., 1000.],
            compact_layout_version: 0,
            ..Default::default()
        };
        prepare_widget_settings(&mut settings);
        assert_eq!(settings.size, [300., 158. * 300. / 310.]);
        prepare_widget_settings(&mut settings);
        assert_eq!(settings.size[0], 300., "narrowing is a one-time migration");
    }

    #[test]
    fn root_painted_text_never_smaller_or_dimmer_than_rate_label() {
        for dark in [false, true] {
            for width in [430., 540., 700.] {
                let mut app = super::widget_revision_tests::demo_app();
                app.tasks_expanded = true;
                app.settings.dark = dark;
                app.settings.size[0] = width;
                let ctx = egui::Context::default();
                app.apply_theme(&ctx);
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(width, 1800.))),
                        ..Default::default()
                    },
                    |ctx| app.widget(ctx),
                );
                let minimum = accessibility::minimum_font(width);
                let expected = accessibility::palette(dark).text;
                let mut count = 0;
                for clipped in output.shapes {
                    if let egui::Shape::Text(text) = clipped.shape {
                        for section in &text.galley.job.sections {
                            assert!(
                                section.format.font_id.size >= minimum - 0.01,
                                "{}",
                                text.galley.job.text
                            );
                            assert_eq!(section.format.color, expected, "{}", text.galley.job.text);
                            count += 1;
                        }
                    }
                }
                assert!(count > 15);
            }
        }
    }

    #[test]
    fn details_scroll_only_when_the_window_cannot_fit_all_rows() {
        let mut app = super::widget_revision_tests::demo_app();
        app.tasks_expanded = true;
        app.settings.task_visible_rows = 1; // An obsolete preference must not truncate the list.
        app.refresh_main_cache();
        app.main_cache.tree = (0..30)
            .map(|n| usage_views::TaskTreeRow::SelfTask {
                task_id: format!("row-{n:02}"),
                task_index: None,
                depth: 0,
                relationship: usage_views::TaskRelationship::Unlinked,
            })
            .collect();
        let ctx = egui::Context::default();
        let frame = |app: &mut WidgetApp, height, events, time| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(310., height))),
                    events,
                    time: Some(time),
                    ..Default::default()
                },
                |ctx| app.widget(ctx),
            )
        };
        let visible = |output: egui::FullOutput| {
            output
                .shapes
                .into_iter()
                .filter_map(|shape| {
                    if let egui::Shape::Text(t) = shape.shape {
                        let bounds = t.galley.rect.translate(t.pos.to_vec2());
                        if t.galley.job.text.starts_with("row-")
                            && bounds.intersects(shape.clip_rect)
                        {
                            return Some(t.galley.job.text.clone());
                        }
                    }
                    None
                })
                .collect::<Vec<_>>()
        };
        let full = visible(frame(&mut app, 1200., vec![], 0.));
        assert_eq!(full.len(), 30, "the old one-row preference is ignored");
        let small = visible(frame(&mut app, 400., vec![], 0.1));
        assert!(small.len() < 30 && small.contains(&"row-00".into()));
        let mut end = Vec::new();
        for time in [0.2, 0.4, 0.8] {
            end = visible(frame(
                &mut app,
                400.,
                vec![
                    egui::Event::PointerMoved(Pos2::new(150., 300.)),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: Vec2::new(0., -1000.),
                        modifiers: Default::default(),
                    },
                ],
                time,
            ));
        }
        assert!(
            end.contains(&"row-29".into()),
            "the last row remains reachable by scrolling: {end:?}"
        );
    }

    #[test]
    fn compact_expanders_are_independent_and_progress_stays_in_details() {
        let mut app = super::widget_revision_tests::demo_app();
        let ctx = egui::Context::default();
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(310., 900.));
        let frame = |app: &mut WidgetApp, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(rect),
                    events,
                    ..Default::default()
                },
                |ctx| app.widget(ctx),
            )
        };
        let text = |output: egui::FullOutput| {
            output
                .shapes
                .into_iter()
                .filter_map(|shape| {
                    if let egui::Shape::Text(t) = shape.shape {
                        Some(t.galley.job.text.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let collapsed = text(frame(&mut app, vec![]));
        assert!(!app.tasks_expanded && !app.recent_expanded);
        for hidden in ["對話", "最近 5 次扣額", "正在讀取", "扣額紀錄", "Token明細"]
        {
            assert!(!collapsed.contains(hidden), "{hidden}");
        }
        let click = |app: &mut WidgetApp, pos| {
            frame(app, vec![egui::Event::PointerMoved(pos)]);
            frame(
                app,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Default::default(),
                }],
            );
            frame(
                app,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Default::default(),
                }],
            )
        };
        let tasks = text(click(&mut app, Pos2::new(207., 129.)));
        assert!(app.tasks_expanded && !app.recent_expanded);
        assert!(tasks.contains("對話"));
        click(&mut app, Pos2::new(265., 129.));
        let both = text(frame(&mut app, vec![]));
        assert!(app.tasks_expanded && app.recent_expanded);
        let recent_heading = format!("最近 {} 次扣額", app.main_cache.recent_events.len());
        assert!(both.contains(&recent_heading), "{both}");
        click(&mut app, Pos2::new(207., 129.));
        assert!(!app.tasks_expanded && app.recent_expanded);
        click(&mut app, Pos2::new(198., 22.));
        assert!(app.history_open);
        click(&mut app, Pos2::new(226., 22.));
        assert!(app.tokens_open);
        click(&mut app, Pos2::new(198., 22.));
        assert!(!app.history_open && app.tokens_open);
        click(&mut app, Pos2::new(226., 22.));
        assert!(!app.tokens_open);
        click(&mut app, Pos2::new(254., 22.));
        assert!(app.settings_open);
        click(&mut app, Pos2::new(254., 22.));
        assert!(!app.settings_open);
    }

    #[test]
    fn narrow_task_tooltip_wraps_inside_native_viewport() {
        let mut app = super::widget_revision_tests::demo_app();
        app.tasks_expanded = true;
        app.settings.size[0] = 430.;
        let ctx = egui::Context::default();
        app.apply_theme(&ctx);
        let mut found = false;
        for time in [0., 0.1, 1., 2., 3.] {
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(430., 510.))),
                    time: Some(time),
                    events: if time == 0. {
                        vec![egui::Event::PointerMoved(Pos2::new(60., 264.))]
                    } else {
                        Vec::new()
                    },
                    ..Default::default()
                },
                |ctx| app.widget(ctx),
            );
            for clipped in output.shapes {
                if let egui::Shape::Text(text) = clipped.shape {
                    if text.galley.job.text.contains("Turn Input:") {
                        found = true;
                        let bounds = text.galley.rect.translate(text.pos.to_vec2());
                        assert!(bounds.left() >= 0. && bounds.right() <= 430., "{bounds:?}");
                        assert!(text
                            .galley
                            .job
                            .sections
                            .iter()
                            .all(|s| s.format.font_id.size >= 18.));
                        for label in [
                            "Input:",
                            "Cached:",
                            "Uncached:",
                            "Output:",
                            "Reasoning:",
                            "Turn Input:",
                            "Turn Output:",
                        ] {
                            assert!(text.galley.job.text.contains(label));
                        }
                    }
                }
            }
        }
        assert!(found);
    }

    #[test]
    fn recent_hover_contains_values_and_compact_coverage_only() {
        let app = super::widget_revision_tests::demo_app();
        let text = &app.main_cache.recent_tooltip;
        assert!(text.contains("估算 USD"));
        assert!(text.contains("部分"));
        assert!(!text.contains("不可比較"));
        assert!(!text.contains("只列真實"));
        assert!(!text.contains("點擊可"));
    }
}

#[cfg(test)]
mod recovered_history_display_tests {
    use super::*;
    #[test]
    fn recovered_decreases_reach_preview_and_table_without_inventing_costs() {
        let row = |t: i64, b: i64, a: i64, reset: i64, account: Option<&str>| -> HistoryEvent {
            serde_json::from_value(serde_json::json!({
                "session":if account.is_some(){"live"}else{"imported-local-observations-v2"},
                "observed_at_unix":t,"source_key":"codex","window_key":"primary",
                "window_label":"week","kind":"QuotaDecrease","epoch":0,
                "before":b.to_string(),"after":a.to_string(),"delta":(b-a).to_string(),
                "before_observed_at_unix":t-1,"partial":false,
                "evidence":{"imported":account.is_none(),"account_id":account,
                    "reset_at":reset,"before_reset_at":reset}
            }))
            .unwrap()
        };
        let mut raw: Vec<_> = (0..13)
            .map(|i| row(10 + i, 13 - i, 12 - i, 1000, None))
            .collect();
        raw.extend((0..8).map(|i| row(30 + i * 10, 12 - i, 11 - i, 2000, None)));
        raw.push(row(120, 4, 3, 2000, Some("synthetic-account")));
        let mut app = WidgetApp::new(
            Settings {
                import_enabled: false,
                ..Default::default()
            },
            PathBuf::new(),
            false,
            true,
            true,
            vec![],
        );
        app.history = Arc::new(quota_widget::history::normalized(&raw));
        app.set_intervals(Arc::new(intervals::completed_intervals(&[], &app.history)));
        app.refresh_main_cache();
        for change in ["4% → 3%", "5% → 4%", "6% → 5%", "7% → 6%", "8% → 7%"] {
            assert!(app.main_cache.recent_tooltip.contains(change), "{change}");
        }
        app.date = local_day(120).unwrap();
        let ctx = egui::Context::default();
        for completed in 1..=8 {
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.history_ui(ui));
            });
            assert_eq!(app.history_cache.table_rows.len(), completed);
        }
        let recovered: Vec<_> = app
            .history_cache
            .table_rows
            .iter()
            .filter(|row| row.interval_index.is_none())
            .collect();
        assert_eq!(recovered.len(), 8);
        assert!(recovered.iter().all(|row| row.presentation.total == "未知"));
        assert!(recovered
            .iter()
            .all(|row| row.presentation.status == "區間沒有 Token 記錄"));
        assert_eq!(app.history_cache.interval_indices.len(), 8);
    }
}
