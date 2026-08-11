# AgentDesktop

AgentDesktop currently consists of a small Linux daemon and a CLI client. At startup, the daemon reads a YAML configuration and discovers Codex, OpenCode, Claude Code, and VS Code installations available on its `PATH`. It exposes its state through an HTTP API over a Unix domain socket.

Build it:

```console
cargo build
```

## Crates

- `agentdesktop-agent`: privileged device daemon for discovery, reconciliation, and fleet connectivity; builds `agentdesktopd`.
- `agentdesktop-controller`: central service for enrollment, inventory, and desired configuration; builds `agentdesktop-controller`.
- `agentdesktop-cli`: command-line interface to the local daemon; builds `agentdesktop`.
- `agentdesktop-client`: reusable client for the daemon's local API transport.
- `agentdesktop-core`: shared configuration, data models, serialization, and telemetry utilities.
- `agentdesktop-proto`: generated FleetAgent gRPC contracts shared by the daemon and controller.
- `agentdesktop-ui`: cross-platform Tauri desktop and tray client under `ui/src-tauri`.

The daemon and controller use structured `tracing` output. Set `RUST_LOG=debug` for verbose events or `LOG_FORMAT=json` for JSON logs.

Run it as your user while developing:

```console
cargo run --bin agentdesktopd -- --config ./config.yaml --socket /tmp/agentdesktop.sock
```

In another terminal:

```console
cargo run --bin agentdesktop -- --socket /tmp/agentdesktop.sock status
cargo run --bin agentdesktop -- --socket /tmp/agentdesktop.sock discover
cargo run --bin agentdesktop -- --socket /tmp/agentdesktop.sock config
```

The production-oriented defaults are `/etc/agentdesktop/config.yaml` and `/run/agentdesktop/agentdesktop.sock`.

## Configuration schema

The checked-in [daemon JSON Schema](schema/daemon-config.json) describes local
daemon startup configuration. It is a superset of the controller-managed
[desired configuration schema](schema/desired-config.json). Generated field
references for the [daemon](schema/daemon-config.md) and
[desired configuration](schema/desired-config.md) contain the same Rust doc
comments in compact tables.

The daemon applies the desired-state fields in its local configuration at
startup. If it connects to a controller, the controller-delivered desired
configuration subsequently replaces that local baseline.

Regenerate both files after changing configuration types or their documentation:

```console
cargo xtask schema
```

The Markdown generator requires `jq` and `sed`. `make gen` regenerates the
schema and then formats the Rust workspace.

## Fleet controller

The optional `agentdesktop-controller` binary exposes the `FleetAgent` gRPC API. The daemon enrolls once, stores its generated identity outside the human-authored YAML, and then maintains an outbound stream for inventory, heartbeats, and desired configuration.

For a local plaintext development run, use `config.controller.yaml.example` or add:

```yaml
controller:
  address: http://127.0.0.1:8443
  heartbeatInterval: 5s
```

Start the controller with a one-time enrollment token:

```console
cargo run --bin agentdesktop-controller -- \
  --enrollment-token development \
  --database-url 'sqlite://agentdesktop-controller.db?mode=rwc'
```

The controller also serves its embedded management UI at
`http://127.0.0.1:8080`. It shows fleet health, device inventory, desired
configuration, and runtime settings, and can remove enrolled devices. The admin
listener is restricted to loopback addresses; use `--admin-listen` to select a
different local port.

Then start the daemon with a writable development state directory:

```console
cargo run --bin agentdesktopd -- \
  --config ./config.controller.yaml.example \
  --socket /tmp/agentdesktop.sock \
  --state-dir /tmp/agentdesktop-state \
  --enrollment-token development
```

The daemon writes `identity.json` and any accepted `remote-config.yaml` under its state directory. To have the controller offer desired configuration when a device connects, pass `--desired-config <path>` and optionally `--desired-config-revision <number>`.

### OIDC enrollment

OIDC enrollment is available for interactive device setup. The agent uses an authorization-code flow with PKCE, receives the browser callback on loopback, and sends the code to the controller for exchange and ID-token validation. The resulting OIDC issuer and subject are recorded on the device; subsequent connections continue to use the device's own credential.

With the local Dex `local-public` client, start the controller with:

```console
cargo run --bin agentdesktop-controller -- \
  --oidc-issuer http://dex.local \
  --oidc-client-id local-public \
  --database-url 'sqlite://agentdesktop-controller.db?mode=rwc'
```

Use a fresh state directory and omit the enrollment token:

```console
cargo run --bin agentdesktopd -- \
  --config ./config.controller.yaml.example \
  --socket /tmp/agentdesktop.sock \
  --state-dir /tmp/agentdesktop-oidc-state
```

Open the authorization URL printed by the daemon, or choose **Enroll with SSO…** from the tray menu. Dex redirects the browser to `http://127.0.0.1:5555/callback`, where the daemon completes enrollment. The callback must remain registered on the OIDC client. For production, use an HTTPS issuer; the plaintext issuer above is only suitable for local development.

When the daemon runs in a Docker container, keep the registered redirect URI on
host loopback but bind the callback server to the container interface:

```console
docker run --rm \
  -p 127.0.0.1:5555:5555 \
  agentdesktop-agent \
  --config /etc/agentdesktop/config.yaml \
  --oidc-callback-listen 0.0.0.0:5555
```

The browser still returns to `http://127.0.0.1:5555/callback`; Docker forwards
that request to the daemon. Restrict the published host port to loopback as
shown above. Without `--oidc-callback-listen`, the daemon continues to bind the
callback directly to the loopback address from the redirect URI.

The controller uses SQLite by default and runs embedded migrations at startup. Point the same binary at Postgres with `--database-url 'postgres://user:password@host/database'`. Enrollment, device metadata, heartbeats, discoveries, and configuration status are persisted through one portable SQLx `AnyPool` query set.

For TLS, give the controller `--tls-certificate` and `--tls-key`, use an `https://` controller address, and set `caCertificatePath` in daemon YAML when using a private CA. Device identity and credentials never belong in YAML.

### Claude Code managed settings

Inference gateways are named, top-level resources so multiple application integrations can reference the same endpoint. `config.claude-code.yaml.example` defines a `corporate` gateway and assigns Claude Code to it:

```yaml
inferenceGateways:
  corporate:
    url: https://gateway.example.com
    authentication:
      type: controllerJwt
      audience: agentgateway

programs:
  claudeCode:
    inferenceGateway: corporate
```

The Claude Code adapter translates this to `ANTHROPIC_BASE_URL` plus an `apiKeyHelper` command that calls the local AgentDesktop CLI. The CLI asks the daemon for a credential, the daemon authenticates to the controller with its device credential, and the controller returns a short-lived RS256 JWT. Claude caches the result for one minute; the JWT lifetime defaults to five minutes. No gateway credential is stored in YAML or managed settings.

Generate a controller signing key for development:

```console
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 \
  -out /tmp/agentdesktop-gateway-jwt.pem
```

Start the controller with the example as desired state and enable JWT issuance:

```console
cargo run --bin agentdesktop-controller -- \
  --enrollment-token development \
  --desired-config ./config.claude-code.yaml.example \
  --gateway-jwt-private-key /tmp/agentdesktop-gateway-jwt.pem \
  --gateway-jwt-issuer agentdesktop-controller \
  --gateway-jwt-key-id agentdesktop
```

Agentgateway must trust the corresponding RSA public key and require issuer `agentdesktop-controller` and audience `agentgateway`. For OIDC-enrolled devices, the gateway JWT subject is the verified OIDC subject; token-enrolled devices use their device ID. The JWT also carries `device_id` and `gateway` claims. In production, keep the private key outside the database with restrictive permissions and distribute only its public JWK to Agentgateway.

On Linux, the daemon atomically reconciles AgentDesktop's dedicated drop-in at `/etc/claude-code/managed-settings.d/50-agentdesktop.json`. For an unprivileged development run, redirect that exact directory:

```console
cargo run --bin agentdesktopd -- \
  --config ./config.controller.yaml.example \
  --socket /tmp/agentdesktop.sock \
  --state-dir /tmp/agentdesktop-state \
  --claude-code-managed-settings-dir /tmp/claude-code-managed-settings.d \
  --enrollment-token development
```

Inspect the result with `jq . /tmp/claude-code-managed-settings.d/50-agentdesktop.json`. When `programs.claudeCode` is absent from a later desired revision, the daemon removes only its own drop-in file.

The generated helper can also be exercised directly:

```console
cargo run --bin agentdesktop -- \
  --socket /tmp/agentdesktop.sock \
  credential corporate
```

It writes only the JWT to stdout, which is the interface Claude Code expects.

To exercise the real privileged path during development, build as your user and elevate only the resulting daemon binary with the wrapper:

```console
AGENTDESKTOP_ENROLLMENT_TOKEN=development \
  ./scripts/run-agentdesktopd-root \
  --config ./config.controller.yaml.example
```

This uses the production defaults for state, the local socket, and Claude Code's `/etc/claude-code/managed-settings.d` directory. Additional daemon arguments are forwarded unchanged. Cargo runs as your user, then its target runner starts only the built daemon with `sudo`. The runner preserves your development `PATH` so discovery can still see user-installed programs. Discovery reads install and package metadata; it never executes discovered programs.

## Tray client

The tray client is a Tauri application under `ui/`. It polls the daemon for health, enrollment, and discovery state and does not manage the privileged daemon's lifecycle. While OIDC enrollment is waiting, the tray exposes an **Enroll with SSO…** action that opens the authorization URL in the user's browser.

```console
cd ui
pnpm install
AGENTDESKTOP_SOCKET=/tmp/agentdesktop.sock pnpm dev
```

On Linux, Tauri's tray support requires either AppIndicator or Ayatana AppIndicator development libraries. The application has no visible window yet; use its tray menu to inspect status, refresh, or quit.

On macOS, build a simple menu bar app bundle from a Mac:

```console
cd ui
pnpm install
pnpm build:mac
```

The build uses the existing `.icns` app icon and the in-app template tray icon. Tauri writes the `.app` and `.dmg` outputs under `target/release/bundle/macos/` and `target/release/bundle/dmg/`.
