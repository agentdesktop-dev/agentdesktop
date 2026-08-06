# Platform compatibility

Last updated: 2026-08-06

`Supported` means the behavior has automated coverage or a documented smoke path in this repository. `Unavailable` means the connector reports the capability as false and does not attempt a partial implementation.

| Capability | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Standalone native gateway forwarding | Supported | Build not validated | Build not validated |
| Managed native gateway forwarding | Experimental | Build not validated | Build not validated |
| Persistent Claude Code connector configuration | Supported | Build not validated | Build not validated |
| Secret Service credentials | Supported | Unavailable | Unavailable |
| Protected-file credential fallback | Supported | Unavailable | Unavailable |
| Owned application execution scope | Gated transient systemd user scope with validated cgroup v2 path | Unavailable | Unavailable |
| Transparent process capture | Supported for the standalone `claude` profile with installer-created Gateway configuration | Unavailable | Unavailable |
| CA trust installation/removal | Supported with explicit consent and fingerprint-scoped removal | Unavailable | Unavailable |
| Transactional bundle installer | Supported | Build not validated | Build not validated |
| User service integration | Generated systemd unit | Unavailable | Unavailable |

The local `/_agentdesktop/status` response exposes the current binary's capability flags. On Linux, native opaque forwarding is unprivileged; transparent capture and trust installation report true. The `claude` launch profile owns a gated transient systemd user scope, validates its exact cgroup, verifies inspection trust, and installs fail-closed nftables rules before release. It provides routed process-tree capture, not sandbox isolation or anti-bypass against local administrators.

## Required validation environments

- Linux capture: private-container coverage validates cgroup v2/nftables TCP/443 redirection, original-destination recovery, bidirectional HBONE forwarding, and UDP/443 denial. A real Gateway smoke validates local token rejection/acceptance and HTTP/2 CONNECT interoperability. A Fedora VM validates installation, trust, process-tree capture, fail-closed Gateway loss, and cleanup. Production validation still requires existing-firewall compatibility, restart behavior, packaging signatures, and an explicit anti-bypass position. Managed capture additionally requires certificate-derived outer-to-inner identity propagation.
- macOS: supported hardware and OS with Network Extension entitlements, code signing, System Extension lifecycle tests, and Keychain trust tests.
- Windows: supported Windows VM or hardware with WFP development/signing, service lifecycle tests, and certificate-store tests.