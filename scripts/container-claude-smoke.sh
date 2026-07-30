#!/usr/bin/env bash
set -euo pipefail

readonly root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly connector_container="agentgateway-edge-connector"
readonly claude_image="localhost/agentgateway-edge-claude:2.1.212"
readonly path="${1:-connector}"

source "$root_dir/scripts/container-engine.sh"

case "$path" in
  connector)
    base_url=http://127.0.0.1:8080
    ;;
  native)
    base_url=http://127.0.0.1:4000
    ;;
  *)
    echo "usage: $0 [connector|native]" >&2
    exit 2
    ;;
esac

"$root_dir/scripts/container-up.sh" claude
"$container_engine" build \
  --tag "$claude_image" \
  --file "$root_dir/container/ClaudeCode.Dockerfile" \
  "$root_dir/container"

"$container_engine" run --rm \
  --network "container:$connector_container" \
  --env ANTHROPIC_BASE_URL="$base_url" \
  --env ANTHROPIC_API_KEY=local-gateway-placeholder \
  "$claude_image" \
  --bare \
  --print \
  --output-format text \
  --model sonnet \
  "Reply with exactly SMOKE_OK"