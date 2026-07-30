# Agent Gateway Edge Connector

An early, policy-free edge connector that forwards Claude Code HTTP traffic from a loopback listener to an independently running Agent Gateway.

The current pre-pre-MVP does not implement user identity, device enrollment, MDM integration, transparent capture, or telemetry export. See [AGENTS.md](AGENTS.md) for the architecture and incremental delivery plan.

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

In standalone mode, the connector can optionally own the lifecycle of a separately installed Agent Gateway process:

```bash
cargo run -- \
  --mode standalone \
  --upstream http://127.0.0.1:4000 \
  --gateway-binary /usr/local/bin/agentgateway \
  --gateway-config "$HOME/.config/agentgateway/config.yaml"
```

The binary and config options must be provided together. The connector starts `agentgateway -f <config>`, stops it during connector shutdown, and exits if the local Agent Gateway process exits unexpectedly. Agent Gateway remains a separate process and retains ownership of policy and provider credentials.

Query connector and gateway reachability on the same loopback listener:

```bash
curl http://127.0.0.1:8080/_agentgateway/healthz
```

A reachable gateway returns `200 OK`:

```json
{"status":"ok","mode":"standalone","gateway":"reachable"}
```

An unreachable gateway returns `503 Service Unavailable` with `status` set to `degraded`. The check establishes a fresh TCP connection to the configured upstream. It reports connector liveness and gateway reachability; it does not interpret Agent Gateway configuration or policy health.

## Test

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

Tests use a local fake Agent Gateway and do not contact Claude, Anthropic, or a remote service.

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

The mock supports Anthropic streaming messages, non-streaming messages, and token counting. The environment remains running for inspection; stop it with `./scripts/container-down.sh`.

## Host Claude Code smoke test

This milestone forwards Claude's incoming authentication headers unchanged. Configure Agent Gateway to accept the chosen placeholder or gateway credential and to provide the real Anthropic credential upstream.

With Agent Gateway and the connector running directly on the host:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8080
export ANTHROPIC_AUTH_TOKEN=local-gateway-placeholder
claude
```

Send a simple prompt and verify:

1. Claude receives a streamed response.
2. Agent Gateway records the request and applies its configured route and policies.
3. Stopping Agent Gateway causes the connector to return `502 Bad Gateway` with `x-agentgateway-edge-error: upstream-unavailable`.
4. The connector never attempts a direct connection to Anthropic.

OAuth, device-bound identity, and connector-only credential stripping are later increments.