# QEMU desktop test environment

This harness builds a Fedora Workstation base image and runs each manual or automated journey on a disposable qcow2 overlay. Resetting the VM removes all connector, Agent Gateway, policy, credential, capture, and trust state without rebuilding the base.

## Host requirements

- `qemu-system-x86_64` and `qemu-img`
- `socat` for lazy guest-to-host loopback forwarding
- Packer with the QEMU plugin, only when building the base
- KVM for interactive use and normal CI performance; QEMU TCG is selected when KVM is unavailable
- `ssh` and `setsid` for guest control

The checked-in Packer definition downloads and verifies the Fedora 44 Everything netinstaller, then performs an unattended Workstation installation. The resulting image is stored under the ignored `tests/vm/.artifacts` directory.

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
tests/vm/vm.sh copy target/release/agentgateway-edge-connector
```

For the installation journey, build and transfer the single embedded artifact:

```bash
scripts/build-embedded-installer.sh \
  ../agentgateway/target/ci/agentgateway \
  tests/vm/fixtures/agentgateway-standalone.yaml
tests/vm/vm.sh copy \
  target/release/agentgateway-edge-installer \
  /home/agentedge/Downloads/agentgateway-edge-installer
```

The VM fixture points Agent Gateway at the laptop mock LLM through `host.test:8000`. It is specific to this QEMU environment and must not be used as a general release configuration.

Simulate the MDM-owned half of managed installation over SSH with an organization-specific executable:

```bash
scripts/build-managed-installer.sh \
  examples/managed-organization.json \
  target/example-managed-installer
tests/vm/vm.sh copy \
  target/example-managed-installer \
  /home/agentedge/Downloads/example-managed-installer
tests/vm/vm.sh ssh \
  /home/agentedge/Downloads/example-managed-installer install --yes
```

This step must preserve existing Claude settings and service activation state and must not install a local Agent Gateway. The user-owned `connect-agents` step needs a reachable HTTPS identity/enrollment simulation and managed gateway. The checked-in example endpoints are documentation values and cannot drive that step. Real managed security validation remains blocked until Agent Gateway enforces the managed identity contract.

The test-only `agentedge` account has passwordless sudo. SSH password authentication is exposed only through the QEMU user-network forward on host loopback. Do not publish the VM's SSH port on a non-loopback host address.

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
| `host.test:18080` | `127.0.0.1:18080` | Mock Anthropic/LLM API |
| `host.test:4000` | `127.0.0.1:4000` | Agent Gateway LLM listener |
| `host.test:15008` | `127.0.0.1:15008` | Agent Gateway HBONE listener |
| `host.test:15021` | `127.0.0.1:15021` | Agent Gateway readiness endpoint |

Override or extend the mappings per run without changing the guest image:

```bash
VM_HOST_FORWARDS=18080:18080,4000:4000,8443:8443 \
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