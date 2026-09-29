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

  # The service has no version resource; agentdesktop_core::VERSION embeds
  # AGENTDESKTOP_VERSION verbatim, so the release string must be present.
  $text = [System.Text.Encoding]::UTF8.GetString([System.IO.File]::ReadAllBytes($resolvedPath))
  if (-not $text.Contains($Version)) {
    throw "$resolvedPath does not embed release version '$Version'"
  }
}
