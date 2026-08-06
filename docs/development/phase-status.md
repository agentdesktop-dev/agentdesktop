# Phase status

Last updated: 2026-08-06

This file records verified implementation status and external blockers. A phase is complete only for the deployment mode named below.

## Implemented and tested

- Phase 0: superseded by the shared opaque CONNECT forwarder. Native Claude traffic preserves bidirectional bytes, streaming, half-close, cancellation, graceful shutdown, and fail-closed Gateway errors without parsing HTTP.
- Phase 1: complete. Standalone mode owns a separate local Agent Gateway process, reports health, preserves user-owned policy configuration, and supports persistent connector-assisted Claude configuration.
- Phase 2, native managed path: complete for the manual walkthrough. OAuth authenticates enrollment and bounded expired-certificate recovery. The authority issues one short-lived mTLS certificate whose SPIFFE URI binds the verified organizational user and device; managed forwarding carries no OAuth bearer token. Agent Desktop protects P-256 enrollment and retry-stable renewal keys, renews within six hours of expiry, rotates the HBONE pool after validated replacement, and recovers for seven days after expiry using OAuth plus enrolled-key proof. The Go service persists and reconciles enrollment, renewal, recovery, and revocation state in PostgreSQL. Agent Gateway validates the client certificate on its outer CONNECT listener and injects provider credentials. Published revocation consumption and managed transparent capture remain pending.
- Phase 3, standalone Linux: complete for the current self-managed milestone. `agentdesktop launch --profile claude` creates a gated systemd user scope, validates its exact cgroup v2 path, verifies the installed inspection CA fingerprint, registers the scope in a root-owned active-scope registry, and atomically reconciles a shared nftables cgroup set before release. Stable rules redirect TCP/443 and reject UDP/443; concurrent captured scopes have independent set members, and stale cgroups are removed without numeric nftables parsing. The connector owns an authenticated in-process HBONE relay and one in-memory 256-bit capability per Agent Gateway process generation. Agent Gateway owns dynamic-CA generation, key storage, TLS interception, policy, and TLS forwarding to the original destination. The installer initializes owner-only CA state, renders only CA paths into a new starter config, preserves existing configs, installs the root-owned network helper through Polkit, and requires separate inspection-trust consent. `agentdesktop trust install|remove` is fingerprint-scoped and reversible; removal refuses while capture is active.
- Phase 4: partial. Standalone and organization-specific development installers, atomic bundle upgrade, verification, service control, support reports, separate agent consent, and the Linux trust journey are implemented. A graphical UI, signed public packaging, and update delivery remain pending.
- Phase 5: partial. Linux native forwarding and standalone transparent capture are validated in a disposable Fedora Workstation VM. macOS and Windows capture remain unavailable.
- Phase 6: mostly complete. Explicit tunnel-establishment and shutdown timeouts, bounded concurrency, no unsafe retries, and deterministic forwarding, disconnect, outage, and lifecycle tests are implemented.
- Phase 7: partial. Structured privacy-safe lifecycle logs and bounded OTLP export are implemented. Application trace propagation belongs to Agent Gateway because the connector does not inspect tunneled HTTP. Metric export and collector-correlation coverage remain pending.
- Phase 8: partial. Bundle integrity manifests and tamper-safe upgrade/uninstall are implemented. Publisher signatures, staged rollout, and minimum-version enforcement remain pending.

## Fedora validation

A clean Fedora Workstation VM validates the complete standalone capture journey with a real Agent Gateway:

- The installer creates `0700` Agent Gateway CA state, a `0600` issuing key, and a public certificate without Agent Desktop reading key bytes.
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
- macOS capture requires Network Extension entitlements and signing. Windows capture requires WFP implementation and signing.

## Next implementation step

Publish and consume certificate revocation state in Agent Gateway, then verify fail-closed rejection before certificate expiry.