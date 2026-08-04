# Agent Desktop

## Purpose

This repository contains an open source edge connector for routing AI application traffic from laptops to [Agent Gateway](https://github.com/agentgateway/agentgateway). In managed mode, Agent Gateway runs remotely in the organization's network. In self-managed mode, Agent Gateway runs on the user's device alongside the connector.

The edge connector is intentionally thin. It integrates gateway-aware applications and redirects traffic from selected applications to Agent Gateway. Agent Gateway is always the sole policy enforcement and content-inspection component. Organizations can reuse centrally managed policy on edge devices, while individual OSS users can run Agent Gateway locally and configure their own policies without a control plane.

## Goals

- Provide first-class support for AI tools that support a custom gateway or base URL, beginning with Claude Code in gateway mode.
- Support a self-managed local mode in which Agent Gateway runs on the edge, reads user-owned local policy configuration without a control plane, and receives traffic redirected from selected applications by the connector.
- Support managed laptops through MDM while retaining a viable enrollment flow for unmanaged devices.
- Associate traffic with a cryptographically verified organizational user and device.
- Route application traffic to Agent Gateway without modifying AI request or response bodies on the device.
- Fail closed when Agent Gateway or required identity services are unavailable.
- Add process-scoped transparent capture early, so the initial release supports applications without native gateway configuration.
- Support Linux, macOS, and Windows.
- Emit OpenTelemetry traces, metrics, and structured logs without collecting prompts or responses at the edge.

## Non-goals

- Reimplementing or embedding the Agent Gateway policy engine inside the connector.
- Creating a second AI policy model or policy control plane.
- Performing TLS MITM or content inspection in the edge connector.
- Storing provider credentials in the connector. A self-managed local Agent Gateway may hold credentials according to its own configuration and security model.
- Depending on DNS rewriting or DNS interception for routing.
- Supporting transparent QUIC interception in the first release.
- Providing policy simulation in this repository; that belongs in Agent Gateway.

## Current milestone: Self-managed standalone mode

The Claude forwarding pre-pre-MVP established the shared forwarding core. The current milestone makes self-managed local operation an explicit standalone product mode before adding managed identity or transparent capture.

Include only:

- An explicit standalone deployment mode whose Agent Gateway upstream is local-only.
- A separate local Agent Gateway process using user-owned configuration and policy.
- Native Claude Code configuration for direct or connector-assisted local use.
- Local lifecycle and health reporting needed for a usable standalone workflow.
- Deterministic tests and a real-Agent-Gateway smoke path for supported behavior.

Do not include OAuth, device identity, MDM integration, transparent capture, HBONE, a control plane, or OpenTelemetry export in this milestone. Agent Gateway remains a separate process and owns provider credentials, policy, and any future TLS inspection. Do not create placeholder abstractions for later phases.

## Architecture

### Deployment modes

#### Managed remote mode

- Agent Gateway runs as an independently deployed service in the organization's network, not on the user's device.
- The connector establishes authenticated remote connectivity and supplies verified user and device context.
- MDM may install and configure the connector, distribute trust roots, and enforce capture settings.
- Agent Gateway policy, provider credentials, TLS interception, and audit data remain centrally managed.
- This is the primary enterprise deployment.

#### Self-managed local mode

- Agent Gateway runs as a separate process on the same device and listens on loopback or another local-only transport.
- The user owns Agent Gateway's local configuration, policies, provider credentials, and data retention choices.
- No MDM, organizational identity, enrollment service, connector-management service, or control plane is required.
- The connector redirects traffic from selected applications to the local Agent Gateway and may provide application adapters, lifecycle integration, and local health reporting.
- Gateway-aware applications may connect directly to the local Agent Gateway. The connector must not capture an application that is already configured for a direct path, which would risk loops or duplicate routing.
- Agent Gateway always evaluates policy and performs content inspection in every deployment mode. Do not duplicate its configuration schema or policy engine in the connector.
- Keep connector and Agent Gateway processes separate. Do not embed Agent Gateway as a library or fork its policy implementation.

The standard self-managed installation may include both Agent Gateway and the connector. Users can mix two paths per application: **native**, where a gateway-aware application connects directly to Agent Gateway, and **captured**, where the connector redirects a selected application without modifying it.

### Components

1. **AI application** connects directly to Agent Gateway through native gateway configuration or is redirected by a connector-owned OS integration.
2. **Edge connector** selects applications, redirects their traffic, preserves the original destination, and forwards it to the local or remote Agent Gateway. In managed remote mode it also identifies the local user and proves device identity.
3. **Agent Gateway** evaluates policy, optionally performs TLS MITM, supplies provider credentials, and routes to the AI provider. In managed remote mode it also authenticates user and device context.
4. **MDM**, managed remote mode only, installs the connector, enrolls the device, distributes trust roots, and configures supported applications or capture rules.
5. **Identity provider**, managed remote mode only, authenticates the user and issues credentials accepted by Agent Gateway or a connector enrollment service.

### Ownership boundary

The edge connector owns:

- Per-user loopback listeners and application adapters.
- Application execution scopes and their process-tree lifecycle when Agent Desktop launches a selected application.
- OS-specific traffic capture.
- User login, token refresh, device enrollment, and device proof in managed remote mode.
- Forwarding and tunnel lifecycle, including authenticated remote connections where required.
- Fail-closed behavior and actionable local errors.
- Connector health and operational telemetry.

Agent Gateway owns:

- Authentication and authorization policy.
- AI routing, rate limits, guardrails, and provider credentials.
- TLS MITM and issuing CA keys. These are centrally managed in remote mode and user-managed by the local Agent Gateway deployment in local mode.
- Request-level policy telemetry and audit records.
- Unknown-endpoint behavior and policy simulation.

In managed remote mode, MDM owns deployment configuration, not runtime AI policy. It may configure gateway addresses, trust roots, enrollment information, application selectors, and enforcement mode.

In self-managed local mode, the user owns Agent Gateway configuration and policy directly. The connector owns application selection and redirection configuration, but must not interpret or rewrite Agent Gateway policy.

## Identity design

The identity design in this section applies to managed remote mode. Self-managed local mode does not require organizational user or device identity by default because the user and Agent Gateway share the same device and administrative boundary. Local process isolation and access to the loopback listener still require explicit security review.

The proposed wire-level user/device trust boundary is specified in [Managed Identity Contract v1](docs/architecture/managed-identity-v1.md). The contract records required Agent Gateway work and must be agreed before OAuth implementation begins.

### User identity

MDM enrollment identity and local usernames are not sufficiently portable or trustworthy as the identity of the current user. Use browser-based OAuth 2.0 Authorization Code with PKCE for the normal laptop flow. Use Device Authorization Flow only as a fallback for headless devices or environments where browser callbacks cannot work.

- Perform login once and silently refresh short-lived credentials afterward.
- Use the system browser to benefit from existing enterprise SSO sessions.
- Prefer the platform credential store. On Linux, an explicitly configured protected-file fallback is allowed; strict mode requires Secret Service and fails during setup when it is unavailable.
- Run the application-facing endpoint in the user session so identity is not ambiguous on shared machines.

### Device identity

- Bind short-lived user tokens to a connector-held DPoP key so proof survives ordinary TLS-terminating gateways.
- Treat the initial DPoP key as connector-instance proof, not verified organizational device identity.
- MDM-managed and unmanaged devices need enrollment that associates an approved device with the DPoP key or a platform-backed replacement key before managed mode is public.
- Agent Gateway must derive device identity from verified cryptographic material. It must not trust a connector-supplied device ID header by itself.
- Include both stable user and device identifiers in the verified policy context so Agent Gateway can authorize or revoke either one.
- A revoked device or user returns a stable `403` response and a machine-readable reason. The connector translates this into an actionable application-facing error.

Different users must not share an identity-bearing tunnel. A privileged system service may own OS capture, but forwarding pools and credentials must remain isolated by user session.

## Traffic modes

### Mode 1: Native gateway integration

This is the preferred path. Its first increment is the pre-pre-MVP.

- In managed mode, start with Claude Code configured to send Anthropic API traffic to a per-user connector loopback endpoint.
- In self-managed local mode, Claude Code may connect directly to Agent Gateway. Use the connector only when it provides required application integration.
- The pre-pre-MVP forwards normal Anthropic-shaped HTTP traffic to Agent Gateway without parsing it or performing a second login flow.
- Preserve methods, paths, query strings, end-to-end headers, status codes, and streaming bodies. Handle hop-by-hop headers according to HTTP proxy semantics.
- Claude may use a deployment-specific placeholder or gateway credential during this milestone. User OAuth, connector-issued local credentials, provider-credential removal, and device proof are later increments.
- In managed mode, once identity is implemented, the connector authenticates upstream with short-lived user credentials and device proof. Agent Gateway validates identity, strips connector-only credentials, applies its normal LLM policies, and supplies the provider credential.
- Preserve request and response bodies byte-for-byte except for protocol changes required to proxy HTTP. Do not parse or transform AI content at the edge.

Additional gateway-aware tools should be implemented as named, tested adapters. An adapter may manage base URL, local placeholder credential, CA trust settings, and application reload behavior.

### Mode 2: Process-scoped transparent capture

Implement this immediately after the Claude pre-MVP and include it in the initial release.

- Select traffic by strong application identity rather than executable name alone where possible.
- Support both local and managed destinations with the same capture semantics: local Agent Gateway over a loopback or local-only transport, and remote Agent Gateway over an authenticated tunnel.
- Preserve the original destination and send the original TLS stream unchanged to Agent Gateway.
- Prefer Agent Gateway's existing HTTP/2 CONNECT/HBONE support rather than designing a custom destination-header protocol. Validate and harden the laptop-client authentication boundary before committing to the wire contract.
- In managed mode, authenticate each outer CONNECT with a DPoP-bound user token and proof. Agent Gateway must turn validated credentials into immutable trusted tunnel context before evaluating policy on inspected inner requests.
- In self-managed local mode, secure connector-to-gateway communication with loopback or local IPC access controls. Do not require organizational OAuth or device enrollment.
- Use one CONNECT stream per captured TCP flow while pooling underlying HTTP/2 connections per user identity.
- Agent Gateway performs TLS MITM according to policy, remotely in managed mode or on-device in local mode. The connector never possesses the issuing CA private key.
- Block selected applications' UDP/443 traffic initially so HTTP/3-capable clients fall back to TLS over TCP. Do not silently allow QUIC to bypass capture.

Preferred process selectors:

- **macOS:** Network Extension source audit token and code-signing identity.
- **Windows:** Windows Filtering Platform application identity, package SID, and publisher/path metadata.
- **Linux:** cgroup or systemd scope, with eBPF connect hooks or routing integration where necessary.

Executable names and paths are useful rollout selectors but are not security identities. Define whether capture follows child and helper processes for every application profile; a managed cgroup, job, or session is preferable to repeated PID attribution.

`agentdesktop launch [--profile NAME] -- COMMAND [ARGS...]` is the stable application-launch boundary. Its first Linux implementation creates an owned transient systemd user scope, starts a gated child, and validates the exact cgroup v2 path before release; it is not yet a sandbox or a supported capture path. The capture controller must activate trust, relay, and network rules at that pre-release boundary. Later concrete requirements may add stronger execution backends such as a Linux namespace sandbox or a VM, but profiles must request explicit guarantees, unavailable backends must fail closed rather than downgrade, and the repository must not add speculative sandbox abstractions before implementing one.

## TLS and trust

- TLS interception occurs only at Agent Gateway.
- In managed remote mode, MDM distributes the Agent Gateway root CA to the platform trust store.
- In self-managed local mode, provide one-click trust setup when inspection is enabled. Before changing system or application trust, explain which CA will be installed, why it is required, and which selected application traffic it enables Agent Gateway to inspect; require explicit confirmation and the normal platform privilege prompt.
- Trust installation and removal must be idempotent and reversible. Uninstall or an explicit removal action must remove only trust material installed by this offering.
- Application adapters must handle tools that use private trust stores, bundled roots, or explicit CA environment variables.
- Certificate-pinned applications require an Agent Gateway MITM exemption or are unsupported for inspected traffic.
- Maintain a tested compatibility matrix covering gateway configuration, trust behavior, HTTP version, QUIC behavior, and helper processes.

## Failure and enforcement

The initial failure policy is fail closed.

- If Agent Gateway is unavailable, authentication expires, enrollment is revoked, or the secure route cannot be established, do not connect directly to the provider.
- Return stable local errors that distinguish connectivity, authentication, authorization, revocation, and TLS failures.
- Native gateway configuration provides routed behavior, but does not prevent a user from changing application settings.
- Transparent capture must deny the original direct connection after redirecting it.
- Strong anti-bypass guarantees may additionally require MDM firewall or egress rules. Document whether a deployment is **routed** or **enforced**.

In local mode, applications connected directly to Agent Gateway fail when it is unavailable. For captured applications, the connector denies the original connection and never bypasses directly to the provider. Smarter fail-open or offline policy may be added later, but the connector must not grow an independent copy of the Agent Gateway policy engine.

## Telemetry

Use OpenTelemetry for traces, metrics, and structured logs. Agent Gateway already supports W3C `traceparent` propagation and OTLP export, so connector and gateway spans should form one distributed trace.

- Create a client/root span when the application does not provide trace context.
- Propagate `traceparent` and `tracestate` to Agent Gateway.
- Emit metrics for authentication failures, token refresh, gateway latency, active/rejected tunnels, capture errors, fail-closed events, CA/TLS failures, bytes forwarded, and connector/config versions.
- Use stable resource attributes for connector version, OS, tenant, and a pseudonymous device identifier.
- Use sampling and bounded local buffering.
- Telemetry export must never block traffic forwarding or change failure policy.
- Never record prompts, responses, request bodies, authorization headers, provider API keys, full URLs/query strings, or arbitrary process command lines.
- Process identity and destination host are potentially sensitive. Keep them out of default traces or use a documented pseudonymization strategy; allow explicit, time-limited diagnostic collection.

## Control plane

Do not create a new policy control plane.

Self-managed local mode has no control plane. Agent Gateway reads user-owned local configuration, and the connector reads only its own endpoint, capture, and application-adapter configuration.

Managed remote mode uses:

- MDM for installation, bootstrap configuration, trust roots, and capture/application configuration.
- Agent Gateway for runtime AI policy and request enforcement.
- A minimal enrollment and connector-management API only when needed for device registration, credential issuance, revocation, gateway discovery, minimum-version enforcement, or fleet health.

If a separate connector-management service becomes necessary for managed remote deployments, implement it in Go. Keep its API narrow and avoid duplicating Agent Gateway policy. Introduce it only after a concrete requirement cannot be met cleanly by Agent Gateway, the IdP, or MDM. Local mode must not depend on this service.

## Implementation language and repository shape

Implement the edge component in Rust. Prioritize memory safety, predictable resource use, static distribution, and reuse of the Rust HTTP/HBONE ecosystem. Keep platform-specific capture behind narrow interfaces while sharing identity, forwarding, configuration, telemetry, and failure semantics.

If required, implement the optional connector-management service in Go. Define versioned protocol contracts independently of language-specific types.

Start with one Rust package and organize functionality as modules:

```text
Cargo.toml
src/
  main.rs               connector entry point and lifecycle
  config.rs             bootstrap and runtime configuration
  error.rs              stable internal and application-facing errors
  launch.rs             application execution-scope lifecycle
  identity/             OAuth, enrollment, token storage, and device proof
  proxy/                loopback HTTP and Agent Gateway transport
  telemetry/            OpenTelemetry setup and semantic conventions
  apps/
    claude.rs            Claude Code configuration and compatibility
  platform/
    linux/               Linux capture and platform integration
    macos/               macOS capture and platform integration
    windows/             Windows capture and platform integration
  bin/                   additional same-package binaries, only when required
control-plane/          optional Go service; create only when justified
docs/
  architecture/         protocols, threat model, and identity design
  deployment/           MDM and unmanaged-device enrollment guides
  compatibility/        tested application and platform matrix
```

Use conditional compilation for platform implementations and keep shared behavior in ordinary modules. If privilege separation requires multiple executables, add binary targets to the same package first.

Extract a separate crate only when a concrete boundary requires it, such as:

- A privileged capture service must expose a minimal API and must not link identity or proxy code.
- A platform extension must be built, signed, or released as an independent artifact.
- Versioned protocol types are consumed by independently released binaries.
- Platform dependencies conflict or make supported cross-compilation impractical.
- Measured build times or feature combinations become materially problematic.

Do not create empty abstraction crates in anticipation of these boundaries.

## Delivery plan

Each increment must be independently usable and tested. Add the narrowest behavior first, add tests that fail without it, and keep all earlier tests as regression coverage. Fixes for escaped defects require a regression test. Do not combine identity, capture, telemetry, and deployment work in one increment.

Verified progress, active blockers, and deferred user-journey findings are maintained in [Phase Status](docs/development/phase-status.md).

### Phase 0: Claude forwarding foundation

1. Create one Rust package and binary with minimal dependencies.
2. Parse and validate the loopback listen address and Agent Gateway upstream URL.
3. Forward Claude HTTP requests to Agent Gateway and stream responses back without buffering AI content.
4. Preserve HTTP behavior, including methods, paths, query strings, end-to-end headers, status codes, error responses, and cancellation.
5. Fail closed with a stable local gateway error when Agent Gateway cannot be reached. Never fall back to Anthropic directly.
6. Add unit tests for configuration and URI/header handling.
7. Add integration tests with a local fake Agent Gateway covering request fidelity, response fidelity, streaming, upstream failure, cancellation, and graceful shutdown.
8. Add a manual smoke-test recipe for Claude Code against a real Agent Gateway.

### Phase 1: Self-managed standalone Agent Gateway

1. Add explicit **standalone** and **managed** deployment modes with mode-specific configuration validation.
2. Keep local Agent Gateway and the connector as separate processes using user-owned Agent Gateway configuration and policy.
3. Configure native applications, beginning with Claude Code, to connect directly to local Agent Gateway or use the connector when integration requires it.
4. Add local lifecycle management and health reporting without embedding Agent Gateway or duplicating its configuration schema.
5. Add example Agent Gateway policy for securing personal agents without creating a connector-specific policy format.
6. Add end-to-end tests against a real local Agent Gateway for policy allow and deny, streaming, gateway restart, and fail-closed behavior.
7. Document provider credential storage, configuration file permissions, logs, and data-retention implications for a single-user machine.
8. Keep standalone mode fully functional without OAuth, MDM, enrollment, or a remote control-plane service.

### Phase 2: User and device identity for managed mode

1. Document trust boundaries and define the versioned user/device identity contract with Agent Gateway.
2. Add OAuth Authorization Code with PKCE, DPoP-bound access tokens, and secure token storage with an explicit protected-file fallback on Linux.
3. Add refresh rotation and restart restoration, then device enrollment that upgrades connector-instance proof to verified device identity. Both are managed-release requirements.
4. Make Agent Gateway construct policy context only from verified identity.
5. Add tests for login, refresh, expiry, logout, token/device binding, concurrent users, and user or device revocation.
6. Verify that identity credentials are stripped before provider forwarding and never appear in logs.

### Phase 3: Transparent capture and Agent Gateway TLS inspection

1. Specify and test the authenticated HBONE/CONNECT contract with Agent Gateway.
2. Implement one platform end to end first, including process selection, original destination, TCP forwarding, UDP/443 denial, and fail-closed behavior against both standalone and managed Agent Gateway deployments.
3. Preserve original TLS bytes in the connector; Agent Gateway alone performs policy-driven TLS MITM.
4. Add per-application profiles with explicit **native** and **captured** paths and prevent both paths from being enabled for one application.
5. Add informed CA trust installation and scoped removal for the first platform.
6. Define child/helper process semantics and publish initial routed/enforced guidance.

### Phase 4: Installation and basic UI

1. Provide an installer that packages Agent Gateway, the connector, starter standalone policy, and optional trust setup while keeping processes separate.
2. Add a basic local UI for standalone and managed modes covering deployment mode, gateway status, application profiles, identity status, and actionable failures.
3. Make installation, upgrades, rollback, trust changes, and uninstall idempotent and reversible.
4. Add Claude Code configuration helpers for standalone and managed modes plus initial MDM deployment examples.
5. Keep Agent Gateway policy editing and inspection controls in Agent Gateway rather than duplicating them in the connector UI.

### Phase 5: Cross-platform parity

1. Support standalone and managed native gateway mode on Linux, macOS, and Windows.
2. Add transparent capture implementations for Linux, macOS, and Windows behind one behavioral contract.
3. Implement platform-native process identity, service management, credential storage, and trust installation/removal.
4. Test installation, upgrades, rollback, native routing, captured routing, helper processes, QUIC denial, and fail-closed behavior on every platform.
5. Publish the compatibility matrix and platform-specific routed/enforced deployment guidance.

### Phase 6: Forwarder reliability

1. Add explicit connection, request, and shutdown timeouts, each with deterministic tests.
2. Add bounded concurrency and resource limits with overload tests.
3. Add retry behavior only if an Agent Gateway request can be proven replay-safe; test that streaming or non-idempotent requests are never duplicated.
4. Test long-lived streams, slow clients, slow upstreams, disconnects, malformed requests, and repeated startup/shutdown.
5. Establish measured latency, memory, and no-breakage baselines across deployment modes and platforms.

### Phase 7: OpenTelemetry and operational visibility

1. Add OTel traces, metrics, and structured logs without changing forwarding behavior.
2. Add trace-correlation tests with Agent Gateway and tests that sensitive values are never exported.
3. Cover identity, capture, trust, gateway health, and application-profile failures with stable operational signals.
4. Verify bounded buffering and that telemetry failure never blocks traffic or changes fail-closed behavior.

### Phase 8: Hardening and fleet operations

1. Add signed updates, rollback, staged rollout, and minimum-version enforcement.
2. Harden local IPC and privilege separation between user identity and privileged capture services.
3. Complete third-party security review and privacy review.
4. Add a Go connector-management service only if fleet requirements demonstrate the need.
5. Evaluate smarter outage behavior without moving AI policy to the edge.
6. Load-test connection pooling, identity isolation, long-lived streams, gateway loss, and token rotation.

## Pre-pre-MVP acceptance criteria

- Claude Code can target the loopback connector and complete a request through either a local or remote Agent Gateway.
- No connector OAuth, device enrollment, MDM, transparent capture, HBONE, or control-plane service is required.
- Requests and responses preserve the tested HTTP semantics and stream without full-body buffering.
- Agent Gateway failure returns a stable local error and never causes direct provider fallback.
- Automated tests cover configuration, forwarding fidelity, streaming, failure, cancellation, and shutdown.
- The test suite is deterministic and does not require Claude Code, Anthropic, or a remote Agent Gateway.
- A separate manual smoke test documents validation with real Claude Code and Agent Gateway.

## Self-managed local mode acceptance criteria

- An OSS user can run Agent Gateway and the connector on one machine without MDM, OAuth, enrollment, or a control plane.
- The user configures policy through Agent Gateway's supported local configuration, not through a connector-specific policy layer.
- Gateway-aware applications can use the native direct path, while selected applications without gateway support can use connector-managed capture.
- The connector prevents native and captured paths from being enabled simultaneously for the same application.
- Captured traffic preserves the original destination and TLS bytes until Agent Gateway performs policy-driven inspection.
- Stopping or restarting the local Agent Gateway never causes direct provider fallback for captured applications.
- Agent Gateway and connector listeners default to loopback and do not expose provider credentials or policy administration to other machines.
- Trust setup is one-click but informed: it explains the CA and inspection scope, requires explicit confirmation and platform authorization, and can be cleanly reversed.
- Local installation documentation explains provider credential storage, CA trust, logs, and data retention.
- Automated end-to-end coverage runs against a real Agent Gateway binary for both native and captured paths in addition to deterministic fake-upstream tests.

## Managed initial release acceptance criteria

- Claude Code can use Agent Gateway after one organizational browser login and without a provider credential on the laptop.
- Agent Gateway policy receives a verified user and device identity.
- Revoking either identity causes new traffic to fail closed with an actionable error.
- The connector does not inspect or persist AI request/response bodies.
- Connector and Agent Gateway spans correlate in an OTel backend.
- Transparent mode captures only configured application scopes, preserves original TLS bytes, prevents direct fallback, and routes through central TLS MITM.
- Identity and traffic from concurrent local users cannot cross sessions.

## Open decisions

- Final project, repository, package, service, and daemon names.
- Which repository owns the self-managed installer that packages Agent Gateway, the connector, trust setup, and starter policy.
- Whether the connector is enabled by default or activated when the user first selects an application for capture.
- How Agent Gateway versions are discovered and compatibility-checked in local mode.
- Initial transparent-capture platform and the exact Linux capture mechanism.
- The standard used to bind OAuth user tokens to device keys.
- Agent Gateway changes required to authenticate laptop-originated HTTP and HBONE traffic and expose only verified identity to CEL.
- MDM products and identity providers used for the first supported deployment recipes.
- Required no-breakage SLO and performance budgets.
- Retention, sampling, and pseudonymization defaults for edge telemetry.
- Whether anti-bypass against local administrators is an initial-release requirement.

## Engineering principles

- Keep the edge data path small, deterministic, and policy-free.
- Keep deployment modes explicit: local mode is self-managed and control-plane-free; remote mode may use organizational identity and fleet management.
- Reuse Agent Gateway's supported local configuration and policy APIs; never introduce a connector-owned policy dialect.
- Treat all connector-provided headers as untrusted until cryptographically authenticated.
- Prefer standard protocols: OAuth/OIDC, mTLS, HTTP, HBONE/CONNECT, W3C Trace Context, and OTLP.
- Keep provider credentials and TLS issuing keys off endpoints in managed remote mode. In self-managed local mode, keep them confined to Agent Gateway and the platform credential or file-permission boundary; never expose them to the connector or applications.
- Use least privilege and explicit privilege separation for OS capture.
- Optimize for compatibility and diagnosability before broad interception.
- Do not add a service, abstraction, or heuristic without a demonstrated requirement.