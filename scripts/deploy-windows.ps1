param(
    [Parameter(Mandatory = $true)][string]$SourcePath,
    [string]$DestinationDirectory = (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
)
$ErrorActionPreference = 'Stop'
$quotaSource = (Resolve-Path -LiteralPath $SourcePath).Path
$quotaDestination = [IO.Path]::GetFullPath($DestinationDirectory)
$quotaExecutable = Join-Path $quotaDestination 'quota-widget.exe'
if ($quotaSource -eq $quotaExecutable) { throw 'Source and installed executable must differ.' }
New-Item -ItemType Directory -Path $quotaDestination -Force | Out-Null
$quotaIncoming = Join-Path $quotaDestination 'quota-widget.next.exe'
$quotaPrevious = Join-Path $quotaDestination 'quota-widget.previous.exe'
$quotaHash = (Get-FileHash -LiteralPath $quotaSource -Algorithm SHA256).Hash
if ((Test-Path -LiteralPath $quotaExecutable) -and
    (Get-FileHash -LiteralPath $quotaExecutable -Algorithm SHA256).Hash -eq $quotaHash) {
    Write-Host "Already deployed: $quotaExecutable"
    return
}
Copy-Item -LiteralPath $quotaSource -Destination $quotaIncoming -Force
if ((Get-FileHash -LiteralPath $quotaIncoming -Algorithm SHA256).Hash -ne $quotaHash) {
    throw 'Staged executable hash mismatch; installed executable preserved.'
}
if (Test-Path -LiteralPath $quotaExecutable) {
    # Replace by renaming on the same volume. A running instance can finish
    # using its old image; its next launch uses the new fixed-path executable.
    [IO.File]::Replace($quotaIncoming, $quotaExecutable, $quotaPrevious, $true)
} else {
    [IO.File]::Move($quotaIncoming, $quotaExecutable)
}
if ((Get-FileHash -LiteralPath $quotaExecutable -Algorithm SHA256).Hash -ne $quotaHash) {
    throw 'Installed executable hash mismatch.'
}
[IO.File]::WriteAllText((Join-Path $quotaDestination 'SHA256SUMS.txt'),
    $quotaHash.ToLowerInvariant() + "  quota-widget.exe`n", [Text.UTF8Encoding]::new($false))
Write-Host "Deployed: $quotaExecutable (restart the widget to use this build)."
