# Windows QEMU test environment

This harness builds a Windows 11 Enterprise Evaluation base and runs development work on a disposable qcow2 overlay. It is intended for native forwarding and WFP driver development on a real Windows kernel. It is not a Windows compatibility substitute for signed-driver testing on physical hardware.

## Host requirements

- QEMU with KVM support and `qemu-img`
- OVMF firmware at `/usr/share/edk2/ovmf/OVMF_CODE.fd`
- `socat`, OpenSSH, and `setsid`
- Packer 1.9.4 or newer when building the base

On Fedora, install the missing virtualization packages with:

```bash
sudo dnf install qemu-img qemu-system-x86-core edk2-ovmf socat openssh-clients util-linux
```

KVM is strongly recommended. The harness falls back to QEMU TCG when `/dev/kvm` is unavailable, but installing Windows under emulation is substantially slower.

## Installation media

Download the official Windows 11 Enterprise Evaluation ISO from Microsoft. The repository does not download or redistribute Windows media. Record its SHA-256 digest locally:

```bash
sha256sum /path/to/windows-11-enterprise-evaluation.iso
export WINDOWS_ISO=/path/to/windows-11-enterprise-evaluation.iso
export WINDOWS_ISO_SHA256=<64-character digest>
```

Packer verifies the ISO against `WINDOWS_ISO_SHA256` before use. The unattended definition selects image index 1 and will fail rather than prompt if the supplied ISO has a different image layout.

## First run

```bash
tests/vm/windows/vm.sh check
tests/vm/windows/vm.sh build
tests/vm/windows/vm.sh reset
tests/vm/windows/vm.sh start --display
tests/vm/windows/vm.sh wait
```

The base build installs the Microsoft Visual C++ x64 runtime, enables OpenSSH and Windows test-signing mode, then shuts the guest down. The local development account is `agentdesktop` with password `agentdesktop`. SSH is exposed only on host loopback, port 2223 by default.

Open a PowerShell-capable SSH session or run a command directly:

```bash
tests/vm/windows/vm.sh ssh
tests/vm/windows/vm.sh ssh cmd.exe /c ver
```

Copy an MSVC build or driver package into the guest:

```bash
tests/vm/windows/vm.sh copy target/x86_64-pc-windows-msvc/release/agentdesktop.exe
tests/vm/windows/vm.sh copy /path/to/driver-package 'C:/Users/agentdesktop/'
```

## Native forwarding smoke test

Build both binaries for MSVC, then copy them and the deterministic Gateway fixture into the guest:

```bash
RUSTFLAGS='-D warnings' cargo xwin build --release \
  --target x86_64-pc-windows-msvc --bin agentdesktop
cargo xwin build --release --target x86_64-pc-windows-msvc \
  -p agentgateway-app --manifest-path ../agentgateway/Cargo.toml

tests/vm/windows/vm.sh ssh powershell.exe -NoProfile -Command \
  'New-Item C:\agentdesktop -ItemType Directory -Force | Out-Null'
tests/vm/windows/vm.sh copy \
  target/x86_64-pc-windows-msvc/release/agentdesktop.exe C:/agentdesktop/
tests/vm/windows/vm.sh copy \
  ../agentgateway/target/x86_64-pc-windows-msvc/release/agentgateway.exe C:/agentdesktop/
tests/vm/windows/vm.sh copy \
  tests/vm/windows/fixtures/agentgateway-native.yaml C:/agentdesktop/
tests/vm/windows/vm.sh copy \
  tests/vm/windows/native-smoke.ps1 C:/agentdesktop/
tests/vm/windows/vm.sh ssh powershell.exe -NoProfile -NonInteractive \
  -ExecutionPolicy Bypass -File C:\agentdesktop\native-smoke.ps1
```

The smoke test requires a real Agent Gateway response through the connector, a healthy status endpoint, and closed native, status, and HBONE listeners after the supervised Gateway is killed. Agent Desktop exits when its owned Gateway exits; it does not remain running in a degraded state.

## Clean-slate lifecycle

```bash
tests/vm/windows/vm.sh stop
tests/vm/windows/vm.sh reset
tests/vm/windows/vm.sh start
tests/vm/windows/vm.sh wait
tests/vm/windows/vm.sh clean
```

`reset` replaces the disposable disk and UEFI variables. `clean` removes only disposable runtime state. Neither command modifies the immutable base image.

## Reaching host services

The guest uses `10.0.2.100` for services explicitly forwarded to host loopback. The default guest and host ports are 8000, 18080, 8090, 8443, 15008, and 15021. Override them with `WINDOWS_VM_HOST_FORWARDS`, where each mapping is `GUEST_PORT:HOST_PORT`:

```bash
WINDOWS_VM_HOST_FORWARDS=8000:8000,8443:8443 \
  tests/vm/windows/vm.sh start --display
```

This is a disposable developer VM. It intentionally runs without Secure Boot or a TPM; unattended setup bypasses those Windows 11 hardware checks. Test-signing mode and a known administrator password are deliberate and unsuitable for production. QEMU user networking provides routed test connectivity; it does not prove enforcement against a local administrator or replace the WFP security review.