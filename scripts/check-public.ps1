[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
Push-Location $root

try {
    Write-Host '==> Inspecting Cargo source-package boundary' -ForegroundColor Cyan
    $entries = @(& cargo package --allow-dirty --no-verify --list)
    if ($LASTEXITCODE -ne 0) {
        throw "cargo package --list failed with exit code $LASTEXITCODE"
    }

    $forbiddenEntries = @(
        $entries | Where-Object {
            $_ -match '^(dist|target|open-source)/' -or
            $_ -match '(^|/)test-evidence\.html$' -or
            $_ -match '\.(exe|zip|sqlite3|sqlite3-shm|sqlite3-wal|log)$' -or
            $_ -match '(^|/)(ledger\.toml|\.env(?:\..*)?)$'
        }
    )
    if ($forbiddenEntries.Count -gt 0) {
        throw "Forbidden files entered the source package: $($forbiddenEntries -join ', ')"
    }

    $textExtensions = @('.rs', '.md', '.toml', '.json', '.jsonl', '.sh', '.ps1', '.lock', '.gitignore')
    $patterns = [ordered]@{
        'Windows user path' = ('[A-Za-z]:\\' + 'Users\\' + '[^\\\s]+')
        'macOS user path' = ('/' + 'Users/' + '[^/\s]+')
        'Linux home path' = ('/' + 'home/' + '[^/\s]+')
        'OpenAI-style secret' = '\bsk-[A-Za-z0-9_-]{20,}\b'
        'GitHub-style secret' = '\bgh[pousr]_[A-Za-z0-9]{20,}\b'
        'AWS access key' = '\bAKIA[0-9A-Z]{16}\b'
        'Private key block' = '-----BEGIN (?:RSA |OPENSSH |EC |DSA )?PRIVATE KEY-----'
    }

    $findings = [System.Collections.Generic.List[string]]::new()
    foreach ($entry in $entries) {
        $path = Join-Path $root ($entry -replace '/', [IO.Path]::DirectorySeparatorChar)
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            continue
        }
        $extension = [IO.Path]::GetExtension($path).ToLowerInvariant()
        $leaf = [IO.Path]::GetFileName($path)
        if ($textExtensions -notcontains $extension -and $leaf -ne 'Cargo.lock' -and $leaf -ne '.gitignore') {
            continue
        }
        $content = [IO.File]::ReadAllText($path)
        foreach ($item in $patterns.GetEnumerator()) {
            if ([regex]::IsMatch($content, $item.Value)) {
                $findings.Add("$entry [$($item.Key)]")
            }
        }
    }
    if ($findings.Count -gt 0) {
        throw "Potential private content found (matching text is intentionally not printed): $($findings -join ', ')"
    }

    Write-Host '==> Inspecting dependency license metadata' -ForegroundColor Cyan
    $metadata = (& cargo metadata --format-version 1 --locked) | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata failed with exit code $LASTEXITCODE"
    }
    $missingLicenses = @(
        $metadata.packages | Where-Object {
            $_.name -ne 'token-ledger' -and
            [string]::IsNullOrWhiteSpace($_.license) -and
            [string]::IsNullOrWhiteSpace($_.license_file)
        }
    )
    if ($missingLicenses.Count -gt 0) {
        throw "Dependencies without declared license metadata: $($missingLicenses.name -join ', ')"
    }

    Write-Host "Public-content check passed: $($entries.Count) packaged entries; no forbidden paths or common secret patterns." -ForegroundColor Green
}
finally {
    Pop-Location
}
