# Validation - Quota Widget 0.3.10

- Native Windows x64, Rust 1.99.1 / MSVC, locked dependencies unchanged.
- Existing suite: 537 passed, 0 failed, 5 opt-in large benchmarks ignored. The additional Windows Shell integration regression passed: 538 passing tests in total.
- Formatting and strict all-target Clippy passed. Test data and compiler outputs used the RAM drive.
- The original executable exposed zero Shell icon groups. The new installed EXE exposes one RT_GROUP_ICON containing seven RT_ICON images at 16, 24, 32, 48, 64, 128 and 256 pixels. Each embedded image was read through Windows resource APIs and matched the corresponding ICO asset bytes.
- Native ExtractIconExW regression verified both large and small icon handles without launching the application or accessing an account.
- Native isolated demo created a responsive window, stayed alive for three seconds and closed gracefully with exit code 0. No real-account or pixel-level taskbar screenshot verification is claimed.
- Fixed-path deployment succeeded and reports version 0.3.10. The companion ICO was deployed; existing app pins were redirected to the fixed EXE and explicit ICO path, with private shortcut backups retained locally. No pins were created, deleted or reordered by this work, and Explorer was not restarted.
- Local home/workspace strings were absent from the final executable in ASCII and UTF-16. Private shortcut backups, usage histories, databases, settings, sessions and detailed logs are excluded from the public source/archive.
- Binary: 28220928 bytes; SHA-256 `27e4cc5f827a7272a22c1a3e4166d44d4cf6ec4ffe221c6e4219bd8a3846f603`. Unsigned MSVC build; the Visual C++ v14 x64 runtime requirement is unchanged.

Accounting, quota calculations, imports and database schemas are unchanged. Detailed machine-specific evidence stays in ignored validation/0.3.10/.
