# Agent Desktop UI

Tauri 2 desktop application for the macOS menu bar and Windows system tray. The app keeps running when its window is closed and restores the window from the tray menu.

The first operational slice includes:

- Live connector and Agent Gateway status, request activity, and failure counters.
- Claude Code installation, connection-state detection, and safe connector configuration.
- Standalone Anthropic API-key setup through the native system credential store.
- Managed organization sign-in, device enrollment, and certificate-health status.
- Trusted system-browser links to organization support and the enrollment administration console.
- Runtime configuration, platform capability, and desktop preference diagnostics.
- Native tray status that updates while the window is hidden.

Connector status is read from `http://127.0.0.1:8080/_agentdesktop/status`. The desktop preference controls whether the window opens at startup; it does not replace the connector's service configuration.

## Development

Install Node.js 20 or newer, Rust, and the [Tauri platform prerequisites](https://v2.tauri.app/start/prerequisites/), then run:

```sh
npm install
npm run dev:desktop
```

`npm run dev:desktop` starts a UI-local Rust development backend, Vite, the native Tauri host, and an Agent Desktop-owned Agent Gateway at `http://127.0.0.1:4100`. It discovers `agentgateway` from `PATH`, uses `config/agentgateway-anthropic.yaml`, and deliberately removes inherited `ANTHROPIC_API_KEY` values so Connect is the provider setup path. Closing the window hides it; use the tray icon to reopen it or quit. Quitting the UI stops the development backend and owned Gateway. Re-running the launcher while the complete session is active exits successfully instead of starting duplicate processes.

The development backend lives under `src-tauri/src/bin/` and reuses the connector's public configuration, forwarding, telemetry, and local-gateway APIs. It intentionally does not implement managed identity or Linux transparent capture; use the installed service for those workflows.

The default development flow prompts for the Anthropic key when Connect is clicked:

```sh
npm run dev:desktop
```

On Connect, standalone mode asks for the key only when Agent Desktop owns the Gateway and no key is already available. The Tauri host stores it in the platform credential store. The development backend retrieves it and injects it only into the Agent Gateway child environment; it is not written to Claude settings, UI settings, diagnostics, or Gateway configuration. Updating the stored key restarts the owned Gateway. Managed organization mode never prompts for a provider key.

To use an independently started Gateway instead, opt out explicitly:

```sh
AGENTDESKTOP_GATEWAY_MODE=external \
AGENTDESKTOP_UPSTREAM=http://127.0.0.1:4000 \
npm run dev:desktop
```

External Gateway mode does not prompt because Agent Desktop does not own that Gateway's credentials.

To launch only the Tauri UI, run `npm start`.

To run only the React frontend in a browser:

```sh
npm run dev
```

Browser mode uses preview data because connector status and filesystem integration are available only through the desktop host.
Append `?preview=managed` to the browser URL to inspect the managed organization and device states.

The native host discovers managed organization configuration from `AGENTDESKTOP_ORGANIZATION_CONFIG`, a bundled `organization.json` resource, or an installed `share/organization.json`. It honors `AGENTDESKTOP_IDENTITY_DIR` when reading the connector's managed credential store. Only organization and public enrollment metadata are returned to React; OAuth tokens, private keys, and certificate PEM never cross the command boundary.

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
