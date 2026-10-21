# Correction checkpoint validation — 2026-10-17

This checkpoint is for review after user-reported defects. A new complete release attachment is intentionally withheld pending review. All execution used the assistant's isolated Linux workspace and synthetic JSONL; no user computer, real account, credentials or live user sessions were accessed.

## Diagnosed causes and fixes

1. Historical quota snapshots: cached `rate_limits` from concurrent session logs had been ordered by log-line time and treated like one authoritative live stream. Unsupported rises created resets/epochs, then replayed old decreases appeared to spend again. Raw evidence is now retained, with origin/millisecond/reset metadata. Presentation and accounting normalize repeated stale transitions and label unsupported recoveries as uncertain. Direct live refills and evidenced reset-deadline rollover remain real.
2. Slowness: unchanged files were reopened/hashed, unchanged pricing rebuilt all normalized events, every repaint regrouped/sorted Task events and repeatedly scanned token/history arrays, and state serialization/writes ran on the GUI thread. Persistent worker ownership, immutable Arc snapshots only on revision changes, stat-based skips, no-op repricing, prefix indexes, cached/virtualized views and asynchronous exports remove these paths. Old snapshot destruction also occurs on the worker where possible.
3. Blank cumulative values: strict all-or-nothing interval pricing suppressed already known costs when a boundary/coverage event was missing. The current interval now shows a numeric known subtotal with an explicit partial marker. Startup finds the last confirmed quota boundary and replays from it; without one, it begins at zero from startup. Unknown charges are not claimed free.
4. Task identity: names and hierarchy now use explicit official metadata/session index only. Per-response counters and matching Turn snapshots are kept separate. Missing counters stay unknown.

## Automated validation

Commands: `cargo fmt --all -- --check`; `cargo clippy --all-targets --locked -- -D warnings`; `cargo test --locked`.

Final result: 205 normal tests passed; two substantial benchmarks were explicitly run in release mode and also passed.

Suites cover response and legacy dedupe, mixed schemas, cumulative resets, missing counters, prices, true/false quota resets, cached history normalization, raw preservation, per-account boundaries, background corruption protection/read-only mode/shutdown, all export formats, startup numeric partial totals, tooltip counters, actual title metadata, tree siblings/nesting/orphans/cycles, 10,000-node stack-safe traversal and visible row limits. Headless egui pointer press/release tests check expand/collapse.

## Substantial synthetic benchmarks (optimized Linux build)

Import benchmark: 200 files, 50,000 events, 100,200 lines, 18,873,790 bytes. Original baseline versus corrected final run:

| Operation | Original | Corrected |
|---|---:|---:|
| Initial import | 455.064 ms | 474.031 ms |
| Unchanged poll, median | 46.678 ms | 0.496 ms |
| One appended event | 246.153 ms | 223.173 ms |
| Identical repricing | 216.319 ms | 0.001 ms |
| Peak test-process RSS | 138,252 KiB | 140,280 KiB |

Startup is slightly slower and memory slightly higher due to preserved metadata/indexes. The substantial improvement is the idle/hot path; changed imports occur off the GUI thread. These are synthetic measurements, not native Windows GUI timing claims.

View benchmark: 100,000 events, 1,000 tasks, 2,000 quota changes.

- Index build median 13.869 ms (background snapshot construction).
- Task query 7.055 ms old → 0.081 ms indexed.
- Original 200-row history accounting 217.622 ms; corrected batch prices all 2,000 history rows in 0.331 ms, then cached visible rows are rendered.
- Benchmark code and full logs are included. Cached-accounting read timing is data-only, not a full paint-time measurement.

## Actual UI validation

Native Linux eframe/OpenGL UI, authenticated Xvfb + Openbox + software Mesa, synthetic fixtures. Screenshots are actual application windows, not mockups.

- Collapsed four-row layout:310×368; expanded and nested layouts reach the default10-row cap at310×476. Setting the cap to5 gives310×386; resizing to400px width gives400×498.
- Two root tasks and 13 known descendants, including a nested child, test collapsed/expanded presentation and numeric parent/self/child totals.
- Tooltip fixture reproduces Input132534 / Cached132096 / Uncached438 / Output95 / Reasoning74 / TurnInput38788508 / TurnOutput60924.
- Appending a new response while the window stays open changes the parent subtotal and overall subtotal by exactly one known response cost.
- Unchanged polling leaves tokens.json size and mtime unchanged.
- Historical3→2→3→2 snapshots render one confirmed drop and one uncertainty marker, not repeated reset epochs.
- Topmost reassertion preserves the focused settings window; width resizing preserves upper geometry and adjusts task height; drag moves the actual native window.

Final screenshot/geometry results and build logs are in validation/revision-20261017. UI interaction testing caught and corrected a background drag hit area overlapping Task controls; the drag region is now confined to the upper quota/title areas.

## Platform limits

Windows x64 GNU cross-build is available for verification; it has not been run natively on Windows in this environment. macOS Apple Silicon code-check is available, but no Apple SDK/linker or signed/notarized app is present, so no Mac binary success is claimed. No new full installer/binary package is attached at this checkpoint. Existing user data is never automatically deleted; private session names/IDs in real persisted state should be treated as private.
