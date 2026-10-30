# Regenerates app/assets/tray.ico: a white monochrome notification-area glyph
# (same shape as app/ui/assets/images/tray-icon.svg) in several PNG-compressed sizes.
Add-Type -AssemblyName System.Drawing
$sizes = 16, 20, 24, 32, 48, 64
$pngs = foreach ($s in $sizes) {
    $bmp = New-Object System.Drawing.Bitmap $s, $s, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.Clear([System.Drawing.Color]::Transparent)
    $k = $s / 24.0
    function P($x, $y) { New-Object System.Drawing.PointF ([single]($x * $k)), ([single]($y * $k)) }
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath ([System.Drawing.Drawing2D.FillMode]::Alternate)
    $path.AddPolygon([System.Drawing.PointF[]]@((P 12 1), (P 23 12), (P 12 23), (P 1 12)))
    $path.AddPolygon([System.Drawing.PointF[]]@((P 9.5 8), (P 16 12), (P 9.5 16)))
    $g.FillPath([System.Drawing.Brushes]::White, $path)
    $g.Dispose()
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    , $ms.ToArray()
}
$out = New-Object System.IO.MemoryStream
$w = New-Object System.IO.BinaryWriter $out
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i]
    $w.Write([byte]$s); $w.Write([byte]$s); $w.Write([byte]0); $w.Write([byte]0)
    $w.Write([uint16]1); $w.Write([uint16]32)
    $w.Write([uint32]$pngs[$i].Length); $w.Write([uint32]$offset)
    $offset += $pngs[$i].Length
}
foreach ($p in $pngs) { $w.Write($p) }
$w.Flush()
$target = Join-Path $PSScriptRoot '..\app\assets\tray.ico'
[System.IO.File]::WriteAllBytes($target, $out.ToArray())
Write-Host "wrote $target"
