# Validation — Quota Widget 0.3.7

## Automated verification

- 539 tests passed, zero failed. Five opt-in large benchmark tests remain ignored by default.
- Locked all-target tests, formatting check and strict all-target Clippy passed.
- Linux native debug build and Windows x64 GNU release cross-build passed with Rust/Cargo 1.99.1 and MinGW-w64 14.2.0.
- Typography tests inspect actual painted root/tooltip text, in dark/light modes and multiple window widths. The font floor matches the main quota-rate label. Tests check 7:1 palette contrast, disabled text opacity, idempotent styling, monitor-height row limits, compact recent-interval presentation and narrow-window tooltip wrapping.
- Existing pricing, model switches, tree attribution, interval coverage, +1% rebound policy, persistence, legacy migration, cancellation and reconnect regressions passed unchanged.

## Actual Linux GUI checks

Synthetic-only demo and a generated 15-task tree were run on the own-cloud native Linux desktop at 1364×1024. Screenshots in validation/accessibility-0.3.7 show the dark/light root, dark/light settings, recent-interval hover, task hover and collapsed/expanded tree.

Verified larger, brighter text; no explanatory paragraphs in the main/settings/table/hover paths; seven requested response/turn token fields retained; compact partial and unknown labels; live theme changes; 10 visible tree rows with scrolling after expansion. Tooltips wrap within the native root instead of clipping on the right. Long tooltips and detail tables can scroll.

All screenshots and fixtures shipped with this release are synthetic. No user history, settings, tokens, credentials or reference screenshots are included.

## Scope and limitations

Only presentation, sizing and their regression tests changed from the recovered, hash-verified 0.3.6 source. No accounting/import/storage schema changes. Underlying interval coverage flags and detailed export reasons remain intact even though the UI now uses compact labels.

Windows is a PE32+ x86-64 cross-build and unsigned. Native Windows runtime and platform-specific DPI have not been tested. No Mac executable is claimed or included. The exact account-specific reason for the user's five partial intervals cannot be established from the screenshot alone. Existing initial CLI failure cause remains unverified.

A first full test rerun caught an obsolete tooltip-width test expectation after narrowing tooltips to the viewport; that expectation was updated to verify both readable width and fit. The final complete suite and cross-build passed afterward.
