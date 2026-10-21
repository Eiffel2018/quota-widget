$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
# Keep local user/workspace paths out of panic metadata in distributed binaries.
# Cargo's encoded form preserves paths with spaces and existing compiler flags.
$quotaSavedEncodedFlags = $env:CARGO_ENCODED_RUSTFLAGS
$quotaFlags = @()
if ($quotaSavedEncodedFlags) {
    $quotaFlags += $quotaSavedEncodedFlags.Split([char]31)
} elseif ($env:RUSTFLAGS) {
    $quotaFlags += $env:RUSTFLAGS -split '\s+' | Where-Object { $_ }
}
if ($env:USERPROFILE) {
    $quotaFlags += "--remap-path-prefix=$env:USERPROFILE=/build-home"
}
$quotaFlags += "--remap-path-prefix=$((Get-Location).Path)=/workspace/quota-widget"
$env:CARGO_ENCODED_RUSTFLAGS = $quotaFlags -join [char]31
try {
    cargo fmt -- --check
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo test --locked
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    Write-Host 'Built release quota-widget.exe (under CARGO_TARGET_DIR when set). Test --demo before connecting your own CLI.'
} finally {
    $env:CARGO_ENCODED_RUSTFLAGS = $quotaSavedEncodedFlags
}
