#!/usr/bin/env bash
set -euo pipefail

readonly root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly connector_container="agentdesktop"
readonly claude_image="localhost/agentdesktop-claude:2.1.212"

source "$root_dir/scripts/container-engine.sh"

readonly base_url=http://127.0.0.1:8080

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