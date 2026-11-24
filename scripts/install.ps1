[CmdletBinding()]
param(
    [switch]$Classic,
    [switch]$NoStartup,
    [switch]$NoPath,
    [switch]$NoStart
)

$ErrorActionPreference = 'Stop'
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path

function Find-Release([string]$Directory) {
    $cli = Join-Path $Directory 'rwm.exe'
    $hostExe = Join-Path $Directory 'rwm-host.exe'
    $dll = Get-ChildItem -LiteralPath $Directory -Filter 'rwm_hook*.dll' -File -ErrorAction SilentlyContinue | Select-Object -First 1
    if ((Test-Path -LiteralPath $cli) -and (Test-Path -LiteralPath $hostExe) -and $dll) {
        return $cli
    }
    return $null
}

$cli = Find-Release $scriptDir
if (-not $cli) { $cli = Find-Release (Split-Path -Parent $scriptDir) }

if (-not $cli) {
    $repo = Split-Path -Parent $scriptDir
    if (Test-Path -LiteralPath (Join-Path $repo 'Cargo.toml')) {
        $release = Join-Path $repo 'target\release'
        Write-Host 'Building RustWindowsMenu release binaries...'
        Push-Location -LiteralPath $repo
        try {
            & cargo build --release -p rwm-cli -p rwm-hook
            if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
        } finally { Pop-Location }
        $cli = Find-Release $release
    }
}

if (-not $cli) {
    throw 'Could not find rwm.exe, rwm-host.exe, and rwm_hook*.dll. Run this script beside the release files or from the repository scripts folder.'
}

$installArgs = @('install')
if ($Classic) { $installArgs += '--classic' }
if ($NoStartup) { $installArgs += '--no-startup' }
if ($NoPath) { $installArgs += '--no-path' }
if ($NoStart) { $installArgs += '--no-start' }

Write-Host "Running $cli $($installArgs -join ' ')"
& $cli @installArgs
if ($LASTEXITCODE -ne 0) { throw "RustWindowsMenu installation failed with exit code $LASTEXITCODE" }
Write-Host 'RustWindowsMenu installed successfully. Run `rwm status` to verify.'
