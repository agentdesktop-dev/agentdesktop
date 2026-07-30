#!/usr/bin/env bash
set -euo pipefail

readonly root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly network="agentgateway-edge-smoke"
readonly gateway_container="agentgateway-edge-gateway"
readonly connector_container="agentgateway-edge-connector"
readonly connector_image="localhost/agentgateway-edge-connector:dev"
readonly gateway_image="${AGENTGATEWAY_IMAGE:-ghcr.io/agentgateway/agentgateway:latest}"
readonly mode="${1:-smoke}"

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
  *)
    echo "usage: $0 [smoke|anthropic]" >&2
    exit 2
    ;;
esac

"$root_dir/scripts/podman-down.sh" >/dev/null

podman build --tag "$connector_image" "$root_dir"
podman network create "$network" >/dev/null

gateway_args=(
  run --detach
  --name "$gateway_container"
  --network "$network"
  --network-alias agentgateway
  --volume "$root_dir/container/$gateway_config:/etc/agentgateway/config.yaml:ro,Z"
)
if [[ "$mode" == "anthropic" ]]; then
  gateway_args+=(--env ANTHROPIC_API_KEY)
fi
gateway_args+=("$gateway_image" -f /etc/agentgateway/config.yaml)
podman "${gateway_args[@]}" >/dev/null

podman run --detach \
  --name "$connector_container" \
  --network "$network" \
  --env AGENTGATEWAY_EDGE_LISTEN=127.0.0.1:8080 \
  --env AGENTGATEWAY_EDGE_UPSTREAM=http://agentgateway:4000 \
  "$connector_image" >/dev/null

if podman exec "$connector_container" \
  curl --fail --silent --output /dev/null \
    --retry 30 --retry-all-errors --retry-delay 1 \
    --connect-timeout 1 --max-time 35 \
    http://agentgateway:15021/healthz/ready; then
  cat <<EOF
Agent Gateway and the edge connector are ready in $mode mode.

Run the smoke request:
  ./scripts/podman-smoke.sh

Open a shell in the connector container:
  podman exec -it $connector_container /bin/sh

Stop and remove the environment:
  ./scripts/podman-down.sh
EOF
  exit 0
fi

echo "Agent Gateway did not become ready" >&2
podman logs "$gateway_container" >&2 || true
exit 1
