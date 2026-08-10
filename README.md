# Agentplane

Agentplane currently consists of a small Linux daemon and a CLI client. At startup, the daemon reads a YAML configuration and discovers Codex, OpenCode, Claude Code, and VS Code installations available on its `PATH`. It exposes its state through an HTTP API over a Unix domain socket.

Build it:

```console
cargo build
```

Run it as your user while developing:

```console
cargo run --bin agentplaned -- --config ./config.yaml --socket /tmp/agentplane.sock
```

In another terminal:

```console
cargo run --bin agentplane -- --socket /tmp/agentplane.sock status
cargo run --bin agentplane -- --socket /tmp/agentplane.sock discover
cargo run --bin agentplane -- --socket /tmp/agentplane.sock config
```

The production-oriented defaults are `/etc/agentplane/config.yaml` and `/run/agentplane/agentplane.sock`.

## Tray client

The tray client is a Tauri application under `ui/`. It polls the daemon for health and Codex discovery state and does not manage the privileged daemon's lifecycle.

```console
cd ui
pnpm install
AGENTPLANE_SOCKET=/tmp/agentplane.sock pnpm dev
```

On Linux, Tauri's tray support requires either AppIndicator or Ayatana AppIndicator development libraries. The application has no visible window yet; use its tray menu to inspect status, refresh, or quit.
