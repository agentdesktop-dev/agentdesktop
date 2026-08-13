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

To run a simple example setup, follow the [Claude Code example](./examples/claude) which walks through
managed Claude Code on managed devices and redirecting traffic through an Agentgateway instance.

## Configuration

The controller watches a configuration file and distributes configuration to connected devices.

A small configuration can manage a shared gateway, telemetry, and agents. For example:

```yaml
inferenceGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [claude-code, claude-desktop, codex, opencode]

telemetry:
  events:
  - session.new
  - tool.use

programs:
  claudeCode:
    permissions:
      defaultMode: plan
    companyAnnouncements: ["Managed by Agentdesktop"]
  claudeDesktop:
    isLocalDevMcpEnabled: true
```

## Enrollment and identity

Devices are enrolled through a dual-authentication scheme.
A private key is bound to a device and never leaves that device.
The public key is used to authenticate the device to the controller.

Additionally, an OIDC flow is used to authenticate the *user* of the device with an IDP-bound identity.


## Telemetry

Sensitive events on from agents on devices, such as tool usages, session creation, etc can be reported
back to the controll (opt-in).

## Project policy

Agentdesktop is available under the [Apache License 2.0](LICENSE). Please read
the [Code of Conduct](CODE_OF_CONDUCT.md) before participating.
