# Agent Desktop

An early, policy-free edge connector that forwards Claude Code HTTP traffic from a loopback listener to an independently running Agent Gateway.

Source: [github.com/agentdesktop-dev/agentdesktop](https://github.com/agentdesktop-dev/agentdesktop)

The current development build includes experimental managed browser login, DPoP-authenticated forwarding, refresh restoration, an enrollment client and mock authority, opt-in OTLP trace export, and an isolated Linux transparent-capture prototype. Supported transparent capture remains unavailable until authentication and lifecycle integration are complete. Production enrollment authority integration, Agent Gateway identity enforcement, MDM product integration, and metric export are not implemented. See [AGENTS.md](AGENTS.md) for the architecture and incremental delivery plan.

For a local installation, including credential ownership, file permissions, logs, retention, and removal, see [Standalone Operations](docs/deployment/standalone.md).

Verified progress and environment-dependent blockers are tracked in [Phase Status](docs/development/phase-status.md).
Tested platform behavior is listed in [Platform Compatibility](docs/compatibility/platforms.md).
The isolated Linux cgroup v2/nftables prototype is documented in [Linux Capture Prototype](docs/deployment/linux-capture.md).
Manual desktop journeys and future headless E2E tests use the [QEMU desktop test environment](tests/vm/README.md).

The managed user/device trust boundary is documented in [Managed Identity Contract v1](docs/architecture/managed-identity-v1.md). Browser PKCE login, connector-instance DPoP proof, refresh restoration, the enrollment client, and the repository's mock enrollment authority are implemented. Production authority integration and Agent Gateway enforcement remain design-contract work.

## Managed identity storage preflight

Managed identity is experimental. Validate credential persistence before login or installation:

```bash
cargo run -- identity storage-check
```

The default `auto` mode uses Linux Secret Service when a write/read/delete preflight succeeds and otherwise persists an owner-only protected-file backend. Require Secret Service with no fallback using:

```bash
cargo run -- identity storage-check --credential-storage secret-service
```

Select the protected file explicitly with `--credential-storage file`. The selected backend is persisted and revalidated on later startup; runtime does not silently switch stores. Override the XDG-based identity directory with `AGENTDESKTOP_IDENTITY_DIR`.

## Experimental managed login

Run browser Authorization Code login against an issuer that advertises PKCE `S256`, DPoP `ES256`, and an ES256 JWT signing key through discovery:

```bash
cargo run -- identity login \
  --issuer https://identity.example/ \
  --client-id agentdesktop \
  --audience https://gateway.example \
  --scope agentgateway.invoke \
  --gateway-origin https://gateway.example
```

The command validates credential storage before opening the browser, listens on an ephemeral loopback callback, verifies the access-token signature and issuer, audience, expiry, scope, and DPoP binding, then persists the token and DPoP key. `--gateway-origin` must be the upstream origin without a path. `--no-open` prints the authorization URL for non-desktop testing.

Attach the persisted session to managed forwarding by supplying the same issuer:

```bash
cargo run -- serve \
  --mode managed \
  --upstream https://gateway.example \
  --identity-issuer https://identity.example/
```

The connector fails at startup if storage or the matching session is unavailable. Before expiry it serializes refresh, uses the same DPoP key, verifies the rotated access token, persists the new refresh token, and replaces the managed upstream connection pool before forwarding continues. Refresh failure fails new requests closed locally; the issuer must enforce refresh-token rotation and reuse detection. For each request the connector removes application-supplied connector identity headers, preserves the application `Authorization` header, and adds a fresh `Proxy-Authorization: DPoP` token and DPoP proof. The DPoP key is not verified organizational device identity, and Agent Gateway still needs the contract's DPoP validation and credential-stripping changes before this is a trusted end-to-end managed identity path.

Delete only the matching local session with:

```bash
cargo run -- identity logout \
  --issuer https://identity.example/ \
  --gateway-origin https://gateway.example
```

This removes the local session and any locally persisted enrollment record. It does not invoke an issuer token-revocation endpoint or revoke an authority-side device approval.

Request authority approval for the current session's DPoP key when the issuer advertises the draft enrollment endpoint:

```bash
cargo run -- identity enroll-request \
  --issuer https://identity.example/ \
  --gateway-origin https://gateway.example
```

The command prints a non-secret pending record containing the authority-generated enrollment ID. After administrator approval, read device and revocation status with:

```bash
cargo run -- identity enroll-status \
  --issuer https://identity.example/ \
  --gateway-origin https://gateway.example
```

Both operations load the existing issuer/gateway-scoped session, refresh it when needed, and send fresh access-token-bound DPoP proofs. Responses are rejected unless their issuer and DPoP thumbprint match the current session. Request and status responses replace the protected issuer/gateway-scoped enrollment record; status uses its enrollment ID by default and accepts `--enrollment-id` for explicit recovery. A stale record from a rotated key is rejected. Agent Gateway does not yet enforce device status.

## Run

Start Agent Gateway with a route that accepts Anthropic-compatible requests at `/v1/messages`, then run:

```bash
cargo run -- serve \
  --mode standalone \
  --upstream http://127.0.0.1:4000
```

The connector listens on `127.0.0.1:8080` by default. Override either setting with flags:

```bash
cargo run -- serve \
  --mode managed \
  --listen 127.0.0.1:8081 \
  --upstream https://agentgateway.example.internal
```

Or use environment variables:

```bash
export AGENTDESKTOP_MODE=managed
export AGENTDESKTOP_LISTEN=127.0.0.1:8081
export AGENTDESKTOP_UPSTREAM=https://agentgateway.example.internal
cargo run -- serve
```

The deployment mode is required. `standalone` accepts only a local Agent Gateway at `localhost` or a loopback IP; `managed` permits a remote upstream. The listen address must always be loopback. The upstream URL must use HTTP or HTTPS and may contain a path prefix, but not a query string or fragment.

Forwarding defaults to a 5-second connection timeout, 30-second response-header timeout, 10-second graceful-shutdown deadline, and 128 in-flight requests. Override them with `--connect-timeout-ms`, `--request-timeout-ms`, `--shutdown-timeout-ms`, and `--max-in-flight`. Concurrency permits remain held until streamed response bodies finish or are dropped. Overload returns `503` with `x-agentdesktop-error: overloaded`; an upstream response-header timeout returns `504` with `x-agentdesktop-error: upstream-timeout`.

The connector does not retry forwarded requests. In particular, non-idempotent and streaming requests are never replayed after an upstream disconnect.

The connector emits JSON structured logs to standard error. Runtime events use fixed event and reason values and omit upstream URLs, paths, queries, request and response bodies, and authorization headers. Valid W3C `traceparent` and `tracestate` headers are propagated to Agent Gateway; when `traceparent` is absent or malformed, the connector generates a new context and removes untrusted `tracestate`. The active `traceparent` is returned on success and stable local error responses.

Set `OTEL_EXPORTER_OTLP_ENDPOINT` to an HTTP(S) OTLP/gRPC collector endpoint, such as `http://127.0.0.1:4317`, to export forwarding spans. Export uses the OpenTelemetry SDK's bounded batch processor, shares the propagated W3C trace ID with Agent Gateway, and flushes on orderly connector shutdown. Spans contain fixed service metadata, deployment mode, and response status only; they omit URLs, process details, identities, headers, and application content. When the variable is absent, no exporter or background export task is created. Metric export and automated collector-correlation coverage are not implemented yet.

## Configure Claude Code

Connect Claude Code when it is already installed for the current user:

```bash
cargo run -- connect-agents
```

After consent, Agent Desktop detects Claude Code and adds the local connector endpoint and placeholder credential to the `env` object in `~/.claude/settings.json`. It preserves unrelated settings, treats matching values as already connected, and refuses to replace an existing provider or gateway configuration. The interactive installer asks for this consent separately after the service is ready.

Launch Claude Code normally after it is connected:

```bash
claude
```

Claude Code reads these user settings for ordinary terminal and IDE launches, so no Claude wrapper is installed. A bundle installation provides the stable `agentdesktop` command through `~/.local/bin`; run `agentdesktop connect-agents` at any time to connect newly installed supported agents or restore matching settings without reinstalling the product. The placeholder is sent to Agent Gateway, not the AI provider. Agent Gateway remains responsible for replacing or removing application credentials before provider forwarding.

## Launch an application scope

On Linux, run any command and its descendants in a uniquely owned transient systemd user scope:

```bash
cargo run -- launch --profile claude -- claude --continue
```

The command preserves the launched argv and working environment, waits for the complete scope, requests control-group cleanup from systemd, and returns the launched command's exit status. Embedded profiles can add process-local integration settings without changing application configuration: `claude` supplies the connector URL and placeholder credential to the launched process tree, while the default `custom` profile adds no environment variables. Unknown profiles fail and list the available names.

This means Claude can use Agent Desktop for one invocation without changing `~/.claude/settings.json`. The connector must already be installed and listening before Claude sends a request.

Profiles that depend on Agent Desktop check local readiness before starting the application. If the connector is stopped or Agent Gateway is unavailable, `launch` fails immediately with recovery steps instead of letting the application retry until it times out. Use `--skip-preflight` only for debugging or when deliberately testing an unhealthy service:

```bash
cargo run -- launch --skip-preflight --profile claude -- claude
```

This is currently process grouping only. It does not yet route traffic, install capture rules, restrict files, or provide a security sandbox. The scope is the process-identity foundation for the transactional Linux capture controller: capture will keep the command stopped until trust, relay, and fail-closed network rules are active. A future profile may request a stronger lightweight-sandbox, container, or VM backend, but Agent Desktop must report the guarantees actually active and must never silently fall back to weaker isolation.

In standalone mode, the connector can optionally own the lifecycle of a separately installed Agent Gateway process:

```bash
cargo run -- serve \
  --mode standalone \
  --upstream http://127.0.0.1:4000 \
  --gateway-binary /usr/local/bin/agentgateway \
  --gateway-config "$HOME/.config/agentgateway/config.yaml"
```

The binary and config options must be provided together. The connector starts `agentgateway -f <config>`, waits up to 10 seconds for its configured loopback upstream to accept TCP connections, and only then opens the application listener. It stops Agent Gateway during connector shutdown and exits if the local process exits unexpectedly. Agent Gateway remains a separate process and retains ownership of policy and provider credentials.

Query connector and gateway reachability on the same loopback listener:

```bash
curl http://127.0.0.1:8080/_agentdesktop/healthz
```

A reachable gateway returns `200 OK`:

```json
{"status":"ok","mode":"standalone","gateway":"reachable"}
```

An unreachable gateway returns `503 Service Unavailable` with `status` set to `degraded`. The check establishes a fresh TCP connection to the configured upstream. It reports connector liveness and gateway reachability; it does not interpret Agent Gateway configuration or policy health.

Read the local operational status API:

```bash
curl http://127.0.0.1:8080/_agentdesktop/status
```

The response contains connector version, deployment mode, gateway reachability, identity readiness, active/maximum forwarding count, configured timeout values, and fixed counters for request attempts, upstream responses, identity failures, overload rejections, upstream timeouts, and upstream failures. Counters have no request-, destination-, process-, or identity-derived labels. The API does not expose gateway addresses, identity claims, credentials, application traffic, or policy. This API is the backend for a future local UI and telemetry exporter; no graphical UI is implemented yet.

## Install a standalone bundle

The current installer is a self-contained Linux development executable containing a tested Agent Gateway and Agent Desktop version set. Public downloads and non-Linux packages are not available yet; build it with `scripts/build-embedded-installer.sh` as shown below, then run:

```bash
chmod +x agentdesktop-installer
./agentdesktop-installer
```

The guided installer shows the components, per-user destination, service behavior, and network ownership boundary before changing files. The connector listener is loopback-only. The current Agent Gateway `llm.port` configuration binds a wildcard address and cannot express a loopback address, so the installer explicitly tells users to review Agent Gateway exposure. A public standalone package remains blocked on an address-capable Agent Gateway listener or equivalent local-only transport. The default root is `$HOME/.local/lib/agentdesktop`; after confirmation the installer verifies and extracts every embedded component, atomically activates the bundle, enables the user systemd service, and waits until the product is ready before reporting success. Agent Gateway remains a separate executable and process after extraction.

If setup fails, the installer creates an owner-only support report under `$XDG_STATE_HOME/agentdesktop`, or `$HOME/.local/state/agentdesktop` by default. It gives the user the report path and directs them to [open an issue](https://github.com/agentdesktop-dev/agentdesktop/issues/new) and attach it. Users are not asked to understand or run a health check.

For unattended installation or installation without starting the service:

```bash
./agentdesktop-installer install --yes
./agentdesktop-installer install --yes --no-start
./agentdesktop-installer install --yes --connect-agents
```

`--yes` accepts installation only and leaves AI agent settings unchanged. `--connect-agents` explicitly permits automatic configuration for scripted setup and requires the service to start.

Use `--root` to override the installation root. Re-running the installer upgrades a manifest-owned bundle and restores the prior tree if activation fails. The starter configuration remains Agent Gateway configuration; the connector does not interpret it.

Verify every manifest-owned file before use or removal:

```bash
"$HOME/.local/lib/agentdesktop/bin/agentdesktop-install" verify \
  --root "$HOME/.local/lib/agentdesktop"
```

The manifest records SHA-256 hashes. Verification, upgrade, and uninstall reject missing, modified, non-regular, symlinked, or path-traversing entries.

The guided installer enables and starts the generated hardened user-systemd unit by default. If installation used `--no-start`, enable it later with the installed control command:

```bash
"$HOME/.local/lib/agentdesktop/bin/agentdesktop-install" \
  service enable \
  --root "$HOME/.local/lib/agentdesktop"
```

The unit starts the connector and its separately packaged Agent Gateway process in standalone mode. Before uninstalling, stop and unlink it:

```bash
"$HOME/.local/lib/agentdesktop/bin/agentdesktop-install" \
  service disable \
  --root "$HOME/.local/lib/agentdesktop"
```

The self-contained installer installs its control command with the bundle. Remove only a manifest-owned bundle:

```bash
"$HOME/.local/lib/agentdesktop/bin/agentdesktop-install" uninstall \
  --root "$HOME/.local/lib/agentdesktop"
```

The installer refuses to remove directories without its manifest or bundles whose owned files were modified. It does not alter CA trust, create application profiles, cryptographically verify a publisher signature, or download/update components at runtime.

Build a development artifact from local release binaries and a compatible Agent Gateway checkout with:

```bash
scripts/build-embedded-installer.sh \
  ../agentgateway/target/ci/agentgateway \
  container/agentgateway-smoke.yaml
```

The resulting `target/release/agentdesktop-installer` is the only file delivered to the user. A build with no embedded payload remains available for repository-wide compilation and reports an actionable error if run.

## Build a managed development installer

Build a generic managed template and customizer, or create an organization-specific one-file executable from the strict non-secret bootstrap example:

```bash
scripts/build-managed-installer.sh
scripts/build-managed-installer.sh \
  examples/managed-organization.json \
  target/release/example-agentdesktop-installer
```

The generic template also accepts `--organization <file>` for two-file development. Managed installation leaves the service inactive and does not open a browser or change AI agent settings. The installed `agentdesktop connect-agents` command owns browser login, enrollment approval, service readiness, and separate Claude consent. Customize the executable before applying a publisher signature. See [Managed installer development](docs/deployment/managed-installer.md) for the schema, MDM ownership boundary, and current security limitations.

## Test

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
node --test tests/fixtures/fake-authorization-server.test.mjs
```

Tests use local fake Agent Gateway and authorization server processes. They do not contact Claude, Anthropic, an identity provider, or a remote service.

## Container kick-the-tires environment

The container environment runs Agent Gateway and the connector as separate containers on a private network. The connector remains bound to `127.0.0.1` inside its container and is not published to the host. The scripts use Podman when it is installed and fall back to Docker otherwise. Set `CONTAINER_ENGINE=podman` or `CONTAINER_ENGINE=docker` to override automatic selection.

Requirements:

- Podman 5 or newer, or Docker
- Network access on the first run to pull images and build dependencies

Start the credential-free environment:

```bash
./scripts/container-up.sh smoke
```

Send an Anthropic-shaped request through the connector to Agent Gateway:

```bash
./scripts/container-smoke.sh
```

The response should contain:

```json
{"id":"msg_smoke","type":"message","role":"assistant","content":[{"type":"text","text":"hello through Agent Desktop"}]}
```

The smoke gateway returns a deterministic direct response, so this path requires no provider credential. It is a manual environment for exploring:

```text
curl in connector container -> connector loopback listener -> Agent Gateway
```

The container scripts manage the environment and send manual requests; they are not test runners. Product behavior such as fail-closed handling is covered by the Rust integration tests under `tests/`.

Inspect the running environment:

```bash
container_engine="$(command -v podman || command -v docker)"
"$container_engine" ps --filter name=agentdesktop
"$container_engine" logs agentdesktop
"$container_engine" logs agentdesktop-gateway
"$container_engine" exec -it agentdesktop /bin/sh
```

Stop and remove the containers and network:

```bash
./scripts/container-down.sh
```

### Real Anthropic request

To replace the deterministic response with Agent Gateway's Anthropic provider:

```bash
export ANTHROPIC_API_KEY=your-provider-key
./scripts/container-up.sh anthropic
./scripts/container-smoke.sh
```

The API key is passed only to the Agent Gateway container. The connector receives the placeholder `x-api-key` header from the test client but never receives the provider credential from the environment.

Override the published Agent Gateway image when testing another version:

```bash
AGENTGATEWAY_IMAGE=ghcr.io/agentgateway/agentgateway:latest \
  ./scripts/container-up.sh smoke
```

### Claude Code with a mock provider

Run an actual pinned Claude Code CLI through the connector and Agent Gateway to a local mock Anthropic API:

```bash
./scripts/container-claude-smoke.sh
```

The command builds `@anthropic-ai/claude-code@2.1.212`, starts the container environment in `claude` mode, and prints:

```text
SMOKE_OK
```

This exercises the complete manual path without a provider credential:

```text
Claude Code -> connector loopback listener -> Agent Gateway -> mock Anthropic API
```

In this mode, the connector and Agent Gateway share a container network namespace. The connector runs in `standalone` mode and reaches Agent Gateway at `127.0.0.1:4000`; Agent Gateway remains a separate container and process. The mock supports Anthropic streaming messages, non-streaming messages, and token counting. The environment remains running for inspection; stop it with `./scripts/container-down.sh`.

Gateway-aware applications can bypass the connector and use the native local Agent Gateway path. Exercise that path with the same real Claude Code client:

```bash
./scripts/container-claude-smoke.sh native
```

The standalone smoke configuration also contains an Agent Gateway authorization rule requiring the local placeholder credential. Display one allowed response and one native Agent Gateway `403` denial through the connector:

```bash
./scripts/container-policy-smoke.sh
```

The connector does not evaluate this rule or rewrite either response. Users own equivalent authorization and provider configuration in Agent Gateway.

## Host Claude Code smoke test

In standalone forwarding, Agent Desktop preserves Claude's end-to-end application authentication headers while removing hop-by-hop headers required by HTTP proxy semantics. Configure Agent Gateway to accept the chosen placeholder or gateway credential and to provide the real Anthropic credential upstream.

With Agent Gateway and the connector running directly on the host:

```bash
cargo run -- connect-agents --yes
claude
```

Send a simple prompt and verify:

1. Claude receives a streamed response.
2. Agent Gateway records the request and applies its configured route and policies.
3. Stopping Agent Gateway causes the connector to return `502 Bad Gateway` with `x-agentdesktop-error: upstream-unavailable`.
4. The connector never attempts a direct connection to Anthropic.

Gateway-side DPoP validation and credential stripping, plus production enforcement of enrolled device identity, remain later increments.
