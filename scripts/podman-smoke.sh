#!/usr/bin/env bash
set -euo pipefail

readonly connector_container="agentgateway-edge-connector"

podman exec "$connector_container" curl \
  --fail-with-body \
  --silent \
  --show-error \
  http://127.0.0.1:8080/v1/messages \
  --header 'content-type: application/json' \
  --header 'x-api-key: local-gateway-placeholder' \
  --data '{"model":"anthropic/claude-sonnet-4-20250514","max_tokens":64,"messages":[{"role":"user","content":"Say hello through the edge connector"}]}'
printf '\n'
