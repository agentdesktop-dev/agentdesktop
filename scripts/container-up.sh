#!/usr/bin/env bash
set -euo pipefail

readonly root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly network="agentdesktop-smoke"
readonly connector_container="agentdesktop"
readonly mock_container="agentdesktop-anthropic-mock"
readonly connector_image="localhost/agentdesktop:dev"
readonly mock_image="localhost/agentdesktop-anthropic-mock:dev"
readonly mode="${1:-smoke}"

source "$root_dir/scripts/container-engine.sh"

case "$mode" in
  smoke)
    gateway_config="agentgateway-native-smoke.yaml"
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
fi

connector_args=(
  run --detach
  --name "$connector_container"
  --network "$network"
  --volume "$root_dir/container/$gateway_config:/etc/agentgateway/config.yaml:ro,Z"
  --env AGENTDESKTOP_MODE=standalone
  --env AGENTDESKTOP_LISTEN=127.0.0.1:8080
  --env AGENTDESKTOP_UPSTREAM=http://127.0.0.1:15008
  --env AGENTDESKTOP_NATIVE_TARGET=native.agentdesktop.internal:4000
)
if [[ "$mode" == "anthropic" ]]; then
  connector_args+=(--env ANTHROPIC_API_KEY)
fi
connector_args+=("$connector_image" serve --gateway-binary /usr/local/bin/agentgateway --gateway-config /etc/agentgateway/config.yaml)
"$container_engine" "${connector_args[@]}" >/dev/null

readiness_url=http://127.0.0.1:15021/healthz/ready

if "$container_engine" exec "$connector_container" \
  curl --fail --silent --output /dev/null \
    --retry 30 --retry-all-errors --retry-delay 1 \
    --connect-timeout 1 --max-time 35 \
    "$readiness_url"; then
  cat <<EOF
Agent Gateway and Agent Desktop are ready in $mode mode using $container_engine.

Run the smoke request:
  ./scripts/container-smoke.sh

Run the real Claude Code smoke path:
  ./scripts/container-claude-smoke.sh

Exercise Agent Gateway policy allow and deny:
  ./scripts/container-policy-smoke.sh

Open a shell in the connector container:
  $container_engine exec -it $connector_container /bin/sh

Stop and remove the environment:
  ./scripts/container-down.sh
EOF
  exit 0
fi

echo "Agent Gateway did not become ready" >&2
"$container_engine" logs "$connector_container" >&2 || true
exit 1