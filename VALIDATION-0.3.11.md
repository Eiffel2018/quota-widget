# Validation - Quota Widget 0.3.11

- Windows x64 / MSVC, Rust 1.99.1, unchanged locked dependencies.
- Full native suite: 540 passed, 0 failed, 5 opt-in benchmarks ignored.
- Formatting and strict all-target Clippy passed. Compiler outputs, test data
  and disposable logs used the RAM drive.
- Estimate regressions cover observer restart with absent Token records,
  unchanged calibration samples, identity/window isolation, reset and epoch
  boundaries, broken endpoints, stale estimates and source ordering.
- The GUI/database integration regression closes the original writer, loads
  an immutable reader holding its own shared lock, acquires exclusive ownership,
  reloads the writable database, saves observations made while read-only and
  successfully imports a new synthetic response.
- The complete raw history is merged on recovery, never replaced by a filtered
  day. Real local history and Token files were inspected read-only and preserved.
- The simplified debit table retains five columns. A row click opens existing
  calibration, raw Token and model detail views. No real-account GUI interaction
  or pixel-level screenshot verification is claimed.

Detailed machine-specific evidence stays in ignored validation/0.3.11/.

- The installed fixed-path EXE reports 0.3.11. An isolated native demo with the
  history viewport opened remained responsive and closed gracefully with exit 0.
- A live older process held the previous EXE backup. Deployment reproduced the
  replacement failure, then succeeded with the fresh-backup fallback. The old
  process stayed alive; repeated deployment correctly reported already current.
- Final EXE: 28,240,896 bytes; SHA-256
  `7874550440ff2209a0e5acc7ebfc3afe5630036e42f4f86599ae8fce98e8784a`.
  Installed and compiled bytes match. No local home/workspace paths were found
  in ASCII or UTF-16. Private histories, settings, shortcuts and databases are
  excluded from the public source archive and Windows package.
