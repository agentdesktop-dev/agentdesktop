# QEMU desktop test environment

This harness builds a Fedora Workstation base image and runs each manual or automated journey on a disposable qcow2 overlay. Resetting the VM removes all connector, Agent Gateway, policy, credential, capture, and trust state without rebuilding the base.

## Host requirements

- `qemu-system-x86_64` and `qemu-img`
- `socat` for lazy guest-to-host loopback forwarding
- Packer with the QEMU plugin, only when building the base
- KVM for interactive use and normal CI performance; QEMU TCG is selected when KVM is unavailable
- `ssh` and `setsid` for guest control

The checked-in Packer definition downloads and verifies the Fedora 44 Everything netinstaller, performs an unattended Workstation installation, and installs the repository-pinned Claude Code version used by the walkthroughs. The resulting image is stored under the ignored `tests/vm/.artifacts` directory. Claude Code is a base-image test prerequisite; connector and Agent Gateway builds remain scenario artifacts and are never baked into the base.

The first build downloads a roughly 1.2 GB installer plus the Workstation package set and can take tens of minutes. Packer caches the verified installer in its user cache, and ordinary clean-slate runs reuse the completed base image rather than reinstalling Fedora. CI should publish or cache the base qcow2 as a separate build artifact.

## First run

```bash
tests/vm/vm.sh check
tests/vm/vm.sh build
tests/vm/vm.sh reset
tests/vm/vm.sh start --display
tests/vm/vm.sh wait
```

The graphical VM keeps running after the command returns. Open an SSH shell separately with:

```bash
tests/vm/vm.sh ssh
```

Copy a connector build or scenario into the guest with:

```bash
tests/vm/vm.sh copy target/release/agentdesktop
```

For the installation journey, build and transfer the single embedded artifact:

```bash
scripts/build-embedded-installer.sh \
  ../agentgateway/target/ci/agentgateway \
  tests/vm/fixtures/agentgateway-standalone.yaml
tests/vm/vm.sh copy \
  target/release/agentdesktop-installer \
  /home/agentdesktop/Downloads/agentdesktop-installer
```

The VM fixture points Agent Gateway at the laptop mock LLM through `host.test:8000`. It is specific to this QEMU environment and must not be used as a general release configuration.

Prepare a complete managed user walkthrough from the immutable Fedora base:

```bash
scripts/vm-managed-walkthrough.sh prepare --reset
```

The harness starts the real rootless Podman walkthrough stack, builds an organization-specific installer, and copies only that installer to Fedora's Downloads directory. It does not install software or trust on the user's behalf.

Perform the user journey in the Fedora desktop:

1. Run `~/Downloads/agentdesktop-installer install` in Terminal.
2. Review the organization, gateway, enrollment, and inspection summary.
3. Choose whether to install the organization's public CA. This consent is separate and is not implied by noninteractive installer acceptance.
4. Approve the normal desktop privilege prompt when installing software or trust.
5. Run `agentdesktop connect-agents`, complete browser sign-in, and approve the separate Claude Code settings change.
6. After an administrator approves the pending enrollment, launch `claude` normally and ask it to reply with exactly `SMOKE_OK`.

Perform the administrator journey in the host browser at `http://localhost:8091/admin/`: sign in, inspect the pending device, and approve, reject, or revoke it. This cleartext endpoint is a walkthrough-only adapter bound to host loopback. The VM-facing identity, enrollment, and Gateway endpoints remain HTTPS with the canonical `host.test` issuer. Production administrators use organization-trusted HTTPS.

Run `scripts/vm-managed-walkthrough.sh stop` afterward. Omit `--reset` to refresh the server stack and installer while preserving the running Fedora VM.

The mock identity provider and Anthropic service are deterministic test fixtures. Enrollment uses the repository control plane, and the real Agent Gateway enforces the issued mTLS device identity before forwarding to the mock provider.

The test-only `agentdesktop` account has passwordless sudo. SSH password authentication is exposed only through the QEMU user-network forward on host loopback. Do not publish the VM's SSH port on a non-loopback host address.

## Clean-slate lifecycle

```bash
tests/vm/vm.sh reset
tests/vm/vm.sh start
tests/vm/vm.sh wait
# Run a scenario or inspect the desktop.
tests/vm/vm.sh clean
```

`reset` and `clean` never modify the base image. Packer rebuilds the base only when the Fedora release or generic VM prerequisites change. Connector and Agent Gateway binaries must be installed by each journey rather than baked into the base.

## Reaching services on the host

QEMU user-mode networking provides two stable guest names:

- `host.test` (`10.0.2.100`) reaches explicitly forwarded services bound to host loopback. This is the preferred test path because it does not expose test services to the LAN.
- `host.internal` (`10.0.2.2`) is QEMU's stable host gateway. A service reached this way must listen on a suitable host interface and may be subject to the host firewall.

The default `host.test` mappings are:

| Guest endpoint | Host endpoint | Intended service |
| --- | --- | --- |
| `host.test:8000` | `127.0.0.1:8000` | Checked-in mock Anthropic API |
| `host.test:18080` | `127.0.0.1:18080` | HTTPS mock identity provider |
| `host.test:8090` | `127.0.0.1:8090` | HTTPS enrollment API |
| `host.test:8443` | `127.0.0.1:8443` | Managed HTTPS/mTLS CONNECT listener |
| `host.test:15008` | `127.0.0.1:15008` | Standalone loopback CONNECT listener |
| `host.test:15021` | `127.0.0.1:15021` | Agent Gateway readiness endpoint |

Override or extend the mappings per run without changing the guest image:

```bash
VM_HOST_FORWARDS=18080:18080,8090:8090,8443:8443 \
  tests/vm/vm.sh start --display
```

Each mapping is `GUEST_PORT:HOST_PORT`; all host targets remain `127.0.0.1`. Start the laptop-side services before probing them:

```bash
tests/vm/vm.sh probe-host
```

`probe-host` reports whether each mapped laptop-loopback target is accepting TCP. Verify the complete guest path with the service protocol, for example `tests/vm/vm.sh ssh curl http://host.test:8000/`.

QEMU resolves forwarding when each guest connection is made, so Agent Gateway and the mock LLM can be restarted without restarting the VM.

## CI shape

A headless CI job can cache the immutable base image, then run:

```bash
tests/vm/vm.sh reset
tests/vm/vm.sh start
tests/vm/vm.sh wait
tests/vm/vm.sh ssh /path/to/guest-scenario
tests/vm/vm.sh clean
```

The job must install a cleanup trap so QEMU is stopped and the overlay is removed even when a scenario fails. Scenario scripts and artifact transfer are the next layer; they should use this lifecycle rather than adding state to the base image.