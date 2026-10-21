# Quota Widget machine interface

Use this local CLI to inspect authorized Codex usage files without authenticating or executing model work. It cannot create authority to read a directory. Never read another user's data or credentials.

1. Confirm the intended sessions directory and optional pricing file.
2. Use `quota-widget inspect --sessions PATH` for read-only JSON.
3. For a durable incremental store, use `quota-widget import --sessions PATH --state PATH` only when authorized to write that destination.
4. Check exit status, schema_version, warnings, model_assumed, partial_flags and nullable cost_usd. Unknown cost is not zero. The price unit is USD per million tokens.
5. Prefer response-scoped token_usage_record usage keyed by (thread_id, response_id). Turn and thread snapshots are cumulative and are never added to response totals. Task identity is the logical thread_id, not the root session_id. Unknown legacy overlap is partial, not complete coverage. Input includes the full request context.
6. Re-running import is idempotent for unchanged source bytes. Stable request identities and session cumulative deltas handle replay; no claim of exactly-once arbitrary external actions.
7. An OS-level directory lock prevents simultaneous GUI/CLI writers. On a conflict, stop or read the existing state. Do not delete a lock to force ownership; the lock releases on process exit/crash.
8. Source JSONL is always read-only. No auth.json, login, inference, purchase or settings/account API is called. No server or MCP access is started.

CLI stdout: one JSON object, schema_version 1, mode, events, turn_summaries, task_summaries, quota_observations, files_scanned, lines_read, warnings. UTC timestamps use Unix seconds. stderr: diagnostic errors. Exit 0 success / 1 failure. No output field promises subscription charges or exact backend debit time.
