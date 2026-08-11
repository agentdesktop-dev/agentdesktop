param(
    [string]$DriverName = 'AGWfp'
)

$ErrorActionPreference = 'Stop'

sc.exe stop $DriverName 2>$null | Out-Null
sc.exe delete $DriverName 2>$null | Out-Null

$target = Join-Path $env:SystemRoot 'System32\drivers\agwfp.sys'
if (Test-Path $target) {
    Remove-Item $target -Force
}

Write-Host "Removed $DriverName"