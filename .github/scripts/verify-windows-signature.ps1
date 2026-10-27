[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)]
  [string[]] $Path
)

$ErrorActionPreference = "Stop"

function Find-SignTool {
  $command = Get-Command signtool.exe -ErrorAction SilentlyContinue
  if ($command) {
    return $command.Source
  }

  $kitsRoot = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
  $architecture = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "x64" }
  $candidates = @(
    Get-ChildItem -Path "$kitsRoot\*\$architecture\signtool.exe" -File -ErrorAction SilentlyContinue
  )
  if ($candidates.Count -eq 0 -and $architecture -ne "x64") {
    $candidates = @(
      Get-ChildItem -Path "$kitsRoot\*\x64\signtool.exe" -File -ErrorAction SilentlyContinue
    )
  }
  $signTool = $candidates | Sort-Object FullName -Descending | Select-Object -First 1
  if (-not $signTool) {
    throw "signtool.exe was not found"
  }
  return $signTool.FullName
}

$signTool = Find-SignTool
foreach ($candidate in $Path) {
  $resolvedPath = (Resolve-Path -LiteralPath $candidate).Path
  & $signTool verify /pa /all /v /tw $resolvedPath
  if ($LASTEXITCODE -ne 0) {
    throw "Signature verification failed for $resolvedPath"
  }

  $signature = Get-AuthenticodeSignature -LiteralPath $resolvedPath
  if ($signature.Status -ne "Valid") {
    throw "Authenticode signature is not valid for $resolvedPath`: $($signature.StatusMessage)"
  }
  $subjectParts = @($signature.SignerCertificate.Subject -split ",\s*")
  if ($subjectParts -notcontains "CN=Solo.io" -or $subjectParts -notcontains "O=Solo.io") {
    throw "Unexpected signer for $resolvedPath`: $($signature.SignerCertificate.Subject)"
  }
  if (-not $signature.TimeStamperCertificate) {
    throw "Authenticode signature is not timestamped for $resolvedPath"
  }
}