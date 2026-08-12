# AgentDesktop

AgentDesktop currently consists of a small Linux daemon and a CLI client. At startup, the daemon reads a YAML configuration and discovers Codex, OpenCode, Claude Code, Claude Desktop, and VS Code installations available on its `PATH`. It exposes its state through an HTTP API over a Unix domain socket.

Build it:

```console
make build
```

## Crates

- `agentdesktop-agent`: privileged device daemon for discovery, reconciliation, and fleet connectivity; builds `agentdesktopd`.
- `agentdesktop-controller`: central service for enrollment, inventory, and daemon configuration; builds `agentdesktop-controller`.
- `agentdesktop-cli`: command-line interface to the local daemon; builds `agentdesktop`.
- `agentdesktop-client`: reusable client for the daemon's local API transport.
- `agentdesktop-core`: shared configuration, data models, serialization, and telemetry utilities.
- `agentdesktop-proto`: generated FleetAgent gRPC contracts shared by the daemon and controller.
- `ui`: React/Vite management UI embedded in the controller binary.

The daemon and controller use structured `tracing` output. Set `RUST_LOG=debug` for verbose events or `LOG_FORMAT=json` for JSON logs.

Run it as your user while developing:

```console
cargo run --bin agentdesktopd -- \
  --config ./examples/claude/agentdesktopd.yaml \
  --socket /tmp/agentdesktop.sock
```

In another terminal:

```console
cargo run --bin agentdesktop -- --socket /tmp/agentdesktop.sock status
cargo run --bin agentdesktop -- --socket /tmp/agentdesktop.sock discover
cargo run --bin agentdesktop -- --socket /tmp/agentdesktop.sock config
```

The production-oriented defaults are `/etc/agentdesktop/config.yaml` and `/run/agentdesktop/agentdesktop.sock`.

## Configuration schema

The checked-in [daemon JSON Schema](schema/daemon-config.json) describes daemon
configuration whether loaded locally or distributed by the controller. The
[controller schema](schema/controller-config.json) describes the fleet controller
process. Generated [daemon](schema/daemon-config.md) and
[controller](schema/controller-config.md) field references contain the same Rust
doc comments in compact tables.

The daemon applies its local configuration at startup. If it connects to a
controller, the controller-delivered daemon configuration subsequently replaces
that local baseline.

Regenerate the schemas and field references after changing configuration types
or their documentation:

```console
cargo xtask schema
```

The Markdown generator requires `jq` and `sed`. `make gen` regenerates the
schema and then formats the Rust workspace.

## Fleet controller

The optional `agentdesktop-controller` binary exposes the `FleetAgent` gRPC API. The daemon enrolls once, stores its generated identity outside the human-authored YAML, and then maintains an outbound stream for inventory, heartbeats, and daemon configuration.

For a local plaintext development run, use
`examples/claude/agentdesktopd.yaml` or add:

```yaml
controller:
  address: http://127.0.0.1:8443
  heartbeatInterval: 5s
```

Device enrollment is OIDC-only. The local scenario includes Dex and explicitly
enables insecure local development:

```console
docker compose -f examples/claude/compose.yaml up -d
make ui
cargo run --bin agentdesktop-controller -- \
  --config ./examples/claude/controller.yaml
```

The controller also serves its embedded management UI at
`http://127.0.0.1:8080`. It shows fleet health, device inventory, daemon
configuration, and runtime settings, and can remove enrolled devices. The admin
listener is restricted to loopback addresses; set `adminListen` in controller
configuration to select a different local port.

Then start the daemon with a writable development state directory:

```console
cargo run --bin agentdesktopd -- \
  --config ./examples/claude/agentdesktopd.yaml \
  --socket /tmp/agentdesktop.sock \
  --state-dir /tmp/agentdesktop-state
```

The daemon writes `identity.json` and any accepted `remote-config.yaml` under its state directory. On restart it reapplies that last accepted controller configuration before connecting. If no cached or non-empty local daemon configuration exists, it preserves managed files until the controller responds rather than temporarily removing them. To have the controller distribute daemon configuration, set `daemonConfig.path` and `daemonConfig.revision` in controller configuration. The controller watches that path and pushes successfully validated changes to connected devices after a short debounce. Atomic file replacement and Kubernetes projected-volume symlink rotation are supported; missing or invalid replacements leave the last good configuration active.

### OIDC enrollment

OIDC enrollment is available for interactive device setup. The agent uses an authorization-code flow with PKCE, receives the browser callback on loopback, and sends the code to the controller for exchange and ID-token validation. The resulting OIDC issuer and subject are recorded on the device; subsequent connections continue to use the device's own credential.

The bundled Dex login is `admin@example.com` / `password`. Start Dex and the
controller with:

```console
docker compose -f examples/claude/compose.yaml up -d
cargo run --bin agentdesktop-controller -- \
  --config ./examples/claude/controller.yaml
```

Use a fresh state directory:

```console
cargo run --bin agentdesktopd -- \
  --config ./examples/claude/agentdesktopd.yaml \
  --socket /tmp/agentdesktop.sock \
  --state-dir /tmp/agentdesktop-oidc-state
```

Open the authorization URL printed by the daemon, or choose **Enroll with SSO…** from the tray menu. Dex redirects the browser to `http://127.0.0.1:5555/callback`, where the daemon completes enrollment. The callback must remain registered on the OIDC client. Production controllers reject plaintext OIDC issuers; this local example sets `allowInsecureDev: true`.

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

The controller uses SQLite by default and runs embedded migrations at startup. Set `databaseUrl: postgres://user:password@host/database` to use Postgres. Enrollment, device metadata, heartbeats, discoveries, and configuration status are persisted through one portable SQLx `AnyPool` query set.

For TLS, configure `tls.certificate` and `tls.key`, use an `https://` controller address, and set `caCertificatePath` in daemon YAML when using a private CA. A non-loopback `fleetListen` is rejected without TLS unless `allowInsecureDev` is explicitly enabled. Device identity and credentials never belong in YAML.

### Claude Code managed settings

AgentDesktop manages one optional top-level inference gateway. Every configured
developer-tool integration uses that gateway:

```yaml
inferenceGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway

programs:
  claudeCode:
    env:
      COMPANY_ENVIRONMENT: production
    permissions:
      defaultMode: plan
```

Keys under `claudeCode` are passed directly into AgentDesktop's managed-settings
drop-in. Objects are deep-merged with generated
settings; AgentDesktop-owned gateway values take precedence on conflicts. The
gateway adapter adds `ANTHROPIC_BASE_URL` plus an `apiKeyHelper` command that
calls the local AgentDesktop CLI. The CLI asks the daemon for a credential, the
daemon authenticates to the controller with its device credential, and the
controller returns a short-lived RS256 JWT. Claude caches the result for one
minute; the JWT lifetime defaults to five minutes. No gateway credential is
stored in YAML or managed settings.

Telemetry is opt-in. `session.new` reports new Claude Code sessions, while
`tool.use` reports the client, tool name, and invocation ID:

```yaml
telemetry:
  events:
  - session.new
  - tool.use
```

Use `tool.use.input` instead to also report tool-input JSON. The generated
hook command is equivalent to:

```console
agentdesktop --socket /run/agentdesktop/agentdesktop.sock hook claude-pre-tool-use
```

Claude supplies hook events on standard input. Unused hook metadata is
discarded. Reporting is fail-open so daemon or controller
availability never prevents tool execution. Events travel over the
authenticated device stream, are stored as timestamped controller telemetry,
and appear under **Recent activity** on the device page. Local clients are not
process-attested, so this telemetry records an assertion by the endpoint rather
than a security audit boundary.

Generate a controller signing key for development:

```console
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 \
  -out /tmp/agentdesktop-gateway-jwt.pem
```

Configure the controller with the daemon configuration and JWT issuer:

```yaml
oidc:
  issuer: https://idp.example.com
  clientId: agentdesktop

daemonConfig:
  path: ./claude-code.yaml
  revision: 1

gatewayJwt:
  privateKey: /tmp/agentdesktop-gateway-jwt.pem
  issuer: agentdesktop-controller
  keyId: agentdesktop
  lifetime: 5m
```

Relative certificate, signing-key, and daemon-configuration paths are resolved
from the controller YAML directory. Start it with only the configuration path:

```console
cargo run --bin agentdesktop-controller -- --config ./controller.yaml
```

Agentgateway must trust the corresponding RSA public key and require issuer `agentdesktop-controller` and audience `agentgateway`. When JWT issuance is enabled, the controller publishes its public key set at `http://127.0.0.1:8080/.well-known/jwks.json`; [examples/claude/agentgateway.yaml](examples/claude/agentgateway.yaml) is a minimal matching configuration. The gateway JWT subject is the verified OIDC subject, `act.sub` identifies the device, and `client_id` is asserted by the arbitrary local client requesting the credential. In production, keep the private key outside the database with restrictive permissions; only its public JWK is exposed.

On Linux, the daemon atomically reconciles AgentDesktop's dedicated drop-in at `/etc/claude-code/managed-settings.d/50-agentdesktop.json`. For an unprivileged development run, redirect that exact directory:

```console
cargo run --bin agentdesktopd -- \
  --config ./examples/claude/agentdesktopd.yaml \
  --socket /tmp/agentdesktop.sock \
  --state-dir /tmp/agentdesktop-state \
  --claude-code-managed-settings-dir /tmp/claude-code-managed-settings.d
```

Inspect the result with `jq . /tmp/claude-code-managed-settings.d/50-agentdesktop.json`. When `programs.claudeCode` is absent from a later daemon configuration revision, the daemon removes only its own drop-in file.

The generated helper can also be exercised directly:

```console
cargo run --bin agentdesktop -- \
  --socket /tmp/agentdesktop.sock \
  credential
```

It writes only the JWT to stdout, which is the interface Claude Code expects.

### Claude Desktop managed settings

Claude Desktop is configured separately from Claude Code:

```yaml
programs:
  claudeDesktop:
    isLocalDevMcpEnabled: true
```

Keys under `claudeDesktop` are passed directly to Desktop's managed settings.
When the shared inference gateway is enabled, AgentDesktop adds Desktop's
gateway fields and, for controller JWT authentication, installs a small
credential-helper script. On Linux these default to
`/etc/claude-desktop/managed-settings.json` and
`/etc/claude-desktop/agentdesktop-credential-helper`. Existing files without
AgentDesktop ownership markers are preserved.

For an unprivileged development run, redirect both paths:

```console
cargo run --bin agentdesktopd -- \
  --config ./examples/claude/agentdesktopd.yaml \
  --socket /tmp/agentdesktop.sock \
  --state-dir /tmp/agentdesktop-state \
  --claude-desktop-managed-settings /tmp/claude-desktop/managed-settings.json \
  --claude-desktop-credential-helper /tmp/claude-desktop/agentdesktop-credential-helper
```

Discovery reads local MCP servers from Desktop's user configuration files.
Desktop does not share Claude Code's skills or hook configuration.

### Codex managed configuration

Codex can use the configured inference gateway and rotating controller JWTs.
Codex also accepts arbitrary organization-managed settings:

```yaml
programs:
  codex:
    managedConfig:
      model_reasoning_effort: high
      approval_policy: on-request
      sandbox_mode: workspace-write
```

Keys under `managedConfig` use Codex's native snake_case names and are written
to `/etc/codex/managed_config.toml`. AgentDesktop adds a custom Responses API
provider for the selected gateway. For `controllerJwt` authentication, its
`auth.command` invokes the local `agentdesktop credential` helper and refreshes
the token every minute. Generated provider settings take precedence on
conflicts, while unrelated custom providers and settings are preserved.

For an unprivileged development run, redirect the managed file:

```console
cargo run --bin agentdesktopd -- \
  --config ./examples/claude/agentdesktopd.yaml \
  --socket /tmp/agentdesktop.sock \
  --state-dir /tmp/agentdesktop-state \
  --claude-code-managed-settings-dir /tmp/claude-code-managed-settings.d \
  --codex-managed-config /tmp/codex-managed_config.toml
```

When `programs.codex` is removed, AgentDesktop removes only the managed TOML
file it owns. It never changes `~/.codex/config.toml`.

### OpenCode managed configuration

OpenCode supports enforced system configuration, so AgentDesktop writes its
owned JSONC file at `/etc/opencode/opencode.jsonc`. The model catalog is
explicit because OpenCode custom providers do not discover arbitrary gateway
models automatically:

```yaml
programs:
  openCode:
    model: gpt-5.6-terra
    models:
      gpt-5.6-terra:
        name: GPT 5.6 Terra
        limit:
          context: 200000
          output: 65536
    managedConfig:
      autoupdate: false
      permission:
        edit: ask
        bash: ask
```

AgentDesktop generates an `@ai-sdk/openai` Responses API provider, selects it
as the only enabled provider, and sets the configured default model. For a
`controllerJwt` gateway it also writes
`/etc/opencode/plugins/agentdesktop.js`. The plugin uses OpenCode's
`chat.headers` hook to obtain a JWT from the local credential helper and caches
it for one minute. Existing `managedConfig.plugin` entries, unrelated provider
definitions, and other general settings are preserved.

For an unprivileged development run, redirect both managed files:

```console
cargo run --bin agentdesktopd -- \
  --config ./examples/claude/agentdesktopd.yaml \
  --socket /tmp/agentdesktop.sock \
  --state-dir /tmp/agentdesktop-state \
  --claude-code-managed-settings-dir /tmp/claude-code-managed-settings.d \
  --codex-managed-config /tmp/codex-managed_config.toml \
  --open-code-managed-config /tmp/opencode/opencode.jsonc \
  --open-code-plugin /tmp/opencode/plugins/agentdesktop.js
```

When `programs.openCode` is removed, AgentDesktop removes only files carrying
its ownership marker. User and project OpenCode configuration remains untouched.

To exercise the real privileged path during development, build as your user and elevate only the resulting daemon binary with the wrapper:

```console
./scripts/run-agentdesktopd-root \
  --config ./examples/claude/agentdesktopd.yaml
```

This uses the production defaults for state, the local socket, and Claude Code's `/etc/claude-code/managed-settings.d` directory. Additional daemon arguments are forwarded unchanged. The wrapper builds both `agentdesktopd` and its sibling `agentdesktop` credential helper as your user, then starts only the daemon with `sudo`. The runner preserves your development `PATH` so discovery can still see user-installed programs. The local socket is intentionally mode `0666`: every local process may inspect daemon state and request gateway credentials, and its requested `client_id` is not process-authenticated. Discovery reads install and package metadata; it never executes discovered programs.
