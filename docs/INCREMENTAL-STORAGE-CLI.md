# Incremental accounting storage and CLI

Persistent accounting uses the SQLite file beside the selected legacy state path: for example, `--state /selected/data/tokens.json` selects `/selected/data/tokens.sqlite3` once it exists. GUI and CLI imports share the data-directory writer lock. An unreadable or unsupported SQLite database is an error; the CLI never silently substitutes an older JSON snapshot.

An explicit import may migrate a selected legacy JSON file. The legacy file remains available as a point-in-time original. Normal imports retain historical observations and consume changed source tails; a 30-day view is not a deletion policy. Reconciliation is scoped to changed logical threads. A very large changed thread can still require replay of that thread.

## Commands

- `quota-widget inspect --sessions /selected/logs [--state /selected/data/tokens.json] [--pricing /selected/prices.json]`
- `quota-widget import --sessions /selected/logs --state /selected/data/tokens.json [--pricing /selected/prices.json]`
- `quota-widget export --state /selected/data/tokens.json --output /selected/backups/current-accounting.json`

`inspect` reads the authoritative cache and overlays explicitly selected source changes in memory. It does not migrate or commit those changes. `import` commits accounting evidence and source cursors atomically. Both emit schema-version-1 JSON on stdout, with diagnostics on stderr. If no pricing file is supplied, the cache's saved pricing assumptions are retained; a fresh cache remains unconfigured.

`export` writes a complete compatibility JSON file from a consistent current database snapshot. It does not scan source logs. Its destination must not already exist, and the source database remains unchanged. Publication uses a same-directory temporary file and atomic no-clobber hard link, so a racing destination is preserved; filesystems without hard-link support fail explicitly. Full inspect/export responses intentionally read all requested retained accounting; they are explicit full-history CLI operations rather than GUI startup paths.

## Rollback after new imports

The original pre-migration JSON does not include later events. To retain all current data before reverting to a compatible older executable, close the writing app and produce a fresh `export` to a new file. Verify the export with the intended executable in read-only mode before selecting it as a legacy state. Keep the SQLite database and the original JSON until that verification succeeds. Do not overwrite an active state file or delete WAL/SHM sidecars manually.

Never launch an older executable against an active migrated data directory and assume it understands SQLite. Older releases may write their own legacy JSON independently. Use the fresh validated export in a separate directory for compatibility testing, retaining the authoritative database.

`--demo` and `--no-settings` keep their existing synthetic/ephemeral GUI behavior. No command discovers or scans a default real session directory: CLI imports require an explicit `--sessions` path.

## Normalization upgrade and historical overlap

Version 0.3.5 retains raw presence for all four turn-snapshot counters. A 0.3.4 cache
only retained raw input/output presence, so its normalization upgrade clears saved
source cursors and performs one extra scan of the selected source logs. Exact
observation identities deduplicate that replay; original source files remain
untouched. Old caches or exports without accessible raw evidence stay uncertain.

Unresolved legacy overlap is not cleared merely because another response is
newer. A later turn can escape an old overlap warning only with an explicit
thread/turn `turn_context` start (the earliest such context), fully present
response counters, and exact equality to a valid monotone recorded turn snapshot.
The start must be strictly after the ambiguous legacy evidence; missing, equal,
conflicting, reset, or overflowing evidence remains partial. Anonymous legacy
ownership is never guessed, and exact response/turn identity conflicts remain
partial regardless of timestamps. Existing historical uncertain intervals are
not retroactively declared complete.
