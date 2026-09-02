# Windows installer

Agent Desktop is distributed on Windows as a per-machine MSI. The package
installs:

- The Agent Desktop tray application under `Program Files`.
- The headless `AgentDesktop` Windows service.
- A default machine configuration under `%ProgramData%\AgentDesktop`.
- A startup entry that launches the tray application for interactive users.

The generated MSI is unsigned until Windows code signing is configured.

## Build host

Build the MSI on Windows. Tauri uses WiX v3 to create MSI packages, and WiX v3
cannot run natively on macOS or Linux.

From macOS, use one of these options:

- Run the Windows instructions below in a Windows 11 virtual machine.
- Push the changes and use the native Windows jobs in the GitHub Actions release
  workflow.
- Use a dedicated Windows CI runner.

`cargo-xwin` can compile and check Windows binaries from macOS, and Tauri can
cross-build NSIS packages with additional tooling. Neither produces this MSI.
The current Windows service registration is implemented through a WiX fragment,
so an NSIS package is not an equivalent substitute.

## Windows prerequisites

Use Windows 10 or Windows 11 and install:

1. Visual Studio 2022 Build Tools with the **Desktop development with C++**
   workload and a Windows SDK.
2. Rust through `rustup`.
3. Node.js 26.8.1, as specified by `frontend/.nvmrc`.
4. Git.
5. The VBSCRIPT Windows optional feature, when it is not already enabled.

For ARM64 builds, also install **MSVC v143 ARM64 build tools** from the
Individual components page in Visual Studio Installer.

To enable VBSCRIPT, open **Settings > Apps > Optional features > More Windows
features**, select **VBSCRIPT**, and restart Windows if prompted. This is required
by WiX when creating MSI packages. A missing feature commonly appears as a
`failed to run light.exe` build error.

WebView2 is normally already installed on supported Windows 10 and Windows 11
systems. The MSI embeds the small WebView2 bootstrapper for machines where it is
missing; that bootstrapper requires internet access during installation.

Tauri downloads its WiX tooling automatically. The first build also requires
internet access for Rust, pnpm, Tauri, and WiX dependencies.

## Prepare the repository

Open PowerShell and change to the repository root:

```powershell
cd C:\projects\agentdesktop

rustup toolchain install 1.98
rustup target add x86_64-pc-windows-msvc --toolchain 1.98

npm install --global pnpm@11.25.0
cd frontend
pnpm --version
pnpm install --frozen-lockfile
```

The version command must print `11.3.0`, as pinned by `frontend/package.json`.
Run pnpm from the `frontend` directory rather than passing `--dir frontend` from
the repository root. Corepack selects a pnpm version from the current directory
before pnpm processes `--dir`; from the repository root it may select another
version and then fail the workspace version check. Do not bypass that check with
`--pm-on-fail=ignore`.

## Build an x64 MSI

Set the version to embed in the application and MSI, then run the packaging
script:

```powershell
$env:AGENTDESKTOP_VERSION = "0.1.0"

pnpm --filter @agentdesktop/desktop-web `
  dist:win -- --target x86_64-pc-windows-msvc
```

The MSI is written to:

```text
target\x86_64-pc-windows-msvc\release\bundle\msi\
```

## Build an ARM64 MSI

On Windows ARM64, or on a Windows build host with the ARM64 C++ tools installed:

```powershell
rustup target add aarch64-pc-windows-msvc --toolchain 1.98

$env:AGENTDESKTOP_VERSION = "0.1.0"

pnpm --filter @agentdesktop/desktop-web `
  dist:win -- --target aarch64-pc-windows-msvc
```

The MSI is written to:

```text
target\aarch64-pc-windows-msvc\release\bundle\msi\
```

Use a native Windows ARM64 machine or CI runner to test the ARM64 installer.

## Rename the installer

`setup.msi` is not a different installer format. Rename or copy the generated
MSI when that filename is required:

```powershell
$installers = @(Get-ChildItem `
  "target\x86_64-pc-windows-msvc\release\bundle\msi" `
  -Filter *.msi)

if ($installers.Count -ne 1) {
  throw "Expected one MSI, found $($installers.Count)"
}

Copy-Item $installers[0].FullName .\setup.msi
```

## Install and verify

Open PowerShell as Administrator. The MSI requires elevation because it installs
a machine service and writes under `Program Files` and `ProgramData`.

Interactive installation:

```powershell
msiexec.exe /i .\setup.msi
```

Silent installation:

```powershell
msiexec.exe /i .\setup.msi /qn /norestart
```

Verify the service and local API:

```powershell
Get-Service AgentDesktop
& "$env:ProgramFiles\Agent Desktop\agentdesktop.exe" status
```

The service should report `Running`, and the status command should exit
successfully.

## Upgrade behavior

The MSI owns process handling for in-place upgrades; MDM does not need a
custom process-kill script. WiX asks every running `agentdesktop.exe` tray
process to close, waits up to 15 seconds, and terminates any remaining process
before replacing files. The existing `ServiceControl` entry stops the
`AgentDesktop` service, waits for shutdown, and starts the new service after the
upgrade.

The MSI does not relaunch the tray application from an MDM deployment running
as `SYSTEM`, because that could put a GUI in the wrong user session. The tray
application starts at the user's next sign-in through its existing Run entry, or
the user can open it manually. Configuration and state under
`%ProgramData%\AgentDesktop` are preserved.

Deploy upgrades silently with:

```powershell
msiexec.exe /i .\agentdesktop.msi /qn /norestart
```

Treat exit code `0` as success and `3010` as success with a reboot required.

## Configure the installed service

The installer creates this default configuration:

```text
%ProgramData%\AgentDesktop\config.yaml
```

It initially contains an empty configuration, so the service can start before a
tenant configuration is provisioned. Replace it through MDM or as an
administrator, then restart the service:

```powershell
Copy-Item .\config.yaml "$env:ProgramData\AgentDesktop\config.yaml" -Force
Restart-Service AgentDesktop
```

The service stores device identity and runtime state below
`%ProgramData%\AgentDesktop\state`. The daemon applies restricted permissions
to that state.

## Uninstall

Interactive uninstall:

```powershell
msiexec.exe /x .\setup.msi
```

Silent uninstall:

```powershell
msiexec.exe /x .\setup.msi /qn /norestart
```

The MSI removes the service and tray application. It intentionally preserves the
machine configuration so upgrades and reinstalls do not overwrite tenant
settings.


## Production signing
Need to set up Azure artifact signing service: https://learn.microsoft.com/en-us/azure/artifact-signing/
