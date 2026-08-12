# Managed Remote Operations

This guide covers development deployments where Agent Desktop runs on an organizational user's laptop and Agent Gateway runs remotely in the organization's network. It describes user login, device enrollment, certificate lifecycle, connector startup, and local cleanup.

Managed mode is not yet a production security release. Published revocation consumption, signed distribution, managed transparent capture, and production platform validation remain incomplete. See [Phase Status](../development/phase-status.md).

## Ownership

The organization owns:

- Agent Gateway policy, provider credentials, routing, inspection, and request-level audit data.
- The OAuth identity provider and public client registration.
- The enrollment authority, certificate authority, administrator approval, and device revocation state.
- Deployment bootstrap and trust roots, usually through MDM.

Agent Desktop owns:

- Browser login and refresh-token handling for enrollment operations.
- The device private key and short-lived client certificate.
- The per-user loopback application endpoint.
- Certificate renewal, bounded expired-certificate recovery, and managed tunnel rotation.
- Fail-closed forwarding to the configured remote Agent Gateway.

The production topology separates these duties across processes. A privileged machine forwarder owns listeners, capture, OS-derived source attribution, and user-keyed tunnel pools. One session agent per logged-in user owns OAuth, enrollment records, and private-key signing. The machine process receives public certificate chains and signing results through authenticated local IPC; it never loads OAuth tokens or private-key bytes. The direct `serve` commands below remain the simplest development topology and do not by themselves exercise that installed split.

## Policy boundary

Managed Agent Desktop is policy-free. The employee application and organization bootstrap contain no model allowlists, team assignments, guardrails, provider credentials, backend routes, or policy documents. The connector selects the configured application path, proves user/device identity, and forwards opaque bytes to the organization-owned Gateway.

The enrollment service owns organizational membership workflows, invitations, device enrollment, certificate lifecycle, and revocation state. It may record which centrally defined access package is assigned to a user or team, but it must not define or evaluate AI policy. Agent Gateway owns the referenced access resources, provider secrets, model and agent routing, authorization, rate limits, guardrails, request telemetry, and enforcement. A future central admin UX must manage Gateway-native resources through a trusted Gateway integration rather than distribute policy to employee devices or create an enrollment-specific policy language.

OAuth tokens are never attached to CONNECT requests or application traffic. Managed forwarding authenticates the outer HTTP/2 connection with the authority-issued mTLS certificate only.

## Prerequisites

A managed deployment needs:

- An HTTPS OAuth issuer supporting Authorization Code with PKCE `S256`, an ES256 signing key published through discovery, and refresh-token issuance and rotation.
- A public OAuth client ID, expected audience, and user enrollment scope.
- An HTTPS enrollment authority trusted by the laptop.
- An HTTPS Agent Gateway CONNECT origin trusted by the laptop.
- An approved device enrollment before the connector starts.

The enrollment URL, identity issuer, and Gateway origin are distinct trust boundaries even when one organization operates all three.

## Credential storage preflight

Managed identity is experimental. Validate credential persistence before login or installation:

```bash
cargo run -- identity storage-check
```

The default `auto` mode uses Linux Secret Service when a write/read/delete preflight succeeds and otherwise persists an owner-only protected-file backend. Require Secret Service with no fallback using:

```bash
cargo run -- identity storage-check \
  --credential-storage secret-service
```

Select the protected file explicitly with `--credential-storage file`. The selected backend is persisted and revalidated on later startup; runtime does not silently switch stores. Override the XDG-based identity directory with `AGENTDESKTOP_IDENTITY_DIR`.

## User login

Start browser Authorization Code login:

```bash
cargo run -- identity login \
  --issuer https://identity.example/ \
  --client-id agentdesktop \
  --audience https://gateway.example \
  --scope agentgateway.invoke \
  --gateway-origin https://gateway.example
```

The command validates credential storage before opening the system browser, listens on an ephemeral loopback callback, and verifies the access-token signature, issuer, audience, expiry, scope, and subject. It then persists the access and refresh tokens. The Gateway origin must not contain credentials, a path, query, or fragment.

Use `--no-open` to print the authorization URL for non-desktop testing. It does not remove the need for an interactive OAuth grant unless the issuer is a deterministic test fixture.

## Device enrollment

Generate a protected P-256 device key and submit its CSR:

```bash
cargo run -- identity enroll-request \
  --issuer https://identity.example/ \
  --enrollment-url https://enrollment.example/ \
  --gateway-origin https://gateway.example
```

The command prints a non-secret pending enrollment ID. The private key remains in protected Agent Desktop storage. An administrator must inspect and approve the pending request through the organization's enrollment workflow.

After approval, retrieve and persist the device ID and certificate chain:

```bash
cargo run -- identity enroll-status \
  --issuer https://identity.example/ \
  --enrollment-url https://enrollment.example/ \
  --gateway-origin https://gateway.example
```

Both commands load the issuer/Gateway-scoped OAuth session and refresh it when needed. Agent Desktop validates that the issued certificate matches its retained private key before replacing the protected enrollment record.

The complete enrollment sequence and code map are in [CONTRIBUTING.md](../../CONTRIBUTING.md#enrollment). The authority API and administrator operations are documented in [the control-plane guide](../../control-plane/README.md).

## Start managed forwarding

Start the connector with the same issuer and Gateway origin used for enrollment:

```bash
cargo run -- serve \
  --mode managed \
  --listen 127.0.0.1:8080 \
  --status-listen 127.0.0.1:8081 \
  --upstream https://gateway.example \
  --native-target native.agentdesktop.internal:4000 \
  --identity-issuer https://identity.example/ \
  --enrollment-url https://enrollment.example/
```

Equivalent environment variables are available:

```bash
export AGENTDESKTOP_MODE=managed
export AGENTDESKTOP_LISTEN=127.0.0.1:8080
export AGENTDESKTOP_STATUS_LISTEN=127.0.0.1:8081
export AGENTDESKTOP_UPSTREAM=https://gateway.example
export AGENTDESKTOP_NATIVE_TARGET=native.agentdesktop.internal:4000
export AGENTDESKTOP_IDENTITY_ISSUER=https://identity.example/
export AGENTDESKTOP_ENROLLMENT_URL=https://enrollment.example/
cargo run -- serve
```

The native target must match an internal bind in the remote Agent Gateway configuration; repository fixtures use port `4000`. The application and status listeners must be distinct loopback addresses. Startup fails if storage, the matching OAuth session, or the approved device certificate is unavailable. Agent Gateway derives organization, user, and device identity from the validated authority-issued SPIFFE URI rather than connector-supplied headers.

Forwarding defaults to a five-second tunnel-establishment timeout, a ten-second graceful-shutdown deadline, and 128 concurrent tunnels. Override these with `--connect-timeout-ms`, `--shutdown-timeout-ms`, and `--max-in-flight`. The connector does not replay failed application requests and never falls back directly to the provider.

## Automatic application routing

After enrollment approval, Agent Desktop automatically reconciles the supported Claude Code native path. It writes the connector loopback endpoint and a placeholder credential while preserving unrelated settings. Employees do not choose a routing destination; Agent Gateway validates or removes the placeholder and supplies the real provider credential.

For headless development without the desktop host, apply the same connector-assisted path explicitly:

```bash
cargo run -- connect-agents
```

Remove only Agent Desktop-owned Claude Code routing values without deleting other Claude settings:

```bash
cargo run -- disconnect-agents
```

The intended managed model is organization-owned agent, MCP, and skill allowlists. The current build does not yet distribute or locally enforce those endpoint allowlists, so discovery must not be treated as authorization. Managed process-scoped capture is not implemented.

## Certificate lifecycle

The connector checks certificate lifetime every 15 minutes and renews within six hours of expiry. Normal renewal requires both the current valid client certificate and the OAuth user session. It rotates to a retry-stable replacement P-256 key, validates and persists the returned certificate, and ensures the next pooled Gateway connection uses the new identity generation.

If the certificate has expired, the connector uses bounded recovery instead of normal mTLS renewal. Recovery requires OAuth plus a signature from the previously enrolled private key over a five-minute challenge and is available for seven days after expiry.

Renewal or recovery failure retains the last persisted identity and retries after one minute. An expired identity cannot forward traffic. Detailed renewal and recovery sequences are in [CONTRIBUTING.md](../../CONTRIBUTING.md#certificate-renewal).

## Status and telemetry

Check connector and Gateway TCP reachability:

```bash
curl --fail http://127.0.0.1:8081/_agentdesktop/healthz
```

Read privacy-safe operational status:

```bash
curl --fail http://127.0.0.1:8081/_agentdesktop/status
```

The status API does not expose Gateway addresses, identity claims, credentials, application traffic, or policy. Health proves TCP reachability, not policy, provider credentials, or provider availability.

Set `OTEL_EXPORTER_OTLP_ENDPOINT` to an HTTP(S) OTLP/gRPC collector endpoint to export connector lifecycle traces. Agent Desktop does not inspect tunneled HTTP, so request-level telemetry remains an Agent Gateway responsibility.

## Logout and revocation

Remove only the matching local OAuth session and enrollment record:

```bash
cargo run -- identity logout \
  --issuer https://identity.example/ \
  --gateway-origin https://gateway.example
```

Local logout does not call an issuer token-revocation endpoint and does not revoke the authority-side device. Device revocation is an administrator operation against the enrollment authority.

Revocation currently prevents renewal and records the certificate revocation time. Until revocation state is published and consumed by Agent Gateway, an already-issued certificate remains usable until its short lifetime expires.

## Walkthroughs

- End-to-end VM server and laptop setup: [Managed Server and Client Walkthrough](managed-vm-walkthrough.md)
- Fast, zero-input managed E2E: `scripts/managed-e2e.sh`
- API-by-API host walkthrough: [Managed Native Walkthrough](../../examples/managed-walkthrough/README.md)
- Interactive Fedora user and administrator journey: [QEMU Desktop Test Environment](../../tests/vm/README.md)
- Managed installer packaging: [Managed Installer Development](managed-installer.md)

The default fixture-based walkthroughs use local identity and provider services and do not contact Anthropic. The manual walkthrough also has an explicit `start-anthropic` variant for real-provider validation.

Linux has the complete interactive managed walkthrough. Windows session transport, external signing, WFP source attribution, and standalone native forwarding are implemented and tested as separate boundaries, but there is not yet one reproducible managed Windows installation walkthrough. macOS runtime validation remains pending.
