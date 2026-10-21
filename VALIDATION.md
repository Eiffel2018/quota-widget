# Quota Widget 0.3.3 validation — 2026-10-18 UTC

All execution used an isolated Linux workspace and synthetic JSONL. No user computer, real account, credentials, or live user-session files were accessed.

## Automated checks

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --offline --locked --all-targets -- -D warnings`: passed.
- `cargo test --offline --locked`: 284 tests passed; three substantial benchmarks are intentionally ignored in that command and were separately run successfully in release mode.
- Rust 1.99.1; eframe 0.32.3; locked dependencies retained. Native Rust, no Electron/runtime web app.

New regression coverage includes fixed Hong Kong day boundaries across UTC midnight, the example 2026-10-17 23:51 UTC → 2026-10-18 07:51 +08:00, interval/CSV/JSON exports, raw epoch persistence, identical display under UTC/Los Angeles/Hong Kong/Kiritimati system timezones, recorded response/turn reasoning effort and conflicting/missing effort, one-time legacy cache enrichment with unchanged-file restart skips, known partial subtotals versus entirely unpriced or genuinely zero subtotals, and isolated per-model price editing/repricing/persistence.

The full suite retains all response deduplication, legacy cumulative delta, mixed schema, model switch, review exclusion, tree, actual quota boundary, background persistence, dark first-frame, completed model-wise interval and export regression tests from 0.3.2. Incomplete intervals remain non-comparable; no hypothetical parent-price feature is included.

## Actual final UI acceptance

Actual final Linux eframe windows were rendered using authenticated Xvfb/Openbox and software OpenGL, not mockups. Synthetic UTC source timestamps were displayed as Hong Kong time. Token Response and Turn views show 07:51:00 on Oct18, recorded max/high separately from reasoning counts, and unknown when no field was recorded. The partial Luna row and its tooltip both show the same $0.0046 known subtotal with an uncertainty marker. The prior HK-day response is not included. Export buttons produced three records with unchanged UTC epochs and explicit +08:00 dates. Relaunch under America/Los_Angeles preserved identical Hong Kong display, recorded effort and dark header styling.

The pricing editor now visibly lists Sol, Luna, Astra and recorded/custom models; a clickable overview exposes configured input/cache/output rates. Actual GUI selection and editing of Luna were checked, including restart. Automated tests independently verify that changing Luna does not change Sol/Astra costs, explicitly configured unknown models can be priced, and unrelated rates/overrides survive restart/recompute. Model selection edits the rate table only, not recorded event attribution.

See validation/release-0.3.3/final-*.png, examples and logs. Blank live quota in offline screenshots is intentional: fixtures do not fabricate an official live balance.

## Substantial synthetic benchmarks

Optimized Linux measurements, not promises about a user's machine:

- 200 files / 50,000 events / 100,200 lines / 18,873,790 bytes: initial import 653.268 ms; unchanged poll median 0.527 ms; one appended event 358.336 ms; identical pricing 0.001 ms. Peak test-process RSS 174,860 KiB.
- 100,000 events / 1,000 Tasks / 2,000 quota rows: indexed Task range median 0.213 ms; full history batch accounting 0.382 ms; cached accounting repaint 0.000133 ms. Original uncached reference frame 224.618 ms, indexed reference frame 27.557 ms.
- 100,000 events / 10,000 quota rows / 79,993 model rows: interval index 15.774 ms; completed interval derivation 26.393 ms.

Import, normalization, indexing, atomic persistence and export remain background-owned. No-op polls do not clone history or rewrite interval files. Price picker model discovery is cached by snapshot generation, not rescanned every settings repaint. The new effort field triggers one intentional replay of selected historical source files to enrich old cache data; subsequent unchanged polls skip parsing.

## Platforms and limits

- Linux x86_64 release built and exercised through actual UI with synthetic fixtures.
- Windows x64 release cross-built successfully; PE headers confirm an x86-64 GUI executable. Imported DLLs are Windows system libraries. NOT executed or accepted on native Windows in this environment.
- macOS Apple Silicon code check passed. No Apple SDK/linker is present, so no Mac binary was linked, packaged, signed or tested.
- No Authenticode signing or Apple notarization. Do not disable security protections or bypass warnings.

Source ZIP includes exact final source, Cargo.lock, full embedded fonts/icon/licenses, fixtures/tests, build instructions and selected evidence. Generated runtime state, private reference screenshots and external build artifacts are excluded. UTC epochs remain canonical; presentation and day grouping are explicitly +08:00. Durations/countdowns are unchanged. CSV uses the existing formula-injection-safe escaping for leading plus signs; explicit timestamp strings also carry +08:00.

Prices are API-equivalent estimates, not subscription charges. Exact recorded codex-auto-review remains excluded by request with audit records retained; no free-billing claim. Model-specific default rates were verified 2026-10-17; high/max effort is not a separate multiplier. Missing/custom model handling and long-context caveats remain documented in docs/PRICING.md.
