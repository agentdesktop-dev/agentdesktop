#!/usr/bin/env bash
set -euo pipefail

readonly root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly connector_container="agentgateway-edge-connector"
readonly request='{"model":"claude-sonnet-5","max_tokens":64,"messages":[{"role":"user","content":"Reply with exactly SMOKE_OK"}]}'

source "$root_dir/scripts/container-engine.sh"

"$root_dir/scripts/container-up.sh" claude

printf 'Allowed by Agent Gateway policy:\n'
allowed_response="$("$container_engine" exec "$connector_container" curl \
  --fail-with-body \
  --silent \
  --show-error \
  http://127.0.0.1:8080/v1/messages \
  --header 'content-type: application/json' \
  --header 'x-api-key: local-gateway-placeholder' \
  --data "$request")"
printf '%s\n' "$allowed_response"
grep --quiet 'SMOKE_OK' <<<"$allowed_response"

printf '\nDenied by Agent Gateway policy:\n'
denied_response="$("$container_engine" exec "$connector_container" curl \
  --silent \
  --show-error \
  --write-out $'\n%{http_code}' \
  http://127.0.0.1:8080/v1/messages \
  --header 'content-type: application/json' \
  --header 'x-api-key: invalid-placeholder' \
  --data "$request")"
denied_status="${denied_response##*$'\n'}"
denied_body="${denied_response%$'\n'*}"
printf 'HTTP %s\n%s\n' "$denied_status" "$denied_body"

[[ "$denied_status" == 403 ]]
grep --quiet '"code":"model_authorization_denied"' <<<"$denied_body"