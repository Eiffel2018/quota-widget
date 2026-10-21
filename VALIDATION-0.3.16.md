# Validation - Quota Widget 0.3.16

- Windows x64 release build completed with locked dependencies; all Cargo build output used R:\Temp\quota-widget-build on the RAM Drive.
- cargo fmt --all -- --check and git diff --check passed.
- Deployed to the fixed path N:\PublicRepo\quota-widget-windows-x64\quota-widget.exe; SHA-256: $sha.
- The history reconstruction joins an interval to the nearest preceding matching percentage within the same source/window and, when available, the same account ID. It tolerates up to 90 seconds of reset timestamp jitter and computes token usage from that historical boundary across app restarts.
- The interval table displays only the observed change time; model event counts and USD use separate lines. A period with no token events stays unknown, including hover details.
- Automated tests and live-account UI were not run. User history, settings, token databases and local build artifacts are excluded from the public release.