# Agent Desktop

Agent Desktop connects AI applications on a laptop to [Agent Gateway](https://github.com/agentgateway/agentgateway). It gives individuals and organizations one place to route AI traffic through Gateway policy without putting policy, provider credentials, or content inspection in a desktop agent.

Source: [github.com/agentdesktop-dev/agentdesktop](https://github.com/agentdesktop-dev/agentdesktop)

> Agent Desktop is under active development. Standalone Linux and the native managed path are usable development milestones, not a signed production release. See [Phase Status](docs/development/phase-status.md) for verified behavior and known gaps.

## Choose a deployment model

Agent Desktop supports two deployment models. They use the same thin connector, but differ in where Agent Gateway runs and who owns it.

| Model | Target audience | Where Agent Gateway runs | Who owns configuration and identity |
| --- | --- | --- | --- |
| **Self-managed local** (`standalone` in the CLI) | Individual developers, independent AI users, and OSS users protecting agents on their own machine | On the user's laptop, as a separate process beside Agent Desktop | The user owns Gateway policy, provider credentials, trust, and data retention. No organization, OAuth login, enrollment service, MDM, or remote control plane is required. |
| **Remote managed** (`managed` in the CLI) | Companies, security teams, and IT administrators managing employee laptops | Remotely in the organization's network | The organization owns Gateway policy and provider credentials. The employee signs in with organizational OAuth, an administrator approves the device, and a short-lived mTLS certificate identifies the organization, user, and device. MDM may install and configure the connector. |

Use **self-managed local** when one person owns both the laptop and its AI policy. Use **remote managed** when an organization needs centrally administered policy and cryptographically verified employee and device identity across a fleet.

## Why this project exists

AI tools do not all connect to models in the same way. Some support a custom gateway URL; others connect directly to provider endpoints. Organizations also need user and device identity, while an individual running locally should not need an enterprise control plane.

Agent Desktop provides the laptop-side integration layer. It:

- Gives gateway-aware applications a stable loopback endpoint.
- Can capture selected Linux application processes that cannot be configured with a gateway.
- Preserves application streams as opaque bytes and forwards them over HTTP/2 CONNECT.
- Proves organizational user and device identity in managed deployments.
- Fails closed instead of bypassing Agent Gateway when the secure route is unavailable.
- Emits operational telemetry without collecting prompts or responses.

Agent Desktop intentionally does **not** evaluate AI policy, inspect content, store provider credentials, or perform TLS interception. Those responsibilities remain in Agent Gateway.

## How the pieces fit

```text
AI application -> Agent Desktop -> Agent Gateway -> AI provider
                      |                 |
             routing and identity   policy, inspection,
                                    credentials, routing
```

Agent Desktop and Agent Gateway remain separate processes in every mode. The connector is designed to stay small, deterministic, and policy-free.

## Traffic paths

Traffic path answers **how an application reaches Agent Desktop**. It is independent of deployment mode.

| Path | How it works | Current support |
| --- | --- | --- |
| **Native** | A gateway-aware application, currently Claude Code, is configured to use the connector's loopback listener. | Standalone Linux and managed remote mode. This is the preferred path. |
| **Captured** | Agent Desktop launches an application in an owned process scope and redirects its TCP/443 traffic without changing application settings. | Standalone Linux only. Managed capture, macOS, and Windows are not implemented. |

An application must use only one path at a time. Configuring both native routing and capture can create duplicate routing or loops.

## Current capabilities

The development build includes:

- Opaque, streaming HTTP/2 CONNECT forwarding with bounded concurrency and fail-closed behavior.
- A separately supervised local Agent Gateway for standalone use.
- Persistent Claude Code configuration through `connect-agents`.
- Standalone Linux process-scoped capture using systemd scopes, cgroup v2, and nftables.
- Browser-authenticated managed enrollment backed by Go and PostgreSQL.
- Authority-issued short-lived mTLS identity, automatic renewal, and bounded expired-certificate recovery.
- Deterministic managed E2E coverage and an interactive Fedora VM user/admin walkthrough.
- Privacy-safe structured logs and opt-in OTLP lifecycle traces.

The current milestone is managed revocation enforcement. Revocation blocks renewal today, but an already-issued certificate is not rejected before expiry until revocation state is published to and consumed by Agent Gateway.

## Choose your next step

- **Self-managed user:** Read [Standalone Operations](docs/deployment/standalone.md), then connect Claude Code or explore Linux capture.
- **Enterprise user or administrator:** Read [Managed Remote Operations](docs/deployment/managed.md) or run the [Managed Walkthrough](examples/managed-walkthrough/README.md).
- **Contributor:** Read [CONTRIBUTE.md](CONTRIBUTE.md) for architecture diagrams, lifecycle sequences, repository layout, tests, and walkthroughs.
- **Platform or security engineer:** Read the [Managed mTLS Contract](docs/architecture/managed-mtls-v1.md), [HBONE Contract](docs/architecture/hbone-connect-v1.md), and [Linux Transparent Capture](docs/deployment/linux-capture.md).

## Try it

Run the Rust unit and integration tests:

```bash
cargo test
```

Run a local standalone connector plus Agent Gateway smoke environment with Podman 5+ or Docker:

```bash
./scripts/container-up.sh smoke
./scripts/container-smoke.sh
./scripts/container-down.sh
```

Run the zero-input managed enrollment, approval, mTLS forwarding, and revocation-bookkeeping journey with Podman 5+, `curl`, and `jq`:

```bash
scripts/managed-e2e.sh
```

These paths use deterministic local fixtures and do not contact Anthropic. See [CONTRIBUTE.md](CONTRIBUTE.md#walkthroughs) for prerequisites, expected behavior, and the interactive Fedora journey.

## Documentation

- [Contributor Guide](CONTRIBUTE.md): architecture, flows, code map, development setup, and walkthroughs.
- [Standalone Operations](docs/deployment/standalone.md): local installation, ownership, credentials, lifecycle, logs, and removal.
- [Managed Remote Operations](docs/deployment/managed.md): login, enrollment, certificate lifecycle, runtime, and logout.
- [Managed Installer Development](docs/deployment/managed-installer.md): organization bootstrap and packaging.
- [Linux Transparent Capture](docs/deployment/linux-capture.md): systemd scopes, cgroup v2, nftables, trust, and testing.
- [Control Plane](control-plane/README.md): enrollment API, administrator operations, PostgreSQL, and CA configuration.
- [Platform Compatibility](docs/compatibility/platforms.md): tested and unavailable platform behavior.
- [Phase Status](docs/development/phase-status.md): verified progress, active blockers, and the next milestone.
- [Architecture and Delivery Plan](AGENTS.md): project boundaries, design decisions, and phased roadmap.
