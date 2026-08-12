# Desktop UI architecture

Agent Desktop uses Tauri as a thin native shell around a React operational experience. The desktop UI is a separate per-user process from the connector and does not move gateway policy or traffic forwarding into the UI.

## Native host: Rust

Code under `src-tauri/` owns operating-system and trust-boundary behavior:

- System tray and native menu lifecycle.
- macOS accessory activation and Windows GUI-process behavior.
- Single-instance enforcement and window show, focus, hide, and quit behavior.
- Per-user desktop preference persistence.
- Standalone-only provider credential storage through the native platform credential store. Managed organization mode never exposes this workflow.
- Managed browser sign-in, device enrollment, approval polling, and redacted status.
- Loopback connector status polling and tray-state updates.
- Safe application adapters shared from the root Rust library.
- Tauri commands exposed to the webview.
- Packaging, application identity, capabilities, and code-signing configuration.

The host currently exposes eight commands:

| Command | Purpose |
| --- | --- |
| `get_bootstrap` | Returns persisted settings plus native version and platform metadata. |
| `save_settings` | Persists desktop-owned window behavior. |
| `get_connector_status` | Reads and validates the connector's structured loopback status response. |
| `get_claude_status` | Inspects Claude Code installation and routing without changing files. |
| `get_managed_device_status` | Returns organization, session, enrollment, and public certificate metadata without credential material. |
| `setup_managed_device` | Opens organization sign-in when needed, requests or refreshes device enrollment, and persists credentials entirely in Rust. |
| `open_managed_page` | Opens only the configured organization support page or enrollment administration console in the system browser. |
| `connect_claude` | Optionally stores a standalone provider key, then safely adds connector routing when no conflict exists. |

The host reuses the root crate for narrow application-adapter and managed-identity operations. It does not start the forwarding runtime in-process. Managed status is projected into a redacted DTO; OAuth tokens, private keys, certificate PEM, policy, model assignments, and provider credentials are never serialized into the webview. The separate UI-local development backend runs the connector service. In standalone mode only, it may supervise Agent Gateway and inject a credential-store provider key into the child environment when the Gateway configuration references `$ANTHROPIC_API_KEY`. In managed mode it waits for an approved client certificate, then starts mTLS forwarding to the organization-owned Gateway. Secrets are never returned by a Tauri command or included in diagnostics. Future installed-service lifecycle or configuration operations belong behind narrow Rust commands or a local authenticated IPC/control API. The UI must not interpret, cache, or enforce Agent Gateway policy.

## Window UI: React and TypeScript

Code under `src/` owns presentation and interaction behavior:

- Operational views, controls, status feedback, and responsive layout.
- Polling presentation, navigation state, and save progress.
- Accessible labels, focus treatment, and user-facing errors.
- Typed wrappers around Tauri `invoke` calls in `src/backend.ts`.

React does not receive direct filesystem, process, credential-store, or unrestricted network access. It requests native work through the smallest command surface needed for the user workflow.

## Runtime flow

```mermaid
flowchart LR
    Tray[Native tray menu] --> Host[Tauri Rust host]
    Host --> Window[Native webview window]
    Window --> React[React and TypeScript UI]
    React -->|invoke command| Host
    Host -->|validated result| React
    Host -->|loopback status| Connector[Agent Desktop connector]
    Host --> Adapter[Shared application adapter]
    Host --> Keychain[Platform credential store]
    Keychain -->|read by owner| DevBackend[UI development backend]
    DevBackend -->|child environment| Gateway
    Connector --> Gateway[Agent Gateway]
```

The native window uses WKWebView on macOS and WebView2 on Windows. Chromium and Node.js are not bundled with the application.

## Adding a feature

Keep purely visual state in React. Add a Rust command when a workflow needs native capabilities, sensitive data, persistent configuration, connector IPC, or validation that must not be bypassed. Return narrow serializable types and add only the Tauri capability permissions required by the window.
