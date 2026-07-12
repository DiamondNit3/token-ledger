[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
Push-Location $root

try {
    Write-Host '==> Inspecting Cargo source-package boundary' -ForegroundColor Cyan
    $entries = @(& cargo package --allow-dirty --locked --list)
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

    $textExtensions = @('.rs', '.md', '.toml', '.json', '.jsonl', '.html', '.hbs', '.txt', '.sh', '.ps1', '.lock', '.gitignore')
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

    Write-Host '==> Verifying third-party notice lock binding' -ForegroundColor Cyan
    $noticePath = Join-Path $root 'THIRD-PARTY-NOTICES.html'
    if (-not (Test-Path -LiteralPath $noticePath -PathType Leaf)) {
        throw 'THIRD-PARTY-NOTICES.html is missing.'
    }
    $lockHash = (Get-FileHash -LiteralPath (Join-Path $root 'Cargo.lock') -Algorithm SHA256).Hash.ToLowerInvariant()
    $notice = [IO.File]::ReadAllText($noticePath)
    if (-not $notice.Contains("Cargo.lock sha256:$lockHash")) {
        throw 'THIRD-PARTY-NOTICES.html does not match Cargo.lock; regenerate it before release.'
    }

    $fixedNoticeHashes = [ordered]@{
        'RUST-1.88-STANDARD-LIBRARY-NOTICES.html' = '3d3f60160f5214efa0a7fd804102d02ce9ea6af04b5249a19eeb243450246ae9'
        'MUSL-1.2.3-COPYRIGHT.txt' = 'f9bc4423732350eb0b3f7ed7e91d530298476f8fec0c6c427a1c04ade22655af'
    }
    foreach ($asset in $fixedNoticeHashes.GetEnumerator()) {
        $assetPath = Join-Path $root $asset.Key
        if (-not (Test-Path -LiteralPath $assetPath -PathType Leaf)) {
            throw "Required toolchain notice is missing: $($asset.Key)"
        }
        $actualHash = (Get-FileHash -LiteralPath $assetPath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actualHash -ne $asset.Value) {
            throw "Toolchain notice hash changed and requires review: $($asset.Key)"
        }
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

    Write-Host "Public-content check passed: $($entries.Count) packaged entries; no forbidden paths or common secret patterns; third-party notices declare the current Cargo.lock digest." -ForegroundColor Green
}
finally {
    Pop-Location
}
