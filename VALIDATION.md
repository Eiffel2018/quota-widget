# Validation — Quota Widget 0.3.0, 2026-10-17 UTC

## Passed

- Rust 1.99.1, locked dependency graph, rustfmt check.
- Strict `cargo clippy --all-targets --locked -- -D warnings`.
- 114 unit tests and 27 integration tests (141 total), all passed.
- Linux x86-64 native debug and optimized release builds.
- Windows x86-64 GNU optimized release build; inspected PE32+ AMD64 GUI executable and imports. Imported DLLs are Windows system libraries; no Electron/Node/WebView/Python runtime is bundled.
- Complete `cargo check --target aarch64-apple-darwin` passes. This is code/type checking, not linking or execution.
- Actual native GUI rendering under an authenticated X11 display, Openbox and Mesa llvmpipe OpenGL 4.5.
- Actual pointer drag: window moved from (35,35) to (155,105). Actual bottom-right resize: 310×360 to 400×430.
- Light/dark theme, separate settings/history/token windows, close and reopen, transparent rounded widget rendering.
- ABOVE/topmost state remains after other-window activation and minimize/restore. The active window stayed the settings window across a 16-second topmost reassertion; no focus stealing was observed.
- GUI daily CSV and JSON exports were opened and checked: 2 synthetic events, 200 input, 80 cached, 120 uncached, 40 output including 16 reasoning, synthetic cost $0.00022. Local timestamps and pricing basis are included.
- New-schema GUI CSV/JSON checked: 7 deduplicated responses across 6 tasks/turns, 20,350 input and 2,035 output, Standard estimated total $0.003443. Unknown model attribution remains visibly assumed. Per-Task scrolling was tested with six tasks; its viewport does not overlap the footer.
- Corrupt settings/history/token files were hash-checked before and after a real GUI run and normal close; unchanged.
- Response-scoped tests cover replayed/updated response IDs, distinct equal requests, logical child-task identity, turn/thread snapshots, mixed-format overlap, incomplete coverage, positive cache-write unknown pricing, and v1 cursor migration. Task-view tests independently verify latest per-event cost and per-task interval reset without double counting.
- Synthetic tests cover 5,087 unchanged 0→0 metadata jitters, true reset then new drops, sparse and 180-day retained histories, duplicate cumulative snapshots, equal distinct requests, counter reset/compaction ambiguity, stable restart offsets, model changes and assumptions, missing prices, exact 272,000 versus 272,001 long-context pricing and Fast multiplier.
- Cross-process writer exclusion, read-only inspection while locked, forced process death and atomic JSON persistence.

All account/CLI tests used a synthetic subprocess. All JSONL tests used generated fixtures and isolated temporary homes. No real account, credentials or user session directory was accessed.

## UI comparison and intentional changes

The five original screenshots were inspected as image pixels. The reconstruction keeps the original native Rust/eframe stack, recovered icon/CJK font, dark green/light palette, rounded outline, prominent percentage, thin progress track, quota/credits/reset rows and native detail windows.

The main window expands from the original 276×250 baseline to 310×360 to fit the requested two buttons and scrollable per-Task cost table and interval summary. This is intentional, not a claim of pixel-identical reproduction. Native DPI, fonts, shadows and window-manager decoration differ across OSes. Current actual UI captures are included in the release's validation/screenshots directory.

## Not validated / blocked

- The Windows EXE was cross-compiled on Linux. It was NOT executed on native Windows; display drivers, DPI, monitor changes, sleep/resume and Windows-specific topmost behavior still need native acceptance testing.
- There is NO macOS executable in this release. Linux lacks an authorized Apple SDK/Xcode and Darwin linking environment. The complete source and Apple Silicon build/package script are included; an authorized Mac is needed to produce and natively validate the app.
- No real authenticated Codex quota retrieval was attempted. The user must use their own already-installed, already-authenticated official CLI.
- No Authenticode signature, Apple Developer ID signature or notarization. Do not disable security protections or bypass warnings.
- Power-loss durability, multi-monitor/DPI transitions and prolonged production load were not tested. Atomic replacement and process-crash recovery were tested.
- Fees are dated API-equivalent estimates with an explicit Standard/Fast assumption, not subscription billing. Cache-write/region/tool add-ons are not modeled separately. See docs/PRICING.md.
