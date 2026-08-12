# Agent Desktop deployment modes

Status: current implementation comparison and convergence target.

## Terminology

Use these names in product UI and user documentation:

- **Self-managed local**: one person owns the laptop, local Agent Gateway, provider connections, policy, and retained data.
- **Remote managed**: an organization owns remote Agent Gateway, identity, device approval, provider connections, and policy.

The existing CLI and configuration values remain `standalone` and `managed`.

## One application, two installation profiles

Agent Desktop should remain one application and one shared connector implementation. The deployment mode is selected by trusted installation state, not by a user-facing runtime toggle:

- Without an organization bootstrap, Agent Desktop runs as **self-managed local**.
- With an installed, validated organization bootstrap, Agent Desktop runs as **remote managed** and the mode is locked.

An employee must not be able to switch a remote managed installation to self-managed local to bypass central routing. An unmanaged self-managed local installation may eventually support **Join organization**, but that must be a transactional conversion using a trusted invitation/bootstrap, not a settings toggle.

The two profiles can share the same app binary while packaging different payloads:

- Self-managed local includes or locates a separate local Agent Gateway and user-owned starter configuration.
- Remote managed includes the public organization bootstrap and points to a remote Gateway; it contains no Gateway policy or provider credentials.

## Ownership invariant

The connector is policy-free in both profiles. It owns application integration, opaque forwarding, identity presentation, status, and fail-closed behavior.

Agent Gateway always owns:

- Provider credentials and provider connections.
- Model and agent routing.
- Authorization, rate limits, guardrails, and content inspection.
- Request-level telemetry, audit, and retained AI data.

In remote managed deployments, the enrollment service owns membership workflows, invitations, device enrollment, certificate lifecycle, and revocation state. It may store assignments to centrally defined Gateway access packages, but it must not define or evaluate an Agent Desktop policy language.

## Current feature comparison

`Implemented` means code and focused tests or a working walkthrough exist. `Partial` means development behavior exists but packaging, platform validation, or a security property remains incomplete.

| Capability | Self-managed local (`standalone`) | Remote managed (`managed`) |
| --- | --- | --- |
| Shared connector | Implemented: same Rust connector and opaque HTTP/2 CONNECT path | Implemented: same connector with mTLS transport |
| Desktop UI | Implemented for development: local setup, explicit Claude connection, health, flow counters, details | Implemented for development: organization sign-in, enrollment status, certificate health, automatic supported-agent routing, flow counters |
| Mode selection | Implemented through absence of organization bootstrap plus standalone service config | Implemented through strict organization bootstrap plus managed service config |
| Gateway location | Separate process on the same laptop | Separate remote organization-owned service |
| Gateway lifecycle | Implemented: connector may supervise a local Gateway | Correctly unavailable: managed mode rejects local Gateway binary/configuration |
| AI policy | User-owned Agent Gateway-native configuration; Agent Desktop does not interpret it | Organization-owned Agent Gateway-native configuration; never delivered to employee Agent Desktop |
| Provider credentials | Standalone UI development flow stores a user key in the platform credential store and passes it only to local Gateway | Correctly unavailable on the client; UI and backend reject provider-key input |
| Organization bootstrap | Not required | Implemented strict public schema: organization, IdP, enrollment URL, Gateway URL, optional CA trust only |
| Organizational sign-in | Not required | Implemented OAuth Authorization Code with PKCE for enrollment/control-plane operations |
| Device identity | Local process/user boundary only | Implemented authority-approved P-256 key and short-lived mTLS certificate binding organization, user, and device |
| Device approval | Not required | Implemented pending/issuing/approved/rejected workflow and central admin UI |
| Certificate renewal/recovery | Not required | Implemented proactive renewal, persisted identity reload, and bounded expired-certificate recovery |
| Certificate revocation persistence | Not applicable | Implemented in enrollment authority |
| Immediate revocation enforcement | Not applicable | Missing: Gateway consumption of published revocation state is the current milestone |
| Claude Code native routing | Implemented through connector loopback URL and placeholder credential | Implemented through the same local adapter and remote authenticated Gateway; reconciled automatically after device approval |
| Native traffic semantics | Implemented opaque byte forwarding, streaming, half-close, cancellation, and fail closed | Implemented with the same semantics over certificate-authenticated HBONE |
| Transparent capture | Implemented for standalone Linux `claude` profile | Missing: managed capture and identity propagation are not complete |
| TLS inspection | Agent Gateway-owned; Linux trust install/remove implemented for standalone capture | Gateway-owned remotely; managed trust may be distributed by MDM/bootstrap, but managed capture is missing |
| Local activity stats | Implemented opaque flow counters in Agent Desktop | Implemented opaque flow counters in Agent Desktop |
| Request/model stats | Agent Gateway responsibility; not a connector metric | Agent Gateway responsibility; not exposed through the enrollment administration service |
| Agent/application discovery | Missing | Implemented for managed macOS while the Tauri host runs; latest report is authenticated with the current device certificate and rendered per device |
| Model/provider inventory | Missing | Partial: Gateway traffic exists, but endpoint model/provider inventory is not reported |
| MCP server and tool inventory | Missing | Partial: managed macOS reports configured MCP names/transports plus skill and plugin names from fixed roots; reachability and effective enablement are not asserted |
| Endpoint policy distribution | Missing | Missing: Agent Gateway inference policy remains the only active policy source |
| Agent allow/deny desired policy | Not applicable | Implemented as one organization policy for five known agents; endpoint enforcement is not available |
| Audit/warn/enforce modes | Missing | Partial: inference routing is enforced; no general endpoint policy modes exist |
| Local sandbox/filesystem policy | Missing outside the narrow Linux capture boundary | Missing |
| Fleet administration | Not applicable | Implemented enrollment queue, device names/owners, certificate state, discovery rescans, desired agent policy, and revocation action |
| Organization creation | Not applicable | Missing: organization is currently supplied to the server through startup environment/configuration |
| Invitations and members | Not applicable | Missing: users currently appear only after IdP login and enrollment request |
| Teams and access-package assignments | Local Gateway policy is directly user-owned | Missing: no team/membership model or trusted Gateway assignment integration exists |
| Admin RBAC | Not applicable | Partial: one admin OAuth scope and optional realm role; no Owner/Access Admin/Device Admin/Auditor split |
| Installer | Implemented development Linux bundle with local Gateway, starter config, service, helper, and integrity manifest | Implemented development organization-specific installer with connector and bootstrap, without local Gateway/policy |
| Signed public distribution | Missing | Missing |
| Updates, staged rollout, minimum version | Missing | Missing |
| Linux validation | Native and transparent-capture walkthroughs implemented | Native managed walkthrough implemented; managed capture missing |
| macOS/Windows | Tauri development UI works on macOS; production connector/package validation and capture are incomplete | Tauri development UI works on macOS; production connector/package validation and capture are incomplete |

## Current packaging split

The code is already mostly unified, but release construction is not:

- `scripts/build-embedded-installer.sh` builds the self-managed local payload with Agent Gateway and starter Gateway configuration.
- `scripts/build-managed-installer.sh` builds the remote managed payload with connector and organization bootstrap only.
- `ui/` is one Tauri/React application that projects either self-managed local or remote managed setup based on trusted bootstrap/runtime state.

This is a reasonable security boundary. "One app" does not require one identical archive: it requires one product, shared source, shared UX shell, and explicit installation profiles.

## Target application bootstrap

The native host should return one explicit deployment descriptor instead of requiring React to infer mode from several status responses:

```text
DeploymentProfile
  SelfManagedLocal
    gatewayOwnership: local | external
    canManageProviderCredential: boolean
    transparentCaptureAvailable: boolean

  RemoteManaged
    organizationName
    supportUrl
    enrollmentUrl
    gatewayUrl
    canLeaveOrganization: false when managed by installation/MDM
```

The UI then renders shared navigation and mode-specific setup steps from capability flags. Security-sensitive mode validation remains in Rust and the installed service.

## First-run journeys

### Self-managed local

1. Open Agent Desktop.
2. Start or connect a local Agent Gateway.
3. Configure provider access in Gateway.
4. Connect Claude Code.
5. Optionally install inspection trust and enable Linux capture.

### Remote managed

1. Install from an organization-specific package, MDM deployment, or future invitation link.
2. Open Agent Desktop and see the organization identity; no mode choice is offered.
3. Sign in through organization SSO.
4. Request device access and wait for approval or trusted invitation handling.
5. Connect Claude Code after enrollment is active.
6. Route through remote Gateway with no provider secret or policy on the laptop.

## Self-managed local to remote managed conversion

This is not implemented. A future **Join organization** flow should:

1. Validate a signed invitation and organization bootstrap.
2. Stop local forwarding and reject conversion while transparent capture is active.
3. Preserve but deactivate user-owned local Gateway data; never upload it.
4. Install the organization bootstrap and trust material transactionally.
5. Reconfigure the service for managed mode.
6. Complete SSO and device enrollment.
7. Resume application routing only after managed readiness succeeds.

Remote managed installations controlled by MDM must not expose **Leave organization**. BYOD removal, if supported later, needs an administrator-defined offboarding policy and explicit cleanup of organization credentials/trust.

## Work required for a fully unified product

1. Add an explicit `DeploymentProfile` to the native UI bootstrap and remove mode inference in React.
2. Package the Tauri UI with both installation profiles and connect it to installed-service lifecycle APIs.
3. Build a first-run self-managed local setup and an invitation-driven remote managed setup in the same shell.
4. Implement central organization creation, invitations, members, and admin RBAC.
5. Integrate the admin console with Gateway-native provider/model/agent resources without creating an enrollment-specific policy dialect.
6. Implement trusted team/access-package resolution at Gateway enforcement time.
7. Complete fail-closed revocation publication and Gateway consumption.
8. Add signed installers, update delivery, rollback, staged rollout, and minimum-version enforcement.
9. Validate native forwarding and packaging on Linux, macOS, and Windows; implement capture per platform separately.

## Acceptance criteria for "one app"

- One product name, desktop shell, connector core, and application-adapter behavior.
- Product terminology is self-managed local and remote managed; CLI compatibility remains `standalone` and `managed`.
- Trusted installation state determines mode before setup begins.
- Remote managed users cannot activate local Gateway/policy/provider controls.
- Self-managed local users are not required to configure OAuth, enrollment, or a control plane.
- Both profiles use the same local application endpoint and fail-closed forwarding semantics.
- Policy and provider credentials remain exclusively in Agent Gateway in both profiles.
- Mode-specific packages differ only where ownership and security require different payloads.
