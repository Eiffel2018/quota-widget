# Validation - Quota Widget 0.3.14

- Windows x64 / MSVC release build completed with locked dependencies and the
  output target on `R:\Temp\quota-widget-build` (ImDisk RAM Drive).
- `cargo fmt --all -- --check` and `git diff --check` passed.
- The deployed fixed-path executable and release build have matching SHA-256:
  `0D11BBF3B0259F67C5C0FF6917DE70C1CD92B2CA55E7DE200C300ED794698866`.
- The automated test suite and live-account UI were not run for this release.
- The change only reads existing completed intervals and token-cost subtotals;
  it does not modify quota history, token records, or stored settings.
- Coverage-only intervals are marked lower bounds. Unknown price, assumed
  model, missing events and unreliable quota boundaries remain unknown.
- Public release archives contain source and program assets only, not local
  settings, account history, token databases or machine-specific evidence.
