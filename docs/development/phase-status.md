# Phase status

Last updated: 2026-08-11

This file records verified implementation status and external blockers. A phase is complete only for the deployment mode named below.

## Implemented and tested

- Phase 0: superseded by the shared opaque CONNECT forwarder. Native Claude traffic preserves bidirectional bytes, streaming, half-close, cancellation, graceful shutdown, and fail-closed Gateway errors without parsing HTTP.
- Phase 1: complete. Standalone mode owns a separate local Agent Gateway process, reports health, preserves user-owned policy configuration, and supports persistent connector-assisted Claude configuration.
- Phase 2, native managed path: complete for the manual walkthrough. OAuth authenticates enrollment and bounded expired-certificate recovery. The authority issues one short-lived mTLS certificate whose SPIFFE URI binds the verified organizational user and device; managed forwarding carries no OAuth bearer token. Agent Desktop protects P-256 enrollment and retry-stable renewal keys, renews within six hours of expiry, rotates the HBONE pool after validated replacement, and recovers for seven days after expiry using OAuth plus enrolled-key proof. The Go service persists and reconciles enrollment, renewal, recovery, and revocation state in PostgreSQL. Agent Gateway validates the client certificate on its outer CONNECT listener and injects provider credentials. Published revocation consumption and managed transparent capture remain pending.
- Phase 3, standalone Linux: complete for the current self-managed milestone. `agentdesktop launch --profile claude` creates a gated systemd user scope, validates its exact cgroup v2 path, verifies the installed inspection CA fingerprint, registers the scope in a root-owned active-scope registry, and atomically reconciles a shared nftables cgroup set before release. Stable rules redirect TCP/443 and reject UDP/443; concurrent captured scopes have independent set members, and stale cgroups are removed without numeric nftables parsing. The connector owns an authenticated in-process HBONE relay and one in-memory 256-bit capability per Agent Gateway process generation. The standalone installer provisions owner-only dynamic-CA state and renders only its paths into a new starter config; the runtime connector never reads the private key. Agent Gateway owns key use, TLS interception, policy, and TLS forwarding to the original destination. The installer preserves existing configs, installs the root-owned network helper through Polkit, and requires separate inspection-trust consent. `agentdesktop trust install|remove` is fingerprint-scoped and reversible; removal refuses while capture is active.
- Phase 4: partial. Standalone and organization-specific development installers, atomic bundle upgrade, verification, service control, support reports, separate agent consent, and the Linux trust journey are implemented. A graphical UI, signed public packaging, and update delivery remain pending.
- Phase 5: in progress. Linux native forwarding and standalone transparent capture are validated in a disposable Fedora Workstation VM. The Linux machine forwarder derives native and captured user identity from exact `NETLINK_SOCK_DIAG` tuples, while authenticated per-user agents retain OAuth, enrollment keys, and self-managed Gateway ownership. Managed and self-managed traffic select per-UID HBONE clients and fail closed without attribution or registration. All Rust targets compile warning-free for `x86_64-pc-windows-msvc`. Windows 11 runtime tests validate native standalone forwarding, explicit named-pipe ACLs, OS-derived client SIDs, timeout-bounded external mTLS signing, certificate registration, SID-keyed pools, and native WFP connect redirection. The WFP callout uses flow-bound authorization-token metadata for `TokenUser`, carries the exact original destination and raw SID through redirect context, rejects a second configuration, and blocks matching flows after the configuring service exits. The Windows machine process never loads user identity in session mode and never attributes from PID or TCP-table snapshots. Production driver packaging/signing, trust, and process-scoped Windows capture remain pending. The QEMU VM intentionally bypasses TPM and Secure Boot because neither is part of the connector or WFP test boundary. macOS development still requires Apple hardware for signed System Extension and Network Extension lifecycle validation.
- Phase 6: mostly complete. Explicit tunnel-establishment and shutdown timeouts, bounded concurrency, no unsafe retries, and deterministic forwarding, disconnect, outage, and lifecycle tests are implemented.
- Phase 7: partial. Structured privacy-safe lifecycle logs and bounded OTLP export are implemented. Application trace propagation belongs to Agent Gateway because the connector does not inspect tunneled HTTP. Metric export and collector-correlation coverage remain pending.
- Phase 8: partial. Bundle integrity manifests and tamper-safe upgrade/uninstall are implemented. Publisher signatures, staged rollout, and minimum-version enforcement remain pending.

## Fedora validation

A clean Fedora Workstation VM validates the complete standalone capture journey with a real Agent Gateway:

- The standalone installer creates `0700` Agent Gateway CA state, a `0600` issuing key, and a public certificate. The installed runtime connector does not read the key bytes.
- The system trust anchor is installed under its SHA-256 fingerprint, removed idempotently, and installed again.
- A captured HTTPS request succeeds with certificate validation and Agent Gateway logs the dynamic-CA route, original destination, and HTTP 200 response.
- A shell descendant remains captured by the parent scope.
- Killing Agent Gateway during a captured session does not permit direct HTTPS fallback.
- UDP/443 is denied.
- The shared nftables capture set is empty after normal and failed sessions without disrupting concurrent scopes.
- Clean install and in-place upgrade preserve user-owned Gateway configuration and CA identity.

Deterministic tests additionally cover protected local-token handling, wrong-token rejection, HBONE byte fidelity, relay readiness, later-flow reconnect without replay, exact cgroup validation, launch gating, preparation failure, trust ownership, registry deduplication, incompatible-scope rejection, and scoped cleanup. The isolated container test covers kernel redirection, original-destination recovery, concurrent set members, independent removal, and stale-cgroup reconciliation.

## Active blockers

- Managed Phase 3 depends on propagating the certificate-derived user/device identity from the authenticated outer CONNECT connection into immutable inner policy context. The standalone local capability must never be used for managed transport.
- Production enrollment still requires fail-closed Agent Gateway consumption of revocation state.
- Public Linux packaging requires publisher signatures and a production Polkit policy/package for the root-owned helper. The development installer currently relies on the desktop's normal Polkit authorization prompt.
- Strong anti-bypass against local administrators is not claimed. The current guarantee is process-scoped routed capture for the selected systemd scope and descendants.
- macOS capture requires Network Extension entitlements and signing. Windows process-scoped capture still requires launch gating, UDP denial, production signing, and lifecycle validation.

## Phase 5 execution plan

1. **Portable native baseline:** Windows MSVC compilation, standalone runtime forwarding, authenticated named-pipe sessions, external signing, SID-keyed pools, and WFP native attribution are implemented. Complete a combined managed Windows walkthrough and production service installation. macOS build/runtime validation, Keychain, LaunchAgent, trust, installer, and native walkthroughs remain.
2. **Shared capture contract:** make the forwarding core consume a captured stream with explicit original destination and verified source identity instead of deriving Linux socket metadata itself. Keep execution scopes, capture registration, trust, credentials, services, and installation behind narrow platform-owned implementations.
3. **macOS capture vertical slice:** add a signed `NETransparentProxyProvider` System Extension, select flows by audit token and code-signing identity, forward TCP with its original endpoint, deny selected UDP/443, and test relay loss, extension loss, sleep, upgrade, and removal. Do not claim fail-closed extension-loss behavior until it is measured on a physical Mac.
4. **Windows capture vertical slice:** the minimal ALE connect-redirect WFP driver, privileged controller boundary, original-destination context, and flow-bound initiating SID are implemented. Add production packaging/signing, Job Object launch gating, descendant semantics, UDP/443 denial, and lifecycle integration. Resolve PID-reuse races without using PID or TCP-table snapshots for attribution before claiming process-tree capture equivalent to Linux cgroups.
5. **Walkthrough environments:** the Windows 11 QEMU/KVM base, unattended installation, test-signing, WDK provisioning, remote control, and disposable overlays are implemented. Add the combined managed Windows journey. Use an Apple Silicon Mac mini for macOS VMs and retain one physical installation for signing, entitlement, approval, and lifecycle tests.
6. **Hardening after vertical slices:** verify concurrent-user isolation, helper processes, Gateway loss, upgrade/rollback, trust removal, and managed certificate-derived policy context; then document MDM/GPO deployment and production signing separately.

## Next implementation step

Package and production-sign the Windows native WFP path, then add process-scoped launch gating and UDP denial before claiming Windows capture. In parallel, prepare the Apple-hardware development environment for native macOS validation.