[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)]
  [string[]] $Path,

  [Parameter(Mandatory = $true)]
  [string] $Version
)

$ErrorActionPreference = "Stop"

foreach ($candidate in $Path) {
  $resolvedPath = (Resolve-Path -LiteralPath $candidate).Path
  $versionInfo = (Get-Item -LiteralPath $resolvedPath).VersionInfo

  if ($versionInfo.ProductVersion) {
    if ($versionInfo.ProductVersion.Trim() -ne $Version) {
      throw "$resolvedPath reports product version '$($versionInfo.ProductVersion)', expected '$Version'"
    }
    if ($versionInfo.FileVersion.Trim() -ne $Version) {
      throw "$resolvedPath reports file version '$($versionInfo.FileVersion)', expected '$Version'"
    }
    continue
  }

  $versionOutput = & $resolvedPath --version
  $exitCode = $LASTEXITCODE
  $versionOutput = ($versionOutput -join "`n").Trim()
  $expectedOutput = "agentdesktop-service $Version"
  if ($exitCode -ne 0 -or $versionOutput -ne $expectedOutput) {
    throw "$resolvedPath reports '$versionOutput' (exit code $exitCode), expected '$expectedOutput'"
  }
}
