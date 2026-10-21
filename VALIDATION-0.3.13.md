# Validation - Quota Widget 0.3.13

- Windows x64 / MSVC, Rust 1.99.1; unchanged locked dependencies.
- Full suite: 545 passed, 0 failed, 5 opt-in benchmarks ignored.
- After the recent-event cache adjustment, all 68 native GUI unit tests passed.
- Formatting and strict all-target Clippy passed. Temporary files and compiler
  outputs used the RAM drive.
- Regressions cover exhausted prior periods, preserved uncertain observations,
  deadline jitter, expired old snapshots, unchanged raw history values,
  ordering, direct/log echo preference and ambiguous account matches.
- A GUI regression verifies recovered declines appear in both the recent-five
  preview and the five-column debit table, with unconfirmed costs left unknown.
- Existing reset/baseline and accounting guards pass. Uncertain observations do
  not become confirmed cost boundaries or price-calibration samples.
- Read-only local verification confirmed recovered observations remain visible
  between surrounding direct observations. Original data files are preserved.
  Detailed machine-specific proof stays in ignored validation/0.3.13/.

- Installed fixed-path EXE reports 0.3.13; 28,309,504 bytes; SHA-256 `46cf8d4861d8fb84e0cc57a2874d3795c1ae77aeba958a9b4b0a634b9a55d635`.
  Installed/compiled bytes match; local home/workspace paths are absent in
  ASCII and UTF-16. An isolated native demo with history opened remained
  responsive and closed gracefully with exit code 0. The existing user process
  was left running. Real-account GUI interaction and screenshot comparison were
  not performed; restart the application to load the installed code.
- Public archives contain code and program assets only, excluding private
  histories, settings, database files and machine-specific evidence.
