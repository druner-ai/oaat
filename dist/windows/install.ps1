# Tune Bridge endpoint — установка на Windows:
# exe в %LOCALAPPDATA%\Programs\tune-bridge + автозапуск (HKCU Run).
# Запустить один раз: .\install.ps1 (при необходимости Set-ExecutionPolicy -Scope Process Bypass)
$ErrorActionPreference = "Stop"
$dir = Join-Path $env:LOCALAPPDATA "Programs\tune-bridge"
$cfgDir = Join-Path $env:APPDATA "tune-bridge"
New-Item -ItemType Directory -Force -Path $dir, $cfgDir | Out-Null

$exeSrc = Join-Path $PSScriptRoot "tune-bridge.exe"
if (-not (Test-Path $exeSrc)) { throw "tune-bridge.exe not found next to install.ps1" }
$exe = Join-Path $dir "tune-bridge.exe"
Copy-Item $exeSrc $exe -Force

$cfg = Join-Path $cfgDir "endpoint.toml"
if (-not (Test-Path $cfg)) {
    @"
[endpoint]
name = "$env:COMPUTERNAME"
port = 9740

[web_ui]
enabled = true
port = 9741
bind = "0.0.0.0"
# control_token is auto-generated on first start (endpoint.toml.token next
# to this config) and picked up by the SoundVault server via mDNS.
"@ | Set-Content -Encoding UTF8 $cfg
    Write-Host "created $cfg"
}

$run = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
Set-ItemProperty -Path $run -Name "TuneBridge" -Value ('"{0}" endpoint --config "{1}"' -f $exe, $cfg)

Write-Host "Tune Bridge installed; autostart registered (HKCU Run)."
Write-Host "Start now:  & '$exe' endpoint --config '$cfg'"
Write-Host "Uninstall:  Remove-ItemProperty -Path '$run' -Name TuneBridge"
