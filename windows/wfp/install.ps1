param(
    [string]$Configuration = 'Release',
    [string]$DriverName = 'AGWfp'
)

$ErrorActionPreference = 'Stop'

$sysPath = Join-Path $PSScriptRoot "build\$Configuration\agwfp.sys"
if (-not (Test-Path $sysPath)) {
    throw "Driver binary not found at $sysPath. Build first."
}

$target = Join-Path $env:SystemRoot 'System32\drivers\agwfp.sys'
Copy-Item $sysPath $target -Force

$existing = sc.exe query $DriverName 2>$null
if ($LASTEXITCODE -ne 0) {
    sc.exe create $DriverName type= kernel start= demand error= normal binPath= $target | Out-Null
}

sc.exe stop $DriverName 2>$null | Out-Null
sc.exe start $DriverName | Out-Null

Write-Host "Installed and started $DriverName from $target"