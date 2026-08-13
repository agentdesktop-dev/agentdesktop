# Agent Desktop UI

Tauri 2 desktop application for the macOS menu bar and Windows system tray. The app keeps running when its window is closed and restores the window from the tray menu.

The first operational slice includes:

- Live connector and Agent Gateway status, request activity, and failure counters.
- Claude Code installation and connection-state detection, with explicit local configuration in self-managed mode and automatic reconciliation after approval in managed mode.
- Standalone Anthropic API-key setup through the native system credential store.
- Managed organization sign-in, device enrollment, and certificate-health status.
- Dedicated managed Coverage tab for inference routing, agent inventory, MCP/skills, and local sandbox posture without claiming unavailable controls.
- Trusted system-browser links to organization support and the enrollment administration console.
- Runtime configuration, platform capability, and desktop preference diagnostics.
- Native tray status that updates while the window is hidden.

Connector status is read from `http://127.0.0.1:8081/_agentdesktop/status`. The desktop preference controls whether the window opens at startup; it does not replace the connector's service configuration.

## Development

Install Node.js 20 or newer, Rust, and the [Tauri platform prerequisites](https://v2.tauri.app/start/prerequisites/), then run:

```sh
npm install
npm run dev:desktop
```

`npm run dev:desktop` starts a UI-local Rust development backend, Vite, the native Tauri host, and an Agent Desktop-owned Agent Gateway at `http://127.0.0.1:4100`. It discovers `agentgateway` from `PATH`, uses `config/agentgateway-anthropic.yaml`, and deliberately removes inherited `ANTHROPIC_API_KEY` values so Connect is the provider setup path. Closing the window hides it; use the tray icon to reopen it or quit. Quitting the UI stops the development backend and owned Gateway. Re-running the launcher while the complete session is active exits successfully instead of starting duplicate processes.

The development backend lives under `src-tauri/src/bin/` and reuses the connector's public configuration, forwarding, telemetry, and local-gateway APIs. In remote managed development it waits for identity established by the Tauri enrollment flow before starting forwarding. It does not implement Linux transparent capture; use the installed service for that workflow.

The default development flow prompts for the Anthropic key when Connect is clicked:

```sh
npm run dev:desktop
```

On Connect, standalone mode asks for the key only when Agent Desktop owns the Gateway and no key is already available. The Tauri host stores it in the platform credential store. The development backend retrieves it and injects it only into the Agent Gateway child environment; it is not written to Claude settings, UI settings, diagnostics, or Gateway configuration. Updating the stored key restarts the owned Gateway. Managed organization mode never prompts for a provider key.

To use an independently started Gateway instead, opt out explicitly:

```sh
AGENTDESKTOP_GATEWAY_MODE=external \
AGENTDESKTOP_UPSTREAM=http://127.0.0.1:4100 \
npm run dev:desktop
```

The external Gateway must expose an HTTP/2 CONNECT listener on `4100` and the internal native route on `4000`, matching `config/agentgateway-anthropic.yaml`. It owns its provider credentials and lifecycle.

External Gateway mode does not prompt because Agent Desktop does not own that Gateway's credentials.

To launch only the Tauri UI, run `npm start`.

`npm run dev` is the Vite process used by `tauri dev`; it does not provide a standalone browser application. Runtime state comes only from the native Tauri host through IPC.

The native host discovers managed organization configuration from `AGENTDESKTOP_ORGANIZATION_CONFIG`, a bundled `organization.json` resource, or an installed `share/organization.json`. It honors `AGENTDESKTOP_IDENTITY_DIR` when reading the connector's managed credential store. Only organization and public enrollment metadata are returned to React; OAuth tokens, private keys, and certificate PEM never cross the command boundary.

The managed macOS desktop reports organization access, device identity, inference connectivity, Claude configuration, opaque local flow activity, and a bounded agent/MCP/skill inventory to the enrollment authority while the Tauri host runs. Claude Desktop MCP Extensions are included. Windows reports a smaller Claude Code-only inventory from fixed user/global npm, configuration, MCP, and skill locations; process state remains unknown. Employees do not choose a routing destination: the current supported Claude adapter is reconciled automatically after approval. The desktop polls for force-rescan requests every 30 seconds. Agent-policy enforcement, sandbox status, and detailed agent actions remain unavailable.

## Build

```sh
npm run build
cargo test --manifest-path src-tauri/Cargo.toml
npm run dist
```

Build release installers on their target operating systems:

```sh
npm run dist:mac
npm run dist:win
```

Tauri writes native bundles under `src-tauri/target/release/bundle/`. Production distribution still requires Apple and Windows code-signing configuration.

Regenerate application icons after changing the icon generator:

```sh
npm run icons
```

## Architecture

The native/React ownership split is documented in [ARCHITECTURE.md](ARCHITECTURE.md).
