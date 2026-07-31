# Platform compatibility

Last updated: 2026-07-30

`Supported` means the behavior has automated coverage or a documented smoke path in this repository. `Unavailable` means the connector reports the capability as false and does not attempt a partial implementation.

| Capability | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Standalone native gateway forwarding | Supported | Build not validated | Build not validated |
| Managed native gateway forwarding | Experimental | Build not validated | Build not validated |
| Claude native/connector launcher | Supported | Build not validated | Build not validated |
| Secret Service credentials | Supported | Unavailable | Unavailable |
| Protected-file credential fallback | Supported | Unavailable | Unavailable |
| Transparent process capture | Prototype rules validated in private container; unavailable at runtime | Unavailable | Unavailable |
| CA trust installation/removal | Unavailable | Unavailable | Unavailable |
| Transactional bundle installer | Supported | Build not validated | Build not validated |
| User service integration | Generated systemd unit | Unavailable | Unavailable |

The local `/_agentgateway/status` response exposes the current binary's capability flags. Application launch and native HTTP forwarding are unprivileged. Transparent capture and trust installation remain false until their platform implementations satisfy process identity, fail-closed routing, scoped removal, and privileged integration tests.

## Required validation environments

- Linux capture: private-container coverage validates cgroup v2/nftables TCP/443 redirection and UDP/443 denial. A disposable privileged host is still required for process attribution, existing-firewall compatibility, original-destination recovery, restart behavior, and anti-bypass verification.
- macOS: supported hardware and OS with Network Extension entitlements, code signing, System Extension lifecycle tests, and Keychain trust tests.
- Windows: supported Windows VM or hardware with WFP development/signing, service lifecycle tests, and certificate-store tests.