# Phase status

Last updated: 2026-07-30

This file records verified implementation status and external blockers. A phase is not complete merely because an interface or document exists.

## Implemented and tested

- Phase 0: streaming Claude-compatible HTTP forwarding, HTTP fidelity, fail-closed upstream errors, cancellation, and graceful shutdown.
- Phase 1: explicit standalone mode, separate local Agent Gateway lifecycle, health reporting, native and connector-assisted Claude paths, policy smoke tests, and standalone operations guidance.
- Phase 2, partial: browser Authorization Code with PKCE, DPoP-bound access tokens, signed-token validation, protected credential storage, refresh rotation, restart restoration, credential-generation pool isolation, local logout, and per-request managed DPoP headers.
- Phase 4, partial: transactional standalone bundle install/upgrade/uninstall, plus a privacy-safe local status API covering mode, connector version, gateway, identity readiness, and forwarding limits. Service integration, application-profile UI, trust workflow, binary download/update, and graphical UI remain pending.
- Phase 5, partial: runtime platform capability reporting and a published compatibility matrix. Linux native forwarding is validated; transparent capture, trust integration, installers, and macOS/Windows builds remain unavailable.
- Phase 6, partial: explicit connection, response-header, and shutdown timeouts; bounded full-stream concurrency; no request retries; deterministic timeout, overload, disconnect, forced-shutdown, and repeated lifecycle tests.
- Phase 7, partial: JSON structured lifecycle and failure logs with fixed privacy-safe event categories; validated W3C trace-context propagation and generation; bounded low-cardinality operational counters. OTel spans, metric export, and OTLP export remain pending.
- Phase 8, partial: hardened non-root container build and SHA-256 bundle integrity manifests with verification and tamper-safe upgrade/uninstall. Publisher signatures, staged fleet rollout, minimum-version policy, and privileged IPC remain pending.

## Active blockers

- Verified device enrollment requires an enrollment authority, approval and revocation API, and an agreed Agent Gateway device-identity contract. A connector-generated DPoP key remains connector-instance proof and is not labeled as organizational device identity.
- Trusted managed identity requires Agent Gateway support for DPoP proof validation, replay protection, `cnf.jkt` binding, connector credential stripping, and immutable trusted policy context.
- Transparent capture requires an authenticated HBONE contract plus a privileged OS integration. Linux implementation needs a selected cgroup/eBPF mechanism and a disposable privileged test host. Container-only tests cannot establish host process attribution or anti-bypass behavior.
- macOS capture, trust installation, packaging, and signing require macOS with Network Extension entitlements. Windows capture, trust installation, packaging, and signing require Windows with WFP development and signing facilities.
- Phase 4 installers and UI depend on the final package, service, and daemon names and on the first supported capture/trust platform.

## Work that can continue locally

- Slow-client, malformed-request, and measured latency/memory reliability coverage.
- OpenTelemetry spans, metrics, bounded export, and backend correlation tests after selecting the supported Rust OTel versions and backend profile.
- Agent Gateway DPoP work when its source checkout and contribution boundary are available.
- Linux capture design and preflight checks once the cgroup/eBPF mechanism is selected; do not expose a captured application path before redirection and fail-closed denial work end to end.