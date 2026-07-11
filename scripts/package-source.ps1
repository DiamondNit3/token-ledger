[CmdletBinding()]
param(
    [string]$OutputDirectory,
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $root 'open-source'
}
elseif (-not [IO.Path]::IsPathRooted($OutputDirectory)) {
    $OutputDirectory = Join-Path $root $OutputDirectory
}

Push-Location $root
$stage = $null
try {
    & (Join-Path $PSScriptRoot 'check-public.ps1')
    if ($LASTEXITCODE -ne 0) {
        throw "Public-content check failed with exit code $LASTEXITCODE"
    }

    $metadata = (& cargo metadata --format-version 1 --no-deps --locked) | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata failed with exit code $LASTEXITCODE"
    }
    $package = $metadata.packages | Where-Object { $_.name -eq 'token-ledger' } | Select-Object -First 1
    if ($null -eq $package) {
        throw 'Unable to locate the token-ledger package in Cargo metadata.'
    }

    Write-Host "==> Building source package $($package.version)" -ForegroundColor Cyan
    & cargo package --allow-dirty --no-verify --locked
    if ($LASTEXITCODE -ne 0) {
        throw "cargo package failed with exit code $LASTEXITCODE"
    }

    $source = Join-Path $root "target\package\token-ledger-$($package.version).crate"
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "Cargo did not create the expected archive: $source"
    }

    New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
    $destination = Join-Path $OutputDirectory "token-ledger-$($package.version)-source.crate"
    $zipDestination = Join-Path $OutputDirectory "token-ledger-$($package.version)-source.zip"
    $artifactPaths = @($destination, "$destination.sha256", $zipDestination, "$zipDestination.sha256")
    $existing = @($artifactPaths | Where-Object { Test-Path -LiteralPath $_ })
    if (-not $Force -and $existing.Count -gt 0) {
        throw "Source artifact already exists. Remove it or rerun with -Force: $($existing -join ', ')"
    }

    Copy-Item -LiteralPath $source -Destination $destination -Force:$Force
    $hash = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText("$destination.sha256", "$hash  $([IO.Path]::GetFileName($destination))`n", [Text.UTF8Encoding]::new($false))

    Write-Host '==> Building reviewer-friendly ZIP from the same allowlist' -ForegroundColor Cyan
    $stage = Join-Path $OutputDirectory (".source-stage-" + [Guid]::NewGuid().ToString('N'))
    $resolvedOutput = [IO.Path]::GetFullPath($OutputDirectory).TrimEnd([IO.Path]::DirectorySeparatorChar)
    $resolvedStage = [IO.Path]::GetFullPath($stage)
    if (-not $resolvedStage.StartsWith($resolvedOutput + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to create staging directory outside the output directory: $resolvedStage"
    }
    New-Item -ItemType Directory -Path $stage | Out-Null

    $entries = @(& cargo package --allow-dirty --no-verify --list)
    if ($LASTEXITCODE -ne 0) {
        throw "cargo package --list failed with exit code $LASTEXITCODE"
    }
    foreach ($entry in $entries) {
        $relative = $entry -replace '/', [IO.Path]::DirectorySeparatorChar
        $entrySource = Join-Path $root $relative
        if (-not (Test-Path -LiteralPath $entrySource -PathType Leaf)) {
            continue
        }
        $entryDestination = Join-Path $stage $relative
        $parent = Split-Path -Parent $entryDestination
        if (-not (Test-Path -LiteralPath $parent)) {
            New-Item -ItemType Directory -Path $parent -Force | Out-Null
        }
        Copy-Item -LiteralPath $entrySource -Destination $entryDestination
    }
    Copy-Item -LiteralPath (Join-Path $root '.gitignore') -Destination (Join-Path $stage '.gitignore')

    Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zipDestination -CompressionLevel Optimal -Force:$Force
    $zipHash = (Get-FileHash -LiteralPath $zipDestination -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText("$zipDestination.sha256", "$zipHash  $([IO.Path]::GetFileName($zipDestination))`n", [Text.UTF8Encoding]::new($false))

    Write-Host "Source archive: $destination" -ForegroundColor Green
    Write-Host "SHA-256: $hash" -ForegroundColor Green
    Write-Host "Source ZIP: $zipDestination" -ForegroundColor Green
    Write-Host "SHA-256: $zipHash" -ForegroundColor Green
}
finally {
    if ($null -ne $stage -and (Test-Path -LiteralPath $stage)) {
        $resolvedOutput = [IO.Path]::GetFullPath($OutputDirectory).TrimEnd([IO.Path]::DirectorySeparatorChar)
        $resolvedStage = [IO.Path]::GetFullPath($stage)
        if ($resolvedStage.StartsWith($resolvedOutput + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
            Remove-Item -LiteralPath $resolvedStage -Recurse -Force
        }
    }
    Pop-Location
}
