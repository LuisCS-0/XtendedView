<#
.SYNOPSIS
  Crea una red Wi-Fi directa desde el portátil, sin router ni internet.

.DESCRIPTION
  Usa la API Wi-Fi Direct de Windows en "modo legacy": el portátil emite una red normal
  (nombre + contraseña WPA2) a la que puede unirse cualquier dispositivo, incluido el iPad,
  que no soporta Wi-Fi Direct. La red existe mientras este script esté abierto; Ctrl+C la cierra.

  El nombre y la contraseña son aleatorios y nuevos cada vez (como en el host en Rust): el
  nombre se emite a todo el que esté cerca y no debe revelar de quién es el ordenador.
  Si el portátil sigue conectado a otra Wi-Fi, la tarjeta tiene que compartir el canal y
  puede haber más variación de latencia: para medir lo mejor posible, desconéctala.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File tools\hotspot.ps1
#>
param(
    [string]$Ssid,
    [string]$Password,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'

# Las APIs de Windows Runtime solo se pueden cargar así desde Windows PowerShell 5.1.
if ($PSVersionTable.PSEdition -ne 'Desktop') {
    $argsList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $PSCommandPath)
    foreach ($k in $PSBoundParameters.Keys) {
        if ($PSBoundParameters[$k] -is [switch]) { $argsList += "-$k" } else { $argsList += "-$k", $PSBoundParameters[$k] }
    }
    & "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" @argsList
    exit $LASTEXITCODE
}

# Texto aleatorio con un generador criptográfico. Alfabetos de 32 símbolos: 256 es múltiplo
# de 32, así que el módulo no favorece a ninguno.
$rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
function Get-RandomText([string]$Alphabet, [int]$Length) {
    $bytes = New-Object byte[] $Length
    $rng.GetBytes($bytes)
    -join ($bytes | ForEach-Object { $Alphabet[$_ % $Alphabet.Length] })
}
# Formato habitual de Wi-Fi Direct ("DIRECT-…"): no dice de quién es ni qué programa es.
if (-not $Ssid) { $Ssid = 'DIRECT-' + (Get-RandomText 'ABCDEFGHJKLMNPQRSTUVWXYZ23456789' 2) + '-' + (Get-RandomText 'ABCDEFGHJKLMNPQRSTUVWXYZ23456789' 6) }
# 16 caracteres entre 32 (80 bits), en grupos de 4 para poder teclearla.
if (-not $Password) { $Password = (1..4 | ForEach-Object { Get-RandomText 'abcdefghijkmnpqrstuvwxyz23456789' 4 }) -join '-' }
if ($Password.Length -lt 8) { throw 'La contraseña debe tener al menos 8 caracteres.' }

Add-Type -AssemblyName System.Runtime.WindowsRuntime
$null = [Windows.Devices.WiFiDirect.WiFiDirectAdvertisementPublisher, Windows.Devices.WiFiDirect, ContentType = WindowsRuntime]
$null = [Windows.Security.Credentials.PasswordCredential, Windows.Security.Credentials, ContentType = WindowsRuntime]

$publisher = New-Object Windows.Devices.WiFiDirect.WiFiDirectAdvertisementPublisher
$adv = $publisher.Advertisement
$adv.IsAutonomousGroupOwnerEnabled = $true
$adv.LegacySettings.IsEnabled = $true
$adv.LegacySettings.Ssid = $Ssid
$cred = New-Object Windows.Security.Credentials.PasswordCredential
$cred.Password = $Password
$adv.LegacySettings.Passphrase = $cred

if ($DryRun) {
    Write-Host "API Wi-Fi Direct disponible. Red preparada (sin emitir): $Ssid"
    exit 0
}

$publisher.Start()
$deadline = (Get-Date).AddSeconds(10)
while ($publisher.Status -eq 'Created' -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
if ($publisher.Status -ne 'Started') {
    throw "No se pudo crear la red (estado: $($publisher.Status)). ¿Está activado el Wi-Fi?"
}

Write-Host ''
Write-Host '  Red Wi-Fi directa activa' -ForegroundColor Green
Write-Host "  Nombre     : $Ssid"
Write-Host "  Contraseña : $Password"
Write-Host "  QR (texto) : WIFI:T:WPA;S:$Ssid;P:$Password;;"
Start-Sleep -Seconds 2
$ips = Get-NetAdapter -IncludeHidden -ErrorAction SilentlyContinue |
    Where-Object { $_.InterfaceDescription -like '*Wi-Fi Direct*' -and $_.Status -eq 'Up' } |
    ForEach-Object { Get-NetIPAddress -InterfaceIndex $_.ifIndex -AddressFamily IPv4 -ErrorAction SilentlyContinue } |
    Select-Object -ExpandProperty IPAddress
if ($ips) {
    foreach ($ip in $ips) { Write-Host "  Pantalla   : https://${ip}:8443/" }
} else {
    Write-Host '  La IP del portátil en esta red suele ser 192.168.137.1 (aparece al conectar el primer dispositivo).'
}
Write-Host ''
Write-Host '  Si el móvil no carga la página, permite el host en redes públicas en el Firewall de Windows.'
Write-Host '  (Con el host en Rust no hace falta este script: usa xtendedview.exe --hotspot.)'
Write-Host '  Ctrl+C para cerrar la red.'

try {
    while ($true) {
        Start-Sleep -Seconds 2
        if ($publisher.Status -ne 'Started') {
            Write-Warning "La red se ha detenido (estado: $($publisher.Status)). Reiniciando…"
            $publisher.Start()
        }
    }
} finally {
    $publisher.Stop()
    Write-Host 'Red cerrada.'
}
