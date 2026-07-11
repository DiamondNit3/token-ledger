[CmdletBinding()]
param(
    [switch]$Msrv
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
Push-Location $root

function Invoke-CargoStep {
    param(
        [Parameter(Mandatory)]
        [string]$Name,
        [Parameter(Mandatory)]
        [string[]]$Arguments
    )

    Write-Host "==> $Name" -ForegroundColor Cyan
    & cargo @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Name failed with exit code $LASTEXITCODE"
    }
}

try {
    $rustc = (& rustc --version).Trim()
    if ($LASTEXITCODE -ne 0 -or $rustc -notmatch '^rustc (\d+\.\d+\.\d+)') {
        throw 'Unable to determine the active Rust version.'
    }
    if ([version]$matches[1] -lt [version]'1.88.0') {
        throw "Rust 1.88.0 or newer is required; active toolchain is $rustc"
    }

    Write-Host "==> Toolchain: $rustc" -ForegroundColor Cyan
    Invoke-CargoStep 'Formatting' @('fmt', '--all', '--', '--check')
    Invoke-CargoStep 'Clippy' @('clippy', '--all-targets', '--all-features', '--locked', '--', '-D', 'warnings')
    Invoke-CargoStep 'Tests' @('test', '--all-targets', '--locked')

    if ($Msrv) {
        Write-Host '==> Minimum Rust version tests (1.88.0)' -ForegroundColor Cyan
        & cargo '+1.88.0' test --all-targets --locked
        if ($LASTEXITCODE -ne 0) {
            throw "Minimum Rust version tests failed with exit code $LASTEXITCODE"
        }
    }

    & (Join-Path $PSScriptRoot 'check-public.ps1')
    if ($LASTEXITCODE -ne 0) {
        throw "Public-content check failed with exit code $LASTEXITCODE"
    }

    Write-Host 'All Token Ledger checks passed.' -ForegroundColor Green
}
finally {
    Pop-Location
}
