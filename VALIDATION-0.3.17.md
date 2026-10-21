# Validation - Quota Widget 0.3.17

- Windows x64 release build completed with locked dependencies; all Cargo build output used R:\Temp\quota-widget-build on the RAM Drive.
- cargo fmt --all -- --check and git diff --check passed.
- Deployed to the fixed path N:\PublicRepo\quota-widget-windows-x64\quota-widget.exe; SHA-256: $sha.
- The compact table uses narrower columns, the final heading is Total, and two-line model counts/prices are no longer truncated.
- Automated tests and live-account UI were not run. User history, settings, token databases and local build artifacts are excluded from the public release.