# Platform compatibility

Last updated: 2026-08-11

`Supported` means the behavior has automated coverage or a documented smoke path in this repository. `Unavailable` means the connector reports the capability as false and does not attempt a partial implementation.

| Capability | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Standalone native gateway forwarding | Supported | Build not validated | Supported in the Windows 11 VM |
| Managed native gateway forwarding | Experimental | Build not validated | Components implemented; combined walkthrough pending |
| Persistent Claude Code connector configuration | Supported | Build not validated | Compiles for MSVC; installed journey pending |
| Secret Service credentials | Supported | Unavailable | Unavailable |
| Protected-file credential fallback | Supported | Unavailable | Unavailable; user keys remain in the session agent |
| Machine/user session separation | Supported with peer UID over Unix socket | Unavailable | Implemented with peer SID over named pipe |
| Native source attribution | Exact `NETLINK_SOCK_DIAG` tuple | Unavailable | WFP flow token SID and redirect context validated |
| Owned application execution scope | Gated transient systemd user scope with validated cgroup v2 path | Unavailable | Unavailable |
| Transparent process capture | Supported for the standalone `claude` profile with installer-created Gateway configuration | Unavailable | Unavailable |
| CA trust installation/removal | Supported with explicit consent and fingerprint-scoped removal | Unavailable | Unavailable |
| Transactional bundle installer | Supported | Build not validated | Build not validated |
| User service integration | Generated systemd unit | Unavailable | Session logic implemented; production service installer pending |
| Managed agent/MCP/skill discovery | Unavailable | Supported while the Tauri background app runs | Unavailable |

The local `/_agentdesktop/status` response exposes the current binary's capability flags. On Linux, native opaque forwarding is unprivileged; transparent capture and trust installation report true. The `claude` launch profile owns a gated transient systemd user scope, validates its exact cgroup, verifies inspection trust, and installs fail-closed nftables rules before release. It provides routed process-tree capture, not sandbox isolation or anti-bypass against local administrators.

## Required validation environments

- Linux capture: private-container coverage validates cgroup v2/nftables TCP/443 redirection, original-destination recovery, bidirectional HBONE forwarding, and UDP/443 denial. A real Gateway smoke validates local token rejection/acceptance and HTTP/2 CONNECT interoperability. A Fedora VM validates installation, trust, process-tree capture, fail-closed Gateway loss, and cleanup. Production validation still requires existing-firewall compatibility, restart behavior, packaging signatures, and an explicit anti-bypass position. Managed capture additionally requires certificate-derived outer-to-inner identity propagation.
- macOS: discovery collection and reporting have deterministic fixture coverage. Production forwarding and capture validation still require supported hardware and OS with Network Extension entitlements, code signing, System Extension lifecycle tests, and Keychain trust tests. Discovery freshness across reboot requires login-item lifecycle support.
- Windows: the QEMU Windows 11 environment validates standalone native forwarding and the WFP producer independently, including exact original destination, initiating SID, and service-death fail-closed behavior. Production signing/package installation, a combined managed session/WFP walkthrough, Job Object launch gating, UDP denial, certificate-store trust, upgrade, and removal remain required.
