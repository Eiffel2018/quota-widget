# 0.3.2 review exclusion and model pricing

Exact recorded codex-auto-review usage is retained for audit but excluded from default Task rows, child counts, estimates and details. Parent/child and mixed-thread usage are priced per recorded event model; known unknown model identifiers are never silently treated as Sol. Existing raw data and custom rates are preserved. Standard/Fast presets now include officially verified Astra. High/max reasoning effort does not invent a price multiplier. Audit view/export and migration/restart regressions are included.

Completed decrement intervals now persist model-wise recorded token/cost subtotals, comparable USD per actual percentage point and explicit missing-coverage/reset reasons. History has a model-detail view; daily JSON and companion model CSV support comparisons. No hypothetical parent-price calculation is included. Saved dark/light preference is pinned before the first frame, with explicit header-button text colors.

# 0.3.1 correction checkpoint (2026-10-17)

Not yet a new end-user release package. Fixes were requested after the first Windows test, with screenshots and validated behavior to be reviewed first.

- Cached, overlapping historical quota snapshots no longer fabricate alternating resets/epochs. Raw evidence remains; uncertain recoveries are explicit, real live/refill/rollover events remain.
- Known per-Task and overall interval subtotals remain numeric and accumulate despite partial coverage. Startup replays the last confirmed quota boundary; missing boundary begins a partial zero subtotal from startup.
- Replaced the Task/LIVE/USD heading with a compact tree. Only explicit official parent metadata links agents. Parent self and all-descendant sums are separate; nested expansion and orphan/cycle handling do not duplicate costs.
- Default dynamic height follows up to 10 visible rows, configurable 1–50; only excess rows scroll.
- Task hover shows actual name/full ID, five latest Response counters and two matching Turn counters, preserving unknown values.
- Background-owned load/import/reconciliation/indexing/writes/export; immutable shared snapshots, unchanged-file stat skips, no-op repricing, indexed range sums, cached/virtualized daily views, safe shutdown joins.
- Added synthetic regression suites and substantial-history benchmarks. See VALIDATION-0.3.1.md for measured results and remaining platform limits.

# 0.3.0 rebuild

This is newly reconstructed, maintainable source based on recoverable 0.2.1 assets, schema, screenshots and selected reverse-engineered behavior. It is not the lost original source tree.

Fixed: metadata-only resets, 0→0 false history entries, fragile all-1pp/±25% prediction suppression, seven-day automatic history deletion, duplicate cumulative token notifications, lost per-session offsets, ambiguous costs treated as zero, mixed-window cost boundaries, same-second double assignment, stale/corrupt-state overwrites and topmost reassertion/focus behavior.

Added: a scrollable per-Task widget table with latest event time/cost and task-only current quota-interval subtotal, official response-scoped TokenUsageRecord import, explicit Response/Turn/Task daily grouping, conservative mixed-schema overlap handling, 30-day daily navigation with older available history retained, local JSONL importer, recorded/assumed model attribution, editable official Standard/Fast price presets, per-request long-context adjustment, separate daily quota and Token views, CSV/JSON export, cost-since-real-boundary, weighted cost-per-percentage-point, guarded persistence and structured read-only CLI.

Current platform status: Windows x64 cross-built; Linux native-tested; macOS arm64 code-checked but no linked app. The full source, tests and Mac build instructions are included.
