# Phase status

Last updated: 2026-08-04

This file records verified implementation status and external blockers. A phase is complete only for the deployment mode named below.

## Implemented and tested

- Phase 0: complete. Claude-compatible streaming HTTP forwarding preserves tested request and response semantics, cancellation, graceful shutdown, and fail-closed upstream errors.
- Phase 1: complete. Standalone mode owns a separate local Agent Gateway process, reports health, preserves user-owned policy configuration, and supports persistent connector-assisted Claude configuration.
- Phase 2: partial. The connector retains experimental DPoP forwarding while the production enrollment path uses ordinary OAuth for user identity and short-lived mTLS certificates for device identity. Agent Desktop reads an explicit enrollment-service URL from organization bootstrap, generates and protects a P-256 private key, submits a signed CSR with bearer authentication, verifies the authority's public-key fingerprint, polls owner-scoped status, persists the returned chain, rejects a leaf certificate whose SPKI does not match its key, and fails managed startup closed unless reqwest/rustls can load and present the approved identity. OAuth pool rotation reapplies mTLS identity. The Go service terminates TLS 1.3 directly, permits certificate-free initial enrollment, verifies every presented client certificate against the enrollment CA, validates issuer-pinned ES256 or RS256 bearer tokens and signed P-256 CSRs, persists pending enrollment in PostgreSQL, requires a separately scoped administrator token for bounded organization-scoped listing, approval, rejection, and device revocation, atomically claims initial and renewal issuance, creates client-auth certificates with authority-controlled SPIFFE identity, reconciles interrupted issuance and renewal with stable request identities, atomically revokes devices and their certificates, and withholds provisional device identity until approval completes. Rust tests exercise CSR signature and fingerprint binding, bearer authentication, protected persistence, certificate-key matching, reqwest identity loading, and managed pool rotation. Go tests cover the direct TLS handshake matrix, request and renewal idempotency, audit, owner isolation, administrator lifecycle actions, duplicate transition rejection, retry-stable issuance, reconciliation, transactional completion, provisional-state isolation, and revocation organization isolation, including a disposable PostgreSQL 17 integration. Production CA integration, connector renewal scheduling and atomic credential rotation, expired-certificate recovery, fail-closed revocation consumption, and Agent Gateway mTLS identity remain incomplete.
- Phase 3, standalone Linux: complete for the current self-managed milestone. `agentdesktop launch --profile claude` creates a gated systemd user scope, validates its exact cgroup v2 path, verifies the installed inspection CA fingerprint, registers the scope in a root-owned active-scope registry, and atomically reconciles a shared nftables cgroup set before release. Stable rules redirect TCP/443 and reject UDP/443; concurrent captured scopes have independent set members, and stale cgroups are removed without numeric nftables parsing. The connector owns an authenticated in-process HBONE relay and one in-memory 256-bit capability per Agent Gateway process generation. Agent Gateway owns dynamic-CA generation, key storage, TLS interception, policy, and TLS forwarding to the original destination. The installer initializes owner-only CA state, renders only CA paths into a new starter config, preserves existing configs, installs the root-owned network helper through Polkit, and requires separate inspection-trust consent. `agentdesktop trust install|remove` is fingerprint-scoped and reversible; removal refuses while capture is active.
- Phase 4: partial. Standalone and organization-specific development installers, atomic bundle upgrade, verification, service control, support reports, separate agent consent, and the Linux trust journey are implemented. A graphical UI, signed public packaging, and update delivery remain pending.
- Phase 5: partial. Linux native forwarding and standalone transparent capture are validated in a disposable Fedora Workstation VM. macOS and Windows capture remain unavailable.
- Phase 6: mostly complete. Explicit connection, response, and shutdown timeouts; bounded concurrency; no unsafe retries; deterministic overload, disconnect, malformed request, and lifecycle tests are implemented.
- Phase 7: partial. Structured privacy-safe logs, W3C trace context, bounded OTLP export, and operational counters are implemented. Metric export and collector-correlation coverage remain pending.
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

- Managed Phase 3 depends on Phase 2. Remote HBONE must use mTLS for device identity plus ordinary OAuth for user identity, immutable verified outer-to-inner context, and connector-credential stripping in Agent Gateway. The standalone local capability must never be used for managed transport.
- Production enrollment still requires protected-key CA integration, connector renewal scheduling and atomic credential rotation, expired-certificate recovery, and fail-closed Agent Gateway consumption of revocation state.
- Public Linux packaging requires publisher signatures and a production Polkit policy/package for the root-owned helper. The development installer currently relies on the desktop's normal Polkit authorization prompt.
- Strong anti-bypass against local administrators is not claimed. The current guarantee is process-scoped routed capture for the selected systemd scope and descendants.
- macOS capture requires Network Extension entitlements and signing. Windows capture requires WFP implementation and signing.

## Next implementation step

Add Agent Gateway mTLS validation and immutable certificate-derived device context, then test it against Agent Desktop's managed upstream pool. Replace the local CA key with a production protected-key adapter that honors the issuance idempotency key before production rollout.