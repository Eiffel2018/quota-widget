# Validation - Quota Widget 0.3.12

- Native Windows x64 / MSVC, Rust 1.99.1; unchanged dependencies.
- Complete suite: 541 passed, 0 failed, 5 opt-in benchmarks ignored.
- Formatting and strict all-target Clippy passed.
- Includes the 0.3.11 writer recovery and five-column table validation.
- Additional regression: a restarted observer missing three quota drops reuses
  the historical calibration without adding missing costs or quota samples.
  Increasing quota still blocks reuse.
- A Windows size-guard fixture previously allocated 256 MiB despite its sparse
  intent. Marking it sparse with FSCTL_SET_SPARSE fixed its storage failure on
  the busy ImDisk NTFS RAM drive. All compiler/test work stayed on RAM storage.
- Fixed-path deployment succeeded while the older application remained running.
  An isolated native demo with history opened stayed responsive and closed with
  exit code 0. Real-account GUI interaction and screenshot comparison were not
  performed; the user must restart the installed application to load new code.
- Installed version: 0.3.12; 28,241,408 bytes; SHA-256 `7ed89baecfba9ab19ac9997b657634caa6f132e260ce88b71c2ee188c451697b`.
  Installed/compiled bytes match. No home/workspace strings in ASCII or UTF-16.
- Public archives contain source and program assets only; private histories,
  settings, databases, shortcuts and machine-specific evidence are excluded.
