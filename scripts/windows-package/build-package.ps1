#Requires -Version 5.1
<#
.SYNOPSIS
Builds your organization's managed Claude Code MSI from the agentdesktop
packaging recipe.

.DESCRIPTION
The agentdesktop project never distributes the Claude Code binary or a built
MSI. This script runs on your own infrastructure: it fetches the pinned
Claude Code version from Anthropic's official release bucket (the same URL
the winget manifest points at), verifies it (SHA256 against the release
manifest.json and the Authenticode subject CN "Anthropic, PBC"; when a
vendor public key is supplied, the manifest's detached GPG signature is also
fetched and verified), renders the launcher scripts and the managed-settings
drop-in from the templates, and builds the MSI with the WiX v4 CLI.

Without a GPG key, the SHA256 comes from a manifest fetched from the same
origin as the binary, so it proves transport integrity, not independent
authenticity; the Authenticode check is then the authenticity anchor.

The rendering is the same single substitution (and install-path validation)
the Rust render() counterpart performs: every occurrence of
{{agentdesktopInstallPath}} in a template is replaced with
-AgentdesktopInstallPath. The one intended difference from the repository
goldens is that the rendered claude.cmd is written with CRLF line endings
for cmd.exe.

See docs/windows-managed-package.md for the full deployment guide, including
what must already be on each device before wrapped launches work.

.PARAMETER ClaudeCodeVersion
The pinned Claude Code version to package, e.g. 2.1.98. Numeric
dot-separated fields only: it goes into the release URL, the version
manifest, and the MSI version's first three fields.

.PARAMETER PackageRevision
This package's own revision, the MSI version's fourth field (default 0).
Bump it to ship a launcher or recipe fix for the SAME Claude Code version:
the .wxs sets AllowSameVersionUpgrades, so the rebuilt MSI replaces the
installed one even though MSI compares only the first three fields.

.PARAMETER UpgradeCode
A GUID your organization generates once (e.g. with New-Guid) and then keeps
identical across every version of this package, so installing a newer build
upgrades the older one in place.

.PARAMETER AgentdesktopInstallPath
The absolute directory agentdesktop.exe is installed to on your devices. It
is fixed into the launcher scripts at build time; the launchers never search
PATH for it.

.PARAMETER ClaudeExePath
An already-obtained claude.exe to package instead of downloading one. It is
verified exactly like a downloaded binary (manifest SHA256 + Authenticode).

.PARAMETER ExecutorPath
The path your fleet's daemon.containment.executor names on each device. The
executor (wxc-exec.exe) is required on every device but is never part of this
package; when the path is missing on this machine the script warns and builds
anyway.

.PARAMETER VendorPublicKeyPath
Anthropic's release-signing GPG public key. When given, the release
manifest.json's detached signature is fetched and verified before its SHA256
values are trusted (gpg must be on PATH, or the build fails). Without it the
script warns, does not fetch the signature, and relies on the SHA256 and
Authenticode checks alone.

.PARAMETER OutDir
The output directory. It must be empty (or absent): nothing from a previous
build may leak into the package.

.EXAMPLE
.\build-package.ps1 -ClaudeCodeVersion 2.1.98 `
    -UpgradeCode 'd2f8b7a4-0000-4a7e-9c3b-123456789abc' `
    -Manufacturer 'Example Corp' `
    -ExecutorPath 'C:\Program Files\mxc\wxc-exec.exe'
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ClaudeCodeVersion,
    [Parameter(Mandatory = $true)][guid]$UpgradeCode,
    [ValidateRange(0, 65534)][int]$PackageRevision = 0,
    [string]$Manufacturer = 'Your organization',
    [ValidateSet('x64', 'arm64')][string]$Architecture = 'x64',
    [string]$AgentdesktopInstallPath = 'C:\Program Files\Agent Desktop',
    [string]$ClaudeExePath,
    [string]$ExecutorPath,
    [string]$VendorPublicKeyPath,
    [string]$OutDir = (Join-Path $PSScriptRoot 'out')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

# The version goes into the release URL, the hand-built JSON manifest and the
# MSI version fields: numeric dot-separated fields only.
if ($ClaudeCodeVersion -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') {
    throw "-ClaudeCodeVersion must be a three-field numeric version (e.g. 2.1.98), got '$ClaudeCodeVersion'"
}
# The install path is substituted verbatim into a cmd line and a double-quoted
# sh word (the same rule the Rust render() counterpart enforces): it must be
# drive-absolute (UNC refused) and must not contain characters that would
# expand, split or escape inside the rendered launchers.
if ($AgentdesktopInstallPath -notmatch '^[A-Za-z]:\\') {
    throw "-AgentdesktopInstallPath must be a drive-absolute Windows path (X:\...), not a UNC or relative one: '$AgentdesktopInstallPath'"
}
if ($AgentdesktopInstallPath -match '["%!$`]' -or $AgentdesktopInstallPath -match "`n|`r") {
    throw "-AgentdesktopInstallPath must not contain `" % ! `$ `` or a newline: '$AgentdesktopInstallPath'"
}

# The official release bucket: the URL the Microsoft-maintained winget
# manifest for Anthropic.ClaudeCode names as its InstallerUrl.
$releaseBase = 'https://storage.googleapis.com/claude-code-dist-86c565f3-f756-42ad-8dfa-d59b1c096819/claude-code-releases'
$platform = if ($Architecture -eq 'x64') { 'win32-x64' } else { 'win32-arm64' }
$templates = Join-Path $PSScriptRoot 'templates'

if ((Test-Path $OutDir) -and @(Get-ChildItem -Force $OutDir).Count -gt 0) {
    throw "refusing to build into the non-empty directory '$OutDir'"
}
$staging = Join-Path $OutDir 'staging'
New-Item -ItemType Directory -Force -Path $staging, (Join-Path $staging 'launchers') | Out-Null

# --- Acquire and verify the vendor binary -----------------------------------

$manifestPath = Join-Path $staging 'release-manifest.json'
Invoke-WebRequest -UseBasicParsing -Uri "$releaseBase/$ClaudeCodeVersion/manifest.json" -OutFile $manifestPath

if ($VendorPublicKeyPath) {
    # The detached signature is fetched only when it will be verified.
    $gpg = Get-Command gpg -ErrorAction SilentlyContinue
    if (-not $gpg) {
        throw '-VendorPublicKeyPath was given but gpg is not on PATH; install gpg or drop the parameter'
    }
    Invoke-WebRequest -UseBasicParsing -Uri "$releaseBase/$ClaudeCodeVersion/manifest.json.sig" -OutFile "$manifestPath.sig"
    $keyring = Join-Path $staging 'vendor-keyring.gpg'
    & $gpg.Source --no-default-keyring --keyring $keyring --import $VendorPublicKeyPath
    if ($LASTEXITCODE -ne 0) { throw 'importing the vendor public key failed' }
    & $gpg.Source --no-default-keyring --keyring $keyring --verify "$manifestPath.sig" $manifestPath
    if ($LASTEXITCODE -ne 0) { throw 'GPG verification of the release manifest failed' }
    Remove-Item $keyring -ErrorAction SilentlyContinue
} else {
    Write-Warning ('release manifest GPG signature NOT verified (no -VendorPublicKeyPath). ' +
        'The SHA256 then comes from the same origin as the binary, so it proves transport ' +
        'integrity only; the Authenticode subject check is the authenticity anchor.')
}

$manifest = Get-Content -Raw $manifestPath | ConvertFrom-Json
$expectedHash = $manifest.platforms.$platform.checksum
if (-not $expectedHash) { throw "the release manifest has no '$platform' entry" }

$claudeExe = Join-Path $staging 'claude.exe'
if ($ClaudeExePath) {
    Copy-Item $ClaudeExePath $claudeExe
} else {
    Invoke-WebRequest -UseBasicParsing -Uri "$releaseBase/$ClaudeCodeVersion/$platform/claude.exe" -OutFile $claudeExe
}

$actualHash = (Get-FileHash -Algorithm SHA256 $claudeExe).Hash.ToLowerInvariant()
if ($actualHash -ne $expectedHash.ToLowerInvariant()) {
    throw "claude.exe SHA256 '$actualHash' does not match the release manifest's '$expectedHash'"
}

$signature = Get-AuthenticodeSignature $claudeExe
if ($signature.Status -ne 'Valid') {
    throw "claude.exe Authenticode signature is not valid: $($signature.Status)"
}
# An exact subject CN comparison (GetNameInfo parses the DN, so the comma in
# the CN is handled), not a substring match any subject containing the text
# would pass.
$signerCn = $signature.SignerCertificate.GetNameInfo(
    [System.Security.Cryptography.X509Certificates.X509NameType]::SimpleName, $false)
if ($signerCn -cne 'Anthropic, PBC') {
    throw "claude.exe signer subject CN is not exactly 'Anthropic, PBC': '$signerCn' ($($signature.SignerCertificate.Subject))"
}

# --- Render the launchers, the drop-in, and the version manifest ------------

function Render-Template([string]$Name) {
    (Get-Content -Raw (Join-Path $templates $Name)).Replace('{{agentdesktopInstallPath}}', $AgentdesktopInstallPath)
}

$utf8NoBom = New-Object System.Text.UTF8Encoding $false
$cmdLauncher = (Render-Template 'claude.cmd.tmpl') -replace "`r?`n", "`r`n"
[System.IO.File]::WriteAllText((Join-Path $staging 'launchers\claude.cmd'), $cmdLauncher, $utf8NoBom)
# The sh launcher keeps LF line endings; Git Bash requires them.
[System.IO.File]::WriteAllText((Join-Path $staging 'launchers\claude'), (Render-Template 'claude.tmpl'), $utf8NoBom)
[System.IO.File]::WriteAllText((Join-Path $staging '40-agentdesktop-package.json'), (Render-Template 'managed-settings-dropin.json.tmpl'), $utf8NoBom)

# The version manifest agentdesktop discovery reads from beside the binary,
# so the managed install's version is known without ever running it.
$versionManifest = "{`n  `"name`": `"claude-code`",`n  `"version`": `"$ClaudeCodeVersion`"`n}`n"
[System.IO.File]::WriteAllText((Join-Path $staging 'agentdesktop-package.json'), $versionManifest, $utf8NoBom)

# --- Sanity checks that warn, never refuse -----------------------------------

if ($ExecutorPath) {
    if (-not (Test-Path $ExecutorPath)) {
        Write-Warning ("wxc-exec.exe not found at '$ExecutorPath' on this machine. " +
            'The package builds anyway: the executor is never part of this package, but every ' +
            'device needs it at the path daemon.containment.executor names before wrapped ' +
            'launches work. See docs/windows-managed-package.md for how to obtain it.')
    }
} else {
    Write-Warning ('no -ExecutorPath given, so the executor prerequisite was not checked. ' +
        'Every device needs wxc-exec.exe at the path daemon.containment.executor names.')
}

# --- Build the MSI -----------------------------------------------------------

# The WiX v4 toolchain is pinned to one exact version: the .wxs targets WiX
# v4 semantics, and a drifting global tool would make the package build
# non-reproducible across build machines.
$wixVersion = '4.0.5'
if (-not (Get-Command wix -ErrorAction SilentlyContinue)) {
    throw "the WiX v4 CLI is required: dotnet tool install --global wix --version $wixVersion"
}
$installedWix = (& wix --version 2>$null | Select-Object -First 1)
if ("$installedWix" -notmatch ('^' + [regex]::Escape($wixVersion) + '([+.-]|$)')) {
    throw "WiX '$installedWix' found, but this recipe pins ${wixVersion}: dotnet tool update --global wix --version $wixVersion"
}
$msi = Join-Path $OutDir "claude-code-managed-$ClaudeCodeVersion.$PackageRevision-$Architecture.msi"
wix build (Join-Path $PSScriptRoot 'claude-code-managed.wxs') `
    -arch $Architecture `
    -d "StagingDir=$staging" `
    -d "PackageVersion=$ClaudeCodeVersion" `
    -d "PackageRevision=$PackageRevision" `
    -d "Manufacturer=$Manufacturer" `
    -d "UpgradeCode=$UpgradeCode" `
    -o $msi
if ($LASTEXITCODE -ne 0) { throw 'wix build failed' }

Write-Host "built $msi"
