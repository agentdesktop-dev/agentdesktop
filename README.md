# Agentplane

Agentplane currently consists of a small Linux daemon and a CLI client. At startup, the daemon reads a YAML configuration and discovers Codex, OpenCode, Claude Code, and VS Code installations available on its `PATH`. It exposes its state through an HTTP API over a Unix domain socket.

Build it:

```console
cargo build
```

## Crates

- `agentplane-agent`: privileged device daemon for discovery, reconciliation, and fleet connectivity; builds `agentplaned`.
- `agentplane-controller`: central service for enrollment, inventory, and desired configuration; builds `agentplane-controller`.
- `agentplane-cli`: command-line interface to the local daemon; builds `agentplane`.
- `agentplane-client`: reusable client for the daemon's local API transport.
- `agentplane-core`: shared configuration, data models, serialization, and telemetry utilities.
- `agentplane-proto`: generated FleetAgent gRPC contracts shared by the daemon and controller.
- `agentplane-ui`: cross-platform Tauri desktop and tray client under `ui/src-tauri`.

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

### OIDC enrollment

OIDC enrollment is available for interactive device setup. The agent uses an authorization-code flow with PKCE, receives the browser callback on loopback, and sends the code to the controller for exchange and ID-token validation. The resulting OIDC issuer and subject are recorded on the device; subsequent connections continue to use the device's own credential.

With the local Dex `local-public` client, start the controller with:

```console
cargo run --bin agentplane-controller -- \
  --oidc-issuer http://dex.local \
  --oidc-client-id local-public \
  --database-url 'sqlite://agentplane-controller.db?mode=rwc'
```

Use a fresh state directory and omit the enrollment token:

```console
cargo run --bin agentplaned -- \
  --config ./config.controller.yaml.example \
  --socket /tmp/agentplane.sock \
  --state-dir /tmp/agentplane-oidc-state
```

Open the authorization URL printed by the daemon, or choose **Enroll with SSO…** from the tray menu. Dex redirects the browser to `http://127.0.0.1:5555/callback`, where the daemon completes enrollment. The callback must remain registered on the OIDC client. For production, use an HTTPS issuer; the plaintext issuer above is only suitable for local development.

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

The Claude Code adapter translates this to `ANTHROPIC_BASE_URL` plus an `apiKeyHelper` command that calls the local Agentplane CLI. The CLI asks the daemon for a credential, the daemon authenticates to the controller with its device credential, and the controller returns a short-lived RS256 JWT. Claude caches the result for one minute; the JWT lifetime defaults to five minutes. No gateway credential is stored in YAML or managed settings.

Generate a controller signing key for development:

```console
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 \
  -out /tmp/agentplane-gateway-jwt.pem
```

Start the controller with the example as desired state and enable JWT issuance:

```console
cargo run --bin agentplane-controller -- \
  --enrollment-token development \
  --desired-config ./config.claude-code.yaml.example \
  --gateway-jwt-private-key /tmp/agentplane-gateway-jwt.pem \
  --gateway-jwt-issuer agentplane-controller \
  --gateway-jwt-key-id agentplane
```

Agentgateway must trust the corresponding RSA public key and require issuer `agentplane-controller` and audience `agentgateway`. For OIDC-enrolled devices, the gateway JWT subject is the verified OIDC subject; token-enrolled devices use their device ID. The JWT also carries `device_id` and `gateway` claims. In production, keep the private key outside the database with restrictive permissions and distribute only its public JWK to Agentgateway.

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

The generated helper can also be exercised directly:

```console
cargo run --bin agentplane -- \
  --socket /tmp/agentplane.sock \
  credential corporate
```

It writes only the JWT to stdout, which is the interface Claude Code expects.

To exercise the real privileged path during development, build as your user and elevate only the resulting daemon binary with the wrapper:

```console
AGENTPLANE_ENROLLMENT_TOKEN=development \
  ./scripts/run-agentplaned-root \
  --config ./config.controller.yaml.example
```

This uses the production defaults for state, the local socket, and Claude Code's `/etc/claude-code/managed-settings.d` directory. Additional daemon arguments are forwarded unchanged. Cargo runs as your user, then its target runner starts only the built daemon with `sudo`. The runner preserves your development `PATH` so discovery can still see user-installed programs. Discovery reads install and package metadata; it never executes discovered programs.

## Tray client

The tray client is a Tauri application under `ui/`. It polls the daemon for health, enrollment, and discovery state and does not manage the privileged daemon's lifecycle. While OIDC enrollment is waiting, the tray exposes an **Enroll with SSO…** action that opens the authorization URL in the user's browser.

```console
cd ui
pnpm install
AGENTPLANE_SOCKET=/tmp/agentplane.sock pnpm dev
```

On Linux, Tauri's tray support requires either AppIndicator or Ayatana AppIndicator development libraries. The application has no visible window yet; use its tray menu to inspect status, refresh, or quit.

On macOS, build a simple menu bar app bundle from a Mac:

```console
cd ui
pnpm install
pnpm build:mac
```

The build uses the existing `.icns` app icon and the in-app template tray icon. Tauri writes the `.app` and `.dmg` outputs under `target/release/bundle/macos/` and `target/release/bundle/dmg/`.
