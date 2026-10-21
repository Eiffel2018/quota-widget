use chrono::{DateTime, Local, NaiveDate, Utc};
use eframe::egui::{
    self, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2, ViewportBuilder,
    ViewportCommand, ViewportId, WindowLevel,
};
use quota_widget::{
    accounting,
    background::{Command as WorkCommand, Event as WorkEvent, Snapshot, Worker},
    bridge::{Bridge, BridgeCommand, BridgeEvent},
    intervals::{self, CompletedInterval},
    model::{HistoryEvent, QuotaModel, WindowView},
    storage::{self, DataLock, Settings},
    tokens::{ModelPrice, Pricing, TokenEvent, TokenStore},
    usage_views::{self, GroupBy},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

#[derive(Default)]
struct MainCache {
    key: Option<(u64, u64, String, String, i64, u64)>,
    tree: Vec<usage_views::TaskTreeRow>,
    tasks: Vec<usage_views::TaskRow>,
    boundary: Option<HistoryEvent>,
    cost: accounting::ObservedCost,
}
#[derive(Default)]
struct TokenCache {
    key: Option<(u64, NaiveDate, GroupBy, bool)>,
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
    key: Option<(u64, u64, NaiveDate, String, bool)>,
    rows: Vec<(HistoryEvent, Option<f64>, bool)>,
    weighted: Option<f64>,
}
struct WidgetApp {
    settings: Settings,
    dir: PathBuf,
    worker: Worker,
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
    selected_interval: Option<String>,
    token_generation: u64,
    history_generation: u64,
    expanded_tasks: BTreeSet<String>,
    tree_generation: u64,
    main_cache: MainCache,
    token_cache: TokenCache,
    history_cache: HistoryCache,
    dates_key: Option<(u64, u64, NaiveDate, u32)>,
    dates_cache: BTreeSet<NaiveDate>,
    layout_key: Option<(usize, u32, u32)>,
    bridge: Option<Bridge>,
    status: String,
    history_open: bool,
    history_selection: Option<(String, Option<String>, String)>,
    tokens_open: bool,
    settings_open: bool,
    date: NaiveDate,
    token_date: NaiveDate,
    token_group: GroupBy,
    show_excluded_records: bool,
    import_pending: bool,
    export_rx: Option<mpsc::Receiver<String>>,
    export_join: Option<std::thread::JoinHandle<()>>,
    last_import: Instant,
    last_save: Instant,
    last_topmost: Instant,
    notice: String,
    import_note: String,
    price_model: String,
    price_values: [f64; 3],
    price_configured: bool,
    last_focus: bool,
    last_minimized: bool,
    dirty: bool,
    demo_start: i64,
}
fn now() -> i64 {
    Utc::now().timestamp()
}
fn local_time(t: i64) -> String {
    DateTime::<Utc>::from_timestamp(t, 0)
        .map(|d| d.with_timezone(&Local).format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "—".into())
}
fn local_day(t: i64) -> Option<NaiveDate> {
    DateTime::<Utc>::from_timestamp(t, 0).map(|d| d.with_timezone(&Local).date_naive())
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
    settings.size[0] = settings.size[0].clamp(276., 1000.);
    settings.task_visible_rows = settings.task_visible_rows.clamp(1, 50);
    settings.size[1] = usage_views::widget_height(0, settings.task_visible_rows as usize)
        * settings.size[0]
        / 310.;
    let mut viewport = ViewportBuilder::default()
        .with_inner_size(settings.size)
        .with_min_inner_size([276., 280.])
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
            app._lock = lock;
            if app._lock.is_none() {
                app.persistent = false;
            }
            if let Some(note) = lock_notice {
                app.notice = note;
            }
            app.apply_theme(&cc.egui_ctx);
            app.initialize();
            if corrupt_settings {
                app.persistent = false;
                app.notice = "設定檔無法讀取；已停用寫入，原檔保留。".into();
            }
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
        Self {
            settings,
            dir,
            worker: Worker::spawn(),
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
            selected_interval: None,
            token_generation: 0,
            history_generation: 0,
            expanded_tasks: BTreeSet::new(),
            tree_generation: 0,
            main_cache: MainCache::default(),
            token_cache: TokenCache::default(),
            history_cache: HistoryCache::default(),
            dates_key: None,
            dates_cache: BTreeSet::new(),
            layout_key: None,
            bridge: None,
            status: "OFFLINE".into(),
            history_selection: None,
            history_open: args.iter().any(|s| s == "--show-history"),
            tokens_open: args.iter().any(|s| s == "--show-tokens"),
            settings_open: false,
            date: Local::now().date_naive(),
            token_date: Local::now().date_naive(),
            token_group: GroupBy::Response,
            show_excluded_records: false,
            import_pending: false,
            export_rx: None,
            export_join: None,
            last_import: Instant::now() - Duration::from_secs(60),
            last_save: Instant::now(),
            last_topmost: Instant::now(),
            notice: String::new(),
            import_note: String::new(),
            price_model: "gpt-6.1-sol".into(),
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
        if self.read_state {
            let _ = self.worker.send(WorkCommand::Load {
                token_path: self.dir.join("tokens.json"),
                history_path: Some(self.dir.join("history.json")),
                pricing: self.settings.pricing.clone(),
            });
            self.import_note = "正在背景載入歷史，畫面可繼續操作。".into();
        }
        if !self.offline {
            self.connect();
        }
        if self.settings.import_enabled {
            self.start_import();
        }
    }
    fn save_path(&self, name: &str) -> Option<PathBuf> {
        self.persistent.then(|| self.dir.join(name))
    }
    fn request_reprice(&mut self) {
        if self.demo {
            return;
        }
        let _ = self.worker.send(WorkCommand::Reprice {
            pricing: self.settings.pricing.clone(),
            save_path: self.save_path("tokens.json"),
        });
    }
    fn apply_theme(&self, ctx: &egui::Context) {
        // Pin the saved application preference; set_visuals alone modifies the
        // currently selected style but leaves System free to switch it on frame 1.
        ctx.set_theme(egui::Theme::from_dark_mode(self.settings.dark));
        ctx.set_visuals(if self.settings.dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
    }
    fn connect(&mut self) {
        self.bridge = None;
        self.quota.reset_continuity();
        self.bridge = Some(Bridge::spawn(
            (!self.settings.codex_path.trim().is_empty())
                .then(|| PathBuf::from(self.settings.codex_path.trim())),
        ));
        self.status = "CONNECTING".into();
    }
    fn start_import(&mut self) {
        if self.demo || self.import_pending || !self.settings.import_enabled {
            return;
        }
        let root = PathBuf::from(self.settings.sessions_path.trim());
        // Only the explicitly selected sessions directory's adjacent official index.
        let metadata = (root.file_name().is_some_and(|n| n == "sessions"))
            .then(|| root.parent().map(|p| p.join("session_index.jsonl")))
            .flatten();
        if self
            .worker
            .send(WorkCommand::Import {
                root,
                metadata,
                pricing: self.settings.pricing.clone(),
                save_path: self.save_path("tokens.json"),
                history_save_path: self.save_path("history.json"),
            })
            .is_ok()
        {
            self.import_pending = true;
        }
        self.last_import = Instant::now();
    }
    fn pump(&mut self) {
        let messages: Vec<_> = self
            .bridge
            .as_ref()
            .map(|b| b.events.try_iter().collect())
            .unwrap_or_default();
        for event in messages {
            match event {
                BridgeEvent::Connecting => self.status = "CONNECTING".into(),
                BridgeEvent::Connected => self.status = "CONNECTED · 等待額度".into(),
                BridgeEvent::RateLimits(v) => match self.quota.ingest_json(&v, now()) {
                    Ok(_) => {
                        self.status = "LIVE".into();
                        if !self.quota.history.is_empty() {
                            let events = std::mem::take(&mut self.quota.history);
                            let _ = self.worker.send(WorkCommand::MergeHistory {
                                events,
                                save_path: self.save_path("history.json"),
                            });
                        }
                    }
                    Err(_) => self.status = "資料格式不支援".into(),
                },
                BridgeEvent::AccountChanged => {
                    self.quota.reset_continuity();
                    self.status = "帳戶已變更".into();
                }
                BridgeEvent::Error(e) => {
                    self.status = "OFFLINE".into();
                    self.notice = format!("CLI：{e}");
                    self.quota.reset_continuity();
                }
                BridgeEvent::Stopped => self.status = "OFFLINE".into(),
            }
        }
        while let Ok(event) = self.worker.try_recv() {
            match event {
                WorkEvent::Loaded {
                    snapshot,
                    normalized_history,
                    history_error,
                    error,
                } => {
                    self.tokens = snapshot;
                    self.token_generation += 1;
                    self.history = normalized_history;
                    self.history_generation += 1;
                    if let Some(error) = error.or(history_error) {
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
                        self.token_generation += 1;
                    }
                    if let Some(history) = normalized_history {
                        self.history = history;
                        self.history_generation += 1;
                    }
                    if let Some(report) = report {
                        self.import_pending = false;
                        self.import_note = format!(
                            "已掃描 {} 檔，本輪新增 {} 筆；格式問題 {}。",
                            report.files_scanned, report.events_added, report.malformed_lines
                        );
                    }
                    if let Some(error) = error {
                        self.import_pending = false;
                        self.import_note = format!("背景匯入：{error}");
                    }
                }
                WorkEvent::HistoryUpdated { normalized } => {
                    if let Some(history) = normalized {
                        self.history = history;
                        self.history_generation += 1;
                    }
                }
                WorkEvent::IntervalsUpdated { intervals } => {
                    self.interval_lookup = intervals
                        .iter()
                        .enumerate()
                        .map(|(i, row)| (row.history_event_key.clone(), i))
                        .collect();
                    self.intervals = intervals;
                    self.history_cache.key = None;
                }
                WorkEvent::SaveFailed { path: _, error } => {
                    self.notice = format!("儲存失敗，畫面仍保留資料：{error}");
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
        if !self.demo && !self.import_pending && self.last_import.elapsed() > Duration::from_secs(5)
        {
            self.start_import();
        }
        if self.dirty && self.last_save.elapsed() > Duration::from_secs(5) {
            self.save_state();
        }
    }
    fn save_state(&mut self) {
        if self.persistent
            && self.dirty
            && self
                .worker
                .send(WorkCommand::SaveSettings {
                    path: self.dir.join("settings.json"),
                    settings: self.settings.clone(),
                })
                .is_ok()
        {
            self.dirty = false;
        }
        self.last_save = Instant::now();
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
            .find(|e| {
                e.source_key == self.settings.bucket
                    && (e.window_label.contains("週") || e.window_label.contains("week"))
            })
            .or_else(|| {
                self.history
                    .iter()
                    .find(|e| drop_event(e) || reset_event(e))
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
    fn browsed_history(&self) -> Vec<HistoryEvent> {
        if let Some((source, key, label)) = &self.history_selection {
            let mut events: Vec<_> = self
                .history
                .iter()
                .filter(|e| {
                    e.source_key == *source
                        && match (&e.window_key, key) {
                            (Some(a), Some(b)) => a == b,
                            _ => e.window_label == *label,
                        }
                })
                .cloned()
                .collect();
            events.sort_by_key(|e| e.observed_at_unix);
            events
        } else {
            self.matching_history()
        }
    }
    fn refresh_main_cache(&mut self) {
        let t = now();
        let key = (
            self.token_generation,
            self.history_generation,
            self.settings.bucket.clone(),
            self.settings.window.clone(),
            t / 60,
            self.tree_generation,
        );
        if self.main_cache.key.as_ref() == Some(&key) {
            return;
        }
        let all = self.matching_history();
        let boundary = all
            .into_iter()
            .filter(|e| {
                e.observed_at_unix <= t
                    && quota_widget::history::is_confirmed_quota_change(e)
                    && !unchanged(e)
            })
            .max_by_key(|e| e.observed_at_unix);
        let start = boundary.as_ref().map(|e| e.observed_at_unix);
        let tasks =
            self.tokens
                .usage_index
                .task_rows(self.tokens.events(), start, self.demo_start, t);
        let mut cost = self
            .tokens
            .cost_index
            .observed(start.unwrap_or(self.demo_start), t);
        cost.partial |= start.is_none();
        let tree =
            usage_views::task_tree_rows(&tasks, &self.tokens.task_parents, &self.expanded_tasks);
        self.main_cache = MainCache {
            tree,
            key: Some(key),
            tasks,
            boundary,
            cost,
        };
    }
    fn event_pricing_tooltip(&self, e: &TokenEvent) -> String {
        let model = format!(
            "模型：{}{}",
            e.model,
            if e.model_assumed {
                "（未記錄，假設）"
            } else {
                ""
            }
        );
        if e.is_excluded_auto_review() {
            return format!("{model}\n依設定排除 codex-auto-review；無公開單價，不能稱為免費。官方額度不受此顯示篩選影響。");
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
            format!("{model}\n{}；USD / 1M：輸入{}／快取{}／輸出{}{}\n推理已含輸出；high／max 不另加倍率。",self.tokens.pricing.tier_label,rate.uncached_input_per_million*im,rate.cached_input_per_million*im,rate.output_per_million*om,if long {"（單次長上下文）"}else{""})
        } else {
            format!("{model}\n此模型沒有設定單價；不套用 Sol 價格。")
        }
    }
    fn task_tooltip(&self, task: &usage_views::TaskRow) -> String {
        let mut text = format!(
            "{}\nTask ID: {}",
            self.tokens.task_name(&task.task_id).unwrap_or("名稱未記錄"),
            task.task_id
        );
        if let Some(e) = self.tokens.events().get(task.latest_event_index) {
            text.push_str(&format!("\n{}", self.event_pricing_tooltip(e)));
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
        text.push_str("\n* 資料不完整／估價假設；點擊複製 ID。");
        text
    }
    fn widget(&mut self, ctx: &egui::Context) {
        self.refresh_main_cache();
        let dark = self.settings.dark;
        let (bg, fg, muted, line, accent) = if dark {
            (
                Color32::from_rgb(23, 39, 34),
                Color32::from_rgb(220, 247, 229),
                Color32::from_rgb(145, 173, 160),
                Color32::from_rgb(48, 74, 62),
                Color32::from_rgb(105, 177, 139),
            )
        } else {
            (
                Color32::from_rgb(240, 248, 240),
                Color32::from_rgb(29, 61, 45),
                Color32::from_rgb(91, 124, 106),
                Color32::from_rgb(199, 218, 203),
                Color32::from_rgb(65, 137, 91),
            )
        };
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                let w = rect.width();
                let sx = w / 310.;
                let sy = sx;
                let visible=usage_views::visible_task_rows(self.main_cache.tree.len(),self.settings.task_visible_rows as usize);
                let layout=(visible,self.settings.task_visible_rows,w.round() as u32);
                if self.layout_key!=Some(layout) {
                    let height=usage_views::widget_height(self.main_cache.tree.len(),self.settings.task_visible_rows as usize)*sx;
                    ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(Vec2::new(276.,height)));
                    ctx.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(w,height)));
                    self.layout_key=Some(layout);
                }
                let scale = sx.min(sy).clamp(0.85, 2.5);
                let painter = ui.painter().clone();
                painter.rect(
                    rect.shrink(1.),
                    22.,
                    bg,
                    Stroke::new(1_f32, line),
                    egui::StrokeKind::Inside,
                );
                let pt = |x: f32, y: f32| Pos2::new(rect.left() + x * sx, rect.top() + y * sy);
                let txt = |x, y, text: String, size: f32, color| {
                    painter.text(
                        pt(x, y),
                        Align2::LEFT_TOP,
                        text,
                        FontId::proportional(size * scale),
                        color,
                    );
                };
                let drag = ui.interact(
                    Rect::from_min_max(pt(4.,40.),pt(306.,196.)),
                    ui.id().with("drag"),
                    Sense::click_and_drag(),
                );
                let title_drag=ui.interact(Rect::from_min_max(pt(4.,4.),pt(155.,39.)),ui.id().with("title-drag"),Sense::click_and_drag());
                if (drag.is_pointer_button_down_on() || title_drag.is_pointer_button_down_on())
                    && ui.input(|i| i.pointer.button_pressed(egui::PointerButton::Primary))
                {
                    ctx.send_viewport_cmd(ViewportCommand::StartDrag);
                }
                drag.context_menu(|ui| {
                    if ui.button("設定 / 連接").clicked() {
                        self.settings_open = true;
                        ui.close();
                    }
                    if ui.button("更新額度").clicked() {
                        if let Some(b) = &self.bridge {
                            let _ = b.commands.send(BridgeCommand::Refresh);
                        }
                        ui.close();
                    }
                    if ui.checkbox(&mut self.settings.dark, "深色外觀").changed() {
                        self.apply_theme(ctx);
                        self.dirty = true;
                    }
                    if ui
                        .checkbox(&mut self.settings.on_top, "保持最上層")
                        .changed()
                    {
                        self.topmost(ctx);
                        self.dirty = true;
                    }
                    if ui.button("關閉小工具").clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                });
                let view = self.selected();
                let title = view
                    .as_ref()
                    .map(|v| {
                        if v.window_duration_mins == Some(10080) {
                            "每週額度".into()
                        } else {
                            v.window_label.clone()
                        }
                    })
                    .unwrap_or_else(|| "每週額度".into());
                painter.circle_filled(
                    pt(22., 24.),
                    3.2 * scale,
                    if self.status == "LIVE" || self.demo {
                        accent
                    } else {
                        muted
                    },
                );
                txt(34., 14., title, 14., fg);
                if ui
                    .put(
                        Rect::from_min_max(pt(161., 12.), pt(224., 37.)),
                        egui::Button::new(RichText::new("扣額紀錄").size(11. * scale).color(fg)).fill(line),
                    )
                    .clicked()
                {
                    self.history_open = true;
                }
                if ui
                    .put(
                        Rect::from_min_max(pt(230., 12.), pt(295., 37.)),
                        egui::Button::new(RichText::new("Token明細").size(11. * scale).color(fg)).fill(line),
                    )
                    .clicked()
                {
                    self.tokens_open = true;
                }
                let remain = view.as_ref().and_then(|v| v.remaining_percent);
                let percent = remain
                    .map(|r| {
                        if r.fract().abs() < 1e-7 {
                            format!("{r:.0}")
                        } else {
                            format!("{r:.1}")
                        }
                    })
                    .unwrap_or_else(|| "—".into());
                txt(20., 51., percent.clone(), 76., fg);
                let percent_width = painter
                    .layout_no_wrap(percent.clone(), FontId::proportional(76. * scale), fg)
                    .size()
                    .x;
                let percent_x = 20. + percent_width / sx + 5.;
                txt(percent_x, 83., "%".into(), 33., fg);
                txt(248., 102., "剩餘".into(), 13., muted);
                let bar = Rect::from_min_max(pt(21., 139.), pt(289., 143.));
                painter.rect_filled(bar, 3., line);
                if let Some(p) = remain {
                    painter.rect_filled(
                        Rect::from_min_size(
                            bar.min,
                            Vec2::new(bar.width() * (p as f32 / 100.).clamp(0., 1.), bar.height()),
                        ),
                        3.,
                        accent,
                    );
                }
                txt(21., 154., "估計消耗".into(), 13., muted);
                let rate = view
                    .as_ref()
                    .and_then(|v| v.rate_per_hour)
                    .map(|r| format!("{r:.2} 百分點 / 小時"))
                    .unwrap_or_else(|| "— 等待量測".into());
                txt(93., 154., rate, 13., fg);
                txt(21., 178., "點數".into(), 13., muted);
                let credits = self.quota.credits(now());
                let credit = if let Some(v) = &view {
                    credits.into_iter().find(|c| c.source_key == v.source_key)
                } else {
                    credits.into_iter().next()
                };
                txt(
                    93.,
                    178.,
                    credit
                        .as_ref()
                        .map(|c| {
                            if c.unlimited {
                                "不限".into()
                            } else {
                                c.balance.clone().unwrap_or_else(|| "—".into())
                            }
                        })
                        .unwrap_or_else(|| "—".into()),
                    13.,
                    fg,
                );
                txt(
                    207.,
                    178.,
                    credit
                        .as_ref()
                        .and_then(|c| c.rate_per_minute)
                        .map(|r| format!("{r:.2} / 分"))
                        .unwrap_or_else(|| "— / 分".into()),
                    12.,
                    muted,
                );
                txt(21., 202., "重設".into(), 13., muted);
                txt(
                    93.,
                    202.,
                    view.as_ref()
                        .and_then(|v| v.reset_at)
                        .map(|t| duration(t - now()))
                        .unwrap_or_else(|| "—".into()),
                    13.,
                    fg,
                );
                painter.line_segment([pt(21., 229.), pt(289., 229.)], Stroke::new(1_f32, line));
                for (x,label) in [(21.,"Task／名稱"),(105.,"時間"),(163.,"用度"),(225.,"本%累計")] {txt(x,239.,label.into(),10.,muted);}
                ui.interact(Rect::from_min_max(pt(21.,238.),pt(289.,255.)),ui.id().with("task-column-help"),Sense::hover()).on_hover_text("用度＝最新單筆 Response 的估算 USD；本%累計＝最近已確認扣額／重設後的已知用量合計。缺少邊界時從啟動開始計 0，並以 * 標示不完整。滑鼠移到 Task 可看名稱及 Token 數字。");
                let table_bottom=257.+18.*visible as f32;
                ui.scope_builder(egui::UiBuilder::new().max_rect(Rect::from_min_max(pt(21.,257.),pt(289.,table_bottom))),|ui| {
                    ui.spacing_mut().item_spacing=Vec2::ZERO;
                    egui::ScrollArea::vertical().id_salt("task-interval-rows").max_height(18.*visible as f32*sy).min_scrolled_height(0.).show_rows(ui,18.*sy,self.main_cache.tree.len().max(1),|ui,range| {
                        if self.main_cache.tree.is_empty() {ui.label(RichText::new("尚無近期 Task 用量").size(10.*scale).color(muted));return;}
                        for i in range {
                            let row=&self.main_cache.tree[i];
                            let (r,response)=ui.allocate_exact_size(Vec2::new(268.*sx,18.*sy),Sense::hover());
                            match row {
                                usage_views::TaskTreeRow::SelfTask {task_id,task_index,depth,relationship}=>{
                                    let task=task_index.and_then(|i|self.main_cache.tasks.get(i));
                                    let mut tooltip=task.map(|t|self.task_tooltip(t)).unwrap_or_else(||format!("{}\nTask ID: {}\n此 Task 沒有近期自身用量記錄。\nInput: 未知\nCached: 未知\nUncached: 未知\nOutput: 未知\nReasoning: 未知\nTurn Input: 未知\nTurn Output: 未知",self.tokens.task_name(task_id).unwrap_or("名稱未記錄"),task_id));
                                    let relation=match relationship {usage_views::TaskRelationship::Unlinked=>"未記錄上層關係；獨立顯示".into(),usage_views::TaskRelationship::Child{parent_id}=>format!("已記錄上層: {parent_id}"),usage_views::TaskRelationship::Orphan{parent_id}=>format!("上層資料不完整: {}",parent_id.as_deref().unwrap_or("未知")),usage_views::TaskRelationship::Cycle{parent_id}=>format!("上層關係形成循環，已獨立顯示: {parent_id}")};
                                    tooltip.push_str(&format!("\n{relation}\n本行只計自身用量。"));
                                    response.on_hover_ui(|ui|{ui.set_max_width(260.*scale);ui.add(egui::Label::new(RichText::new(&tooltip).size(10.5*scale)).wrap());});
                                    let indent=(*depth as f32*8.).min(32.);
                                    let name=self.tokens.task_name(task_id).map(str::to_owned).unwrap_or_else(||task_id.chars().take(7).collect());
                                    let rect=Rect::from_min_size(r.min+Vec2::new(indent*sx,0.),Vec2::new((81.-indent)*sx,18.*sy));
                                    let label=if *depth>0 {format!("└ {name}")}else{name};
                                    let font=FontId::proportional(9.5*scale);
                                    let mut short=label.clone();
                                    while ui.painter().layout_no_wrap(short.clone(),font.clone(),fg).size().x>rect.width()-2.*sx && short.chars().count()>2 {if short.ends_with('…'){short.pop();}short.pop();short.push('…');}
                                    ui.painter().with_clip_rect(rect.intersect(ui.clip_rect())).text(rect.min+Vec2::new(0.,2.*sy),Align2::LEFT_TOP,short,font,fg);
                                    if ui.interact(rect,ui.id().with(("task-name",task_id)),Sense::click()).on_hover_ui(|ui|{ui.set_max_width(260.*scale);ui.add(egui::Label::new(RichText::new(&tooltip).size(10.5*scale)).wrap());}).clicked(){ctx.copy_text(task_id.clone());}
                                    let values=if let Some(task)=task {[(84.,local_time(task.latest_timestamp)),(142.,money(task.latest_cost_usd)),(204.,format!("{}{}",money(task.interval_cost_usd),if task.partial {"*"}else{""}))]}else{[(84.,"—".into()),(142.,"—".into()),(204.,"$0.0000*".into())]};
                                    for (x,text) in values {ui.painter().text(r.min+Vec2::new(x*sx,2.*sy),Align2::LEFT_TOP,text,FontId::proportional(9.5*scale),fg);}
                                }
                                usage_views::TaskTreeRow::ChildrenSummary {parent_id,depth,descendant_count,direct_child_count,known_cost_usd,partial,expanded}=>{
                                    let indent=(*depth as f32*8.).min(32.);
                                    let label=format!("[{}] 子代理 {}",if *expanded {"−"}else{"+"},descendant_count);
                                    let help=format!("全部已知子代理 {descendant_count} 個（直接子代理 {direct_child_count} 個）。\n合計所有後代在本額度區間的已知用量，每個 Task 只計一次，不含主代理自己的用量。\n未匯入或未記錄關係的子代理無法推測；* 表示資料不完整或估價假設。點擊展開／收起。");
                                    response.on_hover_text(&help);
                                    let button_rect=Rect::from_min_size(r.min+Vec2::new(indent*sx,0.),Vec2::new((200.-indent)*sx,18.*sy));
                                    ui.painter().text(button_rect.min+Vec2::new(0.,2.*sy),Align2::LEFT_TOP,label,FontId::proportional(9.5*scale),accent);
                                    if ui.interact(button_rect,ui.id().with(("children-toggle",parent_id)),Sense::click()).on_hover_text(&help).clicked(){if *expanded {self.expanded_tasks.remove(parent_id);}else{self.expanded_tasks.insert(parent_id.clone());}self.tree_generation+=1;}
                                    ui.painter().text(r.min+Vec2::new(204.*sx,2.*sy),Align2::LEFT_TOP,format!("{}{}",money(Some(*known_cost_usd)),if *partial {"*"}else{""}),FontId::proportional(9.5*scale),accent);
                                }
                            }
                        }
                    });
                });
                let footer=if let Some(e)=&self.main_cache.boundary {
                    format!("{} {} 分鐘／累計 {}{}",if reset_event(e){"距本次重設"}else{"距上次扣額"},(now()-e.observed_at_unix).max(0)/60,money(Some(self.main_cache.cost.known_usd)),if self.main_cache.cost.partial {" *"}else{""})
                } else {format!("自啟動 {} 分鐘／累計 {} *",(now()-self.demo_start).max(0)/60,money(Some(self.main_cache.cost.known_usd)))};
                txt(21.,table_bottom+10.,footer,11.,muted);
                ui.interact(Rect::from_min_max(pt(21.,table_bottom+9.),pt(289.,table_bottom+29.)),ui.id().with("cost-coverage"),Sense::hover()).on_hover_text("累計為 API 模型等價的已知小計，並非訂閱扣款。* 表示邊界或部分歷史／價格／模型假設不完整；缺資料不會阻止已知用量繼續相加。無扣額歷史時，自本次啟動開始累計。價格及狀態見設定／Token 明細。");
                if ui
                    .put(
                        Rect::from_min_max(pt(272., 198.), pt(294., 220.)),
                        egui::Button::new("≡").frame(false),
                    )
                    .clicked()
                {
                    self.settings_open = true;
                }
                // Native borderless resize handle. Never changes focus or activates the window.
                let resize = ui.interact(
                    Rect::from_min_max(Pos2::new(rect.right()-16.*sx,rect.bottom()-16.*sy),rect.max),
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
        ui.horizontal(|ui| {
            if ui.button("◀ 前一天").clicked() {
                *date = date.pred_opt().unwrap_or(*date);
            }
            egui::ComboBox::from_id_salt(id)
                .selected_text(date.to_string())
                .show_ui(ui, |ui| {
                    for d in dates.iter().rev() {
                        ui.selectable_value(date, *d, d.to_string());
                    }
                });
            if ui.button("後一天 ▶").clicked() {
                *date = date.succ_opt().unwrap_or(*date);
            }
            if ui.button("今天").clicked() {
                *date = Local::now().date_naive();
            }
        });
    }
    fn days(&mut self) -> BTreeSet<NaiveDate> {
        let today = Local::now().date_naive();
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
            for e in self.tokens.events() {
                if let Some(d) = local_day(e.timestamp) {
                    self.dates_cache.insert(d);
                }
            }
            self.dates_key = Some(key);
        }
        self.dates_cache.clone()
    }
    fn detail_windows(&mut self, ctx: &egui::Context) {
        if self.history_open {
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("quota-history"),
                ViewportBuilder::default()
                    .with_title("Quota Widget · 扣額紀錄")
                    .with_inner_size([780., 610.])
                    .with_min_inner_size([560., 400.]),
                |ctx, _| {
                    if ctx.input(|i| i.viewport().close_requested()) {
                        self.history_open = false;
                    }
                    egui::CentralPanel::default().show(ctx, |ui| {
                        self.history_ui(ui);
                    });
                },
            );
        }
        if self.tokens_open {
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("token-history"),
                ViewportBuilder::default()
                    .with_title("Quota Widget · Token 明細")
                    .with_inner_size([1120., 650.])
                    .with_min_inner_size([680., 420.]),
                |ctx, _| {
                    if ctx.input(|i| i.viewport().close_requested()) {
                        self.tokens_open = false;
                    }
                    egui::CentralPanel::default().show(ctx, |ui| {
                        self.token_ui(ui);
                    });
                },
            );
        }
        if self.settings_open {
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("settings"),
                ViewportBuilder::default()
                    .with_title("Quota Widget · 設定 / 連接")
                    .with_inner_size([650., 700.])
                    .with_min_inner_size([520., 420.]),
                |ctx, _| {
                    if ctx.input(|i| i.viewport().close_requested()) {
                        self.settings_open = false;
                    }
                    egui::CentralPanel::default().show(ctx, |ui| {
                        egui::ScrollArea::vertical().show(ui, |ui| self.settings_ui(ui, ctx));
                    });
                },
            );
        }
    }
    fn history_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("扣額紀錄");
        if self.demo {
            ui.colored_label(
                if self.settings.dark {
                    Color32::YELLOW
                } else {
                    Color32::from_rgb(130, 75, 0)
                },
                "DEMO · 合成使用量與虛構價格，僅示範介面。",
            );
        }
        ui.label("本機觀察時間；不是後台保證的實際扣額時間。金額為模型等價估算，不是訂閱帳單。");
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
            self.history_cache.sources_generation = Some(self.history_generation);
        }
        egui::ComboBox::from_label("來源 / 額度視窗")
            .selected_text(
                self.history_selection
                    .as_ref()
                    .map(|(s, k, l)| format!("{s} · {l} · {}", k.as_deref().unwrap_or("舊版")))
                    .unwrap_or_else(|| "目前小工具選擇".into()),
            )
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.history_selection, None, "目前小工具選擇");
                for (s, k, l) in &self.history_cache.sources {
                    ui.selectable_value(
                        &mut self.history_selection,
                        Some((s.clone(), k.clone(), l.clone())),
                        format!("{s} · {l} · {}", k.as_deref().unwrap_or("舊版")),
                    );
                }
            });
        let dates = self.days();
        Self::date_controls(ui, &mut self.date, &dates, "quota-date");
        ui.horizontal(|ui| {
            if ui
                .checkbox(&mut self.settings.hide_unchanged, "忽略相同百分比")
                .changed()
            {
                self.dirty = true;
            }
            if ui.button("匯出當日 CSV").clicked() {
                self.export_day(false, false);
            }
            if ui.button("匯出當日 JSON").clicked() {
                self.export_day(false, true);
            }
        });
        let current_view = self.selected();
        if let Some(v) = &current_view {
            ui.separator();
            ui.label(format!("{} · {}", v.window_label, v.prediction.explanation));
            if let Some(t) = v.prediction.estimated_at {
                ui.label(format!(
                    "下次下降約 {}（估算，{}）",
                    local_time(t),
                    v.prediction.confidence.label()
                ));
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
            self.history_generation,
            self.token_generation,
            self.date,
            selection_key,
            self.settings.hide_unchanged,
        );
        if self.history_cache.key.as_ref() != Some(&key) {
            let all = self.browsed_history();
            // One batch pass, instead of one full-history scan for every row.
            let costs = accounting::history_costs(&self.tokens.cost_index, &all);
            self.history_cache.rows = all
                .into_iter()
                .zip(costs)
                .filter_map(|(event, (cost, partial))| {
                    (local_day(event.observed_at_unix) == Some(self.date)
                        && (!self.settings.hide_unchanged || !unchanged(&event)))
                    .then_some((event, cost, partial))
                })
                .collect();
            self.history_cache.rows.reverse();
            let mut cost_total = 0.0;
            let mut points_total = 0.0;
            for (event, _, _) in &self.history_cache.rows {
                if let Some(interval) = self
                    .interval_lookup
                    .get(&intervals::history_event_key(event))
                    .and_then(|i| self.intervals.get(*i))
                    .filter(|interval| interval.comparable)
                {
                    cost_total += interval.known_usd;
                    points_total += interval.percentage_points;
                }
            }
            self.history_cache.weighted = (points_total > 0.0).then_some(cost_total / points_total);
            self.history_cache.key = Some(key);
        }
        ui.label(format!(
            "{} 筆 · * 代表資料不完整或價格未設定。較早資料只有實際額度觀察時才會還原。",
            self.history_cache.rows.len()
        ));
        ui.small(
        "重複快照已合併顯示；有差異的觀察不當作已確認回補，也不重開費用累計區間。原始記錄保留。",
    );
        ui.separator();
        ui.label(format!(
            "可完整歸屬區間的加權成本：{} / 百分點",
            money(self.history_cache.weighted)
        ));
        if self.history_cache.rows.is_empty() {
            ui.label("這一天沒有可用的額度變動觀察。Token 使用量不能反推訂閱額度。");
        } else {
            egui::ScrollArea::vertical()
                .id_salt("quota-history-rows")
                .show_rows(ui, 58.0, self.history_cache.rows.len(), |ui, range| {
                    for index in range {
                        let (event, cost, partial) = &self.history_cache.rows[index];
                        let (rect, _) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 58.0),
                            Sense::hover(),
                        );
                        let mut row = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                        row.set_clip_rect(rect.intersect(ui.clip_rect()));
                        let interval = self
                            .interval_lookup
                            .get(&intervals::history_event_key(event))
                            .and_then(|i| self.intervals.get(*i));
                        let description = if event.reconciliation.is_some() {
                            format!(
                                "{}% → {}% · 額度觀察有差異，未確認回補",
                                event.before, event.after
                            )
                        } else if event.kind == quota_widget::model::HistoryKind::QuotaDecrease {
                            format!(
                                "{}% → {}% · 扣除 {} 百分點 · {}{}",
                                event.before,
                                event.after,
                                event.delta,
                                interval
                                    .map(|v| format!(
                                        "{} · {}/百分點",
                                        money(Some(v.known_usd)),
                                        money(v.cost_per_point)
                                    ))
                                    .unwrap_or_else(|| money(*cost)),
                                if interval.map(|v| v.partial).unwrap_or(*partial) {
                                    " *"
                                } else {
                                    ""
                                }
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
                        };
                        row.horizontal(|ui| {
                            ui.strong(local_time(event.observed_at_unix));
                            if let Some(interval) = interval {
                                if ui.small_button("模型分項").clicked() {
                                    self.selected_interval =
                                        Some(interval.history_event_key.clone());
                                }
                            }
                            ui.add(egui::Label::new(&description).truncate())
                                .on_hover_text(&description);
                        });
                        row.add(
                            egui::Label::new(
                                RichText::new(format!(
                                    "{} · {} · 週期 {}{}",
                                    event.window_label,
                                    event.source_key,
                                    event.epoch,
                                    if event.reconciliation.is_some() {
                                        " · 不作累計分界"
                                    } else {
                                        ""
                                    }
                                ))
                                .small(),
                            )
                            .truncate(),
                        );
                        row.separator();
                    }
                });
        }
        if let Some(interval) = self
            .selected_interval
            .as_ref()
            .and_then(|key| self.interval_lookup.get(key))
            .and_then(|i| self.intervals.get(*i))
        {
            let mut open = true;
            egui::Window::new("扣額區間 · 模型分項").id(egui::Id::new("completed-interval-detail")).open(&mut open).default_width(710.).show(ui.ctx(), |ui| {
                ui.label(format!("{} → {} · 實際下降 {} 百分點", interval.start.map(local_time).unwrap_or_else(||"開始時間未知".into()), local_time(interval.end), interval.percentage_points));
                ui.strong(format!("已知估算總和 {}{} · 每百分點 {}", money(Some(interval.known_usd)), if interval.partial {" *"}else{""}, money(interval.cost_per_point)));
                ui.small("按每筆實際 model 計價。多百分點跳變只顯示平均，不拆成虛構 1% 區間；推理已含輸出。排除 codex-auto-review，不能據此判定官方訂閱計費。");
                if !interval.reasons.is_empty() { ui.colored_label(Color32::from_rgb(210,150,70),format!("不可直接比較／資料限制：{}",interval.reasons.iter().map(|r|r.label()).collect::<Vec<_>>().join(" · "))); }
                egui::ScrollArea::both().max_height(320.).show(ui, |ui| {
                    egui::Grid::new("interval-model-breakdown").striped(true).show(ui, |ui| {
                        for label in ["模型","筆數","Input","Cached","Uncached","Output","Reasoning","估算 USD"] {ui.strong(label);} ui.end_row();
                        for model in &interval.models {
                            ui.label(format!("{}{}",model.model,if model.assumed_model_events>0 {"（含假設）"}else{""}));
                            ui.label(model.events.to_string()); ui.label(model.input.to_string()); ui.label(model.cached.to_string()); ui.label(model.uncached.to_string()); ui.label(model.output.to_string()); ui.label(model.reasoning.to_string()); ui.label(format!("{}{}",money(Some(model.known_usd)),if model.unknown_cost_events>0 {" *"}else{""})); ui.end_row();
                        }
                    });
                });
                ui.small(format!("{} 筆事件；{} 筆價格未知。CSV / JSON 匯出包含完整區間及 model 分項。",interval.events,interval.unknown_cost_events));
            });
            if !open {
                self.selected_interval = None;
            }
        }
        if !self.notice.is_empty() {
            ui.label(&self.notice);
        }
    }

    fn token_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Token 明細");
        if self.demo {
            ui.colored_label(
                if self.settings.dark {
                    Color32::YELLOW
                } else {
                    Color32::from_rgb(130, 75, 0)
                },
                "DEMO · 合成使用量與虛構價格，僅示範介面。",
            );
        }
        ui.label("讀取本機 Codex JSONL；輸入是整份上下文（含快取），推理含於輸出，不重複計費。$ 為模型等價估算，不是實際訂閱扣款。");
        ui.small(format!(
            "價格基準：{}；未建模 cache-write／區域／工具附加費。",
            self.tokens.pricing.tier_label
        ));
        let dates = self.days();
        Self::date_controls(ui, &mut self.token_date, &dates, "token-date");
        ui.horizontal(|ui| {
            if ui.button("重新掃描").clicked() {
                self.start_import();
            }
            if ui.button("匯出當日 CSV").clicked() {
                self.export_day(true, false);
            }
            if ui.button("匯出當日 JSON").clicked() {
                self.export_day(true, true);
            }
            if ui.button("編輯價格").clicked() {
                self.settings_open = true;
            }
        });
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut self.token_group,
                GroupBy::Response,
                "Response／舊版事件",
            );
            ui.selectable_value(&mut self.token_group, GroupBy::Turn, "Turn（當日）");
            ui.selectable_value(&mut self.token_group, GroupBy::Task, "Task／Thread（當日）");
        });
        ui.small("Turn／Task 只加總已去重的 response；混合模型逐筆計價，不跟隨主代理模型。high／max 不另乘單價；沒有 Turn ID 時不猜測合併。");
        ui.horizontal(|ui| {
            ui.checkbox(
                &mut self.show_excluded_records,
                "查看排除紀錄（codex-auto-review）",
            );
            ui.small(format!(
                "保留 {} 筆；不計入 Task／子代理數及費用",
                self.tokens.excluded_events.len()
            ));
        });
        ui.small("只依精確模型 codex-auto-review 排除，普通 Sol 審查工作仍計入；官方額度讀數不變，排除不代表免費。");
        let key = (
            self.token_generation,
            self.token_date,
            self.token_group,
            self.show_excluded_records,
        );
        if self.token_cache.key != Some(key) {
            let events: Vec<_> = if self.show_excluded_records {
                &self.tokens.excluded_events[..]
            } else {
                self.tokens.events()
            }
            .iter()
            .filter(|e| local_day(e.timestamp) == Some(self.token_date))
            .collect();
            let mut cache = TokenCache {
                key: Some(key),
                count: events.len(),
                rows: usage_views::group_with_excluded(
                    &events,
                    self.token_group,
                    self.show_excluded_records,
                ),
                ..TokenCache::default()
            };
            for e in events {
                cache.input = cache.input.saturating_add(e.input);
                cache.cached = cache.cached.saturating_add(e.cached);
                cache.output = cache.output.saturating_add(e.output);
                cache.reasoning = cache.reasoning.saturating_add(e.reasoning);
                cache.missing += usize::from(e.cost_usd.is_none());
                cache.cost += e.cost_usd.unwrap_or(0.);
            }
            self.token_cache = cache;
        }
        let cache = &self.token_cache;
        if self.show_excluded_records {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "以下只供稽核，全部已排除，不會加入目前百分比區間或整體費用。",
            );
        }
        ui.label(format!(
            "{} 筆 · 已記錄輸入 {}（快取 {}／未快取 {}）· 輸出 {}（推理 {}）· 已可估部分 {}{}",
            cache.count,
            cache.input,
            cache.cached,
            cache.input.saturating_sub(cache.cached),
            cache.output,
            cache.reasoning,
            money(Some(cache.cost)),
            if cache.missing > 0 {
                format!("；{} 筆費用未知", cache.missing)
            } else {
                String::new()
            }
        ));
        ui.small(&self.import_note);
        ui.separator();
        let widths = [72., 165., 115., 120., 45., 90., 90., 90., 80., 80., 110.];
        let headers = [
            "時間",
            "模型",
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
            let (header,_) = ui.allocate_exact_size(Vec2::new(total_width,24.),Sense::hover());
            let mut x=header.left();
            for (label,width) in headers.iter().zip(widths){ui.painter().text(Pos2::new(x+3.,header.top()+3.),Align2::LEFT_TOP,label,FontId::proportional(12.),ui.visuals().strong_text_color());x+=width;}
            egui::ScrollArea::vertical().id_salt("token-rows").show_rows(ui,24.,cache.rows.len(),|ui,range| {
                for i in range {
                    let e=&cache.rows[cache.rows.len()-1-i];
                    let number=|n:u64,flag:&str|if e.partial_flags.iter().any(|f|f==flag){"未知".to_owned()}else{n.to_string()};
                    let uncached=if e.partial_flags.iter().any(|f|f=="input_count_unavailable"||f=="cached_count_unavailable"){"未知".into()}else{e.uncached.to_string()};
                    let identity=match self.token_group {GroupBy::Response=>e.response_id.as_deref().unwrap_or("舊版／未知"),GroupBy::Turn=>e.turn_id.as_deref().unwrap_or("未知 Turn"),GroupBy::Task=>"當日事件合計"};
                    let values=[local_time(e.last_timestamp),format!("{}{}",e.models.join(" / "),if e.model_assumed {"（假設）"}else{""}),e.session.chars().take(12).collect::<String>(),identity.chars().take(12).collect(),e.event_count.to_string(),number(e.input,"input_count_unavailable"),number(e.cached,"cached_count_unavailable"),uncached,number(e.output,"output_count_unavailable"),number(e.reasoning,"reasoning_count_unavailable"),format!("{}{}",money(e.cost_usd),if e.partial_flags.is_empty(){""}else{" *"})];
                    let tooltip=format!("{}\nTask: {}\nTurn: {}\nResponse: {}\n{} 至 {}\n模型: {}\n已可估部分 {}；{} 筆費用未知\n{}",self.tokens.task_name(&e.session).unwrap_or("名稱未記錄"),e.session,e.turn_id.as_deref().unwrap_or("未知"),e.response_id.as_deref().unwrap_or("未知"),local_time(e.first_timestamp),local_time(e.last_timestamp),e.models.join(" / "),money(Some(e.known_cost_subtotal_usd)),e.unknown_cost_events,e.partial_flags.join("\n"));
                    let (rect,response)=ui.allocate_exact_size(Vec2::new(total_width,24.),Sense::hover());response.on_hover_text(tooltip);
                    if i%2==1 {ui.painter().rect_filled(rect,0.,ui.visuals().faint_bg_color);}
                    let mut x=rect.left();
                    for (value,width) in values.iter().zip(widths) {let cell=Rect::from_min_size(Pos2::new(x,rect.top()),Vec2::new(width,24.));ui.painter().with_clip_rect(cell.intersect(ui.clip_rect())).text(Pos2::new(x+3.,rect.top()+3.),Align2::LEFT_TOP,value,FontId::proportional(12.),ui.visuals().text_color());x+=width;}
                }
            });
        });
        if cache.count == 0 {
            ui.label("這一天沒有可用的 Token 記錄；不會補成零使用量。");
        }
        if !self.notice.is_empty() {
            ui.label(&self.notice);
        }
    }
    fn settings_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("設定 / 連接");
        ui.label(format!("狀態：{}", self.status));
        if self.demo {
            ui.colored_label(
                if self.settings.dark {
                    Color32::YELLOW
                } else {
                    Color32::from_rgb(130, 75, 0)
                },
                "DEMO：全部為合成範例，不連線、不讀取真實歷史，也不儲存。",
            );
        }
        ui.horizontal(|ui| {
            if ui.checkbox(&mut self.settings.dark, "深色外觀").changed() {
                self.apply_theme(ctx);
                self.dirty = true;
            }
            if ui
                .checkbox(&mut self.settings.on_top, "保持最上層")
                .changed()
            {
                self.topmost(ctx);
                self.dirty = true;
            }
        });
        ui.horizontal(|ui| {
            ui.label("Task 顯示行數（超過才捲動）");
            if ui
                .add(egui::DragValue::new(&mut self.settings.task_visible_rows).range(1..=50))
                .changed()
            {
                self.dirty = true;
            }
        });
        ui.small("最上層在恢復／焦點生命週期及低頻檢查重套用，不呼叫啟用或搶焦點。其他 App 的全螢幕／安全桌面仍受 OS 規則限制。");
        ui.separator();
        ui.strong("官方 Codex CLI（唯讀）");
        ui.label("留空自動尋找原生 CLI。只讀取帳戶額度；不登入、不執行工作、不讀取憑證。");
        if ui
            .text_edit_singleline(&mut self.settings.codex_path)
            .changed()
        {
            self.dirty = true;
        }
        ui.add_enabled_ui(!self.demo, |ui| {
            ui.horizontal(|ui| {
                if ui.button("重新連線").clicked() {
                    self.offline = false;
                    self.connect();
                }
                if ui.button("停止連線").clicked() {
                    self.bridge = None;
                    self.offline = true;
                    self.status = "OFFLINE".into();
                    self.quota.reset_continuity();
                }
                if ui.button("更新額度").clicked() {
                    if let Some(b) = &self.bridge {
                        let _ = b.commands.send(BridgeCommand::Refresh);
                    }
                }
            });
        });
        let views = self.quota.views(now());
        egui::ComboBox::from_label("額度來源 / 視窗")
            .selected_text(
                self.selected()
                    .map(|v| format!("{} · {}", v.source_label, v.window_label))
                    .unwrap_or_else(|| "等待來源".into()),
            )
            .show_ui(ui, |ui| {
                for v in views {
                    if ui
                        .selectable_label(
                            self.settings.bucket == v.source_key
                                && self.settings.window == v.window_key,
                            format!("{} · {}", v.source_label, v.window_label),
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
        ui.strong("本機 Token 歷史");
        if ui
            .checkbox(&mut self.settings.import_enabled, "啟用唯讀增量匯入")
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
        ui.horizontal(|ui| {
            if ui.button("掃描所有可用歷史").clicked() {
                self.start_import();
            }
            ui.label("預設日期範圍");
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
        ui.small("預設30天瀏覽，保留更早歷史。精確 codex-auto-review 保留原始用量，但從預設 Task／子代理數／估算中排除；可在 Token 明細稽核。官方額度不變，不表示免費。");
        ui.label(&self.import_note);
        ui.separator();
        ui.strong("模型等價價格（USD / 1M tokens）");
        ui.label("全新設定採官方 Standard 費率假設；不是訂閱扣款。只有未記錄模型時假設 6.1 Sol；有名稱但無單價的模型保持未知。Sol／Luna／Astra 按每筆實際模型計價。");
        ui.horizontal(|ui| {
            for (label, preset) in [
                ("官方 Standard", Pricing::official_standard()),
                ("官方 Fast / Priority", Pricing::official_fast()),
            ] {
                if ui.button(label).clicked() {
                    let current_override = self.settings.pricing.model_override.clone();
                    self.settings.pricing = preset;
                    self.settings.pricing.model_override = current_override;
                    self.request_reprice();
                    if let Some(p) = self.settings.pricing.rates.get(&self.price_model) {
                        self.price_values = [
                            p.uncached_input_per_million,
                            p.cached_input_per_million,
                            p.output_per_million,
                        ];
                        self.price_configured = true;
                    }
                    self.dirty = true;
                }
            }
        });
        ui.label(format!(
            "目前：{} · 核對 {}",
            self.settings.pricing.tier_label,
            self.settings
                .pricing
                .verified_at
                .as_deref()
                .unwrap_or("未提供")
        ));
        ui.hyperlink_to(
            "官方價格與服務等級",
            "https://developers.openai.com/api/docs/pricing",
        );
        if ui
            .checkbox(
                &mut self.settings.pricing.apply_long_context,
                "Sol／Luna >272,000 輸入／單次請求：輸入與快取 ×2、輸出 ×1.5",
            )
            .changed()
        {
            self.request_reprice();
            self.dirty = true;
        }
        ui.small("單次請求量不明時金額為未知。未另計 cache-write、區域附加費、工具費；服務等級需自行核對。價格可變，核對日 2026-10-17。");
        if ui.text_edit_singleline(&mut self.price_model).changed() {
            if let Some(p) = self.settings.pricing.rates.get(&self.price_model) {
                self.price_values = [
                    p.uncached_input_per_million,
                    p.cached_input_per_million,
                    p.output_per_million,
                ];
                self.price_configured = true;
            } else {
                self.price_values = [0.; 3];
                self.price_configured = false;
            }
        }
        ui.checkbox(&mut self.price_configured, "已確認此模型價格");
        egui::Grid::new("prices").show(ui, |ui| {
            for (i, label) in ["未快取輸入", "快取輸入", "輸出（已含推理）"]
                .iter()
                .enumerate()
            {
                ui.label(*label);
                ui.add(
                    egui::DragValue::new(&mut self.price_values[i])
                        .range(0.0..=100000.0)
                        .speed(0.05)
                        .max_decimals(6),
                );
                ui.end_row();
            }
        });
        if ui.button("套用價格並重算").clicked() {
            self.settings.pricing.tier_label = "自訂價格".into();
            self.settings.pricing.verified_at = None;
            if self.price_configured && !self.price_model.trim().is_empty() {
                self.settings.pricing.rates.insert(
                    self.price_model.trim().into(),
                    ModelPrice {
                        uncached_input_per_million: self.price_values[0],
                        cached_input_per_million: self.price_values[1],
                        output_per_million: self.price_values[2],
                    },
                );
            } else {
                self.settings.pricing.rates.remove(self.price_model.trim());
            }
            self.request_reprice();
            self.dirty = true;
            self.notice = "價格已套用。估算不是訂閱實際費用。".into();
        }
        let mut override_model = self
            .settings
            .pricing
            .model_override
            .clone()
            .unwrap_or_default();
        ui.label("模型覆寫（留空依記錄；會明示覆寫）");
        if ui.text_edit_singleline(&mut override_model).changed() {
            self.settings.pricing.model_override =
                (!override_model.trim().is_empty()).then(|| override_model.trim().to_string());
            self.request_reprice();
            self.dirty = true;
        }
        ui.separator();
        ui.label("$—：價格或可歸屬資料不足。*：部分資料，可能有未觀察區間／首次累計快照／模型歸屬不完整。額度在 0% 時仍可持續累計 Token 等價費用。");
        if !self.notice.is_empty() {
            ui.label(&self.notice);
        }
        if ui.button("儲存設定").clicked() {
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
        let day = if tokens { self.token_date } else { self.date };
        let folder = self.dir.join("exports");
        let pricing = snapshot.pricing.clone();
        let hide_unchanged = self.settings.hide_unchanged;
        let bucket = self.settings.bucket.clone();
        let selection = self.history_selection.clone().or_else(|| {
            self.selected()
                .map(|view| (view.source_key, Some(view.window_key), view.window_label))
        });
        let (tx, rx) = mpsc::channel();
        self.export_rx = Some(rx);
        self.notice = "正在背景匯出…".into();
        let spawned = std::thread::Builder::new()
            .name("quota-export".into())
            .spawn(move || {
                let kind = if audit_excluded {"tokens-excluded"}else if tokens { "tokens" } else { "quota" };
                let usage_events=if audit_excluded {&snapshot.excluded_events[..]}else{snapshot.events()};
                let filename = format!(
                    "{kind}-{day}-{}.{}",
                    Utc::now().format("%H%M%S%3f"),
                    if json { "json" } else { "csv" }
                );
                let path = folder.join(filename);
                let mut completed_export: Vec<CompletedInterval> = Vec::new();
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
                    let costs = accounting::history_costs(&snapshot.cost_index, &all);
                    completed_export = intervals::completed_intervals(snapshot.events(), &all).into_iter().filter(|interval|local_day(interval.end)==Some(day)).collect();
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
                            }
                            v
                        })
                        .collect()
                };
                for event in &mut events {
                    let timestamp = event[if tokens {
                        "timestamp"
                    } else {
                        "observed_at_unix"
                    }]
                    .as_i64();
                    event["local_time"] = serde_json::json!(timestamp.map(local_time));
                    event["local_datetime"] = serde_json::json!(timestamp
                        .and_then(|t| DateTime::<Utc>::from_timestamp(t, 0))
                        .map(|d| d
                            .with_timezone(&Local)
                            .format("%Y-%m-%d %H:%M:%S %:z")
                            .to_string()));
                    event["excluded_from_default_totals"] = serde_json::json!(audit_excluded);
                    event["pricing_basis"] = serde_json::json!(pricing.tier_label);
                    event["pricing_verified_at"] = serde_json::json!(pricing.verified_at);
                    event["long_context_adjustment"] =
                        serde_json::json!(pricing.apply_long_context);
                }
                let result = (|| -> std::io::Result<()> {
                    std::fs::create_dir_all(&folder)?;
                    if json {
                        storage::save_json(
                            &path,
                            &serde_json::json!({
                                "schema_version": 1,
                                "date": day.to_string(),
                                "timezone": Local::now().offset().to_string(),
                                "kind": kind,
                                "cost_basis": "estimated_model_equivalent_not_subscription_charge",
                                "view_scope":if audit_excluded {"excluded_auto_review_audit"}else{"included_default"},
                                "excluded_auto_review_records":snapshot.excluded_events.iter().filter(|e|local_day(e.timestamp)==Some(day)).count(),
                                "exclusion_is_not_free_billing_claim":true,
                                "pricing": pricing,
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
                            }),
                        )
                    } else {
                        let columns: Vec<String> = if tokens {
                            vec![
                                "timestamp",
                                "local_time",
                                "local_datetime",
                                "model",
                                "model_assumed",
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
                            ]
                        } else {
                            vec![
                                "observed_at_unix",
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
                                "interval_end",
                                "interval_percentage_points",
                                "interval_known_usd",
                                "interval_cost_per_point",
                                "interval_comparable",
                                "interval_reasons",
                                "interval_models",
                                "pricing_basis",
                                "pricing_verified_at",
                                "long_context_adjustment",
                            ]
                        }
                        .into_iter()
                        .map(str::to_owned)
                        .collect();
                        let mut out = columns.join(",");
                        out.push('\n');
                        for e in &events {
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
                        std::fs::write(&path, out)?;
                        if !tokens {
                            let model_path = path.with_file_name(format!("{}-models.csv", path.file_stem().unwrap_or_default().to_string_lossy()));
                            let mut models_csv=String::from("interval_key,start,end,percentage_points,interval_known_usd,interval_cost_per_point,comparable,partial,reasons,model,events,input,cached,uncached,output,reasoning,model_known_usd,unknown_cost_events,assumed_model_events\n");
                            for interval in &completed_export {for model in &interval.models {
                                let values=vec![interval.history_event_key.clone(),interval.start.map(|v|v.to_string()).unwrap_or_default(),interval.end.to_string(),interval.percentage_points.to_string(),interval.known_usd.to_string(),interval.cost_per_point.map(|v|v.to_string()).unwrap_or_default(),interval.comparable.to_string(),interval.partial.to_string(),interval.reasons.iter().map(|r|r.label()).collect::<Vec<_>>().join("; "),model.model.clone(),model.events.to_string(),model.input.to_string(),model.cached.to_string(),model.uncached.to_string(),model.output.to_string(),model.reasoning.to_string(),model.known_usd.to_string(),model.unknown_cost_events.to_string(),model.assumed_model_events.to_string()];
                                models_csv.push_str(&values.into_iter().map(|mut value|{if value.starts_with(['=','+','-','@','\t','\r']){value.insert(0,'\'');}format!("\"{}\"",value.replace('"',"\"\""))}).collect::<Vec<_>>().join(",")); models_csv.push('\n');
                            }}
                            std::fs::write(model_path,models_csv)?;
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
            });
        match spawned {
            Ok(join) => self.export_join = Some(join),
            Err(error) => {
                self.export_rx = None;
                self.notice = format!("無法啟動背景匯出：{error}");
            }
        }
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
    }
}
impl eframe::App for WidgetApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
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
        ctx.request_repaint_after(Duration::from_secs(1));
    }
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        [0., 0., 0., 0.]
    }
    fn on_exit(&mut self, _: Option<&eframe::glow::Context>) {
        self.save_state();
        self.bridge = None;
        if let Some(join) = self.export_join.take() {
            let _ = join.join();
        }
        let _ = self.worker.shutdown();
    }
}

impl Drop for WidgetApp {
    fn drop(&mut self) {
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
    fn async_export_is_joined_before_app_drop_finishes() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = synthetic_export_app(temp.path().to_path_buf());
        app.export_day(true, true);
        drop(app);
        let files: Vec<_> = std::fs::read_dir(temp.path().join("exports"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1);
        let data: serde_json::Value = storage::load_json(&files[0]).unwrap();
        assert_eq!(data["kind"], "tokens");
        assert!(!data["events"].as_array().unwrap().is_empty());
    }
}

#[cfg(test)]
mod widget_revision_tests {
    use super::*;
    fn demo_app() -> WidgetApp {
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
        let ctx = egui::Context::default();
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(310., 332.));
        let pos = Pos2::new(60., 284.);
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
    fn saved_theme_pins_first_frame_and_button_text_against_system_theme() {
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
            let mut found = 0;
            for shape in output.shapes {
                if let egui::Shape::Text(text) = shape.shape {
                    if ["扣額紀錄", "Token明細"].contains(&text.galley.job.text.as_str()) {
                        found += 1;
                        assert!(text
                            .galley
                            .job
                            .sections
                            .iter()
                            .all(|section| section.format.color == expected));
                    }
                }
            }
            assert_eq!(found, 2);
        }
    }
}
