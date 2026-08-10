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

## Fleet controller

The optional `agentplane-controller` binary exposes the `FleetAgent` gRPC API. The daemon enrolls once, stores its generated identity outside the human-authored YAML, and then maintains an outbound stream for inventory, heartbeats, and desired configuration.

For a local plaintext development run, update `config.yaml`:

```yaml
controller:
  address: http://127.0.0.1:8443
  heartbeatInterval: 5s
```

Start the controller with a one-time enrollment token:

```console
cargo run --bin agentplane-controller -- \
  --enrollment-token development
```

Then start the daemon with a writable development state directory:

```console
cargo run --bin agentplaned -- \
  --config ./config.yaml \
  --socket /tmp/agentplane.sock \
  --state-dir /tmp/agentplane-state \
  --enrollment-token development
```

The daemon writes `identity.json` and any accepted `remote-config.yaml` under its state directory. To have the controller offer desired configuration when a device connects, pass `--desired-config <path>` and optionally `--desired-config-revision <number>`.

For TLS, give the controller `--tls-certificate` and `--tls-key`, use an `https://` controller address, and set `caCertificatePath` in daemon YAML when using a private CA. Device identity and credentials never belong in YAML.

## Tray client

The tray client is a Tauri application under `ui/`. It polls the daemon for health and Codex discovery state and does not manage the privileged daemon's lifecycle.

```console
cd ui
pnpm install
AGENTPLANE_SOCKET=/tmp/agentplane.sock pnpm dev
```

On Linux, Tauri's tray support requires either AppIndicator or Ayatana AppIndicator development libraries. The application has no visible window yet; use its tray menu to inspect status, refresh, or quit.
