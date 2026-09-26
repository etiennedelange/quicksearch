[CmdletBinding()]
param(
    [switch]$Offline
)

$ErrorActionPreference = 'Stop'
$crateRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\src-tauri')).Path

$cargoArgs = @('build', '--release', '--locked')
if ($Offline) {
    $cargoArgs += '--offline'
}

Push-Location $crateRoot
try {
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "Release build failed with exit code $LASTEXITCODE."
    }
} finally {
    Pop-Location
}

$executable = Join-Path $crateRoot 'target\release\quicksearch.exe'
if (-not (Test-Path -LiteralPath $executable)) {
    throw "Cargo completed but the expected executable was not found: $executable"
}
Write-Host "Release build: $executable"
