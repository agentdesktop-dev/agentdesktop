#!/usr/bin/env bash
set -euo pipefail

readonly root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly connector_container="agentgateway-edge-connector"
readonly claude_image="localhost/agentgateway-edge-claude:2.1.212"

source "$root_dir/scripts/container-engine.sh"

"$root_dir/scripts/container-up.sh" claude
"$container_engine" build \
  --tag "$claude_image" \
  --file "$root_dir/container/ClaudeCode.Dockerfile" \
  "$root_dir/container"

"$container_engine" run --rm \
  --network "container:$connector_container" \
  --env ANTHROPIC_BASE_URL=http://127.0.0.1:8080 \
  --env ANTHROPIC_API_KEY=local-gateway-placeholder \
  "$claude_image" \
  --bare \
  --print \
  --output-format text \
  --model sonnet \
  "Reply with exactly SMOKE_OK"