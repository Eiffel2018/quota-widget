$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
cargo fmt -- --check
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo test --locked
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo build --release --locked
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Write-Host 'Built target/release/quota-widget.exe. Test --demo before connecting your own CLI.'
