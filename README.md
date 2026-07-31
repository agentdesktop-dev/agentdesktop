# Agent Gateway Edge Connector

An early, policy-free edge connector that forwards Claude Code HTTP traffic from a loopback listener to an independently running Agent Gateway.

The current pre-pre-MVP includes experimental managed browser login, DPoP-authenticated forwarding, and refresh restoration, but does not yet implement device enrollment, MDM integration, transparent capture, or telemetry export. See [AGENTS.md](AGENTS.md) for the architecture and incremental delivery plan.

For a local installation, including credential ownership, file permissions, logs, retention, and removal, see [Standalone Operations](docs/deployment/standalone.md).

Verified progress and environment-dependent blockers are tracked in [Phase Status](docs/development/phase-status.md).
Tested platform behavior is listed in [Platform Compatibility](docs/compatibility/platforms.md).

The managed user/device trust boundary is documented in [Managed Identity Contract v1](docs/architecture/managed-identity-v1.md). Browser PKCE login and connector-instance DPoP proof are implemented; the rest remains a design contract.

## Managed identity storage preflight

Managed identity is experimental. Validate credential persistence before login or installation:

```bash
cargo run --bin agentgateway-edge-identity -- storage-check
```

The default `auto` mode uses Linux Secret Service when a write/read/delete preflight succeeds and otherwise persists an owner-only protected-file backend. Require Secret Service with no fallback using:

```bash
cargo run --bin agentgateway-edge-identity -- \
  storage-check --credential-storage secret-service
```

Select the protected file explicitly with `--credential-storage file`. The selected backend is persisted and revalidated on later startup; runtime does not silently switch stores. Override the XDG-based identity directory with `AGENTGATEWAY_EDGE_IDENTITY_DIR`.

## Experimental managed login

Run browser Authorization Code login against an issuer that advertises PKCE `S256`, DPoP `ES256`, and an ES256 JWT signing key through discovery:

```bash
cargo run --bin agentgateway-edge-identity -- login \
  --issuer https://identity.example/ \
  --client-id agentgateway-edge \
  --audience https://gateway.example \
  --scope agentgateway.invoke \
  --gateway-origin https://gateway.example
```

The command validates credential storage before opening the browser, listens on an ephemeral loopback callback, verifies the access-token signature and issuer, audience, expiry, scope, and DPoP binding, then persists the token and DPoP key. `--gateway-origin` must be the upstream origin without a path. `--no-open` prints the authorization URL for non-desktop testing.

Attach the persisted session to managed forwarding by supplying the same issuer:

```bash
cargo run -- \
  --mode managed \
  --upstream https://gateway.example \
  --identity-issuer https://identity.example/
```

The connector fails at startup if storage or the matching session is unavailable. Before expiry it serializes refresh, uses the same DPoP key, verifies the rotated access token, persists the new refresh token, and replaces the managed upstream connection pool before forwarding continues. Refresh failure fails new requests closed locally; the issuer must enforce refresh-token rotation and reuse detection. For each request the connector removes application-supplied connector identity headers, preserves the application `Authorization` header, and adds a fresh `Proxy-Authorization: DPoP` token and DPoP proof. The DPoP key is not verified organizational device identity, and Agent Gateway still needs the contract's DPoP validation and credential-stripping changes before this is a trusted end-to-end managed identity path.

Delete only the matching local session with:

```bash
cargo run --bin agentgateway-edge-identity -- logout \
  --issuer https://identity.example/ \
  --gateway-origin https://gateway.example
```

This removes local credentials but does not invoke an issuer revocation endpoint.

## Run

Start Agent Gateway with a route that accepts Anthropic-compatible requests at `/v1/messages`, then run:

```bash
cargo run -- \
  --mode standalone \
  --upstream http://127.0.0.1:4000
```

The connector listens on `127.0.0.1:8080` by default. Override either setting with flags:

```bash
cargo run -- \
  --mode managed \
  --listen 127.0.0.1:8081 \
  --upstream https://agentgateway.example.internal
```

Or use environment variables:

```bash
export AGENTGATEWAY_EDGE_MODE=managed
export AGENTGATEWAY_EDGE_LISTEN=127.0.0.1:8081
export AGENTGATEWAY_EDGE_UPSTREAM=https://agentgateway.example.internal
cargo run
```

The deployment mode is required. `standalone` accepts only a local Agent Gateway at `localhost` or a loopback IP; `managed` permits a remote upstream. The listen address must always be loopback. The upstream URL must use HTTP or HTTPS and may contain a path prefix, but not a query string or fragment.

Forwarding defaults to a 5-second connection timeout, 30-second response-header timeout, 10-second graceful-shutdown deadline, and 128 in-flight requests. Override them with `--connect-timeout-ms`, `--request-timeout-ms`, `--shutdown-timeout-ms`, and `--max-in-flight`. Concurrency permits remain held until streamed response bodies finish or are dropped. Overload returns `503` with `x-agentgateway-edge-error: overloaded`; an upstream response-header timeout returns `504` with `x-agentgateway-edge-error: upstream-timeout`.

The connector does not retry forwarded requests. In particular, non-idempotent and streaming requests are never replayed after an upstream disconnect.

The connector emits JSON structured logs to standard error. Runtime events use fixed event and reason values and omit upstream URLs, paths, queries, request and response bodies, and authorization headers. Valid W3C `traceparent` and `tracestate` headers are propagated to Agent Gateway; when `traceparent` is absent or malformed, the connector generates a new context and removes untrusted `tracestate`. The active `traceparent` is returned on success and stable local error responses. OpenTelemetry span and metric export is not implemented yet.

## Configure Claude Code

Launch Claude Code directly against the default standalone Agent Gateway listener:

```bash
cargo run --bin agentgateway-edge-claude -- --path native
```

Or route it through the connector:

```bash
cargo run --bin agentgateway-edge-claude -- --path connector
```

The helper selects exactly one path and launches `claude` with `ANTHROPIC_BASE_URL` and a local placeholder `ANTHROPIC_API_KEY`. The native and connector defaults are `http://127.0.0.1:4000` and `http://127.0.0.1:8080`, respectively. Override the selected loopback endpoint and pass Claude arguments after `--`:

```bash
cargo run --bin agentgateway-edge-claude -- \
  --path native \
  --base-url http://127.0.0.1:4040 \
  -- --model sonnet
```

Set `AGENTGATEWAY_EDGE_CLAUDE_CREDENTIAL` when the local Agent Gateway policy expects a different placeholder. This value is sent to Agent Gateway, not the AI provider credential. Agent Gateway remains responsible for replacing or removing application credentials before provider forwarding.

In standalone mode, the connector can optionally own the lifecycle of a separately installed Agent Gateway process:

```bash
cargo run -- \
  --mode standalone \
  --upstream http://127.0.0.1:4000 \
  --gateway-binary /usr/local/bin/agentgateway \
  --gateway-config "$HOME/.config/agentgateway/config.yaml"
```

The binary and config options must be provided together. The connector starts `agentgateway -f <config>`, waits up to 10 seconds for its configured loopback upstream to accept TCP connections, and only then opens the application listener. It stops Agent Gateway during connector shutdown and exits if the local process exits unexpectedly. Agent Gateway remains a separate process and retains ownership of policy and provider credentials.

Query connector and gateway reachability on the same loopback listener:

```bash
curl http://127.0.0.1:8080/_agentgateway/healthz
```

A reachable gateway returns `200 OK`:

```json
{"status":"ok","mode":"standalone","gateway":"reachable"}
```

An unreachable gateway returns `503 Service Unavailable` with `status` set to `degraded`. The check establishes a fresh TCP connection to the configured upstream. It reports connector liveness and gateway reachability; it does not interpret Agent Gateway configuration or policy health.

Read the local operational status API:

```bash
curl http://127.0.0.1:8080/_agentgateway/status
```

The response contains connector version, deployment mode, gateway reachability, identity readiness, active/maximum forwarding count, configured timeout values, and fixed counters for request attempts, upstream responses, identity failures, overload rejections, upstream timeouts, and upstream failures. Counters have no request-, destination-, process-, or identity-derived labels. The API does not expose gateway addresses, identity claims, credentials, application traffic, or policy. This API is the backend for a future local UI and telemetry exporter; no graphical UI is implemented yet.

## Install a standalone bundle

Build the connector binaries, then stage them with a separately obtained Agent Gateway binary and starter configuration into a dedicated installation root:

```bash
cargo build --release
cargo run --bin agentgateway-edge-install -- install \
  --root "$HOME/.local/lib/agentgateway-edge" \
  --connector target/release/agentgateway-edge-connector \
  --identity target/release/agentgateway-edge-identity \
  --claude target/release/agentgateway-edge-claude \
  --agentgateway /path/to/agentgateway \
  --starter-config container/agentgateway-smoke.yaml
```

Installation builds a complete sibling staging tree, verifies all inputs before replacing the active tree, and restores the previous tree if activation fails. Re-running the command upgrades the dedicated root. The starter configuration is installed as an example and remains Agent Gateway configuration; the connector does not interpret it.

Verify every manifest-owned file before use or removal:

```bash
cargo run --bin agentgateway-edge-install -- verify \
  --root "$HOME/.local/lib/agentgateway-edge"
```

The manifest records SHA-256 hashes. Verification, upgrade, and uninstall reject missing, modified, non-regular, symlinked, or path-traversing entries.

Remove only a manifest-owned bundle:

```bash
cargo run --bin agentgateway-edge-install -- uninstall \
  --root "$HOME/.local/lib/agentgateway-edge"
```

The installer refuses to remove directories without its manifest or bundles whose owned files were modified. It does not yet install a system/user service, alter CA trust, create application profiles, cryptographically verify a publisher signature, or download/update either binary.

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
{"id":"msg_smoke","type":"message","role":"assistant","content":[{"type":"text","text":"hello through the edge connector"}]}
```

The smoke gateway returns a deterministic direct response, so this path requires no provider credential. It is a manual environment for exploring:

```text
curl in connector container -> connector loopback listener -> Agent Gateway
```

The container scripts manage the environment and send manual requests; they are not test runners. Product behavior such as fail-closed handling is covered by the Rust integration tests under `tests/`.

Inspect the running environment:

```bash
container_engine="$(command -v podman || command -v docker)"
"$container_engine" ps --filter name=agentgateway-edge
"$container_engine" logs agentgateway-edge-connector
"$container_engine" logs agentgateway-edge-gateway
"$container_engine" exec -it agentgateway-edge-connector /bin/sh
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

This milestone forwards Claude's incoming authentication headers unchanged. Configure Agent Gateway to accept the chosen placeholder or gateway credential and to provide the real Anthropic credential upstream.

With Agent Gateway and the connector running directly on the host:

```bash
cargo run --bin agentgateway-edge-claude -- --path connector
```

Send a simple prompt and verify:

1. Claude receives a streamed response.
2. Agent Gateway records the request and applies its configured route and policies.
3. Stopping Agent Gateway causes the connector to return `502 Bad Gateway` with `x-agentgateway-edge-error: upstream-unavailable`.
4. The connector never attempts a direct connection to Anthropic.

Gateway-side DPoP validation and stripping and enrolled device identity are later increments.