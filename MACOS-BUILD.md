# macOS Apple Silicon arm64 建置

本雲端是 Linux，沒有 Apple SDK／Xcode。已安裝 Rust 的 aarch64-apple-darwin 標準庫並檢查跨平台程式碼；這不等於完成 Mac 連結或原生執行驗證。

請在你授權的 Apple Silicon Mac 上使用官方 Xcode Command Line Tools 與官方 stable Rust。若尚未安裝，請先自行依 Apple／Rust 官方流程完成安裝及條款。腳本不會代替你安裝軟體、接受條款、改安全設定或登入。

1. 把完整原碼解壓縮到一般資料夾。
2. 終端機進入該資料夾，執行 bash scripts/build-macos.sh。
3. 腳本會跑 locked tests/build，產出 dist/Quota Widget.app 與 arm64 ZIP。
4. 可先以 Contents/MacOS/quota-widget --demo 試合成資料。

最低 macOS 版本設為 11.0，目標只有 Apple Silicon arm64。未簽發 Developer ID、未 notarize。腳本的本機 ad-hoc codesign 只為 app bundle 結構完整，不代表 Apple 認證或免除 Gatekeeper。不要停用 Gatekeeper 或移除來源隔離屬性來繞過警告。

原生驗收：深淺色、拖曳／縮放、最上層、Space／全螢幕／休眠恢復、獨立紀錄視窗、自己的已登入 CLI 連線、daily 匯出。只讀測試，不需 API key。
