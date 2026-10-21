# Validation - Quota Widget 0.3.15

- Windows x64 release build completed with locked dependencies and the build target on the R: ImDisk RAM Drive.
- `cargo fmt --all -- --check` and `git diff --check` passed.
- Deployed to the fixed path `the established fixed executable path`; SHA-256: `AED41B03026874F4C41BEF124E5AB87CB12238F3DEB6A828A5C6FA8E9BA91F34`.
- Automated tests and live-account UI were not run; source formatting and build checks passed.
- Generic `account/updated` notifications now refresh quota data without resetting the historical interval. A confirmed account ID change resets the current boundary.
- The history table joins sequential percentage drops only when source, window and reset cycle match. Recovered USD is based on local token events and model pricing; missing coverage remains identified as a lower bound.
- Public release contents are source and application assets only. User history, settings, token databases and local build artifacts remain excluded.
