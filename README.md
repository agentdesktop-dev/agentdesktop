<picture>
  <source media="(prefers-color-scheme: dark)" srcset="images/logo-light.svg">
  <img src="images/logo.svg" alt="Agentdesktop" width="520">
</picture>

# Open-source visibility and control for AI tools across your desktop fleet

[![CI](https://github.com/agentdesktop-dev/agentdesktop/actions/workflows/ci.yml/badge.svg)](https://github.com/agentdesktop-dev/agentdesktop/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/agentdesktop-dev/agentdesktop?display_name=tag&sort=semver)](https://github.com/agentdesktop-dev/agentdesktop/releases/latest)
[![License](https://img.shields.io/github/license/agentdesktop-dev/agentdesktop)](LICENSE)
[![GitHub stars](https://img.shields.io/github/stars/agentdesktop-dev/agentdesktop?style=flat&logo=github)](https://github.com/agentdesktop-dev/agentdesktop)
[![Join Discord](https://img.shields.io/discord/1538954092486070444?style=flat&label=Join%20Discord&color=6D28D9)](https://discord.com/invite/uKX2FvCVpS)

Agentdesktop discovers AI developer tools, inventories MCP servers and skills,
applies tool-native configuration and sandbox policy, and connects each device
to an LLM gateway with user and device identity.

Keep developers in Claude Code, Codex, Cursor, OpenCode, VS Code, and Grok Build
while giving platform teams one place to understand and manage the fleet.

[Website](https://agentdesktop.dev) ·
[Documentation](https://agentdesktop.dev/docs/) ·
[Announcement](https://agentdesktop.dev/blog/2026/09/introducing-agentdesktop/) ·
[Releases](https://github.com/agentdesktop-dev/agentdesktop/releases)

## Why Agentdesktop?

AI agents increasingly run on employee workstations, but the controls around
them are fragmented across tool-specific settings, MCP connections, skills,
provider credentials, and local configuration files.

MDM remains the right layer for enrolling devices, deploying software, and
enforcing OS posture. Agentdesktop adds the AI-tool-aware layer above it.

| See what is running | Manage tools natively | Control model access |
| --- | --- | --- |
| Discover supported tools and versions, then inventory MCP servers, skills, and models without collecting their secrets or contents. | Define configuration and sandbox intent once. Agentdesktop translates it into each supported tool's native format and reports whether it was applied. | Route tools through your LLM gateway with short-lived credentials carrying user, device, and allowed client context. Provider API keys remain at the gateway. |

![How MDM, Agentdesktop, and an LLM gateway work together](images/layers.png)

## Start on one workstation

Try the same endpoint daemon used in a managed fleet without deploying a
controller.

### 1. Install Agentdesktop

Download the current device binary from
[GitHub Releases](https://github.com/agentdesktop-dev/agentdesktop/releases), or
build it from source:

```sh
git clone https://github.com/agentdesktop-dev/agentdesktop.git
cd agentdesktop
corepack enable
make install
```

### 2. Preview the standalone example

The repository includes a working standalone configuration for Claude Code,
OIDC, and Agentgateway. Preview every proposed file action without changing
tool configuration:

```sh
agentdesktop daemon \
  --config examples/standalone/config.yaml \
  --user \
  --dry-run
```

### 3. Run and inspect

Remove `--dry-run` to reconcile the configuration and leave the daemon
running:

```sh
export ANTHROPIC_API_KEY=sk-ant-...
docker compose -f examples/standalone/compose.yaml up -d

agentdesktop daemon \
  --config examples/standalone/config.yaml \
  --user
```

In another terminal, check the daemon and list discovered tools and models:

```sh
agentdesktop status
agentdesktop discover
```

The desktop and fleet interfaces also expose the MCP server and skill
inventory. See the [standalone quickstart](https://agentdesktop.dev/docs/getting-started/standalone/)
for prerequisites, test credentials, and a walkthrough of the local services.

## Local LLM proxy

For clients without an API-key helper, enable the daemon's optional loopback proxy
in the local configuration file (startup settings are local only, not delivered by
the controller). The proxy is available in `--user` mode only: it hands out the
current user's gateway credential, so a system daemon rejects
`daemon.llmProxy.listen`. If the address cannot be bound at startup, the daemon
keeps running without the proxy and reports `llmProxy.bound: false` with the
reason in daemon-info (the desktop app's "Daemon information" panel, or
`curl --unix-socket <socket> http://localhost/v1/daemon-info`). If the proxy
stops later, the daemon also keeps running; that is only visible in the daemon
log until the next restart:

```yaml
daemon:
  llmProxy:
    listen: 127.0.0.1:4000
```

Then start the daemon as usual:

```sh
agentdesktop daemon --user --config <your-config>.yaml
```

Point the client's model endpoint at `http://127.0.0.1:4000/v1/chat/completions`,
`/v1/messages`, or `/v1/responses`, according to the protocol it uses, and give it
the pairing header (see **Pairing** below). The proxy forwards request bodies
(buffered, see below) to `llmGateway.proxyUrl` (or `llmGateway.url` when unset),
streams the response back, and adds the credential from the same authentication
flow used by the daemon's API-key helpers. Login and refresh stay with the
existing authentication implementation. No client API key is needed. Incoming
authorization/API-key headers are replaced, and upstream HTTP errors and
redirects are passed back without following redirects.

The incoming path and query are appended to the gateway base URL: with
`llmGateway.proxyUrl: https://gateway.example/prefix` (or `url`, when `proxyUrl`
is unset), `/v1/messages` goes to `https://gateway.example/prefix/v1/messages`.
Avoid repeating `/v1` in both URLs.
The proxy does not discover models or rewrite model IDs; configure those in the
client. VS Code's built-in **Custom Endpoint** provider can use this endpoint
without the experimental extension in `vscode/`. The provider reads
`chatLanguageModels.json` in the VS Code user directory (on Linux
`~/.config/Code/User/`); keep the file owner-only, since it carries the pairing
value. Give it the pairing value in the model's `requestHeaders` (the file's
`apiKey` is not sent reliably without an OS keyring):

```json
[
  {
    "name": "agentdesktop",
    "vendor": "customendpoint",
    "apiKey": "unused",
    "apiType": "chat-completions",
    "models": [
      { "id": "gpt-4.1-mini", "name": "gpt-4.1-mini (agentdesktop)",
        "url": "http://127.0.0.1:4000/v1/chat/completions",
        "toolCalling": true, "vision": false,
        "maxInputTokens": 128000, "maxOutputTokens": 16000,
        "requestHeaders": { "x-agentdesktop-pairing": "<value>" } }
    ]
  }
]
```

Upgrade note: before the pairing existed, this route accepted any local caller.
A manual setup made against that version now gets `403 pairing_invalid` until
the header is added.

Controller JWT authentication uses client ID `vscode` by default on the
prefix-less route; choose another ID with `daemon.llmProxy.clientId`. Every client
ID the proxy presents (`vscode` or your choice, plus `copilot-cli` and
`vscode-copilot` for the routes below) must be in the controller's
`allowedClientIds`. With `authentication.type: oidc` the client ID has no effect: the
OIDC token is the same for every route. The listen address must be loopback.

**Routes.** Besides the prefix-less route (the manual shape used above, with the
credential mode from `llmGateway.githubOAuth` and the upstream from
`llmGateway.proxyUrl`), the proxy serves fixed path prefixes for the managed
Copilot programs, one per program route, so the client ID is decided by the file
the daemon writes and never by request headers. The prefix is stripped before
forwarding. A path whose first segment looks like a route name without matching
one exactly is refused with `404 route_unknown` rather than served as the
prefix-less route; this also applies to a gateway sub-path that happens to start
with a route name (for example `/vscode-copilot-proxy/...`), so do not use such
paths on the prefix-less route:

| Prefix | Client ID | Sends upstream | Upstream |
| --- | --- | --- | --- |
| `/copilot-cli/` | `copilot-cli` | gateway identity only | `llmGateway.url` |
| `/vscode-copilot/` | `vscode-copilot` | gateway identity only | `llmGateway.url` |
| `/vscode-copilot-passthrough/` | `vscode-copilot` | gateway identity in `Authorization`, the client's own bearer token in `x-llm-token` | `llmGateway.proxyUrl` (required) |

**Pairing.** Every route requires the per-device pairing value in the
`x-agentdesktop-pairing` header. The daemon creates it in its state directory
(`llm-proxy-pairing`, owner-only) on first use and keeps it across restarts; a
manual setup copies it from there. The state directory in `--user` mode is
`$XDG_STATE_HOME/agentdesktop`, by default `~/.local/state/agentdesktop` (the
home directory from `HOME` or `USERPROFILE`, on every platform), unless
`daemon.stateDir` says otherwise. The Copilot CLI's
`providers.json` carries it as `"headers": {"x-agentdesktop-pairing": "<value>"}`
on the provider entry. It keeps other local users on a shared host
and browser pages off the proxy, provided the client files that carry it are
owner-only too. It does not restrict the current user's own processes, which can
obtain a credential from the daemon directly. If the value cannot be created the
proxy stays off, reported in daemon-info like a failed bind.

**Requests.** `Host` must be a loopback address; `Origin`, `CONNECT` and
`OPTIONS` are refused; `..` path segments are refused. Request bodies are
buffered (32 MiB cap) so a request can be sent once more with a fresh
credential when the gateway rejects a cached one with 401 (only when no client
token was forwarded); responses stream as they arrive, with a 600 s limit on
the response headers and no total limit on the body; a non-streaming completion
sends nothing until it is done, so the header limit bounds it too.
Controller-issued credentials are cached in the daemon for at most 60 s per
client ID, and are not used within 30 s of their own expiry, nor after a logout
or re-enrollment. Concurrent requests that find no cached credential share one
controller fetch; its credential is cached as soon as it arrives and dropped
again if the gateway rejects it. The cache adds no exposure beyond the credential's own
lifetime: a token the controller has issued stays valid at the gateway until it
expires whether or not the daemon caches it, and a logout does not revoke it.
OIDC credentials are not cached. A controller that does not answer within 15 s
fails the request with `agentdesktop_credential`; an OAuth refresh in flight
ahead of the controller call is not cut off, so its result is saved. When a
client token is forwarded (pass-through, or `githubOAuth.source: request`) a
401 is returned as is and the cached gateway credential is kept. With the one
retry, the worst case for one request is the header limit (600 s), a credential
fetch (20 s) and a second header limit. Refused and failed requests
return an OpenAI-style JSON body, `{"error": {"message", "type", "code"}}`, so the
LLM clients display the message. Codes: `host_not_allowed`, `pairing_invalid`,
`browser_not_allowed`, `method_not_allowed`, `path_invalid`, `route_unknown`,
`client_credential_missing`, `body_too_large`, `body_invalid`,
`agentdesktop_credential` (no gateway credential for this device: not enrolled,
revoked, controller unreachable or slow), `agentdesktop_unavailable`,
`gateway_unreachable`, `gateway_timeout`.

To attach the local user's GitHub App OAuth token as well, add the App's client ID
to the gateway configuration (keep the existing `authentication` block):

```yaml
llmGateway:
  url: https://gateway.example
  authentication:
    type: oidc
    issuer: https://login.microsoftonline.com/YOUR_TENANT/v2.0
    clientId: YOUR_ENTRA_CLIENT_ID
  githubOAuth:
    clientId: YOUR_GITHUB_APP_CLIENT_ID
```

Enable **Device Flow** in the GitHub App settings. With the proxy enabled, the
daemon opens an Agentdesktop sign-in page with a copyable code and a link to
GitHub. GitHub opens in another tab; the Agentdesktop page automatically shows
success or failure when authorization finishes. The client ID is configurable; no App client secret is
needed. The App's permissions and the user's Copilot access must permit the
upstream requests. This flow currently targets GitHub.com.

The proxy sends `Authorization: Bearer <gateway identity token>` and
`x-llm-token: <GitHub access token>`. Incoming values for both are discarded on
this route (with `githubOAuth.source: request`, and on the pass-through route,
the client's own token is what goes into `x-llm-token`).
The GitHub access and refresh tokens are stored in the daemon's existing secret
store, separately for each App client ID. Expiring tokens refresh automatically,
including refresh-token rotation; expired/rejected refresh credentials trigger
device authorization again. Non-expiring App user tokens are also supported.
Request bodies remain untouched. The existing Claude credential-helper flow is
unchanged; the local proxy always uses the gateway identity in `Authorization`.

## Start locally, grow into a fleet

Agentdesktop uses the same daemon and tool-native configuration model at every
stage.

| Standalone | Controller-managed |
| --- | --- |
| Read policy from local YAML, discover and configure tools on one workstation, and authenticate the user directly to a compatible LLM gateway. No controller or device identity is required. | Centrally inventory a fleet, distribute versioned configuration, enroll users and devices, report reconciliation status, and issue short-lived gateway credentials. |
| [Run the standalone quickstart](https://agentdesktop.dev/docs/getting-started/standalone/) | [Run the managed quickstart](https://agentdesktop.dev/docs/getting-started/managed/) |

![Agentdesktop controller device inventory](images/controller-ui.png)

## Core capabilities

- **AI tool discovery:** detect supported developer tools and their versions
  across Linux, macOS, and Windows.
- **Secret-minimizing inventory:** report configured MCP servers and skills
  without collecting MCP command arguments, environment variables, HTTP
  headers, or skill bodies.
- **Tool-native configuration:** safely merge managed values into the formats
  expected by each tool while preserving unrelated user settings.
- **Shared sandbox policy:** translate filesystem and network restrictions into
  the native sandbox configuration supported by Claude Code and Codex.
- **User and device identity:** bind a locally generated device key and
  certificate to the user who enrolled the workstation through OIDC.
- **Runtime credentials:** give supported tools short-lived credentials instead
  of distributing long-lived provider keys to workstations.
- **Identity-aware gateway integration:** attach user, device, and allowed client
  context for gateway routing, policy, logging, and usage attribution.
- **Opt-in telemetry:** collect selected session and tool-use events when an
  organization enables them.

## Supported tools

| Tool | Discovery | Managed configuration | MCP and skills inventory | Sandbox policy |
| --- | --- | --- | --- | --- |
| Claude Code | Yes | Yes | MCP and skills | Yes |
| Claude Desktop | Yes | Yes | MCP | — |
| Codex | Yes | Yes | MCP and skills | Yes |
| Cursor | Yes | — | MCP and skills | — |
| OpenCode | Yes | Yes | MCP | — |
| VS Code | Yes | — | MCP and skills | — |
| Grok Build | Yes | System mode | MCP and skills | — |

> **Don't see your tool?** We're actively expanding this list and would love
> your help. [Open an integration request](https://github.com/agentdesktop-dev/agentdesktop/issues/new)
> to tell us what you use, or contribute discovery and configuration support
> for another AI developer tool or harness.

The project targets Linux, macOS, and Windows. Support varies where a tool or
operating system does not expose an equivalent native configuration surface.

## How it works

The daemon runs on each workstation and reconciles developer-tool
configuration. It can receive desired configuration from the controller or
read the same YAML directly in standalone mode. Managed tools continue to run
locally and can request credentials for an LLM gateway through the daemon.

![Agentdesktop controller, workstation daemon, and LLM gateway architecture](images/overview.png)

In controller-managed mode, the daemon generates its private device key on the
workstation and sends only a certificate signing request to the controller.
After enrollment, protected controller operations require the device
certificate and a valid token for the user who enrolled it.

When a supported tool requests gateway access, the controller can issue a
short-lived JWT containing the user, device ID, allowed client label, audience,
issuer, and expiry. The client label is asserted by the local helper; it is
useful for policy and attribution, but it is not cryptographic proof of the
calling executable.

Read the [announcement](https://agentdesktop.dev/blog/2026/09/introducing-agentdesktop/)
for a deeper walkthrough of standalone mode, enrollment, and short-lived tool
credentials.

## Project and community

Agentdesktop is fully open source under the [Apache License 2.0](LICENSE).

- Read the [documentation](https://agentdesktop.dev/docs/).
- Browse or report [issues](https://github.com/agentdesktop-dev/agentdesktop/issues).
- Help us [add support for another AI developer tool](https://github.com/agentdesktop-dev/agentdesktop/issues/new).
- Review the [Code of Conduct](CODE_OF_CONDUCT.md) before contributing.
- See the [production guide](https://agentdesktop.dev/docs/operations/production/)
  for Kubernetes, certificates, MDM, and endpoint enrollment.

<!-- markdownlint-disable-file first-line-heading no-inline-html -->
