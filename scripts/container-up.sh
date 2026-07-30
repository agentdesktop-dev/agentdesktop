#!/usr/bin/env bash
set -euo pipefail

readonly root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly network="agentgateway-edge-smoke"
readonly gateway_container="agentgateway-edge-gateway"
readonly connector_container="agentgateway-edge-connector"
readonly mock_container="agentgateway-edge-anthropic-mock"
readonly connector_image="localhost/agentgateway-edge-connector:dev"
readonly gateway_image="${AGENTGATEWAY_IMAGE:-ghcr.io/agentgateway/agentgateway:latest}"
readonly mock_image="localhost/agentgateway-edge-anthropic-mock:dev"
readonly mode="${1:-smoke}"

source "$root_dir/scripts/container-engine.sh"

case "$mode" in
  smoke)
    gateway_config="agentgateway-smoke.yaml"
    ;;
  anthropic)
    gateway_config="agentgateway-anthropic.yaml"
    if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
      echo "ANTHROPIC_API_KEY is required for anthropic mode" >&2
      exit 2
    fi
    ;;
  claude)
    gateway_config="agentgateway-claude.yaml"
    ;;
  *)
    echo "usage: $0 [smoke|anthropic|claude]" >&2
    exit 2
    ;;
esac

"$root_dir/scripts/container-down.sh" >/dev/null

"$container_engine" build --tag "$connector_image" "$root_dir"
"$container_engine" network create "$network" >/dev/null

if [[ "$mode" == "claude" ]]; then
  "$container_engine" build \
    --tag "$mock_image" \
    --file "$root_dir/container/MockAnthropic.Dockerfile" \
    "$root_dir/container"
  "$container_engine" run --detach \
    --name "$mock_container" \
    --network "$network" \
    --network-alias anthropic-mock \
    "$mock_image" >/dev/null

  "$container_engine" run --detach \
    --name "$connector_container" \
    --network "$network" \
    --env AGENTGATEWAY_EDGE_MODE=standalone \
    --env AGENTGATEWAY_EDGE_LISTEN=127.0.0.1:8080 \
    --env AGENTGATEWAY_EDGE_UPSTREAM=http://127.0.0.1:4000 \
    "$connector_image" >/dev/null
fi

gateway_args=(
  run --detach
  --name "$gateway_container"
  --volume "$root_dir/container/$gateway_config:/etc/agentgateway/config.yaml:ro,Z"
)
if [[ "$mode" == "claude" ]]; then
  gateway_args+=(--network "container:$connector_container")
else
  gateway_args+=(--network "$network" --network-alias agentgateway)
fi
if [[ "$mode" == "anthropic" ]]; then
  gateway_args+=(--env ANTHROPIC_API_KEY)
fi
gateway_args+=("$gateway_image" -f /etc/agentgateway/config.yaml)
"$container_engine" "${gateway_args[@]}" >/dev/null

if [[ "$mode" != "claude" ]]; then
  "$container_engine" run --detach \
    --name "$connector_container" \
    --network "$network" \
    --env AGENTGATEWAY_EDGE_MODE=managed \
    --env AGENTGATEWAY_EDGE_LISTEN=127.0.0.1:8080 \
    --env AGENTGATEWAY_EDGE_UPSTREAM=http://agentgateway:4000 \
    "$connector_image" >/dev/null
fi

if [[ "$mode" == "claude" ]]; then
  readiness_url=http://127.0.0.1:15021/healthz/ready
else
  readiness_url=http://agentgateway:15021/healthz/ready
fi

if "$container_engine" exec "$connector_container" \
  curl --fail --silent --output /dev/null \
    --retry 30 --retry-all-errors --retry-delay 1 \
    --connect-timeout 1 --max-time 35 \
    "$readiness_url"; then
  cat <<EOF
Agent Gateway and the edge connector are ready in $mode mode using $container_engine.

Run the smoke request:
  ./scripts/container-smoke.sh

Run the real Claude Code smoke path:
  ./scripts/container-claude-smoke.sh

Open a shell in the connector container:
  $container_engine exec -it $connector_container /bin/sh

Stop and remove the environment:
  ./scripts/container-down.sh
EOF
  exit 0
fi

echo "Agent Gateway did not become ready" >&2
"$container_engine" logs "$gateway_container" >&2 || true
exit 1