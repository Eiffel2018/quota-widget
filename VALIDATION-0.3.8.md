# Validation — Quota Widget 0.3.8

- 546 tests passed, zero failed; five opt-in large benchmark tests remain ignored.
- Formatting and strict all-target Clippy passed. Linux native build and Windows x64 GNU release cross-build passed using the locked Rust/Cargo 1.99.1 toolchain and MinGW-w64 14.2.0.
- Seven new synthetic scope regressions cover 286 stale sibling/descendant edges, retained priced/zero/unpriced current-interval contributors, necessary ancestors, cycle safety, nested unique costs/counts, time expiry and boundary changes through both reducers. Existing review exclusion and persistence/accounting tests also passed.

## Verified cause

usage_views::task_tree_rows correctly received recently active/current-interval Task rows, but its reverse parent-edge walk re-added all recorded descendants of visible roots. This undid the caller's eligibility selection. The patch removes that reverse traversal; it walks upward only to retain required ancestors. No token import, accounting, storage schema, pricing or Codex lifecycle operation changed.

## Native Linux GUI

A synthetic corpus has 286 recorded descendants: five relevant tree nodes (including one required metadata ancestor) and 281 stale branches. Collapsed and fully expanded views show five, preserve the active grandchild's ancestry and an older-than-two-hours current-interval contributor, and show the same overall $0.0207 subtotal. Screenshots are in validation/relevant-tree-0.3.8.

The SQLite store still contains all 286 usage events and 287 thread records, including the historical hidden branches. Restart was checked from the retained SQLite state; the old branches did not reappear. The count represents displayed relevant descendants, not live agent slots. The app does not close, delete, archive or change Codex sessions.

## Limitations and validation environment

Windows is cross-built and unsigned; native Windows runtime is untested. No Mac binary. No user account or source session files were read, and no private supplied screenshot/history/settings/token data is included in the archives. Actual user data has not been used for this regression; the screenshot pattern is reproduced with synthetic data.

One aggregate build attempt exhausted space in the disposable compilation cache. Only generated quota-widget build artifacts were cleaned, leaving source, private input copies, fixtures and immutable delivered ZIPs untouched. The final complete tests and build were rerun after restoring capacity.
