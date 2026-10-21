# 0.3.5

- Replace monolithic recurring tokens.json writes with validated, resumable SQLite migration, compressed audit shards and indexed canonical events. Preserve the original file.
- Reconcile only changed logical threads. Incrementally update current-%/daily totals; scope Task/Turn refreshes to affected groups, and fetch Response pages with SQL limits.
- Display cached records before the one-time corrective source replay, with migration/import progress. Healthy unchanged scans avoid snapshot and interval rebuilds.
- Treat exact +1 percentage-point rebound as unchanged within a quota cycle, including live header, ledger, prediction and cost boundaries. Retain raw observations and trusted genuine rollovers.
- Repair mirrored quota-source continuity and conservative mixed-schema coverage proofs. Do not fuzzy-deduplicate independent response records.
- Show bounded actionable malformed-line diagnostics, accurate long-context adjustment metadata and stable dynamic-height convergence.
- Preserve0.3.4's HK time, individual model prices, review audit exclusion, task tree, partial subtotal and interval-ledger behavior.

See VALIDATION-0.3.5.md and docs/INCREMENTAL-STORAGE-CLI.md.
