#!/usr/bin/env bash
set -euo pipefail

readonly root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly connector_container="agentdesktop"

source "$root_dir/scripts/container-engine.sh"

"$container_engine" exec "$connector_container" curl \
  --fail-with-body \
  --silent \
  --show-error \
  http://127.0.0.1:8080/v1/messages \
  --header 'content-type: application/json' \
  --header 'x-api-key: local-gateway-placeholder' \
  --data '{"model":"anthropic/claude-sonnet-4-20250514","max_tokens":64,"messages":[{"role":"user","content":"Say hello through Agent Desktop"}]}'
printf '\n'