<#
.SYNOPSIS
  Crea una red Wi-Fi directa desde el portátil, sin router ni internet.

.DESCRIPTION
  Usa la API Wi-Fi Direct de Windows en "modo legacy": el portátil emite una red normal
  (nombre + contraseña WPA2) a la que puede unirse cualquier dispositivo, incluido el iPad,
  que no soporta Wi-Fi Direct. La red existe mientras este script esté abierto; Ctrl+C la cierra.

  La contraseña se guarda en certs\hotspot.txt para que el móvil recuerde la red.
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

$stateFile = Join-Path $PSScriptRoot '..\certs\hotspot.txt'
$saved = @{}
if (Test-Path $stateFile) {
    foreach ($line in Get-Content $stateFile) {
        $k, $v = $line -split '=', 2
        if ($v) { $saved[$k] = $v }
    }
}
if (-not $Ssid) { $Ssid = if ($saved.ssid) { $saved.ssid } else { ('Pantalla-' + $env:COMPUTERNAME).Substring(0, [Math]::Min(32, 9 + $env:COMPUTERNAME.Length)) } }
if (-not $Password) {
    if ($saved.password) { $Password = $saved.password }
    else {
        $chars = 'abcdefghijkmnpqrstuvwxyz23456789'.ToCharArray()
        $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
        $bytes = New-Object byte[] 12
        $rng.GetBytes($bytes)
        $Password = -join ($bytes | ForEach-Object { $chars[$_ % $chars.Length] })
    }
}
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

New-Item -ItemType Directory -Force (Split-Path $stateFile) | Out-Null
Set-Content -Path $stateFile -Value "ssid=$Ssid", "password=$Password" -Encoding ASCII

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
Write-Host '  Si el móvil no carga la página, permite Python en redes públicas en el Firewall de Windows.'
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
