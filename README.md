<p align="center">
  <img src="images/agentdesktop.svg" width="96" alt="Agentdesktop logo">
</p>

# Agentdesktop

**The open-source control plane for AI developer tools.**

MDM can install applications and push files. Agentdesktop manages the agent
layer: which tools are running, what they can connect to, how they are
configured, who is using them, and what they are doing.

Agentdesktop brings discovery, policy, identity, gateway access, and telemetry
into one fully open-source system. Keep developers in Claude, Codex, OpenCode,
and VS Code while giving platform teams a fleet-wide management experience
built for AI agents - not retrofitted from device management scripts.

## What you can do

- See which AI agents are installed on each device, including their versions.
- Inventory MCP servers and skills without collecting MCP credentials, command
  arguments, environment variables, or skill bodies.
- Configure a shared inference gateway to send agent traffic through.
- Enroll devices through OIDC and associate them with the signed-in user.
- Issue short-lived controller-signed JWTs for an inference gateway such as
  Agentgateway.
- Collect selected events such as new sessions and tool use.

## Fleet management UI

Inspect device health, configuration state, recent agent activity, installed
tools, MCP servers, and skills from one local management console.

![Agentdesktop device details](images/device-details.png)

## Supported tools

| Tool | Discovery | Managed configuration | MCP and skills |
| --- | --- | --- | --- |
| Claude Code | Yes | Yes | MCP and skills |
| Claude Desktop | Yes | Yes | MCP |
| Codex | Yes | Yes | MCP and skills |
| OpenCode | Yes | Yes | MCP |
| VS Code | Yes | Not yet | — |

Agentdesktop targets Linux, macOS, and Windows.

## Architecture

The daemon runs on each managed device and maintains an outbound connection to
the controller. The controller distributes configuration and stores inventory
and telemetry. Developer tools continue to run locally and can request
short-lived credentials for the inference gateway through the daemon.

![Agentdesktop architecture](images/overview.svg)

## Try it locally

The included scenario starts Dex as the identity provider, Agentgateway as the
inference gateway, the Agentdesktop controller, and a local device daemon.

You need Rust, pnpm, Docker Compose, and OpenSSL.

1. Create a development signing key:

   ```console
   openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 \
     -out /tmp/agentdesktop-gateway-jwt.pem
   ```

2. Start Dex and Agentgateway:

   ```console
   docker compose -f examples/claude/compose.yaml up -d
   ```

3. Build the UI and start the controller:

   ```console
   make ui
   cargo run --bin agentdesktop-controller -- \
     --config examples/claude/controller.yaml
   ```

4. In another terminal, start the device daemon:

   ```console
   ./scripts/run-agentdesktop-root \
     --config examples/claude/agentdesktop.yaml
   ```

Open <http://127.0.0.1:8080> and enroll with:

```text
admin@example.com
password
```

The complete local setup is in [examples/claude](examples/claude/README.md).

## Configuration

The controller watches a daemon configuration file and distributes each valid
revision to connected devices. The management UI includes a configuration
wizard that produces YAML for you to review and place in that file; it does not
write to the controller filesystem.

A small configuration can manage a shared gateway, telemetry, Claude Code, and
Claude Desktop:

```yaml
inferenceGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway

telemetry:
  events:
  - session.new
  - tool.use

programs:
  claudeCode:
    permissions:
      defaultMode: plan
  claudeDesktop:
    isLocalDevMcpEnabled: true
```

## Enrollment and gateway identity

Devices enroll with an OIDC authorization-code flow using PKCE. After
enrollment, the daemon maintains an outbound connection to the controller for
inventory, configuration, heartbeat, and telemetry traffic.

When an agent requests a gateway credential, the controller issues a
short-lived JWT containing the verified OIDC identity, the device ID, and the
requesting client name. Agentgateway can trust the controller's JWKS and use
those claims for authentication and policy.

## Telemetry

Telemetry is disabled unless events are selected in configuration.

- `session.new` records a new agent session and its session identifier.
- `tool.use` records tool-use metadata.
- `tool.use.input` also includes the tool's input JSON.

## Project policy

Agentdesktop is available under the [Apache License 2.0](LICENSE). Please read
the [Code of Conduct](CODE_OF_CONDUCT.md) before participating.
