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
$quotaIcon = Join-Path (Split-Path -Parent $PSScriptRoot) 'assets\icon.ico'
if (Test-Path -LiteralPath $quotaIcon) {
    Copy-Item -LiteralPath $quotaIcon -Destination (Join-Path $quotaDestination 'quota-widget.ico') -Force
}
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
    $quotaInstalledHash = (Get-FileHash -LiteralPath $quotaExecutable -Algorithm SHA256).Hash
    try {
        [IO.File]::Replace($quotaIncoming, $quotaExecutable, $quotaPrevious, $true)
    } catch [IO.IOException] {
        # A process may still run the image now named previous.exe after an
        # earlier update. Preserve that mapped backup and use a fresh rollback
        # file. Retry only if the installed and staged files remain intact.
        if (-not (Test-Path -LiteralPath $quotaIncoming) -or
            -not (Test-Path -LiteralPath $quotaExecutable) -or
            (Get-FileHash -LiteralPath $quotaExecutable -Algorithm SHA256).Hash -ne $quotaInstalledHash -or
            (Get-FileHash -LiteralPath $quotaIncoming -Algorithm SHA256).Hash -ne $quotaHash) {
            throw
        }
        $quotaRetainedBackup = Join-Path $quotaDestination ("quota-widget.previous-" + [Guid]::NewGuid().ToString('N') + '.exe')
        [IO.File]::Replace($quotaIncoming, $quotaExecutable, $quotaRetainedBackup, $true)
        Write-Host "Previous backup in use; rollback image retained at $quotaRetainedBackup"
    }
} else {
    [IO.File]::Move($quotaIncoming, $quotaExecutable)
}
if ((Get-FileHash -LiteralPath $quotaExecutable -Algorithm SHA256).Hash -ne $quotaHash) {
    throw 'Installed executable hash mismatch.'
}
[IO.File]::WriteAllText((Join-Path $quotaDestination 'SHA256SUMS.txt'),
    $quotaHash.ToLowerInvariant() + "  quota-widget.exe`n", [Text.UTF8Encoding]::new($false))
Write-Host "Deployed: $quotaExecutable (restart the widget to use this build)."
