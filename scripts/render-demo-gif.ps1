[CmdletBinding()]
param(
    [string]$Binary,
    [string]$Output
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not $Binary) {
    $Binary = Join-Path $repoRoot 'target\debug\token-ledger.exe'
}
if (-not $Output) {
    $Output = Join-Path $repoRoot 'docs\images\token-ledger-demo.gif'
}

if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
    Push-Location $repoRoot
    try {
        & cargo build --quiet
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build failed with exit code $LASTEXITCODE"
        }
    }
    finally {
        Pop-Location
    }
}

# Capture the real command at a deterministic width. Color and Unicode are
# applied by the GIF renderer rather than embedded as terminal escape codes.
$previousWidth = $env:TOKEN_LEDGER_WIDTH
$env:TOKEN_LEDGER_WIDTH = '120'
try {
    $demoLines = @(& $Binary --color never --unicode never demo)
    if ($LASTEXITCODE -ne 0) {
        throw "token-ledger demo failed with exit code $LASTEXITCODE"
    }
}
finally {
    $env:TOKEN_LEDGER_WIDTH = $previousWidth
}

if ($demoLines.Count -lt 10 -or $demoLines[0] -ne 'TOKEN LEDGER / DEMO') {
    throw 'token-ledger demo returned an unexpected presentation; refusing to record it'
}

Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName PresentationCore
Add-Type -AssemblyName WindowsBase

$width = 1200
$height = 720
$left = 34
$top = 28
$lineHeight = 22
$prompt = 'PS> '
$commandText = 'token-ledger demo'

$regularFont = [System.Drawing.Font]::new(
    'Consolas',
    14,
    [System.Drawing.FontStyle]::Regular,
    [System.Drawing.GraphicsUnit]::Pixel
)
$boldFont = [System.Drawing.Font]::new(
    'Consolas',
    14,
    [System.Drawing.FontStyle]::Bold,
    [System.Drawing.GraphicsUnit]::Pixel
)
$heroFont = [System.Drawing.Font]::new(
    'Consolas',
    16,
    [System.Drawing.FontStyle]::Bold,
    [System.Drawing.GraphicsUnit]::Pixel
)

$backgroundColor = [System.Drawing.ColorTranslator]::FromHtml('#0b1020')
$foregroundBrush = [System.Drawing.SolidBrush]::new(
    [System.Drawing.ColorTranslator]::FromHtml('#d9e2ee')
)
$mutedBrush = [System.Drawing.SolidBrush]::new(
    [System.Drawing.ColorTranslator]::FromHtml('#7f8da3')
)
$accentBrush = [System.Drawing.SolidBrush]::new(
    [System.Drawing.ColorTranslator]::FromHtml('#58d6e7')
)
$successBrush = [System.Drawing.SolidBrush]::new(
    [System.Drawing.ColorTranslator]::FromHtml('#72d69d')
)
$warningBrush = [System.Drawing.SolidBrush]::new(
    [System.Drawing.ColorTranslator]::FromHtml('#f3c65b')
)

function Get-LineStyle {
    param([string]$Line)

    if ($Line -eq 'TOKEN LEDGER / DEMO' -or
        $Line -eq 'BY MODEL' -or
        $Line -eq 'PROVIDER UNITS' -or
        $Line -eq 'BILLING EVIDENCE' -or
        $Line -eq 'TRY IT WITH YOUR DATA') {
        return @($boldFont, $accentBrush)
    }
    if ($Line.StartsWith('Synthetic data only') -or
        $Line.StartsWith('API-equivalent') -or
        $Line.StartsWith('Details:')) {
        return @($regularFont, $mutedBrush)
    }
    if ($Line.StartsWith('$') -or $Line.Contains('[RANGE]') -or
        $Line.Contains('[INCOMPLETE EVIDENCE]') -or
        $Line.Contains('[NOT ATTESTED]')) {
        return @($heroFont, $warningBrush)
    }
    if ($Line.StartsWith('OK Snapshot') -or $Line.Contains('[STABLE]') -or
        $Line.Contains('[EXACT]')) {
        return @($boldFont, $successBrush)
    }
    if ($Line.StartsWith('MODEL ')) {
        return @($boldFont, $foregroundBrush)
    }
    return @($regularFont, $foregroundBrush)
}

function New-TerminalBitmap {
    param(
        [string]$TypedCommand,
        [int]$VisibleLineCount,
        [bool]$CursorVisible
    )

    $bitmap = [System.Drawing.Bitmap]::new(
        $width,
        $height,
        [System.Drawing.Imaging.PixelFormat]::Format32bppPArgb
    )
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    try {
        $graphics.Clear($backgroundColor)
        $graphics.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit
        $graphics.DrawString($prompt, $boldFont, $accentBrush, $left, $top)
        $promptWidth = $graphics.MeasureString($prompt, $boldFont).Width
        $graphics.DrawString(
            $TypedCommand,
            $regularFont,
            $foregroundBrush,
            $left + $promptWidth,
            $top
        )
        if ($CursorVisible) {
            $typedWidth = $graphics.MeasureString($TypedCommand, $regularFont).Width
            $cursorX = [int]($left + $promptWidth + $typedWidth + 1)
            $graphics.FillRectangle($accentBrush, $cursorX, $top + 2, 8, 16)
        }

        $visible = [Math]::Min($VisibleLineCount, $demoLines.Count)
        for ($index = 0; $index -lt $visible; $index++) {
            $line = [string]$demoLines[$index]
            $style = Get-LineStyle -Line $line
            $graphics.DrawString(
                $line,
                $style[0],
                $style[1],
                $left,
                $top + (($index + 2) * $lineHeight)
            )
        }
    }
    finally {
        $graphics.Dispose()
    }
    return $bitmap
}

function ConvertTo-GifFrame {
    param(
        [System.Drawing.Bitmap]$Bitmap,
        [UInt16]$Delay
    )

    $png = [System.IO.MemoryStream]::new()
    try {
        $Bitmap.Save($png, [System.Drawing.Imaging.ImageFormat]::Png)
        $png.Position = 0
        $decoder = [System.Windows.Media.Imaging.PngBitmapDecoder]::new(
            $png,
            [System.Windows.Media.Imaging.BitmapCreateOptions]::PreservePixelFormat,
            [System.Windows.Media.Imaging.BitmapCacheOption]::OnLoad
        )
        $source = $decoder.Frames[0]
        $metadata = [System.Windows.Media.Imaging.BitmapMetadata]::new('gif')
        $metadata.SetQuery('/grctlext/Disposal', [byte]2)
        $metadata.SetQuery('/grctlext/Delay', $Delay)
        return [System.Windows.Media.Imaging.BitmapFrame]::Create(
            $source,
            $source.Thumbnail,
            $metadata,
            $source.ColorContexts
        )
    }
    finally {
        $png.Dispose()
    }
}

$encoder = [System.Windows.Media.Imaging.GifBitmapEncoder]::new()
$script:frameDelays = [System.Collections.Generic.List[UInt16]]::new()
function Add-TerminalFrame {
    param(
        [string]$TypedCommand,
        [int]$VisibleLineCount,
        [UInt16]$Delay,
        [bool]$CursorVisible
    )

    $bitmap = New-TerminalBitmap `
        -TypedCommand $TypedCommand `
        -VisibleLineCount $VisibleLineCount `
        -CursorVisible $CursorVisible
    try {
        $frame = ConvertTo-GifFrame -Bitmap $bitmap -Delay $Delay
        $encoder.Frames.Add($frame)
        $script:frameDelays.Add($Delay)
    }
    finally {
        $bitmap.Dispose()
    }
}

function Set-GifAnimationMetadata {
    param(
        [string]$Path,
        [System.Collections.Generic.IReadOnlyList[UInt16]]$Delays
    )

    # WPF writes every frame but does not preserve per-frame GIF metadata.
    # Patch the generated Graphic Control Extension blocks deterministically,
    # then add the standard NETSCAPE2.0 infinite-loop extension.
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 14 -or
        [System.Text.Encoding]::ASCII.GetString($bytes, 0, 6) -notmatch '^GIF8[79]a$') {
        throw 'WPF did not produce a valid GIF89a stream'
    }

    $packed = $bytes[10]
    $globalTableBytes = 0
    if (($packed -band 0x80) -ne 0) {
        $globalTableBytes = 3 * [Math]::Pow(2, (($packed -band 0x07) + 1))
    }
    $dataStart = 13 + [int]$globalTableBytes
    $position = $dataStart
    $controlOffsets = [System.Collections.Generic.List[int]]::new()

    while ($position -lt $bytes.Length) {
        switch ($bytes[$position]) {
            0x21 {
                if ($position + 2 -ge $bytes.Length) {
                    throw 'truncated GIF extension'
                }
                $label = $bytes[$position + 1]
                if ($label -eq 0xF9) {
                    if ($bytes[$position + 2] -ne 4 -or $position + 7 -ge $bytes.Length) {
                        throw 'invalid GIF Graphic Control Extension'
                    }
                    $controlOffsets.Add($position)
                    $position += 8
                    continue
                }
                $position += 2
                while ($position -lt $bytes.Length) {
                    $blockLength = [int]$bytes[$position]
                    $position++
                    if ($blockLength -eq 0) {
                        break
                    }
                    $position += $blockLength
                }
                continue
            }
            0x2C {
                if ($position + 9 -ge $bytes.Length) {
                    throw 'truncated GIF image descriptor'
                }
                $imagePacked = $bytes[$position + 9]
                $position += 10
                if (($imagePacked -band 0x80) -ne 0) {
                    $position += 3 * [Math]::Pow(2, (($imagePacked -band 0x07) + 1))
                }
                # LZW minimum code size, followed by image-data sub-blocks.
                $position++
                while ($position -lt $bytes.Length) {
                    $blockLength = [int]$bytes[$position]
                    $position++
                    if ($blockLength -eq 0) {
                        break
                    }
                    $position += $blockLength
                }
                continue
            }
            0x3B {
                $position = $bytes.Length
                continue
            }
            default {
                throw "unexpected GIF block marker 0x$('{0:X2}' -f $bytes[$position])"
            }
        }
    }

    if ($controlOffsets.Count -ne $Delays.Count) {
        throw "expected $($Delays.Count) GIF control blocks, found $($controlOffsets.Count)"
    }
    for ($index = 0; $index -lt $controlOffsets.Count; $index++) {
        $offset = $controlOffsets[$index]
        $delay = [UInt16]$Delays[$index]
        # Preserve transparency and set disposal method 2 (restore background).
        $bytes[$offset + 3] = [byte](($bytes[$offset + 3] -band 0xE3) -bor 0x08)
        $bytes[$offset + 4] = [byte]($delay -band 0xFF)
        $bytes[$offset + 5] = [byte](($delay -shr 8) -band 0xFF)
    }

    $loopExtension = [byte[]](
        0x21, 0xFF, 0x0B,
        0x4E, 0x45, 0x54, 0x53, 0x43, 0x41, 0x50, 0x45, 0x32, 0x2E, 0x30,
        0x03, 0x01, 0x00, 0x00, 0x00
    )
    $animated = [byte[]]::new($bytes.Length + $loopExtension.Length)
    [Array]::Copy($bytes, 0, $animated, 0, $dataStart)
    [Array]::Copy($loopExtension, 0, $animated, $dataStart, $loopExtension.Length)
    [Array]::Copy(
        $bytes,
        $dataStart,
        $animated,
        $dataStart + $loopExtension.Length,
        $bytes.Length - $dataStart
    )
    [System.IO.File]::WriteAllBytes($Path, $animated)
}

try {
    # Type two characters per frame: about 0.9 seconds.
    for ($count = 2; $count -lt $commandText.Length; $count += 2) {
        Add-TerminalFrame `
            -TypedCommand $commandText.Substring(0, $count) `
            -VisibleLineCount 0 `
            -Delay ([UInt16]10) `
            -CursorVisible $true
    }
    Add-TerminalFrame `
        -TypedCommand $commandText `
        -VisibleLineCount 0 `
        -Delay ([UInt16]40) `
        -CursorVisible $false

    # Reveal two real output lines per frame: about 4.5 seconds.
    for ($visible = 2; $visible -le $demoLines.Count; $visible += 2) {
        Add-TerminalFrame `
            -TypedCommand $commandText `
            -VisibleLineCount $visible `
            -Delay ([UInt16]30) `
            -CursorVisible $false
    }
    if (($demoLines.Count % 2) -ne 0) {
        Add-TerminalFrame `
            -TypedCommand $commandText `
            -VisibleLineCount $demoLines.Count `
            -Delay ([UInt16]30) `
            -CursorVisible $false
    }

    # Hold the complete report. Total playback is approximately 15 seconds.
    Add-TerminalFrame `
        -TypedCommand $commandText `
        -VisibleLineCount $demoLines.Count `
        -Delay ([UInt16]900) `
        -CursorVisible $true

    $outputDirectory = Split-Path -Parent $Output
    New-Item -ItemType Directory -Force -Path $outputDirectory | Out-Null
    $stream = [System.IO.File]::Open(
        $Output,
        [System.IO.FileMode]::Create,
        [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::None
    )
    try {
        $encoder.Save($stream)
    }
    finally {
        $stream.Dispose()
    }
    Set-GifAnimationMetadata -Path $Output -Delays $script:frameDelays
}
finally {
    $regularFont.Dispose()
    $boldFont.Dispose()
    $heroFont.Dispose()
    $foregroundBrush.Dispose()
    $mutedBrush.Dispose()
    $accentBrush.Dispose()
    $successBrush.Dispose()
    $warningBrush.Dispose()
}

$item = Get-Item -LiteralPath $Output
Write-Host "Rendered $($encoder.Frames.Count) frames (~15 seconds) to $($item.FullName)"
