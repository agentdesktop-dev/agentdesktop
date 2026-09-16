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
> for another AI developer tool or harness using the [contributor guide below](#adding-an-agent-or-harness).

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

## Adding an agent or harness

Discovery and managed configuration are separate capabilities. Start with
discovery; add configuration, gateway authentication, sandbox policy, or
telemetry only where the harness exposes a supported native mechanism.
Verify paths and configuration formats against a released harness version on
each supported OS, and document any limitations in the supported-tools table.

### 1. Implement and register discovery

Add a module under [crates/agent/src/discovery](crates/agent/src/discovery)
with the entry point `pub(super) fn discover(context: &ScanContext) -> Option<Agent>`.
Use [crates/agent/src/discovery/codex.rs](crates/agent/src/discovery/codex.rs)
for a native configuration example or
[crates/agent/src/discovery/opencode.rs](crates/agent/src/discovery/opencode.rs)
for installation-only discovery.

- Choose a stable `Agent.kind`, such as `claude-code`. Return `None` when no
  installation is found; an unknown version should remain `None`, not hide
  an installed harness. Missing or malformed configuration should not hide
  the installation or valid inventory from other sources.
- Use `ScanContext` for homes, PATH, working-directory ancestors, and captured
  environment overrides. Add any new override names to `ScanContext::capture`
  in [crates/agent/src/discovery/context.rs](crates/agent/src/discovery/context.rs).
  Route fixed system paths through `context.system_path` so fixture tests can
  exclude them. Keep user, project, managed, and override scope rules native
  to the harness; do not read process-global environment inside the adapter.
- Use `context.find_executable` for PATH-first lookup. If names can collide
  with unrelated tools, inspect candidates from `context.executable_candidates`
  and continue past rejected installations. Never execute discovered binaries
  to obtain versions; use installation or package metadata instead.
- Declare the module and append its entry point to `HARNESSES` in
  [crates/agent/src/discovery/mod.rs](crates/agent/src/discovery/mod.rs).
  Keep registry order stable. No new trait, core enum, or wire type is needed
  for ordinary harness discovery.

### 2. Reuse the inventory helpers

| Module | Responsibility |
| --- | --- |
| [crates/agent/src/discovery/files.rs](crates/agent/src/discovery/files.rs) | Best-effort JSON, JSON5, and TOML reads. Add another format here when needed; keep native field interpretation in the adapter. |
| [crates/agent/src/discovery/mcp.rs](crates/agent/src/discovery/mcp.rs) | MCP projection and endpoint disclosure. Use `server` after native parsing, or supply the adapter's enablement predicate to `from_json_map` or `from_mcp_servers_file` for compatible JSON layouts. |
| [crates/agent/src/discovery/metadata.rs](crates/agent/src/discovery/metadata.rs) | Version metadata and shared skill traversal. Pass the harness's native skill roots to `discover_skills`. |
| [crates/core/src/model.rs](crates/core/src/model.rs) | Shared `Agent`, `McpServer`, and `Skill` output types. |

Never collect MCP arguments, environment values, headers, credentials, or skill
bodies. The MCP helper exposes only HTTP(S) origins, omitting paths, user
information, query strings, and fragments. Socket, variable-based, and invalid
endpoints are undisclosed, but their native registrations remain in inventory.
Never resolve endpoint variables to discover credentials. Review other copied
strings and skill front matter for unintended sensitive data. Preserve each MCP
server's `source`: same-named servers from different files are distinct inventory
entries, not an effective merged configuration. Leave unimplemented capability
lists empty rather than guessing support.

### 3. Add fixtures and UI metadata

Extend [crates/agent/src/discovery/tests.rs](crates/agent/src/discovery/tests.rs)
to exercise the new adapter through the registry. Use `ScanContext::isolated`
and temporary files, not real user configurations or process-global environment
mutation. Keep version probes and skill traversal inside the fixture tree.

Cover present and missing installations, unknown versions, native paths and
overrides, malformed files, enablement, source ordering, and duplicate names.
Serialize the result and assert that sentinel secrets and skill bodies do not
appear. Run platform-specific tests on the relevant OS.

Add the canonical ID, aliases, display name, and icon to `toolPresentations` in
[frontend/ui/src/tools.tsx](frontend/ui/src/tools.tsx). `friendlyTool` and
`ToolIcon` share this catalog; unknown IDs retain their name and use the generic
icon. Keep configuration support and authorization separate from presentation
metadata. Update the desktop and controller inventory fixtures/stories, the
desktop empty-state tool list, and the supported-tools table above.

### 4. Add managed configuration separately, if supported

- Add the typed program configuration to `ProgramsConfig`, update `is_empty`,
  and validate gateway/model requirements and unsupported sandbox combinations
  in [crates/core/src/config.rs](crates/core/src/config.rs).
- Add a native reconciler and wire it through
  [crates/agent/src/reconcile/mod.rs](crates/agent/src/reconcile/mod.rs) and
  [crates/agent/src/daemon.rs](crates/agent/src/daemon.rs), including user/system
  paths and `validate_one_shot`. Preserve unrelated settings, ownership, and
  permissions; test idempotence, conflicts, removal, and non-mutating dry runs.
  For files entirely owned through a header, reuse `HeaderOwnedFile` from
  [crates/agent/src/reconcile/managed_file.rs](crates/agent/src/reconcile/managed_file.rs),
  as Codex and OpenCode do. Pass unmarked body bytes; the helper prepends its
  ownership header. Keep native rendering, final newlines, and dependency-safe
  multi-file ordering in the adapter. Remove references before deleting their
  targets; sidecar ownership and user-settings merge/rollback are separate.
- For authenticated gateway access, use the harness's renewable credential
  mechanism with `agentdesktop credential --client-id <kind>`. Include the ID
  in controller-JWT `allowedClientIds`; do not persist short-lived tokens in
  generated configuration. Do not invent system-managed paths the harness
  does not actually read.
- Add the configuration key to `AgentKind` in
  [frontend/controller/src/types.ts](frontend/controller/src/types.ts) and
  update `configurableAgents` and sandbox restrictions in
  [frontend/controller/src/views/ConfigurationView.tsx](frontend/controller/src/views/ConfigurationView.tsx).
  Keep authorization defaults separate in
  [frontend/controller/src/configuration.ts](frontend/controller/src/configuration.ts).
  Configuration keys such as `claudeCode` may differ from inventory/client IDs
  such as `claude-code`. Preserve untouched document fields and imported
  authentication, use the shared YAML codec, and add round-trip and interaction
  tests. Malformed additional settings must block copying rather than produce
  invalid configuration.
- Run `cargo xtask schema` after changing core configuration types; it requires
  `jq` and regenerates the checked-in schemas and schema documentation.

Keep discovery read-only and independent of reconciliation. Model-runtime
discovery, such as Ollama, remains separate from the harness registry.

### 5. Validate the integration

From the repository root, run `cargo test -p agentdesktop-agent discovery::`
for focused discovery coverage, then `cargo test --workspace`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check`,
and `git diff --check`. For UI changes, also run `pnpm -C frontend check`.
For a fresh checkout, install frontend dependencies and run
`pnpm -C frontend build` before the workspace Rust checks; the controller embeds
the built frontend. Fixture tests do not replace a live smoke test with the
supported harness version and isolated configuration.

## Project and community

Agentdesktop is fully open source under the [Apache License 2.0](LICENSE).

- Read the [documentation](https://agentdesktop.dev/docs/).
- Browse or report [issues](https://github.com/agentdesktop-dev/agentdesktop/issues).
- Help us [add support for another AI developer tool](#adding-an-agent-or-harness).
- Review the [Code of Conduct](CODE_OF_CONDUCT.md) before contributing.
- See the [production guide](https://agentdesktop.dev/docs/operations/production/)
  for Kubernetes, certificates, MDM, and endpoint enrollment.

<!-- markdownlint-disable-file first-line-heading no-inline-html -->
