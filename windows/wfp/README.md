# Agent Desktop Windows WFP Driver

This directory contains the native WDM/WFP producer for Windows 11 x64 and the redirect-context ABI consumed by `src/platform/windows.rs`.

Current scope:

- ALE connect-redirect callouts for IPv4 and IPv6.
- Machine-only control device ACL for `SYSTEM` and built-in administrators.
- One versioned buffered IOCTL for live configuration.
- Exact TCP filters for the configured public loopback destination and port.
- Native-flow redirect contexts shaped to the Rust parser: `AGWF`, version `1`, flow kind `1`, `sockaddr_len`, `sid_len`, zero reserved bytes, then raw sockaddr bytes followed by raw SID bytes.

Out of scope in this drop:

- UDP.
- Process-scoped transparent capture.
- Production signing and package distribution.

This driver currently supports native loopback attribution, not process-scoped transparent capture. The machine service configures it only after binding the hidden proxy listener, and matching flows fail closed when the retained configuring process exits. See [the Windows VM guide](../../tests/vm/windows/README.md) for the reproducible build and smoke boundary.

## Files

- `agwfp_abi.h`: shared IOCTL and redirect-context ABI.
- `agwfp.c`: WDM driver and WFP callout/filter logic.
- `agentdesktop-wfp.vcxproj`: WDK project with warnings-as-errors.
- `agentdesktop-wfp.inf`: non-PnP service INF.
- `build.ps1`: local WDK/MSBuild build helper.
- `install.ps1`: copies the built `.sys` and creates or starts the kernel service.
- `uninstall.ps1`: stops and deletes the service and removes the staged binary.

## IOCTL contract

The device is exposed as `\\.\AGWfp` and accepts `AGWFP_IOCTL_SET_CONFIGURATION` with an `AGWFP_CONFIGURATION_V1` input buffer.

Validation performed before any WFP state is added:

- `version == 1`
- `size == sizeof(AGWFP_CONFIGURATION_V1)`
- `flags == 0`
- `live_service_pid` resolves to a live process
- public and proxy families match
- public and proxy destinations are loopback addresses with non-zero ports

After validation, the driver opens the filtering engine, adds the provider, sublayer, two callouts, and the exact active-family filter, then creates the redirect handle and publishes the runtime. Partial activation is explicitly removed before the IOCTL fails. Configuration is one-shot until driver unload.

## Redirect behavior

For matching flows, the classify function:

- blocks unless WFP supplies its flow-bound authorization-token metadata
- queries `TokenUser` from that WFP token handle and does not fall back to PID, process lookup, or TCP-table attribution
- queries redirect state to avoid redirect loops
- acquires writable layer data, preserves the original remote sockaddr, builds the exact Rust-consumable redirect context, sets `localRedirectTargetPID`, `localRedirectHandle`, and `localRedirectContext`, and rewrites the remote destination to the hidden proxy endpoint

## Build

Requirements:

- Windows 11 x64 build machine
- Visual Studio with MSBuild
- WDK with the `WindowsKernelModeDriver10.0` toolset
- test-signing or a signing path appropriate for your environment

Build from an elevated PowerShell prompt:

```powershell
pwsh -File .\windows\wfp\build.ps1 -Configuration Release
```

Or directly:

```powershell
msbuild .\windows\wfp\agentdesktop-wfp.vcxproj /t:Build /p:Configuration=Release /p:Platform=x64 /m
```

## Install

The provided install helper is intentionally simple and uses the SCM for a demand-start kernel service:

```powershell
pwsh -File .\windows\wfp\install.ps1 -Configuration Release
```

Remove it with:

```powershell
pwsh -File .\windows\wfp\uninstall.ps1
```

The INF is included for packaging/signing workflows, but the helper script does not currently stage the service through `pnputil`.

## Validation status

The Windows 11 QEMU environment builds this project with WDK `10.0.26100.0`, warnings-as-errors, Universal API validation, and SHA-256 test signing. `tests/vm/windows/wfp-smoke.ps1` validates the 80-byte configuration ABI, machine control device, one-shot configuration, real loopback connect redirect, original destination, and initiating account SID. Run it against a fresh driver with `-ServiceDeath` to configure from a child process and verify that matching connections remain blocked after that process exits.