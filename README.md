# Quota Widget v3.35

Reset predictions now clear after a confirmed reset. Use **Reset預測** to enter an announcement time in PST (UTC−08:00) or your configured display UTC offset. Settings windows are positioned near the widget and brought forward when opened.

設定可即時切換繁體中文、简体中文及 English，並記住選擇。標題為「Codex 額度」／「Codex Quota」。消耗率在連續觀測不足時，使用同一重設週期最近已確認扣額計算歷史平均，並標示為歷史；不改動下一次扣額預測的即時樣本。

額度更新通知會觸發即時刷新，另每 10 秒作後備查詢。只有額度變化才新增即時歷史；相同額度只更新連線狀態，不重複寫入。官方介面：[Codex App Server](https://learn.chatgpt.com/docs/app-server)。

## v3.21

每日百分比表由 UTC+08 午夜開始，以當日首次出現的下降作分段邊界；最新分段先計先顯示，之後每完成一段即追加一行。當日以外的日期只在選取時載入；當前額度另列為時間未知的進行中分段。欄位顯示每個模型家族的唯一對話數和 USD。主視窗標題顯示 `v3.21`。

[更新說明](docs/RELEASE-0.3.18.md) · [Windows 驗證](VALIDATION-0.3.18.md)

## 0.3.10

修正 Windows 工作列釘選圖示，提供固定 EXE 及圖示路徑。
[更新說明](docs/RELEASE-0.3.10.md)

## 0.3.9

0.3.9 的歷史版本曾對連續區間套用共同平均估值；0.3.14 起改用每段 Token 成本除以該段百分點，保留實際波動。原始 Token 小計仍獨立保留。

[0.3.9 計法與更新說明](docs/RELEASE-0.3.9.md) · [Windows 驗證](VALIDATION-0.3.9.md) · [已恢復的版本歷史](docs/SOURCE-HISTORY.md)

建置及測試：`cargo test --locked`、`cargo build --release --locked`。
編譯輸出及測試暫存可透過 `CARGO_TARGET_DIR`、`TEMP`、`TMP` 放到 RAM Drive；
重要 source、發佈包及驗證結果應保存在有明確名稱的正式目錄。

`scripts/build-windows.ps1` 完成檢查及編譯後，會固定部署到 repository 上一層的
`quota-widget.exe`；之後只需重啟同一個 EXE，捷徑毋須隨版本修改。
未設定 `CARGO_TARGET_DIR` 時，編譯 cache 使用系統 `TEMP` 下的 `quota-widget-build`。
部署會校驗 SHA-256，並保留上一個 EXE 為 `quota-widget.previous.exe`。
亦可用 `scripts/deploy-windows.ps1 -SourcePath <已建置的EXE>` 單獨部署。

# Quota Widget 0.3.8

Hide inactive historical subagent branches from the live Task tree. Keep recent/current-interval contributors and their required ancestors. Historical accounting is preserved. See [release notes](docs/RELEASE-0.3.8.md) and [validation](VALIDATION-0.3.8.md).

# Quota Widget 0.3.7

Readable, high-contrast text and compact label/value UI. See [release notes](docs/RELEASE-0.3.7.md) and [validation](VALIDATION-0.3.7.md). Windows x64 is cross-built and unsigned; native Windows runtime remains unverified. Accounting and data storage are unchanged.

# Quota Widget 0.3.6

Reconnect recovery and cooperative responsive shutdown. See [release notes](docs/RELEASE-0.3.6.md) and [validation](VALIDATION-0.3.6.md). Windows x64 is cross-built; native Windows runtime is unverified. Original CLI failure cause remains unknown.

# Quota Widget 0.3.5

原生 Rust + eframe 桌面小工具，重建自 0.2.1 的畫面、已恢復字型／圖示、格式與行為證據。本專案是新的可維護實作，不是聲稱找回遺失的原始碼。沒有 Electron、WebView、Node 或 Python 常駐服務。

## 0.3.5 增量儲存與速度修正

- 用量改存本機 SQLite（隨 EXE 靜態編譯，不需另外安裝）。原始證據按 Task 壓縮保留，新增資料只更新受影響的 Task、索引與區間快取，不再每次重寫整份 tokens.json。
- 第一次開啟會驗證及轉換舊 JSON，顯示進度；原 tokens.json 保留原樣。轉換中斷可以續做，只有驗證成功才啟用新資料庫。第一次仍須完整讀取舊 JSON，之後啟動直接查索引。
- 主視窗只接收近期 Task 的最新事件；每日明細每頁 500 行，總和仍涵蓋全日。增量調整當日／本%總和；Turn／Task 分組只重查受影響的群組。未知／部分小計繼續明示。
- 同一額度週期內反覆 +1 百分點依使用者指定視為無變動：86→87→86 的即時讀數固定為 86，不重設累計或多計扣額；有可信到期／新週期證據的真正重設仍保留。亦修正鏡像來源假缺口及零變動 epoch 干擾；原始額度觀察不刪除。
- 支援完整相容 JSON 匯出供備份／回退，詳見 docs/INCREMENTAL-STORAGE-CLI.md。舊 EXE 不認識 SQLite，請勿混用同一資料目錄。
- 保留既有深色、香港時間、每個模型自訂價、Task 樹及每日／每%明細。

## 0.3.4 試用修正

- 修復原版 history.json 數字 session、settings.json 的 bucket:null 相容性。首次實際改寫舊格式前建立獨立、位元組完全相同的備份；讀取不改動原檔。
- 正確呈現原版 metadata 變動誤標為重設的下降紀錄，保留原來百分比／時間／來源；0→0 不作新的扣額分界。舊版匿名來源不猜測對應現有帳戶。
- 修復匯入快照的重設證據與基準處理；有差異的觀察保留原數字，部分小計不當作每%價格。
- 新增每%用度表、起迄時間／歷時／總用度／每百分點及模型分項。主視窗底部累計行懸停顯示最近 5 個區間，點擊開表格。
- 近期 Task 保留期限由 24 小時縮短為 2 小時；本扣額區間有用量的 Task 及既有樹狀關係規則保留。

Windows 請先關閉舊版再開新 EXE；不需要刪除設定或歷史。這是交叉編譯的 Windows 試用包，沒有宣稱已在 Windows 原生環境運行測試。實際金額仍是 API 模型等價估算。

## 功能

- 深／淺色、拖曳、右下角縮放、最上層偏好與位置保存。
- 官方 Codex CLI app-server 唯讀額度連線：只有 initialize、initialized、account/rateLimits/read。
- 真實來源的額度、點數、重設倒數；缺資料顯示「—」。不登入、不開啟模型工作、不讀 auth.json。
- 額度 metadata／重設時間漂移不製造 0→0 重設事件。實際回補建立新的週期。
- 近期下降的加權速率與具理由／信心範圍的估計；過期的估計保留過期狀態，不假裝一定準時扣額。
- 每日扣額紀錄、每日 Token 明細、日期選單、前後一天、CSV／JSON 匯出。
- 主視窗按實際 parent_thread_id 關係列樹狀 Task；主代理自身費用獨立一行，其下 [+] 子代理列合計近期／本%範圍的後代及必要連接節點，每個只計一次。預設收起，展開可看子代理名稱、最新時間、單筆費用和本%累計，支援多層；無關係不猜測、循環獨立顯示。預設最多 10 個可見行，超過才捲動，可在設定修改。滑鼠移到 Task 可看名稱、完整 ID、最新 Response 的 Input／Cached／Uncached／Output／Reasoning 及匹配 Turn 累計 Input／Output。
- 本%累計及底行使用最後已確認扣額／重設作分界，啟動會重算該分界後的已記錄用量。沒有可用分界時自本次啟動以 $0.0000 開始累加並標示 *；已知部分持續相加，未知事件不假裝免費。
- 多個 session 內快取額度快照的衝突保留為不確定觀察，不再當成重設；原始觀察保留，畫面只整理可確認的變動。
- 匯入／存檔／匯出在背景執行，無變更檔案不重讀，不變的價格不重算；主視窗與每日明細使用索引、快取和可視行繪製。
- 預設 30 天日期瀏覽，保留可匯入的更早歷史。只有實際額度觀察才還原額度歷史；Token 不能反推百分比。
- 新版 token_usage_record 以 (thread_id, response_id) 去重，每筆 usage 是單次 response；舊版 token_count 以累計差分回退。Turn／Task（thread_id）分組只加總去重後事件，不累加累計快照。混合格式無法確定對應時優先 response 記錄並明示部分覆蓋，避免雙計。
- 遞迴掃描指定 sessions 目錄所有 JSONL；增量位元組 offsets、session 累計差分與去重可跨重啟保存。
- 記錄相同累計值的重複通知不重計；不同請求的相同用量不靠數字去重。截斷、重寫、累計重設與缺漏標示部分資料。
- 輸入是單次請求整份上下文（不是僅新增提示詞），輸入＝快取＋未快取；推理包含在輸出內，不重複計費。模型未知預設 gpt-6.1-sol 並明示假設；6 Luna 只來自實際記錄或手動覆寫。
- 精確模型 codex-auto-review 依要求從預設 Task 行、子代理數、區間／整體估算及每日明細排除；原始記錄保留，可切到 Token 明細「查看排除紀錄」稽核及匯出。排除不代表免費，官方額度讀數不變。普通 Sol 審查工作不按名稱排除。
- 每筆 Response／可歸屬舊版事件各依實際模型計價；Luna 子代理不繼承 Sol 主代理價格。high／max 不另乘單價。只有未記錄模型時才假設 Sol；有模型名稱但無價格時保持費用未知。
- 新設定預設採 2026-10-17 核對的官方 Standard 價格假設；可切 Fast／自訂，不覆蓋既有自訂表。完整單次輸入超過 272,000 時，整筆輸入／快取 ×2、輸出 ×1.5。未知價格或單次請求歧義顯示 $—。所有 $ 都是模型等價估算，不是 ChatGPT 訂閱實際帳單。

## 啟動

Windows：解壓後執行 quota-widget.exe；合成範例可用 quota-widget.exe --demo。

macOS：請看 MACOS-BUILD.md。若交付包沒有真正的 Mach-O arm64 執行檔，不能把原碼／建置腳本當成已建置 Mac App。

Linux：桌面需 X11／Wayland 及 OpenGL。執行 quota-widget。

- 右鍵或「≡」開設定／連接。
- 「扣額紀錄」「Token明細」各開獨立原生視窗。
- CLI 路徑留空可找官方原生執行檔。Windows 不接受 .cmd/.bat/.ps1 或受保護的 WindowsApps 路徑。
- 官方 CLI 需由你先正常安裝、登入。小工具不要求密碼、API key 或 token。
- JSONL 預設位置為 $CODEX_HOME/sessions，否則 ~/.codex/sessions；可改為任意獲准本機資料夾。只讀取，不修改來源。

## 資料與安全

資料目錄：Windows %LOCALAPPDATA%/quota-widget；Mac ~/Library/Application Support/quota-widget；Linux $XDG_CONFIG_HOME/quota-widget 或 ~/.config/quota-widget。

settings.json 保存偏好／價格；tokens.sqlite3 保存最新 reset 週期的去重用量、壓縮原始用量證據、必要的模型／session 識別、明確對話名稱及上層關係、來源 offsets 和 quota 觀察事件。程式不會自動讀寫舊 history.json，也不會刪除它；如需舊記錄，須另行匯入。確認 quota 回到 100% 時，舊週期的 TokenEvent 明細分別存入 quota-cycle-YYYYMMDD-HHMMSS.sqlite3，保留舊週期檔；主 tokens.sqlite3 只保留最新週期用量，來源 offsets 保留以增量匯入而不重掃 Codex session。舊 tokens.json 原樣保留為轉換前備份；新的用量不再寫入它。資料庫開啟時可能有 -wal／-shm 側檔，請勿自行刪除；程式會先建立週期整理前備份。名稱來自 metadata 或所選 sessions 同層 session_index.jsonl，不從對話正文猜測。沒有對話正文或憑證。匯出存到 exports/。請把含本機 session／來源路徑的資料視為私人資料。

只有一個寫入者能持有該資料目錄的 OS 鎖；其他視窗改為唯讀。損壞的儲存檔不會以空資料覆寫。程式不自動刪除舊歷史；自行備份後可手動管理。Demo 不讀取或保存真實資料。

估算限制：觀察時間不是精確的後台扣額時間，第一筆累計快照、記錄缺口、重設／compaction 或跨模型累計有歧義時以 *／部分資料顯示；部分 Token 覆蓋只作下限，無可用記錄或價格時顯示未知。$— 不是 $0。0% 額度時仍可能有 Token 費用。

## 建置與測試

請使用官方 stable Rust（本次驗證 1.99.1），首次建置需要下載 Cargo.lock 所鎖定的 crates。

    cargo fmt -- --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --locked
    cargo build --release --locked

Linux 原生建置需 C 編譯器、pkg-config、X11、Wayland、xkbcommon、OpenGL 開發套件。
Windows GNU 交叉建置需官方 Rust x86_64-pc-windows-gnu target 及 MinGW-w64 GCC：

    cargo build --release --locked --target x86_64-pc-windows-gnu

此原碼版本與 Windows 包的驗證結果以 VALIDATION-0.3.5.md 為準；舊版 VALIDATION.md 僅作先前版本記錄。跨編譯不是 Windows 或 Mac 原生實測。未做 Authenticode／Apple notarization。不要停用安全防護或繞過系統警告。

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

## 每次扣額的模型分項

扣額紀錄每行的「模型分項」顯示實際觀察區間的起迄、下降百分點、已知估算總和、每百分點平均，以及各 model 的 Input / Cached / Uncached / Output / Reasoning / 費用。按每筆實際模型計價，不使用 parent 同價假設。多百分點跳變只保留整段平均，不製造單一百分點資料。

完整區間才能直接比較；首次觀察、重設邊界、缺口、未知價格或假設模型都附限制。官方 quota 與 API 等價費用不是相同計費單位，單次粗粒度觀察不能證明訂閱扣額規則。不同額度視窗是同一使用量的不同觀察，請勿把它們相加。

每百分比統計由已記錄的 quota 變化時間界定，TokenEvent 按區間即時彙總；各週期 SQLite 分別保留重設週期的明細，主 tokens.sqlite3 只留最新週期。舊版 quota-intervals.json 不再由資料庫工作器更新。扣額 JSON 包含 completed_intervals；CSV 同時另存 -models.csv，每列一個區間／模型，方便比較實驗。

## 合成樹狀驗證（不接觸真實帳戶）

    python3 fixtures/generate_tree_fixture.py ./synthetic-tree
    quota-widget --offline --data-dir ./synthetic-tree/state

此產生器只在指定資料夾建立合成的15個Task、13個子代理關係、巢狀子代理及重複額度快照；程式不連線。請測試 [+] 展開／收起、七項tooltip、設定行數上限、日期及匯出。離線額度本身會顯示「—」，這是預期狀態。

## 合成模型／排除驗證

    python3 fixtures/generate_review_fixture.py ./synthetic-review
    quota-widget --offline --data-dir ./synthetic-review/state

另加 --with-completed 可產生已完成的 2 百分點區間供查看分項。此資料全為合成，不會更動真實 Codex 設定。

## 0.3.3 時間及明細修正

所有畫面、日期選擇、每日分組及匯出固定用香港 UTC+08:00，不再跟隨電腦時區。原始 UTC Unix timestamps 不變；JSON／CSV 同時提供明確的 UTC epoch 欄位及 +08:00 顯示值。倒數與經過時間不變。

「推理強度」顯示每筆記錄的 high／max 等 effort，與「推理」Token 數分開；未記錄或有衝突時顯示未知，不能由 parent 模型推測。首次升級會在背景重新讀取可用的既有來源檔來補回 effort，之後沿用增量匯入。

Turn／Task 金額現在顯示已知小計及 * 不完整標記，不再因覆蓋警告隱藏已知金額。全部未能估價仍顯示未知；真正已知的零仍顯示 $0。這不會把不完整區間變成可比較的每百分點成本。

價格編輯器提供模型選單（Sol／Luna／Astra／已記錄／自訂模型）及可點選的單價一覽。先選 model，編輯未快取輸入／快取／輸出單價，按「套用此模型價格並重算」；只改該模型。未知 model 可輸入完整 ID 後確認自訂單價。選單只選擇編輯對象，不改事件歸屬。官方預設按鈕會取代全部自訂費率。
