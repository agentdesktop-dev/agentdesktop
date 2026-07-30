# Agent Gateway Edge Connector

An early, policy-free edge connector that forwards Claude Code HTTP traffic from a loopback listener to an independently running Agent Gateway.

The current pre-pre-MVP does not implement user identity, device enrollment, MDM integration, transparent capture, or telemetry export. See [AGENTS.md](AGENTS.md) for the architecture and incremental delivery plan.

## Run

Start Agent Gateway with a route that accepts Anthropic-compatible requests at `/v1/messages`, then run:

```bash
cargo run -- --upstream http://127.0.0.1:4000
```

The connector listens on `127.0.0.1:8080` by default. Override either setting with flags:

```bash
cargo run -- \
  --listen 127.0.0.1:8081 \
  --upstream https://agentgateway.example.internal
```

Or use environment variables:

```bash
export AGENTGATEWAY_EDGE_LISTEN=127.0.0.1:8081
export AGENTGATEWAY_EDGE_UPSTREAM=https://agentgateway.example.internal
cargo run
```

The listen address must be loopback. The upstream URL must use HTTP or HTTPS and may contain a path prefix, but not a query string or fragment.

## Test

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

Tests use a local fake Agent Gateway and do not contact Claude, Anthropic, or a remote service.

## Podman kick-the-tires environment

The Podman environment runs Agent Gateway and the connector as separate containers on a private network. The connector remains bound to `127.0.0.1` inside its container and is not published to the host. Use `podman exec` to exercise it.

Requirements:

- Podman 5 or newer
- Network access on the first run to pull images and build dependencies

Start the credential-free environment:

```bash
./scripts/podman-up.sh smoke
```

Send an Anthropic-shaped request through the connector to Agent Gateway:

```bash
./scripts/podman-smoke.sh
```

The response should contain:

```json
{"id":"msg_smoke","type":"message","role":"assistant","content":[{"type":"text","text":"hello through the edge connector"}]}
```

The smoke gateway returns a deterministic direct response, so this path requires no provider credential. It is a manual environment for exploring:

```text
curl in connector container -> connector loopback listener -> Agent Gateway
```

The Podman scripts manage the environment and send manual requests; they are not test runners. Product behavior such as fail-closed handling is covered by the Rust integration tests under `tests/`.

Inspect the running environment:

```bash
podman ps --filter name=agentgateway-edge
podman logs agentgateway-edge-connector
podman logs agentgateway-edge-gateway
podman exec -it agentgateway-edge-connector /bin/sh
```

Stop and remove the containers and network:

```bash
./scripts/podman-down.sh
```

### Real Anthropic request

To replace the deterministic response with Agent Gateway's Anthropic provider:

```bash
export ANTHROPIC_API_KEY=your-provider-key
./scripts/podman-up.sh anthropic
./scripts/podman-smoke.sh
```

The API key is passed only to the Agent Gateway container. The connector receives the placeholder `x-api-key` header from the test client but never receives the provider credential from the environment.

Override the published Agent Gateway image when testing another version:

```bash
AGENTGATEWAY_IMAGE=ghcr.io/agentgateway/agentgateway:latest \
  ./scripts/podman-up.sh smoke
```

## Claude Code smoke test

This milestone forwards Claude's incoming authentication headers unchanged. Configure Agent Gateway to accept the chosen placeholder or gateway credential and to provide the real Anthropic credential upstream.

With Agent Gateway and the connector running:

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