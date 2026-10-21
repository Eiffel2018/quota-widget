use chrono::{DateTime, Local, NaiveDate, Utc};
use eframe::egui::{
    self, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2, ViewportBuilder,
    ViewportCommand, ViewportId, WindowLevel,
};
use quota_widget::{
    accounting,
    bridge::{Bridge, BridgeCommand, BridgeEvent},
    model::{HistoryEvent, QuotaModel, WindowView},
    storage::{self, DataLock, Settings},
    tokens::{ModelPrice, Pricing, TokenStore},
    usage_views::{self, GroupBy},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize, Default)]
struct HistoryFile {
    version: u32,
    events: Vec<HistoryEvent>,
}
struct ImportResult {
    store: TokenStore,
    history: Option<Vec<HistoryEvent>>,
    report: String,
    error: Option<String>,
}
struct WidgetApp {
    settings: Settings,
    dir: PathBuf,
    _lock: Option<DataLock>,
    persistent: bool,
    read_state: bool,
    demo: bool,
    offline: bool,
    quota: QuotaModel,
    tokens: TokenStore,
    bridge: Option<Bridge>,
    status: String,
    history_open: bool,
    history_selection: Option<(String, Option<String>, String)>,
    tokens_open: bool,
    settings_open: bool,
    date: NaiveDate,
    token_date: NaiveDate,
    token_group: GroupBy,
    import_rx: Option<mpsc::Receiver<ImportResult>>,
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
    settings.size[1] = settings.size[1].clamp(350., 1200.);
    let mut viewport = ViewportBuilder::default()
        .with_inner_size(settings.size)
        .with_min_inner_size([276., 350.])
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
            _lock: None,
            persistent,
            read_state: persistent,
            demo,
            offline,
            quota: QuotaModel::new(format!("local-{}", now())),
            tokens: TokenStore::default(),
            bridge: None,
            status: "OFFLINE".into(),
            history_selection: None,
            history_open: args.iter().any(|s| s == "--show-history"),
            tokens_open: args.iter().any(|s| s == "--show-tokens"),
            settings_open: false,
            date: Local::now().date_naive(),
            token_date: Local::now().date_naive(),
            token_group: GroupBy::Response,
            import_rx: None,
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
        if self.read_state {
            match storage::load_json::<HistoryFile>(&self.dir.join("history.json")) {
                Ok(h) if h.version == 1 || h.version == 2 => self.quota.history = h.events,
                Ok(_) => {
                    self.notice = "歷史檔版本較新；已停用寫入，保留原檔。".into();
                    self.persistent = false;
                }
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    self.notice = "既有歷史檔無法讀取；已停用寫入，原檔保留。".into();
                    self.persistent = false;
                }
                _ => {}
            }
            match TokenStore::load(self.dir.join("tokens.json")) {
                Ok(s) => self.tokens = s,
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    self.notice = "Token 儲存檔無法讀取；已停用寫入，原檔保留。".into();
                    self.persistent = false;
                }
                _ => {}
            }
        }
        self.tokens.reprice(&self.settings.pricing);
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
        } else {
            if !self.offline {
                self.connect();
            }
            if self.settings.import_enabled {
                self.start_import();
            }
        }
    }
    fn apply_theme(&self, ctx: &egui::Context) {
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
        if self.demo || self.import_rx.is_some() || !self.settings.import_enabled {
            return;
        }
        let root = PathBuf::from(self.settings.sessions_path.trim());
        if !root.is_dir() {
            self.import_note = "尚未找到本機 Codex sessions 資料夾。可在設定選擇路徑。".into();
            self.last_import = Instant::now();
            return;
        }
        let mut store = self.tokens.clone();
        let pricing = self.settings.pricing.clone();
        let quota_count = store.quota_observations().len();
        let history = self.quota.history.clone();
        let (tx, rx) = mpsc::channel();
        self.import_rx = Some(rx);
        self.last_import = Instant::now();
        std::thread::spawn(move || {
            let result = store.import_dir(&root, &pricing);
            let (report, error) = match result {
                Ok(r) => (
                    format!(
                        "已掃描 {} 個檔案，本輪新增 {} 筆；格式問題 {}，未完成行 {}。",
                        r.files_scanned, r.events_added, r.malformed_lines, r.partial_lines
                    ),
                    None,
                ),
                Err(_) => (
                    String::new(),
                    Some("讀取本機歷史失敗；請檢查路徑與正常讀取權限。".into()),
                ),
            };
            let history = if store.quota_observations().len() != quota_count {
                Some(quota_widget::history::merge(
                    history,
                    quota_widget::history::replay(store.quota_observations()),
                ))
            } else {
                None
            };
            let _ = tx.send(ImportResult {
                store,
                history,
                report,
                error,
            });
        });
    }
    fn pump(&mut self) {
        let messages: Vec<_> = self
            .bridge
            .as_ref()
            .map(|b| b.events.try_iter().collect())
            .unwrap_or_default();
        for e in messages {
            match e {
                BridgeEvent::Connecting => self.status = "CONNECTING".into(),
                BridgeEvent::Connected => self.status = "CONNECTED · 等待額度".into(),
                BridgeEvent::RateLimits(v) => match self.quota.ingest_json(&v, now()) {
                    Ok(_) => {
                        self.status = "LIVE".into();
                        self.dirty = true;
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
        let imported = match self.import_rx.as_ref().map(|rx| rx.try_recv()) {
            Some(Ok(r)) => Some(r),
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.import_rx = None;
                self.import_note = "匯入工作中斷；可重新掃描。".into();
                None
            }
            _ => None,
        };
        if let Some(r) = imported {
            self.import_rx = None;
            if let Some(e) = r.error {
                self.import_note = e;
            } else {
                self.tokens = r.store;
                self.tokens.reprice(&self.settings.pricing);
                self.import_note = r.report;
                if let Some(history) = r.history {
                    self.quota.history = quota_widget::history::merge(
                        history,
                        std::mem::take(&mut self.quota.history),
                    );
                }
                self.dirty = true;
            }
        }
        if !self.demo && self.last_import.elapsed() > Duration::from_secs(30) {
            self.start_import();
        }
        if self.dirty && self.last_save.elapsed() > Duration::from_secs(5) {
            self.save_state();
        }
    }
    fn save_state(&mut self) {
        if !self.persistent {
            return;
        }
        let h = HistoryFile {
            version: 2,
            events: self.quota.history.clone(),
        };
        let result = storage::save_json(&self.dir.join("settings.json"), &self.settings)
            .and_then(|_| storage::save_json(&self.dir.join("history.json"), &h))
            .and_then(|_| self.tokens.save(self.dir.join("tokens.json")));
        if result.is_err() {
            self.notice = "儲存失敗；目前畫面資料仍保留，請檢查磁碟空間與資料夾權限。".into();
        } else {
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
            .quota
            .history
            .iter()
            .filter(|e| drop_event(e) || reset_event(e))
            .find(|e| {
                e.source_key == self.settings.bucket
                    && (e.window_label.contains("週") || e.window_label.contains("week"))
            })
            .or_else(|| {
                self.quota
                    .history
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
            .quota
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
                .quota
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
    fn event_cost(&self, event: &HistoryEvent, all: &[HistoryEvent]) -> (Option<f64>, bool) {
        accounting::event_cost(self.tokens.events(), event, all)
    }
    fn widget(&mut self, ctx: &egui::Context) {
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
                let h = rect.height();
                let sx = w / 310.;
                let sy = h / 360.;
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
                    rect.shrink(4.),
                    ui.id().with("drag"),
                    Sense::click_and_drag(),
                );
                if drag.is_pointer_button_down_on()
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
                        egui::Button::new(RichText::new("扣額紀錄").size(11. * scale)).fill(line),
                    )
                    .clicked()
                {
                    self.history_open = true;
                }
                if ui
                    .put(
                        Rect::from_min_max(pt(230., 12.), pt(295., 37.)),
                        egui::Button::new(RichText::new("Token明細").size(11. * scale)).fill(line),
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
                let status = if self.demo {
                    "DEMO · 示範資料"
                } else if view
                    .as_ref()
                    .is_some_and(|v| v.status == quota_widget::model::WindowStatus::Stale)
                {
                    "STALE · 資料過期"
                } else {
                    self.status.as_str()
                };
                txt(
                    21.,
                    238.,
                    format!("Task 用量 · {status} · 估算 USD"),
                    11.,
                    muted,
                );
                let all = self.matching_history();
                let last = all.iter().filter(|e| e.observed_at_unix <= now() && (drop_event(e) || reset_event(e)) && !unchanged(e)).max_by_key(|e| e.observed_at_unix);
                let boundary = last.map(|e| e.observed_at_unix);
                let tasks = usage_views::task_rows(self.tokens.events(), boundary, now());
                for (x, label) in [(21., "Task ID"), (75., "時間"), (139., "用度"), (213., "本%累計")] { txt(x, 255., label.into(), 10., muted); }
                ui.interact(Rect::from_min_max(pt(21.,254.),pt(289.,271.)), ui.id().with("task-column-help"), Sense::hover()).on_hover_text("用度＝該 Task 最新單筆 Token／Response 的估算 USD；本%累計＝最近真正扣額或重設之後，僅此 Task 已記錄事件的合計。缺漏以 *／$— 標示，沒有新事件顯示 0。價格／模型假設見 Token 明細。");
                ui.scope_builder(egui::UiBuilder::new().max_rect(Rect::from_min_max(pt(21.,272.),pt(289.,324.))), |ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(0.,0.);
                    egui::ScrollArea::vertical().id_salt("task-interval-rows").max_height(52.*sy).min_scrolled_height(0.).show(ui, |ui| {
                        if tasks.is_empty() { ui.label(RichText::new("尚無近期 Task 用量").size(10.*scale).color(muted)); }
                        for task in &tasks {
                            let (r,_) = ui.allocate_exact_size(Vec2::new(268.*sx,18.*sy),Sense::hover());
                            let id_rect = Rect::from_min_size(r.min,Vec2::new(50.*sx,18.*sy));
                            let short = task.task_id.chars().take(7).collect::<String>();
                            if ui.put(id_rect,egui::Button::new(RichText::new(short).size(9.5*scale).color(fg)).frame(false)).on_hover_text(format!("{}\n點擊複製完整 Task ID；最近 24 小時或本區間內有記錄的 Task。",task.task_id)).clicked() { ctx.copy_text(task.task_id.clone()); }
                            for (x,text) in [(54.,local_time(task.latest_timestamp)),(118.,money(task.latest_cost_usd)),(192.,format!("{}{}",money(task.interval_cost_usd),if task.partial {"*"} else {""}))] {
                                ui.painter().text(r.min+Vec2::new(x*sx,2.*sy),Align2::LEFT_TOP,text,FontId::proportional(9.5*scale),fg);
                            }
                        }
                    });
                });
                let footer = if let Some(e) = last {
                    let t = e.observed_at_unix;
                    let (c, p, _) = accounting::sum_cost(self.tokens.events(), t, now());
                    format!(
                        "{} {} 分鐘／累計 {}{}",
                        if reset_event(e) {
                            "距本次重設"
                        } else {
                            "距上次扣額"
                        },
                        (now() - t).max(0) / 60,
                        money(c),
                        if p { " *" } else { "" }
                    )
                } else {
                    "距上次扣額 — 分鐘／累計 $—".into()
                };
                txt(21., 330., footer, 11., muted);
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
                    Rect::from_min_max(pt(294., 344.), pt(310., 360.)),
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
    fn days(&self) -> BTreeSet<NaiveDate> {
        let today = Local::now().date_naive();
        let mut dates: BTreeSet<_> = (0..self.settings.history_days.clamp(1, 3650))
            .filter_map(|i| today.checked_sub_days(chrono::Days::new(i as u64)))
            .collect();
        for e in &self.quota.history {
            if let Some(d) = local_day(e.observed_at_unix) {
                dates.insert(d);
            }
        }
        for e in self.tokens.events() {
            if let Some(d) = local_day(e.timestamp) {
                dates.insert(d);
            }
        }
        dates
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
        let sources: BTreeSet<_> = self
            .quota
            .history
            .iter()
            .map(|e| {
                (
                    e.source_key.clone(),
                    e.window_key.clone(),
                    e.window_label.clone(),
                )
            })
            .collect();
        egui::ComboBox::from_label("來源 / 額度視窗")
            .selected_text(
                self.history_selection
                    .as_ref()
                    .map(|(s, k, l)| format!("{s} · {l} · {}", k.as_deref().unwrap_or("舊版")))
                    .unwrap_or_else(|| "目前小工具選擇".into()),
            )
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.history_selection, None, "目前小工具選擇");
                for (s, k, l) in sources {
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
        if let Some(v) = self.selected() {
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
        let all = self.browsed_history();
        let selected: Vec<_> = all
            .iter()
            .filter(|e| {
                local_day(e.observed_at_unix) == Some(self.date)
                    && (!self.settings.hide_unchanged || !unchanged(e))
            })
            .collect();
        ui.label(format!(
            "{} 筆 · * 代表資料不完整或價格未設定。較早資料只有實際額度觀察時才會還原。",
            selected.len()
        ));
        ui.separator();
        let buckets: Vec<_> = selected
            .iter()
            .filter(|e| drop_event(e))
            .map(|e| {
                let (c, p) = self.event_cost(e, &all);
                accounting::CostBucket {
                    start: 0,
                    end: e.observed_at_unix,
                    percentage_points: e.delta.parse().unwrap_or(0.),
                    estimated_usd: c,
                    partial: p,
                    events: 1,
                }
            })
            .collect();
        ui.label(format!(
            "可完整歸屬區間的加權成本：{} / 百分點",
            money(accounting::weighted_per_point(&buckets))
        ));
        egui::ScrollArea::vertical().show(ui, |ui| {
            if selected.is_empty() {
                ui.label("這一天沒有可用的額度變動觀察。Token 使用量不能反推訂閱額度。");
            }
            for e in selected.iter().rev() {
                ui.horizontal(|ui| {
                    ui.strong(local_time(e.observed_at_unix));
                    if drop_event(e) {
                        let (c, p) = self.event_cost(e, &all);
                        ui.label(format!(
                            "{}% → {}% · 扣除 {} 百分點 · {}{}",
                            e.before,
                            e.after,
                            e.delta,
                            money(c),
                            if p { " *" } else { "" }
                        ));
                    } else if reset_event(e) {
                        ui.label(format!("{}% → {}% · 額度補回 / 新週期", e.before, e.after));
                    } else {
                        ui.label(format!("{} → {} · {}", e.before, e.after, kind(e)));
                    }
                });
                ui.small(format!(
                    "{} · {} · 週期 {}",
                    e.window_label, e.source_key, e.epoch
                ));
                ui.separator();
            }
        });
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
            self.settings.pricing.tier_label
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
        ui.small("Turn／Task 只加總已去重的 response 用量，不累加 turn／thread 快照。Task＝原始 thread_id；沒有 Turn ID 時不猜測合併。");
        let events: Vec<_> = self
            .tokens
            .events()
            .iter()
            .filter(|e| local_day(e.timestamp) == Some(self.token_date))
            .collect();
        let (input, cached, output, reasoning) =
            events.iter().fold((0u64, 0u64, 0u64, 0u64), |a, e| {
                (
                    a.0.saturating_add(e.input),
                    a.1.saturating_add(e.cached),
                    a.2.saturating_add(e.output),
                    a.3.saturating_add(e.reasoning),
                )
            });
        let missing = events.iter().filter(|e| e.cost_usd.is_none()).count();
        let cost = events.iter().filter_map(|e| e.cost_usd).sum::<f64>();
        ui.label(format!(
            "{} 筆 · 輸入 {}（快取 {}／未快取 {}）· 輸出 {}（推理 {}）· 已可估部分 {}{}",
            events.len(),
            input,
            cached,
            input.saturating_sub(cached),
            output,
            reasoning,
            money(Some(cost)),
            if missing > 0 {
                format!("；{} 筆未定價", missing)
            } else {
                String::new()
            }
        ));
        ui.small(&self.import_note);
        ui.separator();
        let rows = usage_views::group(&events, self.token_group);
        egui::ScrollArea::both().show(ui, |ui| {
            egui::Grid::new("tokens-table")
                .striped(true)
                .min_col_width(65.)
                .show(ui, |ui| {
                    for h in [
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
                    ] {
                        ui.strong(h);
                    }
                    ui.end_row();
                    for e in rows.iter().rev() {
                        ui.label(local_time(e.last_timestamp))
                            .on_hover_text(format!(
                                "{} 至 {}",
                                local_time(e.first_timestamp),
                                local_time(e.last_timestamp)
                            ));
                        ui.label(format!(
                            "{}{}",
                            e.models.join(" / "),
                            if e.model_assumed { "（假設）" } else { "" }
                        ))
                        .on_hover_text(e.partial_flags.join("\n"));
                        ui.label(e.session.chars().take(12).collect::<String>())
                            .on_hover_text(&e.session);
                        let identity = match self.token_group {
                            GroupBy::Response => e.response_id.as_deref().unwrap_or("舊版／未知"),
                            GroupBy::Turn => e.turn_id.as_deref().unwrap_or("未知 Turn"),
                            GroupBy::Task => "當日事件合計",
                        };
                        ui.label(identity.chars().take(12).collect::<String>())
                            .on_hover_text(format!(
                                "Turn: {}\nResponse: {}\n來源: {}",
                                e.turn_id.as_deref().unwrap_or("未知"),
                                e.response_id.as_deref().unwrap_or("未知"),
                                e.source_kinds.join(", ")
                            ));
                        ui.monospace(e.event_count.to_string());
                        for n in [e.input, e.cached, e.uncached, e.output, e.reasoning] {
                            ui.monospace(n.to_string());
                        }
                        ui.label(format!(
                            "{}{}",
                            money(e.cost_usd),
                            if e.partial_flags.is_empty() { "" } else { " *" }
                        ))
                        .on_hover_text(format!(
                            "已可估部分 {}；{} 筆費用未知",
                            money(Some(e.known_cost_subtotal_usd)),
                            e.unknown_cost_events
                        ));
                        ui.end_row();
                    }
                });
        });
        if events.is_empty() {
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
        ui.small("預設 30 天瀏覽，仍保留並可選更早日期。只儲存用量、模型、匿名 session／來源識別與必要觀察，不儲存對話內容。");
        ui.label(&self.import_note);
        ui.separator();
        ui.strong("模型等價價格（USD / 1M tokens）");
        ui.label("全新設定採官方 Standard 費率假設；不是訂閱扣款。未知模型預設 6.1 Sol 並顯示假設；6 Luna 只依記錄或覆寫。");
        ui.horizontal(|ui| {
            for (label, preset) in [
                ("官方 Standard", Pricing::official_standard()),
                ("官方 Fast / Priority", Pricing::official_fast()),
            ] {
                if ui.button(label).clicked() {
                    let current_override = self.settings.pricing.model_override.clone();
                    self.settings.pricing = preset;
                    self.settings.pricing.model_override = current_override;
                    self.tokens.reprice(&self.settings.pricing);
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
            self.tokens.reprice(&self.settings.pricing);
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
            self.tokens.reprice(&self.settings.pricing);
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
            self.tokens.reprice(&self.settings.pricing);
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
        let day = if tokens { self.token_date } else { self.date };
        let kind = if tokens { "tokens" } else { "quota" };
        let folder = self.dir.join("exports");
        let filename = format!(
            "{kind}-{day}-{}.{}",
            Utc::now().format("%H%M%S%3f"),
            if json { "json" } else { "csv" }
        );
        let path = folder.join(filename);
        let mut events: Vec<serde_json::Value> = if tokens {
            self.tokens
                .events()
                .iter()
                .filter(|e| local_day(e.timestamp) == Some(day))
                .filter_map(|e| serde_json::to_value(e).ok())
                .collect()
        } else {
            let all = self.browsed_history();
            all.iter()
                .filter(|e| {
                    local_day(e.observed_at_unix) == Some(day)
                        && (!self.settings.hide_unchanged || !unchanged(e))
                })
                .map(|e| {
                    let (mut v, (cost, partial)) = (
                        serde_json::to_value(e).unwrap_or_default(),
                        self.event_cost(e, &all),
                    );
                    v["estimated_cost_usd"] =
                        serde_json::json!(if drop_event(e) { cost } else { None });
                    v["partial"] = serde_json::json!(partial);
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
            event["pricing_basis"] = serde_json::json!(self.settings.pricing.tier_label);
            event["pricing_verified_at"] = serde_json::json!(self.settings.pricing.verified_at);
            event["long_context_adjustment"] =
                serde_json::json!(self.settings.pricing.apply_long_context);
        }
        let result = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(&folder)?;
            if json {
                storage::save_json(
                    &path,
                    &serde_json::json!({"schema_version":1,"date":day.to_string(),"timezone":Local::now().offset().to_string(),"kind":kind,"cost_basis":"estimated_model_equivalent_not_subscription_charge","pricing":self.settings.pricing,"events":events,"turn_summaries": if tokens { Some(usage_views::group(&self.tokens.events().iter().filter(|e|local_day(e.timestamp)==Some(day)).collect::<Vec<_>>(),GroupBy::Turn)) } else { None },"task_summaries": if tokens { Some(usage_views::group(&self.tokens.events().iter().filter(|e|local_day(e.timestamp)==Some(day)).collect::<Vec<_>>(),GroupBy::Task)) } else { None }}),
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
                            let mut text = v.as_str().map(str::to_owned).unwrap_or_else(|| {
                                if v.is_null() {
                                    String::new()
                                } else {
                                    v.to_string()
                                }
                            });
                            if v.is_string() && text.starts_with(['=', '+', '-', '@', '\t', '\r']) {
                                text.insert(0, '\'');
                            }
                            format!("\"{}\"", text.replace('"', "\"\""))
                        })
                        .collect();
                    out.push_str(&row.join(","));
                    out.push('\n');
                }
                std::fs::write(&path, out)
            }
        })();
        self.notice = if result.is_ok() {
            format!("已匯出：{}", path.display())
        } else {
            "匯出失敗；請檢查資料夾權限。".into()
        };
    }
    fn make_demo(&mut self) {
        self.status = "DEMO".into();
        self.settings.sessions_path = "DEMO：不讀取任何真實歷史".into();
        self.settings.pricing = Pricing::demo();
        self.tokens = TokenStore::demo(self.demo_start);
        let base = self.demo_start - 1500;
        for i in 0..=50 {
            let used = [55., 56., 58., 59., 60.][(i / 10).min(4)];
            let t = base + i as i64 * 30;
            let _=self.quota.ingest_json(&serde_json::json!({"rateLimits":{"limitId":"codex","limitName":"Codex","primary":{"usedPercent":used,"windowDurationMins":10080,"resetsAt":self.demo_start+390000},"credits":{"hasCredits":true,"unlimited":false,"balance":"62315.77"}}}),t);
        }
        let _=self.quota.ingest_json(&serde_json::json!({"rateLimits":{"limitId":"codex","limitName":"Codex","primary":{"usedPercent":60,"windowDurationMins":10080,"resetsAt":self.demo_start+390000},"credits":{"hasCredits":true,"unlimited":false,"balance":"62315.77"}}}),now());
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
    }
}
