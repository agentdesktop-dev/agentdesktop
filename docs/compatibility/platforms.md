# Platform compatibility

Last updated: 2026-07-30

`Supported` means the behavior has automated coverage or a documented smoke path in this repository. `Unavailable` means the connector reports the capability as false and does not attempt a partial implementation.

| Capability | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Standalone native gateway forwarding | Supported | Build not validated | Build not validated |
| Managed native gateway forwarding | Experimental | Build not validated | Build not validated |
| Persistent Claude Code connector configuration | Supported | Build not validated | Build not validated |
| Secret Service credentials | Supported | Unavailable | Unavailable |
| Protected-file credential fallback | Supported | Unavailable | Unavailable |
| Transparent process capture | Prototype kernel-to-HBONE path validated in private container; unsupported for applications | Unavailable | Unavailable |
| CA trust installation/removal | Unavailable | Unavailable | Unavailable |
| Transactional bundle installer | Supported | Build not validated | Build not validated |
| User service integration | Generated systemd unit | Unavailable | Unavailable |

The local `/_agentgateway/status` response exposes the current binary's capability flags. Application launch and native HTTP forwarding are unprivileged. Transparent capture and trust installation remain false until their platform implementations satisfy process identity, fail-closed routing, scoped removal, and privileged integration tests.

## Required validation environments

- Linux capture: private-container coverage validates cgroup v2/nftables TCP/443 redirection, original-destination recovery, bidirectional HBONE forwarding, and UDP/443 denial. An opt-in smoke test validates local token rejection/acceptance and HTTP/2 CONNECT interoperability with a real Agent Gateway. A disposable privileged host is still required for production process attribution, existing-firewall compatibility, restart behavior, and anti-bypass verification. Integrated token lifecycle and managed DPoP authentication remain required before application support.
- macOS: supported hardware and OS with Network Extension entitlements, code signing, System Extension lifecycle tests, and Keychain trust tests.
- Windows: supported Windows VM or hardware with WFP development/signing, service lifecycle tests, and certificate-store tests.