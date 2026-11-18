<#
.SYNOPSIS
  Genera los iconos de XtendedView a partir de una imagen cuadrada.

.DESCRIPTION
  - Si la imagen tiene fondo blanco (sin transparencia), recorta el cuadrado redondeado del
    icono y deja las esquinas transparentes; si ya es transparente, la usa tal cual.
  - Crea:
      assets\xtendedview.png   1024 px, la referencia
      assets\xtendedview.ico   el del ejecutable (de 16 a 256 px)
      web\favicon.ico          el de las pestañas del navegador
      web\icon-192.png, web\icon-512.png   los de "Añadir a pantalla de inicio"
      web\apple-touch-icon.png el de iPad / iPhone
  Si la imagen no es PNG (p. ej. WebP), hace falta ffmpeg en el PATH para convertirla.
  Después hay que recompilar el host para que el ejecutable lleve el icono nuevo.

.PARAMETER Margen
  Espacio libre alrededor del cuadrado redondeado, en % del lado, por cada lado (1 por defecto:
  el icono aprovecha casi todo el espacio, como la mayoría de los de la barra de tareas).

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File tools\iconos.ps1 C:\ruta\icono.webp
#>
param([Parameter(Mandatory)][string]$Fuente, [double]$Margen = 1)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$root = Split-Path $PSScriptRoot -Parent
$assets = Join-Path $root 'assets'
$web = Join-Path $root 'web'
New-Item -ItemType Directory -Force $assets | Out-Null

# 1. System.Drawing no lee WebP: se pasa a PNG con ffmpeg.
$png = (Resolve-Path $Fuente).Path
if ([IO.Path]::GetExtension($png) -ne '.png') {
    $tmp = Join-Path $env:TEMP 'xtendedview-fuente.png'
    & ffmpeg -v error -y -i $png -pix_fmt rgba $tmp
    if ($LASTEXITCODE) { throw 'ffmpeg no ha podido convertir la imagen.' }
    $png = $tmp
}
$src = [System.Drawing.Bitmap]::FromFile($png)
$w = $src.Width
$h = $src.Height

function New-Canvas([int]$n) {
    $bmp = New-Object System.Drawing.Bitmap $n, $n, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    return @($bmp, $g)
}

# 2. Imagen de referencia de 1024 px.
$N = 1024
if ($src.GetPixel(0, 0).A -lt 128) {
    # Ya trae transparencia: solo se escala.
    $master, $g = New-Canvas $N
    $g.DrawImage($src, 0, 0, $N, $N)
    $g.Dispose()
} else {
    # Fondo blanco: se busca el cuadrado redondeado desde el centro de cada lado y su radio
    # por la diagonal (el borde curvo corta la diagonal a r·(1 − 1/√2) de la esquina).
    function Test-Bg($c) { ($c.R + $c.G + $c.B) -ge 600 }
    $midX = [int]($w / 2); $midY = [int]($h / 2)
    $l = 0; while ($l -lt $midX -and (Test-Bg $src.GetPixel($l, $midY))) { $l++ }
    $r = $w - 1; while ($r -gt $midX -and (Test-Bg $src.GetPixel($r, $midY))) { $r-- }
    $t = 0; while ($t -lt $midY -and (Test-Bg $src.GetPixel($midX, $t))) { $t++ }
    $b = $h - 1; while ($b -gt $midY -and (Test-Bg $src.GetPixel($midX, $b))) { $b-- }
    $d = 0; while ($d -lt $midX -and (Test-Bg $src.GetPixel($l + $d, $t + $d))) { $d++ }
    $radius = $d / (1 - [math]::Sqrt(0.5))
    Write-Host ("Icono encontrado: x {0}-{1}, y {2}-{3}, radio de las esquinas {4:N0} px" -f $l, $r, $t, $b, $radius)

    # Recorte cuadrado centrado en el icono, con el margen pedido por cada lado.
    $side = [math]::Max($r - $l, $b - $t) * (1 + 2 * $Margen / 100)
    $cx = ($l + $r) / 2; $cy = ($t + $b) / 2
    $crop = New-Object System.Drawing.RectangleF ([single]($cx - $side / 2)), ([single]($cy - $side / 2)), ([single]$side), ([single]$side)
    $k = $N / $side

    $flat, $g = New-Canvas $N
    $g.DrawImage($src, (New-Object System.Drawing.RectangleF 0, 0, $N, $N), $crop, [System.Drawing.GraphicsUnit]::Pixel)
    $g.Dispose()

    # Solo el cuadrado redondeado, con el borde suavizado. Se mete 1,5 px hacia dentro para no
    # arrastrar el halo blanco del borde original.
    $inset = 1.5
    $x0 = ($l + $inset - $crop.X) * $k; $y0 = ($t + $inset - $crop.Y) * $k
    $x1 = ($r - $inset - $crop.X) * $k; $y1 = ($b - $inset - $crop.Y) * $k
    $dia = 2 * $radius * $k
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.AddArc($x0, $y0, $dia, $dia, 180, 90)
    $path.AddArc($x1 - $dia, $y0, $dia, $dia, 270, 90)
    $path.AddArc($x1 - $dia, $y1 - $dia, $dia, $dia, 0, 90)
    $path.AddArc($x0, $y1 - $dia, $dia, $dia, 90, 90)
    $path.CloseFigure()
    $master, $g = New-Canvas $N
    $brush = New-Object System.Drawing.TextureBrush $flat
    $g.FillPath($brush, $path)
    $g.Dispose(); $brush.Dispose(); $flat.Dispose()
}
$src.Dispose()

# Ojo: en PowerShell las variables no distinguen mayúsculas ($size no puede llamarse $n,
# que pisaría a $N).
function Get-Resized([int]$size) {
    $bmp, $g = New-Canvas $size
    $attr = New-Object System.Drawing.Imaging.ImageAttributes
    $attr.SetWrapMode([System.Drawing.Drawing2D.WrapMode]::TileFlipXY) # sin bordes oscuros al reducir
    $g.DrawImage($master, (New-Object System.Drawing.Rectangle 0, 0, $size, $size), 0, 0, $master.Width, $master.Height, [System.Drawing.GraphicsUnit]::Pixel, $attr)
    $g.Dispose()
    return $bmp
}

function Get-PngBytes([System.Drawing.Bitmap]$bmp) {
    $ms = New-Object IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    return , $ms.ToArray()
}

function Save-Png([int]$size, [string]$path) {
    $bmp = Get-Resized $size
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host "  $path"
}

# Imagen de icono clásica (DIB de 32 bits con alfa + máscara AND), la que entiende cualquier
# lector de .ico. Las filas van de abajo arriba.
function Get-DibBytes([System.Drawing.Bitmap]$bmp) {
    $n = $bmp.Width
    $rect = New-Object System.Drawing.Rectangle 0, 0, $n, $n
    $data = $bmp.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $pixels = New-Object byte[] ($n * $n * 4)
    [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $pixels, 0, $pixels.Length) # BGRA, sin premultiplicar
    $bmp.UnlockBits($data)
    $maskStride = [int]([math]::Ceiling($n / 32) * 4)
    $ms = New-Object IO.MemoryStream
    $bw = New-Object IO.BinaryWriter $ms
    $bw.Write([uint32]40); $bw.Write([int32]$n); $bw.Write([int32]($n * 2)) # alto doble: color + máscara
    $bw.Write([uint16]1); $bw.Write([uint16]32); $bw.Write([uint32]0)
    $bw.Write([uint32]($n * $n * 4 + $maskStride * $n))
    $bw.Write([int32]0); $bw.Write([int32]0); $bw.Write([uint32]0); $bw.Write([uint32]0)
    for ($y = $n - 1; $y -ge 0; $y--) { $bw.Write($pixels, $y * $n * 4, $n * 4) }
    for ($y = $n - 1; $y -ge 0; $y--) {
        $row = New-Object byte[] $maskStride
        for ($x = 0; $x -lt $n; $x++) {
            if ($pixels[($y * $n + $x) * 4 + 3] -eq 0) { $row[[int][math]::Floor($x / 8)] = $row[[int][math]::Floor($x / 8)] -bor (0x80 -shr ($x % 8)) }
        }
        $bw.Write($row)
    }
    $bw.Flush()
    return , $ms.ToArray()
}

# .ico con todos los tamaños: PNG para 256 px (como hace Windows) y DIB para el resto.
function Save-Ico([int[]]$sizes, [string]$path) {
    $images = New-Object System.Collections.Generic.List[byte[]]
    foreach ($size in $sizes) {
        $bmp = Get-Resized $size
        if ($size -ge 256) { $images.Add((Get-PngBytes $bmp)) } else { $images.Add((Get-DibBytes $bmp)) }
        $bmp.Dispose()
    }
    $fs = [IO.File]::Create($path)
    $bw = New-Object IO.BinaryWriter $fs
    $bw.Write([uint16]0); $bw.Write([uint16]1); $bw.Write([uint16]$sizes.Count)
    $offset = 6 + 16 * $sizes.Count
    for ($i = 0; $i -lt $sizes.Count; $i++) {
        $dim = [byte]($sizes[$i] % 256) # 256 se escribe como 0
        $bw.Write($dim); $bw.Write($dim); $bw.Write([byte]0); $bw.Write([byte]0)
        $bw.Write([uint16]1); $bw.Write([uint16]32)
        $bw.Write([uint32]$images[$i].Length); $bw.Write([uint32]$offset)
        $offset += $images[$i].Length
    }
    foreach ($img in $images) { $bw.Write($img) }
    $bw.Close()
    Write-Host "  $path"
}

Write-Host 'Creando:'
$master.Save((Join-Path $assets 'xtendedview.png'), [System.Drawing.Imaging.ImageFormat]::Png)
Write-Host "  $(Join-Path $assets 'xtendedview.png')"
Save-Ico @(16, 20, 24, 32, 40, 48, 64, 96, 128, 256) (Join-Path $assets 'xtendedview.ico')
Save-Ico @(16, 32, 48) (Join-Path $web 'favicon.ico')
Save-Png 192 (Join-Path $web 'icon-192.png')
Save-Png 512 (Join-Path $web 'icon-512.png')
Save-Png 180 (Join-Path $web 'apple-touch-icon.png')
$master.Dispose()
