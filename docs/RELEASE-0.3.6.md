# Quota Widget 0.3.6 — reconnect and responsive shutdown

## Fixed

- Refresh now reconnects when the official CLI worker has stopped; it no longer silently sends to a disconnected channel. Retryable quota-unavailable replies keep their existing connection.
- Quota connection status/error is shown independently of Token import progress, both on the widget and beside the CLI setting. The status strip opens diagnostics; native window dragging begins only on an actual drag, so it cannot consume that click.
- Closing no longer joins a long-running worker on the UI thread. The window keeps repainting while cooperative cancellation ends imports, migration, queries, and export. Accepted settings and raw quota-history writes are flushed, and the writer lock remains held until the workers finish.
- Cancellation checkpoints cover legacy JSON reading, source traversal, line parsing, hydration, stable sorting, normalization, serialization, compression, SQLite queries/busy locks, migration, and pre-commit boundaries. Canceled transactions roll back, migration staging remains resumable, and original JSON files stay untouched.
- CLI shutdown has priority over refresh backlog, with bounded asynchronous pipe writes and cleanup of the exact child process it owns.
- Incomplete exports are not published. Export filenames are create-new, so collisions cannot replace an existing export.

## What remains unknown

The user's original reason for receiving no live quota value has not been identified from the screenshots. This release fixes recovery and error visibility; it does not establish that every account/CLI connection problem is solved. If quota is still absent, open 設定 / 連接 and report the status and the fixed-text CLI error. No credentials or authentication files are needed.

## Data safety and shutdown limits

Do not delete history, tokens.json, tokens.sqlite3, or SQLite sidecars to troubleshoot. Cancellation preserves committed work; an interrupted source scan resumes from the committed cursor on restart. Settings and already accepted raw quota observations are retained. Original pre-migration JSON remains available.

Cancellation is cooperative. An operating-system filesystem call, disk flush, process-start call, or child-process termination can still take time on a faulty or overloaded system. The UI remains responsive during normal close, but there is no universal millisecond deadline. During OS/process termination, normal cleanup may be cut short; SQLite and atomic-file recovery remain the durability mechanisms.

## Verification

See VALIDATION-0.3.6.md and validation/live-quota-draft-20261018/. Automated synthetic tests cover close/cancel at migration phases, database rollback and reopen, read-only protection, queued control writes, JSON reads, scans, normalization, exports, hung CLI handshake/RPC, refresh backlog, and repeated close. Native Linux GUI checks use only synthetic data. Windows x64 is cross-built and unsigned; native Windows execution has not been verified. No macOS binary is included.
