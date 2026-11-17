<#
.SYNOPSIS
  Registra resoluciones propias en el monitor virtual de Parsec VDD (máximo 5).

.DESCRIPTION
  El host en Rust crea un monitor virtual con el modo más parecido a la pantalla del cliente.
  Si ninguno de los modos de serie coincide (el Galaxy Fold desplegado, por ejemplo, es casi
  cuadrado), el host te indica la resolución exacta para añadirla aquí. Hay que hacerlo una
  sola vez por dispositivo.

  Las resoluciones se guardan en HKLM\SOFTWARE\Parsec\vdd\0..4 (valores w, h y hz) y el
  driver las lee al iniciarse, así que el script reinicia el adaptador virtual.
  Desconecta antes el visor: reiniciar el adaptador quita los monitores virtuales activos.

.EXAMPLE
  .\vdd-resoluciones.ps1 -Add 2184x1968@120
.EXAMPLE
  .\vdd-resoluciones.ps1 -List
.EXAMPLE
  .\vdd-resoluciones.ps1 -Clear
#>
#Requires -RunAsAdministrator
param(
    [string[]]$Add,
    [switch]$List,
    [switch]$Clear
)

$ErrorActionPreference = 'Stop'
$base = 'HKLM:\SOFTWARE\Parsec\vdd'

function Get-Slots {
    foreach ($i in 0..4) {
        $p = Join-Path $base $i
        if (Test-Path $p) {
            $v = Get-ItemProperty $p
            if ($v.w -and $v.h) { [pscustomobject]@{ Slot = $i; Width = $v.w; Height = $v.h; Hz = $v.hz } }
        }
    }
}

function Restart-ParsecAdapter {
    $dev = Get-PnpDevice -Class Display -ErrorAction SilentlyContinue |
        Where-Object { $_.HardwareID -contains 'Root\Parsec\VDA' } | Select-Object -First 1
    if (-not $dev) { Write-Warning 'No encuentro el adaptador de Parsec VDD. ¿Está instalado el driver?'; return }
    Write-Host 'Reiniciando el adaptador virtual para que lea las resoluciones…'
    pnputil /restart-device "$($dev.InstanceId)" | Out-Null
}

if ($Clear) {
    foreach ($i in 0..4) { Remove-Item (Join-Path $base $i) -Recurse -ErrorAction SilentlyContinue }
    Write-Host 'Resoluciones propias borradas.'
    Restart-ParsecAdapter
    return
}

$changed = $false
foreach ($spec in $Add) {
    if ($spec -notmatch '^(\d{3,4})x(\d{3,4})(?:@(\d{2,3}))?$') { throw "Formato no válido: '$spec' (usa ANCHOxALTO@HZ, p. ej. 2184x1968@120)" }
    $w = [int]$Matches[1]; $h = [int]$Matches[2]; $hz = if ($Matches[3]) { [int]$Matches[3] } else { 60 }
    $slots = @(Get-Slots)
    if ($slots | Where-Object { $_.Width -eq $w -and $_.Height -eq $h -and $_.Hz -eq $hz }) {
        Write-Host "Ya estaba registrada: ${w}x${h}@${hz}"
        continue
    }
    $free = (0..4 | Where-Object { $_ -notin $slots.Slot } | Select-Object -First 1)
    if ($null -eq $free) { throw 'Ya hay 5 resoluciones propias (el máximo del driver). Borra con -Clear y vuelve a añadir las que necesites.' }
    $key = Join-Path $base $free
    New-Item -Path $key -Force | Out-Null
    New-ItemProperty -Path $key -Name w -Value $w -PropertyType DWord -Force | Out-Null
    New-ItemProperty -Path $key -Name h -Value $h -PropertyType DWord -Force | Out-Null
    New-ItemProperty -Path $key -Name hz -Value $hz -PropertyType DWord -Force | Out-Null
    Write-Host "Registrada en el hueco ${free}: ${w}x${h}@${hz}"
    $changed = $true
}

if ($List -or -not $Add) {
    $slots = @(Get-Slots)
    if ($slots) { $slots | Format-Table -AutoSize } else { Write-Host 'No hay resoluciones propias registradas.' }
}
if ($changed) { Restart-ParsecAdapter }
