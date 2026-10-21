# Recovered source history

Each `Import supplied ... source snapshot` commit comes from the matching
user-supplied full-source ZIP, in version order. Tags identify these imports.
Archive SHA-256 values and exclusions are recorded in `source-imports.json`.
Import commit dates reflect recovery, not original development dates.

| Source tag | Main changes visible in the supplied snapshots |
| --- | --- |
| v0.3.0 | Native Rust widget, quota history and local token import |
| v0.3.2 | Background accounting, completed intervals, reconciliation and task tree |
| v0.3.3 | UI/import refinements over 0.3.2 |
| v0.3.4 | Legacy history/settings compatibility and completed-interval table |
| v0.3.5 | Incremental SQLite storage and indexed views |
| v0.3.6 | Connection recovery and cancellable shutdown |
| v0.3.7 | Readability, contrast and accessible text sizing |
| v0.3.8 | Relevant descendants and required ancestors in the task tree |

Use `git diff v0.3.7 v0.3.8`, for example, to inspect exact source changes.
No separate 0.3.1 source archive was supplied. The earlier 0.3.0 checkpoint is
superseded by the supplied 0.3.0 full-source archive. The 0.2.1 executable has no
matching recovered source here; this history does not claim to reconstruct it.

Source, licenses, synthetic fixtures and upstream validation descriptions are
retained. Build outputs, validation captures/logs, packaging hashes and machine
data are excluded. Old validation descriptions are historical claims from their
respective archives, not new verification of those releases.

Private account settings, authentication, usage history, token databases,
session transcripts, exports and screenshots are not part of this repository.
`.gitignore` protects common runtime filenames and directories; a review of
tracked contents is also required before publishing. Only the synthetic JSONL
fixtures under `fixtures/` are allowlisted.

The earlier Electron implementation is retained under `legacy/quota-glass/`.
It contains useful references for opt-in alerts and manual reset annotations.
Its supplied source and assets are copied without modification, with individual
hashes in `SOURCE-MANIFEST.json`. Runtime data, dependencies and packaged apps
are excluded. It is historical reference material; the maintained widget is the
Rust project at the repository root.
