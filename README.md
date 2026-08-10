# Agentplane

Agentplane currently consists of a small Linux daemon and a CLI client. At startup, the daemon reads a YAML configuration and discovers Codex, OpenCode, Claude Code, and VS Code installations available on its `PATH`. It exposes its state through an HTTP API over a Unix domain socket.

Build it:

```console
cargo build
```

The Rust code is a virtual Cargo workspace under `crates/`: `core` owns shared configuration and models, `proto` owns the FleetAgent contract, `agent` builds `agentplaned`, `controller` builds `agentplane-controller`, `client` is the reusable local UDS client, and `cli` builds `agentplane`. The Tauri Rust crate remains under `ui/src-tauri` as another workspace member.

The daemon and controller use structured `tracing` output. Set `RUST_LOG=debug` for verbose events or `LOG_FORMAT=json` for JSON logs.

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

For a local plaintext development run, use `config.controller.yaml.example` or add:

```yaml
controller:
  address: http://127.0.0.1:8443
  heartbeatInterval: 5s
```

Start the controller with a one-time enrollment token:

```console
cargo run --bin agentplane-controller -- \
  --enrollment-token development \
  --database-url 'sqlite://agentplane-controller.db?mode=rwc'
```

Then start the daemon with a writable development state directory:

```console
cargo run --bin agentplaned -- \
  --config ./config.controller.yaml.example \
  --socket /tmp/agentplane.sock \
  --state-dir /tmp/agentplane-state \
  --enrollment-token development
```

The daemon writes `identity.json` and any accepted `remote-config.yaml` under its state directory. To have the controller offer desired configuration when a device connects, pass `--desired-config <path>` and optionally `--desired-config-revision <number>`.

The controller uses SQLite by default and runs embedded migrations at startup. Point the same binary at Postgres with `--database-url 'postgres://user:password@host/database'`. Enrollment, device metadata, heartbeats, discoveries, and configuration status are persisted through one portable SQLx `AnyPool` query set.

For TLS, give the controller `--tls-certificate` and `--tls-key`, use an `https://` controller address, and set `caCertificatePath` in daemon YAML when using a private CA. Device identity and credentials never belong in YAML.

### Claude Code managed settings

`config.claude-code.yaml.example` demonstrates centrally managed Claude Code settings. Start the controller with it as desired state:

```console
cargo run --bin agentplane-controller -- \
  --enrollment-token development \
  --desired-config ./config.claude-code.yaml.example
```

On Linux, the daemon atomically reconciles Agentplane's dedicated drop-in at `/etc/claude-code/managed-settings.d/50-agentplane.json`. For an unprivileged development run, redirect that exact directory:

```console
cargo run --bin agentplaned -- \
  --config ./config.controller.yaml.example \
  --socket /tmp/agentplane.sock \
  --state-dir /tmp/agentplane-state \
  --claude-code-managed-settings-dir /tmp/claude-code-managed-settings.d \
  --enrollment-token development
```

Inspect the result with `jq . /tmp/claude-code-managed-settings.d/50-agentplane.json`. When `programs.claudeCode` is absent from a later desired revision, the daemon removes only its own drop-in file.

To exercise the real privileged path during development, build as your user and elevate only the resulting daemon binary with the wrapper:

```console
AGENTPLANE_ENROLLMENT_TOKEN=development \
  ./scripts/run-agentplaned-root \
  --config ./config.controller.yaml.example
```

This uses the production defaults for state, the local socket, and Claude Code's `/etc/claude-code/managed-settings.d` directory. Additional daemon arguments are forwarded unchanged. Cargo runs as your user, then its target runner starts only the built daemon with `sudo`. The runner preserves your development `PATH` so discovery can still see user-installed programs. Discovery reads install and package metadata; it never executes discovered programs.

## Tray client

The tray client is a Tauri application under `ui/`. It polls the daemon for health and Codex discovery state and does not manage the privileged daemon's lifecycle.

```console
cd ui
pnpm install
AGENTPLANE_SOCKET=/tmp/agentplane.sock pnpm dev
```

On Linux, Tauri's tray support requires either AppIndicator or Ayatana AppIndicator development libraries. The application has no visible window yet; use its tray menu to inspect status, refresh, or quit.
