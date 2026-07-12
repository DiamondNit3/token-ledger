[CmdletBinding()]
param(
    [switch]$Check,
    [string]$CargoAbout = 'cargo-about'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
$output = Join-Path $root 'THIRD-PARTY-NOTICES.html'
$temporary = Join-Path ([IO.Path]::GetTempPath()) ("token-ledger-third-party-" + [Guid]::NewGuid().ToString('N') + '.html')
$expectedVersion = 'cargo-about 0.9.1'

Push-Location $root
try {
    $actualVersion = (& $CargoAbout --version | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $actualVersion -ne $expectedVersion) {
        throw "Expected $expectedVersion, found '$actualVersion'. Install it with: cargo install --locked --features cli cargo-about --version 0.9.1"
    }

    & $CargoAbout generate --locked --offline --fail --config about.toml --output-file $temporary about.hbs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo-about generation failed with exit code $LASTEXITCODE"
    }

    $lockHash = (Get-FileHash -LiteralPath 'Cargo.lock' -Algorithm SHA256).Hash.ToLowerInvariant()
    $header = "<!-- Generated with cargo-about 0.9.1 from Cargo.lock sha256:$lockHash; do not edit. -->`n"
    $body = [IO.File]::ReadAllText($temporary).Replace("`r`n", "`n").Replace("`r", "`n")
    $generated = $header + $body

    if ($Check) {
        if (-not (Test-Path -LiteralPath $output -PathType Leaf)) {
            throw 'THIRD-PARTY-NOTICES.html is missing; regenerate it before release.'
        }
        $tracked = [IO.File]::ReadAllText($output)
        if (-not [StringComparer]::Ordinal.Equals($generated, $tracked)) {
            throw 'THIRD-PARTY-NOTICES.html is stale; regenerate it with scripts/generate-third-party-notices.ps1.'
        }
        Write-Host 'Third-party notices match the locked four-target dependency graph.' -ForegroundColor Green
    }
    else {
        [IO.File]::WriteAllText($output, $generated, [Text.UTF8Encoding]::new($false))
        Write-Host "Generated $output" -ForegroundColor Green
    }
}
finally {
    if (Test-Path -LiteralPath $temporary) {
        Remove-Item -LiteralPath $temporary -Force
    }
    Pop-Location
}
