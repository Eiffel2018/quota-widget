# Validation - Quota Widget 0.3.9

- Native Windows x64, Rust 1.99.1, MSVC target `x86_64-pc-windows-msvc`.
- Complete platform-applicable suite: **537 passed, 0 failed, 5 ignored**. The ignored tests are opt-in large synthetic benchmarks. Earlier Linux archive counts include platform-specific tests unavailable on Windows.
- `cargo fmt -- --check` and `cargo clippy --locked --all-targets -- -D warnings` passed.
- Native optimized release build passed. Binary SHA-256: `bbce9ec263f983e438bfa2d9e62ed288f62808d41a589a8aa42784b96108fe08`; 28195328 bytes.
- All final test temporary data and Cargo output were on an ImDisk RAM drive. Synthetic source imports, path aliases, read-only database inspection, writer-lock exclusion and the existing accounting/recovery suites passed there.
- Seven new shared-estimate regressions cover variable one-point costs, multi-point weighting, coverage-only lower bounds, invalid boundaries/prices, account/window/session/epoch isolation, missing links, ordering/duplicates, insufficient samples, numeric validity, original-ledger preservation and export annotations. A source-path regression verifies real path identity and reopening.
- The release executable reports `quota-widget 0.3.9`. An isolated `--demo --no-settings --show-history` run created a responsive native window, stayed alive for four seconds and closed gracefully with exit code 0. Its process was reaped. No live-account connection or pixel-level screenshot comparison is claimed for 0.3.9.
- Import-table inspection found standard Windows/OpenGL dependencies and `VCRUNTIME140.dll`. The MSVC package requires the [Microsoft Visual C++ v14 x64 Redistributable](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist). It is unsigned.
- The historical source revisions and staged new source were scanned before publishing. Runtime history, credentials, session files, databases, private settings, exports and validation captures are excluded from Git. Only synthetic fixtures are tracked. Machine-specific detailed logs remain local under the ignored `validation/0.3.9/` directory.

The existing running application was left in place. The new executable was tested only with isolated synthetic GUI state; local interval inspection for calibration was read-only. No RAM-drive remount, format, filesystem repair or global temporary-directory change was performed.

The final distributable was rebuilt with home/workspace path remapping. Scans found zero occurrences of the local user-home or workspace path in ASCII/UTF-16. The remapped release repeated the native responsive-window/graceful-exit smoke successfully. The build script also passed mocked preservation/restoration checks for pre-existing encoded flags containing spaces.
