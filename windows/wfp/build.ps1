param(
    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Release'
)

$ErrorActionPreference = 'Stop'

$project = Join-Path $PSScriptRoot 'agentdesktop-wfp.vcxproj'
$candidates = @(
    'C:\VisualStudio\MSBuild\Current\Bin\MSBuild.exe',
    'C:\BuildTools\MSBuild\Current\Bin\MSBuild.exe',
    'C:\Program Files\Microsoft Visual Studio\2022\Community\MSBuild\Current\Bin\MSBuild.exe',
    'C:\Program Files\Microsoft Visual Studio\2022\BuildTools\MSBuild\Current\Bin\MSBuild.exe'
)
$msbuild = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $msbuild) {
    $msbuild = Get-Command msbuild.exe -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source
}
if (-not $msbuild) {
    throw 'MSBuild with the WindowsKernelModeDriver10.0 toolset was not found'
}

Remove-Item Env:VCTargetsPath -ErrorAction SilentlyContinue
Write-Output "Using $msbuild"
& $msbuild $project /t:Build /p:Configuration=$Configuration /p:Platform=x64 /m
if ($LASTEXITCODE -ne 0) {
    throw "driver build failed with exit code $LASTEXITCODE"
}

$driver = Join-Path $PSScriptRoot "build\$Configuration\agwfp.sys"
if (-not (Test-Path $driver)) {
    throw "driver build did not produce $driver"
}
Write-Output $driver