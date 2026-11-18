<#
.SYNOPSIS
  Prepara el logotipo de texto de XtendedView (sobre fondo negro) para el panel.

.DESCRIPTION
  - Quita el fondo negro: la transparencia sale del brillo de cada píxel, así que los bordes
    suavizados quedan bien sobre cualquier fondo.
  - Recorta el texto y crea dos versiones: con «Xtended» en blanco (tema oscuro) y en gris muy
    oscuro (tema claro). Lo que tiene color (el degradado azul de «View») se deja igual.
  - Crea assets\logo-tema-oscuro.png y assets\logo-tema-claro.png a tamaño original, y
    web\logo-tema-oscuro.png y web\logo-tema-claro.png a 120 px de alto para el panel.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File tools\logo.ps1 C:\ruta\logo.jpg
#>
param([Parameter(Mandatory)][string]$Fuente)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;

public static class Logo {
    static byte[] Read(Bitmap b, out int w, out int h) {
        w = b.Width; h = b.Height;
        var d = b.LockBits(new Rectangle(0, 0, w, h), ImageLockMode.ReadOnly, PixelFormat.Format32bppArgb);
        var px = new byte[w * h * 4];
        for (int y = 0; y < h; y++) Marshal.Copy(d.Scan0 + y * d.Stride, px, y * w * 4, w * 4);
        b.UnlockBits(d);
        return px;
    }

    static Bitmap Write(byte[] px, int w, int h) {
        var b = new Bitmap(w, h, PixelFormat.Format32bppArgb);
        var d = b.LockBits(new Rectangle(0, 0, w, h), ImageLockMode.WriteOnly, PixelFormat.Format32bppArgb);
        for (int y = 0; y < h; y++) Marshal.Copy(px, y * w * 4, d.Scan0 + y * d.Stride, w * 4);
        b.UnlockBits(d);
        return b;
    }

    // Fondo negro -> transparente. Sobre negro, cada píxel es color × opacidad: la opacidad es su
    // canal más brillante y el color, el píxel dividido por ella. Por debajo de `floor` es ruido
    // de la compresión JPEG y se descarta.
    public static Bitmap Cutout(Bitmap src, int floor, int pad) {
        int w, h;
        var px = Read(src, out w, out h);
        int x0 = w, y0 = h, x1 = -1, y1 = -1;
        for (int i = 0; i < w * h; i++) {
            int b = px[i * 4], g = px[i * 4 + 1], r = px[i * 4 + 2];
            int m = Math.Max(r, Math.Max(g, b));
            if (m <= floor) { px[i * 4] = px[i * 4 + 1] = px[i * 4 + 2] = px[i * 4 + 3] = 0; continue; }
            px[i * 4] = (byte)Math.Min(255, b * 255 / m);
            px[i * 4 + 1] = (byte)Math.Min(255, g * 255 / m);
            px[i * 4 + 2] = (byte)Math.Min(255, r * 255 / m);
            int a = (m - floor) * 255 / (255 - floor);
            px[i * 4 + 3] = (byte)a;
            if (a > 64) {
                int x = i % w, y = i / w;
                if (x < x0) x0 = x; if (x > x1) x1 = x; if (y < y0) y0 = y; if (y > y1) y1 = y;
            }
        }
        if (x1 < 0) throw new Exception("no se ha encontrado el texto del logo");
        x0 = Math.Max(0, x0 - pad); y0 = Math.Max(0, y0 - pad);
        x1 = Math.Min(w - 1, x1 + pad); y1 = Math.Min(h - 1, y1 + pad);
        int cw = x1 - x0 + 1, ch = y1 - y0 + 1;
        var crop = new byte[cw * ch * 4];
        for (int y = 0; y < ch; y++) Buffer.BlockCopy(px, ((y + y0) * w + x0) * 4, crop, y * cw * 4, cw * 4);
        return Write(crop, cw, ch);
    }

    // Pinta del color dado lo que es blanco o gris (poca saturación), sin tocar su opacidad.
    public static Bitmap RecolorNeutral(Bitmap src, Color c) {
        int w, h;
        var px = Read(src, out w, out h);
        for (int i = 0; i < w * h; i++) {
            int b = px[i * 4], g = px[i * 4 + 1], r = px[i * 4 + 2];
            if (Math.Max(r, Math.Max(g, b)) - Math.Min(r, Math.Min(g, b)) < 60) {
                px[i * 4] = c.B; px[i * 4 + 1] = c.G; px[i * 4 + 2] = c.R;
            }
        }
        return Write(px, w, h);
    }

    public static Bitmap Resize(Bitmap src, int height) {
        int width = (int)Math.Round(src.Width * (double)height / src.Height);
        var b = new Bitmap(width, height, PixelFormat.Format32bppPArgb);
        using (var g = Graphics.FromImage(b)) {
            g.InterpolationMode = System.Drawing.Drawing2D.InterpolationMode.HighQualityBicubic;
            g.PixelOffsetMode = System.Drawing.Drawing2D.PixelOffsetMode.HighQuality;
            g.CompositingQuality = System.Drawing.Drawing2D.CompositingQuality.HighQuality;
            using (var attr = new ImageAttributes()) {
                attr.SetWrapMode(System.Drawing.Drawing2D.WrapMode.TileFlipXY);
                g.DrawImage(src, new Rectangle(0, 0, width, height), 0, 0, src.Width, src.Height, GraphicsUnit.Pixel, attr);
            }
        }
        return b;
    }
}
'@

$root = Split-Path $PSScriptRoot -Parent
$src = [System.Drawing.Bitmap]::FromFile((Resolve-Path $Fuente).Path)
$dark = [Logo]::Cutout($src, 24, 12)
$light = [Logo]::RecolorNeutral($dark, [System.Drawing.Color]::FromArgb(27, 30, 36)) # el texto del panel claro
$src.Dispose()
Write-Host "Logo recortado: $($dark.Width)x$($dark.Height) px"
foreach ($v in @(@($dark, 'logo-tema-oscuro.png'), @($light, 'logo-tema-claro.png'))) {
    $full = Join-Path $root "assets\$($v[1])"
    $v[0].Save($full, [System.Drawing.Imaging.ImageFormat]::Png)
    $small = [Logo]::Resize($v[0], 120)
    $web = Join-Path $root "web\$($v[1])"
    $small.Save($web, [System.Drawing.Imaging.ImageFormat]::Png)
    $small.Dispose()
    Write-Host "  $full`n  $web"
}
