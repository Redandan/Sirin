param(
  [Parameter(Mandatory = $true)][string]$ImagePath,
  [Parameter(Mandatory = $true)][string]$Needle,
  [int]$MaxResults = 5
)

$ErrorActionPreference = "Stop"
$OutputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)

function Emit-Json {
  param([object]$Obj)
  $Obj | ConvertTo-Json -Depth 10 -Compress
}

function Normalize-Text {
  param([string]$Text)
  if ([string]::IsNullOrWhiteSpace($Text)) { return "" }
  $v = $Text.ToLowerInvariant()

  # Normalize common Traditional Chinese glyphs to Simplified by codepoint.
  # Use ASCII-only source to avoid WinPS 5.1 script-encoding pitfalls.
  $zhMap = @{
    28204 = 27979  # U+6E2C -> U+6D4B
    35430 = 35797  # U+8A66 -> U+8BD5
    36023 = 20080  # U+8CB7 -> U+4E70
    36067 = 21334  # U+8CE3 -> U+5356
    24115 = 36134  # U+5E33 -> U+8D26
    34399 = 21495  # U+865F -> U+53F7
    30908 = 30721  # U+78BC -> U+7801
    37636 = 24405  # U+9304 -> U+5F55
    32178 = 32593  # U+7DB2 -> U+7F51
    38913 = 39029  # U+9801 -> U+9875
    40670 = 28857  # U+9EDE -> U+70B9
    25802 = 20987  # U+64CA -> U+51FB
    37429 = 38062  # U+9235 -> U+94AE
    39023 = 26174  # U+986F -> U+663E
    38364 = 20851  # U+95DC -> U+5173
    38281 = 38381  # U+9589 -> U+95ED
    21855 = 21551  # U+555F -> U+542F
    27402 = 26435  # U+6B0A -> U+6743
    39511 = 39564  # U+9A57 -> U+9A8C
    35657 = 35777  # U+8B49 -> U+8BC1
    21209 = 21153  # U+52D9 -> U+52A1
    36039 = 36164  # U+8CC7 -> U+8D44
    35338 = 35759  # U+8A0A -> U+8BAF
    36664 = 36755  # U+8F38 -> U+8F93
    30906 = 30830  # U+78BA -> U+786E
    35469 = 35748  # U+8A8D -> U+8BA4
    37679 = 38169  # U+932F -> U+9519
    35492 = 35823  # U+8AA4 -> U+8BEF
    35531 = 35831  # U+8ACB -> U+8BF7
    36984 = 36873  # U+9078 -> U+9009
    38917 = 39033  # U+9805 -> U+9879
    21934 = 21333  # U+55AE -> U+5355
    38617 = 21452  # U+96D9 -> U+53CC
    20729 = 20215  # U+50F9 -> U+4EF7
    40845 = 40857  # U+9F8D -> U+9F99
    33274 = 21488  # U+81FA -> U+53F0
    28771 = 28286  # U+7063 -> U+6E7E
  }
  $sb = New-Object System.Text.StringBuilder
  foreach ($ch in $v.ToCharArray()) {
    $code = [int][char]$ch
    if ($zhMap.ContainsKey($code)) {
      [void]$sb.Append([char]$zhMap[$code])
    }
    else {
      [void]$sb.Append($ch)
    }
  }
  $v = $sb.ToString()

  $v = [System.Text.RegularExpressions.Regex]::Replace($v, "\s+", "")
  $v = [System.Text.RegularExpressions.Regex]::Replace($v, "[\p{P}\p{S}]", "")
  return $v
}

function Await-WinRT {
  param(
    [Parameter(Mandatory = $true)]$AsyncOperation,
    [Parameter(Mandatory = $true)][Type]$ResultType
  )

  $asTaskMethod = [System.WindowsRuntimeSystemExtensions].GetMethods() |
    Where-Object {
      $_.Name -eq 'AsTask' -and
      $_.GetParameters().Count -eq 1 -and
      $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
    } |
    Select-Object -First 1

  if (-not $asTaskMethod) {
    throw 'Cannot resolve WindowsRuntimeSystemExtensions.AsTask(IAsyncOperation<T>).'
  }

  $generic = $asTaskMethod.MakeGenericMethod($ResultType)
  $task = $generic.Invoke($null, @($AsyncOperation))
  $task.GetAwaiter().GetResult()
}

try {
  Add-Type -AssemblyName System.Runtime.WindowsRuntime
  $null = [Windows.Storage.StorageFile, Windows.Storage, ContentType = WindowsRuntime]
  $null = [Windows.Storage.FileAccessMode, Windows.Storage, ContentType = WindowsRuntime]
  $null = [Windows.Graphics.Imaging.BitmapDecoder, Windows.Graphics.Imaging, ContentType = WindowsRuntime]
  $null = [Windows.Graphics.Imaging.SoftwareBitmap, Windows.Graphics.Imaging, ContentType = WindowsRuntime]
  $null = [Windows.Graphics.Imaging.BitmapPixelFormat, Windows.Graphics.Imaging, ContentType = WindowsRuntime]
  $null = [Windows.Graphics.Imaging.BitmapAlphaMode, Windows.Graphics.Imaging, ContentType = WindowsRuntime]
  $null = [Windows.Media.Ocr.OcrEngine, Windows.Media.Ocr, ContentType = WindowsRuntime]
  $null = [Windows.Globalization.Language, Windows.Globalization, ContentType = WindowsRuntime]

  $fileOp = [Windows.Storage.StorageFile]::GetFileFromPathAsync($ImagePath)
  $file = Await-WinRT -AsyncOperation $fileOp -ResultType ([Windows.Storage.StorageFile])

  $streamOp = $file.OpenAsync([Windows.Storage.FileAccessMode]::Read)
  $stream = Await-WinRT -AsyncOperation $streamOp -ResultType ([Windows.Storage.Streams.IRandomAccessStream])

  $decoderOp = [Windows.Graphics.Imaging.BitmapDecoder]::CreateAsync($stream)
  $decoder = Await-WinRT -AsyncOperation $decoderOp -ResultType ([Windows.Graphics.Imaging.BitmapDecoder])

  $bitmapOp = $decoder.GetSoftwareBitmapAsync()
  $bitmap = Await-WinRT -AsyncOperation $bitmapOp -ResultType ([Windows.Graphics.Imaging.SoftwareBitmap])
  $bitmap = [Windows.Graphics.Imaging.SoftwareBitmap]::Convert(
    $bitmap,
    [Windows.Graphics.Imaging.BitmapPixelFormat]::Bgra8,
    [Windows.Graphics.Imaging.BitmapAlphaMode]::Premultiplied
  )

  $availableTags = @([Windows.Media.Ocr.OcrEngine]::AvailableRecognizerLanguages | ForEach-Object { $_.LanguageTag })
  $engine = $null
  $engineTag = $null
  $preferredTags = @('zh-Hant', 'zh-TW', 'zh-Hans', 'zh-CN', 'en-US')

  foreach ($tag in $preferredTags) {
    try {
      $lang = [Windows.Globalization.Language]::new($tag)
      $candidate = [Windows.Media.Ocr.OcrEngine]::TryCreateFromLanguage($lang)
      if ($candidate) {
        $engine = $candidate
        $engineTag = $tag
        break
      }
    }
    catch {
      # Continue trying next language tag.
    }
  }

  if (-not $engine) {
    $engine = [Windows.Media.Ocr.OcrEngine]::TryCreateFromUserProfileLanguages()
    if ($engine) {
      $engineTag = 'user-profile'
    }
  }

  if (-not $engine) {
    throw "Windows OCR engine unavailable. Check Windows language packs."
  }

  $ocrOp = $engine.RecognizeAsync($bitmap)
  $ocr = Await-WinRT -AsyncOperation $ocrOp -ResultType ([Windows.Media.Ocr.OcrResult])
  $needleCmp = $Needle.Trim()
  $needleNorm = Normalize-Text $needleCmp
  $matches = @()

  foreach ($line in $ocr.Lines) {
    $lineText = [string]$line.Text
    if ([string]::IsNullOrWhiteSpace($lineText)) { continue }

    $lineNorm = Normalize-Text $lineText
    if ($lineText.IndexOf($needleCmp, [System.StringComparison]::OrdinalIgnoreCase) -ge 0 -or
      ($needleNorm.Length -gt 0 -and $lineNorm.Contains($needleNorm))) {
      $rects = @($line.Words | ForEach-Object { $_.BoundingRect })
      if ($rects.Count -gt 0) {
        $minX = [double]::PositiveInfinity
        $minY = [double]::PositiveInfinity
        $maxX = [double]::NegativeInfinity
        $maxY = [double]::NegativeInfinity

        foreach ($r in $rects) {
          $x = [double]$r.X
          $y = [double]$r.Y
          $w = [double]$r.Width
          $h = [double]$r.Height
          if ($x -lt $minX) { $minX = $x }
          if ($y -lt $minY) { $minY = $y }
          if (($x + $w) -gt $maxX) { $maxX = $x + $w }
          if (($y + $h) -gt $maxY) { $maxY = $y + $h }
        }

        $width = [Math]::Max(0, $maxX - $minX)
        $height = [Math]::Max(0, $maxY - $minY)
        $matches += [PSCustomObject]@{
          text = $lineText
          x = [Math]::Round($minX, 2)
          y = [Math]::Round($minY, 2)
          width = [Math]::Round($width, 2)
          height = [Math]::Round($height, 2)
          centerX = [Math]::Round($minX + ($width / 2), 2)
          centerY = [Math]::Round($minY + ($height / 2), 2)
          score = 1.0
          source = "line"
        }
      }
    }

    if ($matches.Count -ge $MaxResults) { break }
  }

  if ($matches.Count -lt $MaxResults) {
    foreach ($line in $ocr.Lines) {
      foreach ($word in $line.Words) {
        $wordText = [string]$word.Text
        if ([string]::IsNullOrWhiteSpace($wordText)) { continue }
        $wordNorm = Normalize-Text $wordText
        if ($wordText.IndexOf($needleCmp, [System.StringComparison]::OrdinalIgnoreCase) -lt 0 -and
          -not ($needleNorm.Length -gt 0 -and $wordNorm.Contains($needleNorm))) { continue }

        $r = $word.BoundingRect
        $x = [double]$r.X
        $y = [double]$r.Y
        $w = [double]$r.Width
        $h = [double]$r.Height

        $matches += [PSCustomObject]@{
          text = $wordText
          x = [Math]::Round($x, 2)
          y = [Math]::Round($y, 2)
          width = [Math]::Round($w, 2)
          height = [Math]::Round($h, 2)
          centerX = [Math]::Round($x + ($w / 2), 2)
          centerY = [Math]::Round($y + ($h / 2), 2)
          score = 0.8
          source = "word"
        }

        if ($matches.Count -ge $MaxResults) { break }
      }

      if ($matches.Count -ge $MaxResults) { break }
    }
  }

  $out = [PSCustomObject]@{
    ok = $true
    engine = "windows-ocr"
    engineLanguage = $engineTag
    availableLanguages = $availableTags
    needle = $needleCmp
    needleNorm = $needleNorm
    textLength = ([string]$ocr.Text).Length
    textPreview = (([string]$ocr.Text) -replace "\s+", " ").Trim().Substring(0, [Math]::Min(120, (([string]$ocr.Text) -replace "\s+", " ").Trim().Length))
    matchCount = $matches.Count
    matches = $matches
  }

  Emit-Json $out
}
catch {
  $out = [PSCustomObject]@{
    ok = $false
    engine = "windows-ocr"
    error = $_.Exception.Message
    matches = @()
  }

  Emit-Json $out
}
