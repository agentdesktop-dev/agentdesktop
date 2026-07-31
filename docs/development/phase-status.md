# Phase status

Last updated: 2026-07-30

This file records verified implementation status and external blockers. A phase is not complete merely because an interface or document exists.

## Implemented and tested

- Phase 0: streaming Claude-compatible HTTP forwarding, HTTP fidelity, fail-closed upstream errors, cancellation, and graceful shutdown.
- Phase 1: explicit standalone mode, separate local Agent Gateway lifecycle, health reporting, native and connector-assisted Claude paths, policy smoke tests, and standalone operations guidance.
- Phase 2, partial: browser Authorization Code with PKCE, DPoP-bound access tokens, signed-token validation, protected credential storage, refresh rotation, restart restoration, credential-generation pool isolation, local logout, and per-request managed DPoP headers. An executable mock enrollment authority derives user and key identity from validated credentials, rejects wrong keys and replayed proofs, requires explicit approval, assigns device IDs, and reports independent revocation. The connector enrollment client and Agent Gateway enforcement are not implemented.
- Phase 3, partial Linux data path: application routing recognizes mutually exclusive native, connector, and captured choices; captured mode still fails before application launch because the prototype is not a trusted supported runtime. A pooled HTTP/2 CONNECT transport validates explicit destination ports and preserves bidirectional bytes. The Linux relay recovers `SO_ORIGINAL_DST`, bounds concurrent tunnels, requires a current-user-owned `0600` token file, and fails closed. Atomic cgroup v2/nftables setup redirects TCP/443 and rejects UDP/443. Private-container coverage validates the complete redirected kernel-to-HBONE path without sharing host network or cgroup namespaces. An opt-in real Agent Gateway smoke test validates wrong-token denial, valid-token forwarding, and HTTP/2 CONNECT interoperability. Managed HBONE authentication, local token and reconnect lifecycle integration, and production host validation remain blocked as documented below.
- Phase 4, partial: transactional standalone bundle install/upgrade/uninstall, generated hardened Linux user-systemd unit with explicit enable/disable lifecycle, and a privacy-safe local status API. Application-profile UI, trust workflow, binary download/update, and graphical UI remain pending.
- Phase 5, partial: runtime platform capability reporting and a published compatibility matrix. Linux native forwarding is validated; transparent capture, trust integration, installers, and macOS/Windows builds remain unavailable.
- Phase 6, partial: explicit connection, upload/response-header, and shutdown timeouts; bounded full-stream concurrency; no request retries; deterministic slow-client, timeout, overload, disconnect, malformed-request isolation, forced-shutdown, and repeated lifecycle tests.
- Phase 7, partial: JSON structured lifecycle and failure logs with fixed privacy-safe event categories; validated W3C trace-context propagation and generation; privacy-safe forwarding spans with opt-in bounded OTLP/gRPC export and orderly flush; bounded low-cardinality operational counters. Metric export and automated collector-correlation coverage remain pending.
- Phase 8, partial: hardened non-root container build and SHA-256 bundle integrity manifests with verification and tamper-safe upgrade/uninstall. Publisher signatures, staged fleet rollout, minimum-version policy, and privileged IPC remain pending.

## Active blockers

- Verified device enrollment requires a production enrollment authority, authenticated administrator approval/revocation, a connector enrollment client, durable records, and an agreed Agent Gateway device-identity contract. The repository's mock authority makes the proposed client protocol executable but is not a production trust source. A connector-generated DPoP key remains connector-instance proof and is not labeled as organizational device identity.
- Trusted managed identity requires Agent Gateway support for DPoP proof validation, replay protection, `cnf.jkt` binding, connector credential stripping, and immutable trusted policy context.
- Transparent capture still requires integrated local token/reconnect lifecycle, managed TLS and DPoP authentication, and production OS validation. Local token enforcement and real Agent Gateway HTTP/2 CONNECT interoperability are proven. The selected Linux mechanism is cgroup v2 with nftables, targeting an externally managed systemd scope, TCP/443 redirection, original-destination forwarding, and UDP/443 denial. Private rootless-Podman coverage validates the isolated kernel-to-HBONE path but cannot establish production host process attribution or anti-bypass guarantees. eBPF is documented as a future strengthening path.
- macOS capture, trust installation, packaging, and signing require macOS with Network Extension entitlements. Windows capture, trust installation, packaging, and signing require Windows with WFP development and signing facilities.
- Additional platform installers and the capture/trust UI depend on final package names and the first supported capture/trust platform.

## Work that can continue locally

- Measured latency and memory reliability baselines.
- OpenTelemetry metric export and backend correlation tests using the selected Rust OTel 0.31 stack.
- Agent Gateway DPoP work when its source checkout and contribution boundary are available.
- Linux local token/reconnect lifecycle integration, managed Agent Gateway DPoP authentication, and disposable-host tests; do not expose a captured application path before those boundaries work end to end.