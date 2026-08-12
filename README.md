# Agent Desktop

Agent Desktop is an open-source workstation application for managing employee AI agents. It is designed to discover local agents and resources, apply organization policy, enforce controls, and report operational activity while employees continue using tools such as Claude Code normally.

The current development milestone implements device enrollment, managed identity, inference routing through [Agent Gateway](https://github.com/agentgateway/agentgateway), revocation bookkeeping, privacy-safe flow statistics, and bounded macOS agent/MCP/skill discovery for managed devices. Cross-platform discovery, centralized endpoint policy distribution, audit/warn modes, sandbox controls, budgets, and detailed agent activity remain target capabilities.

Source: [github.com/agentdesktop-dev/agentdesktop](https://github.com/agentdesktop-dev/agentdesktop)

> Agent Desktop is under active development. Standalone Linux and the native managed path are usable development milestones, not a signed production release. See [Phase Status](docs/development/phase-status.md) for verified behavior and known gaps.

## Choose a deployment model

Agent Desktop supports two deployment models. They use the same thin connector, but differ in where Agent Gateway runs and who owns it.

| Model | Target audience | Where Agent Gateway runs | Who owns configuration and identity |
| --- | --- | --- | --- |
| **Self-managed local** (`standalone` in the CLI) | Individual developers, independent AI users, and OSS users protecting agents on their own machine | On the user's laptop, as a separate process beside Agent Desktop | The user owns Gateway policy, provider credentials, trust, and data retention. No organization, OAuth login, enrollment service, MDM, or remote control plane is required. |
| **Remote managed** (`managed` in the CLI) | Companies, security teams, and IT administrators managing employee laptops | Remotely in the organization's network | The organization owns Gateway policy and provider credentials. The employee signs in with organizational OAuth, an administrator approves the device, and a short-lived mTLS certificate identifies the organization, user, and device. MDM may install and configure the connector. |

Use **self-managed local** when one person owns both the laptop and its AI policy. Use **remote managed** when an organization needs centrally administered policy and cryptographically verified employee and device identity across a fleet. See [Deployment Modes](docs/architecture/deployment-modes.md) for the implemented feature comparison and one-app convergence plan.

## Product direction

| Area | Target experience | Current implementation |
| --- | --- | --- |
| Discovery | Inventory agents, models, providers, MCP servers, tools, and relevant configuration on every endpoint. | Remote managed macOS reports Claude Code, Claude Desktop, Codex CLI, OpenClaw, VS Code Copilot, configured MCP names/transports, skills, and plugins from fixed user-level roots. Claude Desktop MCP Extensions are included. Project, model/provider, Linux, and Windows discovery are not implemented. |
| Policy | Scope agent, inference, MCP/tool, sandbox, usage, and observability policy by organization, group, user, device, project, or resource. | Administration stores one organization-wide Allow/Deny desired policy for supported agents. Endpoint enforcement is not implemented; Agent Gateway remains the inference-policy boundary. |
| Enforcement | Reconcile policy locally through application configuration, routing, capture/filtering, or sandboxing. | Native inference routing is implemented. Standalone Linux capture is a narrow platform-specific path. |
| Observability | Report inventory, policy decisions, usage, and agent activity locally and centrally. | Enrollment/device state, opaque flow counts, Gateway health, and request outcomes are available. Prompts and responses are not collected. |
| Operation | Run quietly in the background across Linux, macOS, and Windows, normally deployed through MDM. | Tauri development UI works on macOS; production packaging and cross-platform enforcement remain incomplete. |

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
| **Native** | A gateway-aware application, currently Claude Code, is configured to use the connector's loopback listener. | Validated on Linux. The Windows 11 VM validates standalone native forwarding plus the session and WFP boundaries separately; a complete managed Windows walkthrough remains pending. This is the preferred path. |
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
- Privilege-separated per-user sessions: user agents retain OAuth and private keys while a machine service owns listeners and routes by OS-derived identity.
- Windows named-pipe session authentication and native WFP flow attribution using the initiating token SID, with no PID or TCP-table fallback.
- Deterministic managed E2E coverage and an interactive Fedora VM user/admin walkthrough.
- Privacy-safe structured logs and opt-in OTLP lifecycle traces.

The current milestone is the cross-platform desktop path. The Windows native vertical slice is validated in a disposable Windows 11 VM; production driver packaging/signing, process-scoped launch gating, and UDP denial remain. macOS capture still requires Apple hardware, signing, and Network Extension lifecycle validation. Managed revocation enforcement also remains a release blocker: revocation blocks renewal today, but an already-issued certificate is not rejected before expiry until revocation state is published to and consumed by Agent Gateway.

## Choose your next step

- **Self-managed user:** Read [Standalone Operations](docs/deployment/standalone.md), then connect Claude Code or explore Linux capture.
- **Enterprise user or administrator:** Read [Managed Remote Operations](docs/deployment/managed.md) or run the [Managed Walkthrough](examples/managed-walkthrough/README.md).
- **Contributor:** Read [CONTRIBUTING.md](CONTRIBUTING.md) for architecture diagrams, lifecycle sequences, repository layout, tests, and walkthroughs.
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

Run the zero-input managed enrollment, approval, mTLS forwarding, and revocation-bookkeeping journey with Podman 5+, Rust, OpenSSL, `curl`, and `jq`:

```bash
scripts/managed-e2e.sh
```

These paths use deterministic local fixtures and do not contact Anthropic. See [CONTRIBUTING.md](CONTRIBUTING.md#walkthroughs) for prerequisites, expected behavior, and the interactive Fedora journey.

## Desktop UI quickstarts

The Tauri 2 desktop application lives in [`ui/`](ui/). Both quickstarts require Node.js 20 or newer, Rust, and the [Tauri platform prerequisites](https://v2.tauri.app/start/prerequisites/). Install its dependencies once:

```bash
cd ui
npm install
```

### Standalone local quickstart

Install `agentgateway` on `PATH`, or set `AGENTDESKTOP_GATEWAY_BINARY` to its path, then run from `ui/`:

```bash
npm run dev:desktop
```

This starts the connector, Vite, the native host, and an Agent Desktop-owned local Gateway. In the UI, add the Anthropic API key and connect Claude Code. The key is stored in the platform credential store and passed only to the Gateway child process.

To use a separately started local Gateway instead:

```bash
AGENTDESKTOP_GATEWAY_MODE=external \
AGENTDESKTOP_UPSTREAM=http://127.0.0.1:4100 \
npm run dev:desktop
```

The external Gateway must expose an HTTP/2 CONNECT listener on `4100` and an internal native route on `4000`, as shown in `ui/config/agentgateway-anthropic.yaml`. Configure provider credentials in that independently managed Gateway.

### Remote managed quickstart

This path deploys the production-shaped server stack on a Linux VM and connects Agent Desktop from a separate laptop. It uses real Keycloak OAuth, PostgreSQL enrollment/device records, authority-issued certificates, Agent Gateway, and real Anthropic. It does not use a mock provider or preloaded enrollment/device records.

It remains a development deployment because it uses generated private CAs, seeded Keycloak users, a file-backed enrollment CA, and nonstandard ports. The VM needs Docker Engine with Compose, Git, OpenSSL, `curl`, and `jq`. The laptop needs Node.js 20+, Rust, and the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/). Create a public DNS record such as `agentdesktop.example.com` for the VM and allow inbound TCP `8444`, `8090`, and `8443`.

On the VM, clone this repository and prepare the server hostname:

```bash
git clone https://github.com/agentdesktop-dev/agentdesktop.git
cd agentdesktop/examples/managed-vm
./prepare.sh agentdesktop.example.com
```

Edit `.env`, keep `PUBLIC_HOST` identical to the name passed to `prepare.sh`, and replace every secret placeholder with a real value:

```dotenv
PUBLIC_HOST=agentdesktop.example.com
ANTHROPIC_API_KEY=sk-ant-...
KEYCLOAK_ADMIN_PASSWORD=choose-a-long-random-value
```

Start and verify the server stack:

```bash
docker compose config --quiet
docker compose up -d --build
./verify.sh
docker compose ps
```

`verify.sh` must print `Managed VM stack is healthy.` The VM now holds real Keycloak, enrollment, device, and certificate state in Docker volumes. The Anthropic key remains in the Gateway container environment and is never sent to Agent Desktop or Claude Code.

Copy only the public bootstrap and server CA from the VM to the laptop through a trusted channel:

```bash
install -d -m 0700 "$HOME/.config/agentdesktop-vm-example"
scp VM_USER@agentdesktop.example.com:~/agentdesktop/examples/managed-vm/runtime/organization.json \
    "$HOME/.config/agentdesktop-vm-example/organization.json"
scp VM_USER@agentdesktop.example.com:~/agentdesktop/examples/managed-vm/runtime/certs/server-ca.crt \
    "$HOME/.config/agentdesktop-vm-example/server-ca.crt"
chmod 0600 "$HOME/.config/agentdesktop-vm-example/organization.json"
```

Do not copy any `.key` file. Trust `server-ca.crt` in the laptop browser or operating-system trust store for OAuth and administration. On macOS, import it into the login keychain and set it to **Always Trust**.

On the laptop, clone the repository, point Agent Desktop at the copied public files, and start the client:

```bash
git clone https://github.com/agentdesktop-dev/agentdesktop.git
cd agentdesktop
export SSL_CERT_FILE="$HOME/.config/agentdesktop-vm-example/server-ca.crt"
export AGENTDESKTOP_ORGANIZATION_CONFIG="$HOME/.config/agentdesktop-vm-example/organization.json"
export AGENTDESKTOP_IDENTITY_DIR="$HOME/.config/agentdesktop-vm-example/identity"
export AGENTDESKTOP_CREDENTIAL_STORAGE=file
npm --prefix ui install
npm --prefix ui run dev:desktop
```

In Agent Desktop, sign in as `employee` / `employee-change-me`. Open `https://agentdesktop.example.com:8090/admin/`, sign in as `administrator` / `administrator-change-me`, and approve the pending user and machine. These are seeded development accounts in the real Keycloak server; the resulting OAuth session, enrollment, device, and certificate records are persistent stack data.

After approval, wait for Agent Desktop to apply the supported Claude Code route automatically, then send a prompt. The request uses the laptop's short-lived mTLS device identity, crosses the remote Gateway, and receives a real Anthropic response. The raw compatibility route preserves current Claude Code payloads without parsing content.

Administration shows the real device, discovery inventory, enrollment, force-rescan, and agent-policy data. For cleanup, troubleshooting, trust boundaries, and production replacements, follow the [Managed Server and Client Walkthrough](docs/deployment/managed-vm-walkthrough.md).

For a non-mock evaluation on one development laptop, use the walkthrough's [laptop-local variant](docs/deployment/managed-vm-walkthrough.md#laptop-local-variant), which uses this same stack with `agentdesktop.localhost` and provides `reset-local.sh` for complete cleanup.

Build and test the UI from `ui/`:

```bash
npm run build
cargo test --manifest-path src-tauri/Cargo.toml
npm run dist
```

Native bundles are written to `ui/src-tauri/target/release/bundle/`. See the [UI guide](ui/README.md) for frontend-only browser development and platform-specific installer commands, or browse the [organized examples](examples/README.md).

## Documentation

- [Contributor Guide](CONTRIBUTING.md): architecture, flows, code map, development setup, and walkthroughs.
- [Standalone Operations](docs/deployment/standalone.md): local installation, ownership, credentials, lifecycle, logs, and removal.
- [Managed Remote Operations](docs/deployment/managed.md): login, enrollment, certificate lifecycle, runtime, and logout.
- [Deployment Modes](docs/architecture/deployment-modes.md): self-managed local versus remote managed features, gaps, and one-app convergence plan.
- [Managed Server and Client Walkthrough](docs/deployment/managed-vm-walkthrough.md): deploy the VM stack, enroll clients, approve devices, and verify per-client statistics.
- [Managed Installer Development](docs/deployment/managed-installer.md): organization bootstrap and packaging.
- [Linux Transparent Capture](docs/deployment/linux-capture.md): systemd scopes, cgroup v2, nftables, trust, and testing.
- [Control Plane](control-plane/README.md): enrollment API, administrator operations, PostgreSQL, and CA configuration.
- [Enrollment Administration UI](admin-ui/README.md): server-hosted React console for enrollment review and device administration.
- [Platform Compatibility](docs/compatibility/platforms.md): tested and unavailable platform behavior.
- [Phase Status](docs/development/phase-status.md): verified progress, active blockers, and the next milestone.
- [Architecture and Delivery Plan](AGENTS.md): project boundaries, design decisions, and phased roadmap.
