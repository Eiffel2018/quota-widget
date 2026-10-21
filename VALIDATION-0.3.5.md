# Quota Widget 0.3.5 validation

Date: 2026-10-18. Rust/Cargo 1.99.1. Native tests and GUI: Linux cloud VM, explicitly synthetic sources. Windows x64: GNU cross-build, no native Windows execution or signing. No macOS executable is claimed.

## Release gates

- Locked-dependency full test suite, formatting and strict all-target Clippy pass; exact counts and compiler logs are included under validation/release-0.3.5 and BUILD-PROVENANCE.json.
- SQLite migration preserves original JSON bytes. Staging resumes after actual subprocess termination; an uncommitted spilled WAL transaction is killed and verified to roll back. Original fingerprint, exact per-thread audit data, canonical-event hash, integer totals, prices, foreign keys and SQLite integrity are verified before activation.
- Legacy numeric session/settings compatibility, response/turn/cumulative reconciliation, per-model pricing, exact review exclusion, unknown/partial totals, parent trees, HK UTC+08:00 boundaries, persistence/restart and safe read failures remain covered.
- Exact same-cycle +1 rebound rule covers both imported history and live QuotaModel::view (the header's source): 86,87,86 stays86, predictor samples and current cost boundary remain unchanged. A subsequent85 is one real decrement. Deadline-confirmed rollover and separate account/window/source cycles remain distinct. Raw evidence is retained.
- Incremental main/day aggregates have randomized reversible differential tests; grouped Task/Turn views compare against full grouping after239 randomized commits across six grouping/exclusion combinations. Database-worker integration tests cover page totals, append, restart, prices, clock advance and original preservation.
- Malformed JSONL counts retain all errors; at most five file/byte/reason samples are displayed per scan. No raw JSON contents are included. The reported real-world120 errors were not reproduced from private raw source logs and are not assigned a guessed cause.

## Measured synthetic corpus

3,245 JSONL files; 1.3319 GB source logs, 785,318 lines, 417,542 usage observations, 312,611 normalized events (250,088 included and62,523 review-excluded), 3,245 logical threads. All monetary benchmark values use synthetic DEMO rates, not billing claims.

Production Rust storage: 585,680,249-byte JSON becomes198,479,872-byte SQLite (66.11% smaller). Compression varies with input. Original JSON remains alongside the database as a safety backup, so migration initially needs additional disk space; it does not delete the large original or old temporary files.

- One-time migration and exhaustive verification:70.06 s; peak RSS1,667,624 KiB. This still parses the full legacy JSON in memory.
- Normalization upgrade's one-time source replay:57.90 s for785,318 lines. Saved source cursors are deliberately invalidated once to recover previously missing turn-snapshot field presence; exact identities deduplicate replay.
- Warm cached database/metadata Loaded snapshot:0.48–0.70 s. This historical fixture has no default-today rows. It is not a fully populated-day render latency claim.
- First selection of the populated day/current interval: Response2.88 s; Turn4.82 s; Task4.05 s. All250,088+ events are in one HK day. This initial preparation happens off the GUI thread and is cached.
- Unchanged3,245-file poll:20–22 ms; zero lines read and no display-snapshot rebuild.
- One new response, including durable commit and updated display: Response0.758 s; Turn0.763 s; Task0.695 s. Only the affected task/turn and indexed page are reloaded; full-day/main totals adjust from exact old/new rows.
- Backend-only one-response reconciliation/export/commit:343.94 ms; exactly one canonical event changed, one98-event thread rewritten, one cursor, zero quota chunks. WAL growth1,450,272 bytes versus rewriting585.68 MB JSON. Independent comparison preserved every preexisting event.

These are single-VM samples, not guarantees for the user's hardware. The historical buffer-only implementation took about5.77 s to display and7.12 s to finish saving one appended response on this synthetic dataset. The optimized production database is distinct from the earlier Python prototype; its size/timings are not substituted with prototype numbers.

## Native GUI evidence

Actual Linux native windows, synthetic data only: dark saved theme, restart with preserved original JSON SHA, compact collapsed tree, expanded scroll threshold, task-name/model tooltip with Input132534/Cached132096/Uncached438/Output95/Reasoning74/TurnInput38788508/TurnOutput60924, HK date/time, daily token table, completed interval table and visible known partial subtotal. Images are under validation/release-0.3.5/gui. A window-manager height clamp was fixed with bounded, drag-aware convergence retries. These are native Linux checks, not Windows-native verification.

## Remaining limits

- First migration and full compatibility exports use substantial memory; exceptionally large individual threads still require replay of that thread. Changing model attribution/price policy can invalidate derived caches and require broader work.
- First full-day/group preparation remains proportional to that selected day. Normal append and unchanged polls are incremental.
- Global header hashing still serializes retained quota/cursors even when their database chunks are unchanged. No claim of strictly constant-cost updates is made.
- Missing raw identities cannot safely be deduplicated by equal token counts or nearby timestamps. Ambiguous historical legacy/response overlaps remain marked partial; counts/costs are not silently deleted. No hypothetical parent-model pricing is used.
- Old executables do not understand SQLite. Close the app before backup, preserve WAL/SHM while it runs, and use the explicit full compatibility export for rollback including post-migration events. Export refuses an existing destination.
- Windows EXE is unsigned and cross-compiled. macOS requires a separate authorized native build/SDK. Do not bypass operating-system security warnings.
