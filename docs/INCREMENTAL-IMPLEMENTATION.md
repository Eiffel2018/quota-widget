# Incremental reconciliation and CLI implementation

Production worktree: `quota-widget-incremental`. The earlier regression draft remains the checkpoint. No real account/session directory was scanned for this work; tests use explicit synthetic roots and isolated homes.

## Implemented boundary

- `TokenStore::from_header` opens a lazy importer. `import_dir_hydrated` resolves each record's actual logical thread before requesting its shard. Original observation identities, sequence, source model provenance, and source effort contexts are retained.
- A changed logical thread is reconciled with the same full normalizer retained as the correctness oracle. Unchanged threads are neither hydrated nor normalized. Model attribution changes can deliberately request a coherent all-shard rebuild; rate-only changes reuse canonical counts.
- `dirty_export` is nondestructive. Header/cursors, changed raw shards, canonical event replacements, and dirty scopes are committed by the backend together. Only `mark_committed` evicts hydrated evidence.
- `next_sequence` is persisted globally. It is never derived from a partial in-memory shard count.
- Migration grouping is linear over retained evidence and includes metadata-only tasks, parent-only ancestors, and event-only compatible caches. The consuming shard export avoids making a second complete raw-history clone.
- Source readers pin the opened file's finite byte high-water mark. Concurrent appends are read next time, and an incomplete boundary line does not advance its cursor.
- Compatibility export hydrates all shards under the backend's pinned read transaction, restores canonical ordering, refreshes derived prices, and detaches lazy mode without inventing a new accounting revision.

## Evidence-bound overlap policy

Equal exact response IDs and provable cumulative overlap are suppressed without declaring coverage unknown. Unresolved legacy overlap remains partial. A later turn escapes a historical warning only if:

1. Its exact logical thread and turn have an explicit recorded `turn_context`; the earliest matching context is used.
2. All four response and turn-snapshot counters are present. Missing cached/reasoning fields are not silently treated as proof of zero.
3. Its valid turn snapshots are monotone and nonconflicting, and its latest snapshot exactly equals the checked (non-overflowing) sum of canonical responses.
4. No retained response version is later than that snapshot or earlier than the recorded turn start.
5. The recorded start is strictly after the unresolved legacy evidence. Equal boundaries and explicit response/turn conflicts remain partial.

Anonymous ownership is never guessed. Anonymous evidence is still suppressed as before when authoritative usage exists; its uncertainty has a conservative per-source upper bound. A prepared `EventPolicy` computes the maximum bound once per query/header. Any missing bound preserves unbounded uncertainty. Applying policy to authoritative responses is constant time, and lazy hydration caches the prepared scalar.

A newly observed response without a cumulative snapshot no longer destroys valid overlap ranges of older responses. Fully covered legacy usage remains proven overlap. Any uncovered residual is additive only when **all** responses have comparable ranges; otherwise it stays unknown.

Normalization version 3 causes one replay of selected sources when upgrading older JSON caches. Replay enriches newly retained cached/reasoning presence on exact duplicate observations without duplicating usage. Old evidence without accessible raw sources is not falsely certified.

## CLI authority and rollback

`inspect`, `import`, and `export` select an existing `tokens.sqlite3` beside the requested legacy JSON path as authoritative. Corrupt/unsupported/read failures never silently fall back to preserved JSON. GUI and CLI writes retain their shared directory writer lock. Saved pricing is retained when `--pricing` is omitted.

`export` produces a fresh compatible JSON snapshot containing post-migration records. Keeping the original JSON alone is only point-in-time rollback. Output publication uses a synced, closed same-directory temporary file and atomic hard-link creation: an existing or racing destination is never replaced. Unsupported hard-link filesystems fail explicitly. Only the temporary file created by that particular export is removed; older temporary files are not discovered or deleted.

The source documentation is `docs/INCREMENTAL-STORAGE-CLI.md`.

## Verification

- Full native suite before the final bounded hardenings: 454 passed, zero failed, five intentional benchmark ignores.
- Latest focused differential suite: 15 passed. It checks canonical event equality and exact retained observation/context multisets against the full oracle, restarts, duplicate enrichment, mixed schemas, cumulative resets, late records, source rewrites, metadata-only tasks, proof boundaries, and concurrent append high-water behavior.
- Deterministic no-clobber race regression passed and preserved both the competing destination and an unrelated old temporary file.
- Prepared-policy operation-count regression passed: 10,000 anonymous-source bound lookups total across 10,000 event applications, rather than 100 million.
- Strict all-target Clippy passed with `-D warnings` after the final changes.
- CLI-specific authority/restart/export/corruption/locking tests and parent-owned final native/cross-platform release validation are separate logs under `validation/release-0.3.5`.

## Deliberate scope limits

This is thread-scoped incremental computation, not a constant-time append algorithm inside an arbitrarily large thread. A very large changed thread still requires its raw shard to be hydrated and replayed. First legacy JSON migration still parses the existing JSON store in memory; consuming shard export reduces peak duplication but is not a streaming JSON parser. Explicit full-history CLI inspect/export operations remain full-cost by design. SQLite storage alone does not establish bounded GUI work; the separate database-backed worker/view integration supplies indexed and bounded UI queries.
