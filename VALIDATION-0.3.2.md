# Quota Widget 0.3.2 validation — 2026-10-17 UTC

All execution used an isolated Linux workspace and synthetic JSONL. No user computer, real account, credentials, or live user-session files were accessed.

## Automated checks

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --offline --locked --all-targets -- -D warnings`: passed.
- `cargo test --offline --locked`: 256 normal tests passed; three substantial benchmarks are intentionally ignored in that normal command and were separately run successfully in release mode.
- Rust 1.99.1; eframe 0.32.3; Cargo.lock retained. No runtime web app or Electron dependency.

Coverage includes response/thread deduplication, legacy cumulative deltas, mixed schema overlap, model switches, unknown models, long-context prices, custom-price migration/restart, exact recorded auto-review exclusion, seven Luna children under a Sol parent, retained excluded raw data, filtered Turn snapshots, tree totals and cycles, real versus cached quota resets, background persistence/corrupt-file guards, numeric partial totals, all daily export formats and excluded audit export, first-frame dark/light theme pinning against an opposite system theme, and completed interval attribution/model sums.

Completed intervals include actual multi-point changes without fabricated subdivisions, startup/reset/gap/unknown-model non-comparability, account/source/window/epoch boundaries, stable identities, missing versus zero cost, and exclusion-aware own-model totals. No hypothetical parent-price feature is included.

## Substantial synthetic benchmarks

Optimized Linux measurements from this release (not promises about a user's machine):

- 200 files / 50,000 events / 100,200 lines / 18,873,790 bytes: initial import 527.284 ms; unchanged poll median 0.479 ms; one appended event 279.156 ms; identical pricing 0.001 ms. Peak test-process RSS 148,656 KiB.
- 100,000 events / 1,000 Tasks / 2,000 quota history rows: indexed Task range median 0.073 ms; full history batch accounting 0.338 ms; cached accounting repaint 0.000133 ms. Original uncached reference frame 220.229 ms, indexed reference frame 34.364 ms.
- 100,000 events / 10,000 quota rows / 79,993 model-breakdown rows: interval index 16.912 ms, completed interval derivation 33.632 ms. Indexed range aggregation avoids history-by-event rescans.

Import, normalization, indexing, atomic persistence and export remain background-owned. No-op polls do not clone full history or rewrite interval files. Retired snapshots are destroyed off the GUI thread.

## Platform artifacts and limits

- Linux x86_64 release built and exercised through actual eframe windows on authenticated Xvfb/Openbox, software OpenGL. Screenshots are actual rendered UI with synthetic fixtures, not mockups.
- Windows x64 release cross-built successfully with the GNU target. PE headers identify an x86-64 GUI executable; imported DLLs are Windows system libraries. It has NOT been run or accepted on native Windows in this environment.
- macOS Apple Silicon `aarch64-apple-darwin` code check passed. No Apple SDK/linker is present, so no Mac application/binary was linked, packaged, signed or tested.
- No Authenticode signing or Apple notarization. Do not disable security protections or bypass warnings.

The full source package contains the exact final source and lockfile, complete embedded fonts/icon and licenses, fixtures, tests, build instructions/scripts and selected validation evidence. External build artifacts and generated runtime state are excluded.

Pricing is dated API-equivalent estimation, not actual subscription charges. Exact recorded `codex-auto-review` is excluded by request, with retained audit records; no free-billing claim. Standard/Fast defaults for Sol/Luna/Astra were checked against official documentation on 2026-10-17. Existing custom rates are preserved. See docs/PRICING.md.

## Actual final UI acceptance

The same final Linux binary was launched, closed and relaunched with persisted settings. Verified collapsed/expanded tree, seven visible Luna descendants rather than 26 (19 excluded review tasks), actual-name/model/rate tooltip with all seven counters, default mixed-model details, separate 19-record review audit, readable dark header buttons after restart, and numeric zero current buckets after a completed decrement.

The synthetic completed 2pp interval displays $0.1362 total and $0.0681 per percentage point. Model detail shows eight Luna responses totaling $0.0192 and three Sol responses totaling $0.1170. The actual GUI's CSV and JSON export buttons were exercised; parsed exports reconcile to those exact amounts and contain no excluded auto-review usage. The initial incomplete interval displays a numeric $0.0000 with uncertainty rather than a negative zero or invented comparable ratio.

See validation/release-0.3.2/final-*.png and selected logs/examples. The blank live quota in offline screenshots is intentional: fixtures supply local usage/history, not a fabricated official live balance.
