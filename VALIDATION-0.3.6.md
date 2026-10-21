# Validation — Quota Widget 0.3.6

- 526 tests passed; 5 explicit large benchmark tests remain opt-in.
- cargo fmt --all -- --check and cargo clippy --locked --all-targets -- -D warnings passed.
- Windows x64 all-target check and release cross-build passed using Rust/Cargo 1.99.1 and MinGW-w64.
- No real accounts, credentials, or user computer access were used. Diagnostic screenshots were inspected; private user input files are excluded from the archives.

## Root-cause evidence

The delivered 0.3.5 bridge worker can terminate after a connection error. Both UI refresh controls ignored channel-send failure and retained an unusable Bridge. Token import runs independently; an import-progress line was not evidence of successful quota connection. Initial account-specific failure cause remains unknown.

Closing previously synchronously joined exports and the FIFO token worker from on_exit/Drop. A queued Shutdown could not interrupt its current scan, migration, or rebuild. This explains a verified mechanism for waiting and an unresponsive window; the user's exact elapsed duration was not reproduced on their Windows machine.

## Synthetic cancellation measurements

The 585,758,983-byte synthetic legacy JSON was copied into a dedicated test directory. Cancellation was requested during a still-active migration (the benchmark rejects a load that already completed). At 5 seconds into the operation, request dispatch took 0.019 ms and safe worker completion took 1.14 ms on this cloud machine. The file's SHA256 was identical afterward. These figures are a specific cooperative checkpoint measurement, not a promise for every migration stage or hardware.

Native Linux 0.3.6 close during active migration was observed complete at 592 ms (the previous check at 527 ms still showed the window); the writer lock was then reacquired successfully. Native GUI tests verified the visible error strip, settings diagnostics, retry control, idle close, and close while the large synthetic migration was running. JSON evidence records observation timestamps; a window check is an upper-bound observation, not a precise OS event timestamp. Writer-lock reacquisition confirms safe ownership release. No real CLI child was launched by GUI tests; mocked-child tests verify owned-child reaping and leave an unrelated process alive.

## Durability and recovery coverage

- Cancellation during migration phases preserves source JSON and permits resume.
- Cancellation after SQL row writes rolls back revision, rows and receipt; reopen/retry succeeds.
- Read-snapshot cleanup, long SQL interruption, and lock-wait cancellation are covered.
- Empty first-run cache initialization can recover after interruption; nonempty unknown/corrupt databases are preserved and rejected.
- Import traversal, metadata, line parsing and long stable normalization have checkpoints. Cancellation cannot be misreported as a benign parse warning.
- Serialization uses a non-retrying internal sentinel to avoid Interrupted retry loops.
- Canceled export leaves no partial target and preserves existing exports.
- Accepted last settings and raw history are flushed even when queued heavy work is discarded; account boundaries and legacy backups remain intact.

OS filesystem calls/fsync are not preemptible. Native Windows runtime, Apple Silicon build/signing, and actual-user initial CLI failure remain unverified.
