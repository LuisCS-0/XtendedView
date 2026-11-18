# Genera el instalador de Windows en dist\: compila la versión sin consola (aparte, en
# host-rs\target\gui, para no tocar la de desarrollo) y la empaqueta con Inno Setup 6.
#   powershell -ExecutionPolicy Bypass -File tools\instalador.ps1
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent

$cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
if (-not $cargo) { $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe' }
$iscc = @(
    (Get-Command ISCC -ErrorAction SilentlyContinue).Source,
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $iscc) { throw 'No encuentro Inno Setup 6 (ISCC.exe). Instálalo desde https://jrsoftware.org/isdl.php' }

# La versión, la de host-rs\Cargo.toml.
$version = (Select-String -Path "$root\host-rs\Cargo.toml" -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value

Write-Host "Compilando XtendedView $version sin consola…"
Push-Location "$root\host-rs"
try {
    & $cargo build --release --features gui --target-dir target\gui
    if ($LASTEXITCODE) { throw 'La compilación ha fallado' }
} finally { Pop-Location }

Write-Host 'Creando el instalador…'
& $iscc /Qp "/DAppVersion=$version" "$root\installer\xtendedview.iss"
if ($LASTEXITCODE) { throw 'Inno Setup ha fallado' }

# Versión portátil: lo mismo en un .zip, con portable.txt (datos en datos\ junto al ejecutable).
Write-Host 'Creando la versión portátil…'
$stage = Join-Path ([IO.Path]::GetTempPath()) "xtendedview-portable-$PID"
$dir = Join-Path $stage 'XtendedView'
New-Item -ItemType Directory -Force "$dir\tools" | Out-Null
Copy-Item "$root\host-rs\target\gui\release\xtendedview.exe" $dir
Copy-Item "$root\web" $dir -Recurse
Copy-Item "$root\tools\vdd-resoluciones.ps1" "$dir\tools"
Copy-Item "$root\installer\portable.txt" $dir
$zip = "$root\dist\XtendedView-$version-portable.zip"
Remove-Item $zip -ErrorAction SilentlyContinue
Compress-Archive -Path $dir -DestinationPath $zip
Remove-Item $stage -Recurse -Force

Get-Item "$root\dist\XtendedView-$version-instalador.exe", $zip
