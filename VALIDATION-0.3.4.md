# Quota Widget 0.3.4 verification

Verified on 2026-10-18 UTC (Hong Kong UTC+08:00). This report describes fresh checks of the recovered release source in the current workspace.

## Automated checks

- 355 unit/integration/UI-state tests passed, zero failures; 3 intentionally ignored large-data benchmarks were run separately and all passed.
- cargo fmt --all -- --check passed.
- cargo clippy --offline --locked --all-targets -- -D warnings passed.
- Native Linux debug build passed, CLI --version reports quota-widget 0.3.4, and read-only synthetic JSONL inspection passed.
- Windows x64 GNU release cross-build passed with Cargo.lock and Rust/Cargo 1.99.1. PE32+ x86-64 GUI executable; only Windows system DLL imports; no separately shipped runtime DLLs.

Coverage includes legacy numeric session IDs and null bucket selection, byte-exact backups and blocked overwrites on corruption/backup failure, restart, raw-history preservation, legacy direction recovery, reset-jitter/replay/rounding behavior, model-wise response deduplication, review exclusion, partial subtotals, fixed Hong Kong time and day rollover, 2-hour task eligibility, cache invalidation, canonical completed intervals, tree accounting, configurable scroll threshold, and demo interval population.

A separate private copy of the two user-supplied legacy files was read successfully through the released parser and background-load path. Original bytes were unchanged; all nonzero presented observations retained their recorded endpoints, times, source/session and deltas. These files and their private verifier are not in this archive. They contain no newer token history, so they cannot establish the origin of an earlier screenshot’s dollar total.

## Large-data checks

The logs in validation/release-0.3.4 record the exact observed timings. Tests cover 200 synthetic JSONL files / 50,000 events, a 100,000-event / 1,000-task view, and 100,000 events / 10,000 quota observations. Timings measure this cloud machine, not a promised Windows latency. Unchanged polling avoids reparsing; cached repaint accounting is separately measured.

## Actual Linux native UI

Executed the actual native debug binary on the own-cloud Linux desktop using isolated synthetic fixtures and --offline. No real account or session directory was accessed.

Visually checked and captured: dark startup controls; compact resize; collapsed and expanded task tree; 10-visible-row scrolling; Luna child model, max effort and all seven token/turn counters; completed-interval table; model breakdown; matching recent-interval footer tooltip; numeric partial known subtotal with comparison disabled. A 2-percentage-point synthetic interval shows total $0.1362 and average $0.0681/pp, split into Luna $0.0192 and Sol $0.1170. A separate unpriced-event interval shows known $0.0024 with an incomplete marker and no per-point comparison. An entirely unpriced model is explicitly unknown rather than displayed as a complete zero charge.

The initial isolated Xvfb attempt was blocked by this environment. Visual verification instead used its supported cloud desktop; this is not Windows runtime verification. The desktop initially enlarged the borderless window; the native resize handle returned it to compact size.

## Limitations

- Windows executable is cross-compiled and unsigned. It has not been run on native Windows here. CLI discovery, topmost/focus lifecycle, high-DPI and multi-monitor behavior need the user's Windows trial.
- No new macOS binary. Full source and macOS build instructions are included.
- Estimated USD is API model-equivalent pricing, not actual subscription charges. Partial, startup, unknown-price and ambiguous quota intervals remain non-comparable.
- No credentials, user history/settings/tokens, private screenshots, account files or real session fixtures are included.
- The execution workspace is not guaranteed permanent. Release ZIPs and an index are independently saved to Library and supplied to the user.

See SHA256SUMS.txt for archive members and BUILD-PROVENANCE.json for the exact binary/source input linkage.
