# Desktop UI architecture

Agent Desktop is a Tauri shell around a React status and setup interface. It is
a separate per-user process from the privileged device daemon.

## Native host

Code under `crates/agentdesktop/` owns the system tray, native window lifecycle,
single-instance handling, desktop preferences, CLI dispatch, and daemon mode.
Its Tauri commands use the shared `agentdesktop-client` crate to call the daemon
over the local Unix socket or Windows named pipe.

The native host does not implement enrollment, discovery, reconciliation,
credential storage, gateway policy, or traffic forwarding. Those operations
belong to the daemon. Only redacted, purpose-specific data is returned to the
webview.

## React interface

Code under `frontend/desktop/src/` owns presentation and interaction state. It
accesses native functionality only through the typed wrappers in
`src/backend.ts`; it does not access the filesystem, spawn processes, or
connect directly to the daemon.

Shared visual primitives, styles, branding, and inventory components live in
`frontend/ui/` and are also used by the controller frontend.

```text
React webview -> Tauri command -> agentdesktop-client -> local daemon API
Native tray  -> Tauri host    -> agentdesktop-client -> local daemon API
```
