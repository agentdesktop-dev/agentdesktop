param(
    [string]$InstallPath = "C:\VisualStudio"
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$downloads = Join-Path $env:TEMP "agentdesktop-wdk"
New-Item $downloads -ItemType Directory -Force | Out-Null

$visualStudio = Join-Path $downloads "vs-community.exe"
$sdkSetup = Join-Path $downloads "winsdksetup.exe"
$wdkSetup = Join-Path $downloads "wdksetup.exe"

function Assert-MicrosoftSignature([string]$Path) {
    $signature = Get-AuthenticodeSignature $Path
    if ($signature.Status -ne 'Valid' -or
        $signature.SignerCertificate.Subject -notmatch '(^|, )O=Microsoft Corporation(,|$)') {
        throw "Downloaded installer does not have a valid Microsoft signature: $Path"
    }
}

Invoke-WebRequest "https://aka.ms/vs/17/release/vs_community.exe" -OutFile $visualStudio
Assert-MicrosoftSignature $visualStudio
$build = Start-Process $visualStudio -ArgumentList @(
    "--quiet",
    "--wait",
    "--norestart",
    "--nocache",
    "--installPath", $InstallPath,
    "--add", "Microsoft.VisualStudio.Workload.NativeDesktop",
    "--add", "Component.Microsoft.Windows.DriverKit",
    "--add", "Microsoft.VisualStudio.Component.VC.Runtimes.x86.x64.Spectre",
    "--includeRecommended"
) -Wait -PassThru
if ($build.ExitCode -notin 0, 3010) {
    throw "Visual Studio installation failed with exit code $($build.ExitCode)"
}

Invoke-WebRequest "https://go.microsoft.com/fwlink/?linkid=2338977" -OutFile $sdkSetup
Assert-MicrosoftSignature $sdkSetup
$sdk = Start-Process $sdkSetup -ArgumentList "/features", "+", "/quiet", "/norestart" -Wait -PassThru
if ($sdk.ExitCode -notin 0, 3010) {
    throw "Windows SDK installation failed with exit code $($sdk.ExitCode)"
}

Invoke-WebRequest "https://go.microsoft.com/fwlink/?linkid=2335869" -OutFile $wdkSetup
Assert-MicrosoftSignature $wdkSetup
$wdk = Start-Process $wdkSetup -ArgumentList "/features", "+", "/quiet", "/norestart" -Wait -PassThru
if ($wdk.ExitCode -notin 0, 3010) {
    throw "Windows Driver Kit installation failed with exit code $($wdk.ExitCode)"
}

$msbuild = Join-Path $InstallPath "MSBuild\Current\Bin\MSBuild.exe"
if (-not (Test-Path $msbuild)) {
    throw "MSBuild was not installed at $msbuild"
}

Write-Output $msbuild
