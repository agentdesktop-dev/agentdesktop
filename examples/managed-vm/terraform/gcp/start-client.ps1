param(
    [string]$IdentityDirectory
)

$ErrorActionPreference = 'Stop'

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..\..')).Path
$bootstrapDirectory = Join-Path $PSScriptRoot 'client-bootstrap'
$organizationConfig = Join-Path $bootstrapDirectory 'organization.json'
$serverCa = Join-Path $bootstrapDirectory 'server-ca.crt'

foreach ($path in @($organizationConfig, $serverCa)) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Managed client bootstrap is missing: $path. Run deploy.sh on the GCP deployment host first."
    }
}

try {
    $organization = Get-Content -LiteralPath $organizationConfig -Raw | ConvertFrom-Json
    $issuer = [Uri]$organization.identity.issuer
    $enrollmentUrl = [Uri]$organization.identity.enrollment_url
    $gatewayUrl = [Uri]$organization.gateway.url
} catch {
    throw "Managed client bootstrap is invalid: $($_.Exception.Message)"
}

foreach ($endpoint in @($issuer, $enrollmentUrl, $gatewayUrl)) {
    if ($endpoint.Scheme -ne 'https' -or -not $endpoint.Host) {
        throw "Managed client endpoint must be an absolute HTTPS URL: $endpoint"
    }
}

$certificate = [System.Security.Cryptography.X509Certificates.X509Certificate2]::new($serverCa)
if (-not (Test-Path -LiteralPath "Cert:\CurrentUser\Root\$($certificate.Thumbprint)")) {
    throw "The development server CA is not trusted. Run: Import-Certificate -FilePath '$serverCa' -CertStoreLocation 'Cert:\CurrentUser\Root'"
}

$listeners = Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue |
    Where-Object { $_.LocalPort -in @(8080, 8081, 1420) }
if ($listeners) {
    $ports = ($listeners.LocalPort | Sort-Object -Unique) -join ', '
    throw "Agent Desktop development ports are already in use ($ports). Quit the existing tray application before changing organization configuration."
}

if (-not $IdentityDirectory) {
    $IdentityDirectory = Join-Path $env:LOCALAPPDATA 'AgentDesktop\gcp-managed-client\identity'
}

$npm = Get-Command npm.cmd -ErrorAction SilentlyContinue
if (-not $npm) {
    throw 'npm.cmd is required; install Node.js 20 or newer'
}
$tauri = Join-Path $repoRoot 'ui\node_modules\.bin\tauri.cmd'
if (-not (Test-Path -LiteralPath $tauri -PathType Leaf)) {
    throw "Windows UI dependencies are not installed. Run: npm --prefix '$repoRoot\ui' ci"
}

$env:SSL_CERT_FILE = $serverCa
$env:AGENTDESKTOP_ORGANIZATION_CONFIG = $organizationConfig
$env:AGENTDESKTOP_IDENTITY_DIR = $IdentityDirectory
$env:AGENTDESKTOP_CREDENTIAL_STORAGE = 'auto'
$env:AGENTDESKTOP_MODE = 'managed'
$env:AGENTDESKTOP_GATEWAY_MODE = 'external'
$env:AGENTDESKTOP_IDENTITY_ISSUER = $issuer.AbsoluteUri
$env:AGENTDESKTOP_ENROLLMENT_URL = $enrollmentUrl.AbsoluteUri
$env:AGENTDESKTOP_UPSTREAM = $gatewayUrl.AbsoluteUri

Write-Output "Organization config: $organizationConfig"
Write-Output "OAuth issuer:       $($issuer.AbsoluteUri)"
Write-Output "Enrollment URL:     $($enrollmentUrl.AbsoluteUri)"
Write-Output "Gateway URL:        $($gatewayUrl.AbsoluteUri)"

Push-Location $repoRoot
try {
    & $npm.Source --prefix ui run dev:desktop
    if ($LASTEXITCODE -ne 0) {
        throw "Agent Desktop exited with code $LASTEXITCODE"
    }
} finally {
    Pop-Location
}
