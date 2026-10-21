# Quota Widget 0.3.0

原生 Rust + eframe 桌面小工具，重建自 0.2.1 的畫面、已恢復字型／圖示、格式與行為證據。本專案是新的可維護實作，不是聲稱找回遺失的原始碼。沒有 Electron、WebView、Node 或 Python 常駐服務。

## 功能

- 深／淺色、拖曳、右下角縮放、最上層偏好與位置保存。
- 官方 Codex CLI app-server 唯讀額度連線：只有 initialize、initialized、account/rateLimits/read。
- 真實來源的額度、點數、重設倒數；缺資料顯示「—」。不登入、不開啟模型工作、不讀 auth.json。
- 額度 metadata／重設時間漂移不製造 0→0 重設事件。實際回補建立新的週期。
- 近期下降的加權速率與具理由／信心範圍的估計；過期的估計保留過期狀態，不假裝一定準時扣額。
- 每日扣額紀錄、每日 Token 明細、日期選單、前後一天、CSV／JSON 匯出。
- 主視窗底部按 Task 分行：可複製的短 ID、最新事件時間、單筆估算費用、本次百分比區間內該 Task 累計；可捲動。保留最近 24 小時或本區間內有記錄的 Task，真實扣額／回補後開新區間，分界未知顯示 $—。底行保留整體累計。
- 預設 30 天日期瀏覽，保留可匯入的更早歷史。只有實際額度觀察才還原額度歷史；Token 不能反推百分比。
- 新版 token_usage_record 以 (thread_id, response_id) 去重，每筆 usage 是單次 response；舊版 token_count 以累計差分回退。Turn／Task（thread_id）分組只加總去重後事件，不累加累計快照。混合格式無法確定對應時優先 response 記錄並明示部分覆蓋，避免雙計。
- 遞迴掃描指定 sessions 目錄所有 JSONL；增量位元組 offsets、session 累計差分與去重可跨重啟保存。
- 記錄相同累計值的重複通知不重計；不同請求的相同用量不靠數字去重。截斷、重寫、累計重設與缺漏標示部分資料。
- 輸入是單次請求整份上下文（不是僅新增提示詞），輸入＝快取＋未快取；推理包含在輸出內，不重複計費。模型未知預設 gpt-6.1-sol 並明示假設；6 Luna 只來自實際記錄或手動覆寫。
- 新設定預設採 2026-10-17 核對的官方 Standard 價格假設；可切 Fast／自訂，不覆蓋既有自訂表。完整單次輸入超過 272,000 時，整筆輸入／快取 ×2、輸出 ×1.5。未知價格或單次請求歧義顯示 $—。所有 $ 都是模型等價估算，不是 ChatGPT 訂閱實際帳單。

## 啟動

Windows：解壓後執行 quota-widget.exe，或 Start Demo.bat 試合成範例。

macOS：請看 MACOS-BUILD.md。若交付包沒有真正的 Mach-O arm64 執行檔，不能把原碼／建置腳本當成已建置 Mac App。

Linux：桌面需 X11／Wayland 及 OpenGL。執行 quota-widget。

- 右鍵或「≡」開設定／連接。
- 「扣額紀錄」「Token明細」各開獨立原生視窗。
- CLI 路徑留空可找官方原生執行檔。Windows 不接受 .cmd/.bat/.ps1 或受保護的 WindowsApps 路徑。
- 官方 CLI 需由你先正常安裝、登入。小工具不要求密碼、API key 或 token。
- JSONL 預設位置為 $CODEX_HOME/sessions，否則 ~/.codex/sessions；可改為任意獲准本機資料夾。只讀取，不修改來源。

## 資料與安全

資料目錄：Windows %LOCALAPPDATA%/quota-widget；Mac ~/Library/Application Support/quota-widget；Linux $XDG_CONFIG_HOME/quota-widget 或 ~/.config/quota-widget。

settings.json 保存偏好／價格；history.json 保存額度觀察事件；tokens.json 保存去重後用量、必要的模型／session 識別、來源 offsets。沒有對話正文或憑證。匯出存到 exports/。請把含本機 session／來源路徑的資料視為私人資料。

只有一個寫入者能持有該資料目錄的 OS 鎖；其他視窗改為唯讀。損壞的儲存檔不會以空資料覆寫。程式不自動刪除舊歷史；自行備份後可手動管理。Demo 不讀取或保存真實資料。

估算限制：觀察時間不是精確的後台扣額時間，第一筆累計快照、記錄缺口、重設／compaction 或跨模型累計有歧義時以 *／部分資料顯示。$— 不是 $0。0% 額度時仍可能有 Token 費用。只有完整可歸屬的觀察區間才能估算每百分點成本。

## 建置與測試

請使用官方 stable Rust（本次驗證 1.99.1），首次建置需要下載 Cargo.lock 所鎖定的 crates。

    cargo fmt -- --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --locked
    cargo build --release --locked

Linux 原生建置需 C 編譯器、pkg-config、X11、Wayland、xkbcommon、OpenGL 開發套件。
Windows GNU 交叉建置需官方 Rust x86_64-pc-windows-gnu target 及 MinGW-w64 GCC：

    cargo build --release --locked --target x86_64-pc-windows-gnu

此原碼版本與 Windows 包的驗證結果以 VALIDATION.md 為準。跨編譯不是 Windows 或 Mac 原生實測。未做 Authenticode／Apple notarization。不要停用安全防護或繞過系統警告。

## CLI / agent 使用

    quota-widget --help
    quota-widget inspect --sessions ./fixtures --pricing ./fixtures/pricing.json
    quota-widget import --sessions ./fixtures --state ./local-state/tokens.json

inspect 不修改來源或儲存檔；import 只修改明確指定 state 並取得寫入鎖。stdout 為 schema_version=1 JSON、stderr 為診斷，exit 0 成功／1 失敗。參考 docs/AGENT_SKILL.md。

## 來源與授權

- 官方協定：https://learn.chatgpt.com/docs/app-server
- 官方 TokenUsageRecord 協定：https://github.com/openai/codex/blob/main/codex-rs/protocol/src/protocol.rs
- 官方 CLI：https://github.com/openai/codex
- GUI：https://docs.rs/eframe/0.32.3/eframe/
- Rust 實作：MIT，見 LICENSE。
- Noto Sans CJK：SIL OFL，見 assets/FONT-LICENSE.txt；其他字型／依賴見 assets/THIRD-PARTY-NOTICES.txt 及 Cargo.lock。
