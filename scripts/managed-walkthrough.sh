#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly root_dir
readonly walkthrough_dir="$root_dir/examples/managed-walkthrough"
readonly pod="agentdesktop-managed-walkthrough"
readonly control_plane_image="localhost/agentdesktop-control-plane:managed-walkthrough"
readonly gateway_image="${AGENTGATEWAY_IMAGE:-ghcr.io/agentgateway/agentgateway:v1.4.1}"
readonly server_dns="${AGENTDESKTOP_WALKTHROUGH_SERVER_DNS:-localhost}"
readonly issuer="https://${server_dns}:18080/"
readonly admin_oauth_origin="${AGENTDESKTOP_WALKTHROUGH_ADMIN_OAUTH_ORIGIN:-http://127.0.0.1:18082/}"
readonly infra_container="${pod}-infra"
readonly stack_label="dev.agentdesktop.walkthrough=$pod"

# shellcheck source=container-engine.sh
source "$root_dir/scripts/container-engine.sh"

# shellcheck disable=SC2154
container() {
  "$container_engine" "$@"
}

stack_container_names() {
  if [[ "$container_engine" == podman ]]; then
    container ps --all --filter "pod=$pod" --format '{{.Names}}'
  else
    container ps --all --filter "label=$stack_label" --format '{{.Names}}'
  fi
}

remove_stack() {
  if [[ "$container_engine" == podman ]]; then
    container pod rm --force "$pod" >/dev/null 2>&1 || true
    return
  fi
  local container_name
  while read -r container_name; do
    if [[ -n "$container_name" && "$container_name" != "$infra_container" ]]; then
      container rm --force "$container_name" >/dev/null 2>&1 || true
    fi
  done < <(stack_container_names)
  container rm --force "$infra_container" >/dev/null 2>&1 || true
}

create_stack() {
  if [[ "$container_engine" == podman ]]; then
    container pod create \
      --name "$pod" \
      --add-host "${server_dns}:127.0.0.1" \
      --publish 127.0.0.1:18080:18080 \
      --publish 127.0.0.1:18082:18082 \
      --publish 127.0.0.1:18081:18081 \
      --publish 127.0.0.1:8090:8090 \
      --publish 127.0.0.1:8091:8091 \
      --publish 127.0.0.1:8443:8443 \
      --publish 127.0.0.1:15021:15021 \
      >/dev/null
    return
  fi
  container run --detach \
    --name "$infra_container" \
    --label "$stack_label" \
    --add-host "${server_dns}:127.0.0.1" \
    --publish 127.0.0.1:18080:18080 \
    --publish 127.0.0.1:18082:18082 \
    --publish 127.0.0.1:18081:18081 \
    --publish 127.0.0.1:8090:8090 \
    --publish 127.0.0.1:8091:8091 \
    --publish 127.0.0.1:8443:8443 \
    --publish 127.0.0.1:15021:15021 \
    docker.io/library/alpine:3.22 sleep infinity \
    >/dev/null
}

run_in_stack() {
  if [[ "$container_engine" == podman ]]; then
    container run --detach --pod "$pod" "$@"
  else
    container run --detach \
      --network "container:$infra_container" \
      --label "$stack_label" \
      "$@"
  fi
}

remove_runtime_state() {
  rm -rf "$walkthrough_dir/certs"
}

rollback_stack() {
  echo "walkthrough startup failed; container logs follow" >&2
  local container_name
  while read -r container_name; do
    container logs "$container_name" >&2 || true
  done < <(stack_container_names)
  remove_stack
  remove_runtime_state
}

wait_for() {
  local description="$1"
  shift
  for _ in {1..60}; do
    if "$@" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "$description did not become ready" >&2
  return 1
}

start() {
  local provider_mode="${1:-mock}"
  if [[ "$provider_mode" == anthropic && -z "${ANTHROPIC_API_KEY:-}" ]]; then
    echo "start-anthropic requires ANTHROPIC_API_KEY in the launching environment" >&2
    return 2
  fi
  remove_stack
  trap 'rollback_stack' ERR
  AGENTDESKTOP_WALKTHROUGH_SERVER_DNS="$server_dns" \
    "$walkthrough_dir/generate-certificates.sh"

  local system_bundle=""
  local candidate
  for candidate in \
    /etc/ssl/cert.pem \
    /etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem \
    /etc/ssl/certs/ca-certificates.crt \
    /etc/pki/tls/certs/ca-bundle.crt; do
    if [[ -r "$candidate" ]]; then
      system_bundle="$candidate"
      break
    fi
  done
  if [[ -z "$system_bundle" ]]; then
    echo "could not locate the system CA bundle" >&2
    exit 1
  fi
  cat \
    "$system_bundle" \
    "$walkthrough_dir/certs/enrollment-ca.crt" \
    "$walkthrough_dir/certs/gateway-server-ca.crt" \
    >"$walkthrough_dir/certs/process-ca-bundle.crt"

  container build --tag "$control_plane_image" "$root_dir/control-plane"
  create_stack

  run_in_stack \
    --name agentdesktop-walkthrough-oidc \
    --volume "$root_dir/tests/fixtures/fake-authorization-server.mjs:/app/fake-authorization-server.mjs:ro,Z" \
    --volume "$walkthrough_dir/certs:/certs:ro,Z" \
    --user 0 \
    --env AGENTDESKTOP_FAKE_ISSUER="$issuer" \
    --env AGENTDESKTOP_FAKE_LISTEN_HOST=0.0.0.0 \
    --env AGENTDESKTOP_FAKE_PORT=18080 \
    --env AGENTDESKTOP_FAKE_ADMIN_SCOPE=agentdesktop.enrollment.admin \
    --env AGENTDESKTOP_FAKE_SECONDARY_HTTP_PORT=18082 \
    --env AGENTDESKTOP_FAKE_TLS_KEY=/certs/enrollment-server.key \
    --env AGENTDESKTOP_FAKE_TLS_CERTIFICATE=/certs/enrollment-server.crt \
    docker.io/library/node:22-alpine \
    node /app/fake-authorization-server.mjs \
    >/dev/null

  if [[ "$provider_mode" == mock ]]; then
    run_in_stack \
      --name agentdesktop-walkthrough-anthropic \
      --volume "$root_dir/container/mock-anthropic.mjs:/app/mock-anthropic.mjs:ro,Z" \
      --env MOCK_ANTHROPIC_HOST=0.0.0.0 \
      --env MOCK_ANTHROPIC_PORT=18081 \
      --env MOCK_ANTHROPIC_API_KEY=mock-provider-key \
      docker.io/library/node:22-alpine \
      node /app/mock-anthropic.mjs \
      >/dev/null
  fi

  run_in_stack \
    --name agentdesktop-walkthrough-postgres \
    --env POSTGRES_USER=agentdesktop \
    --env POSTGRES_PASSWORD=agentdesktop \
    --env POSTGRES_DB=agentdesktop \
    docker.io/library/postgres:17 \
    >/dev/null

  wait_for "mock OIDC" curl --noproxy '*' --fail --silent \
    --cacert "$walkthrough_dir/certs/gateway-server-ca.crt" \
    --resolve "${server_dns}:18080:127.0.0.1" \
    "${issuer}jwks"
  if [[ "$provider_mode" == mock ]]; then
    wait_for "mock Anthropic" curl --noproxy '*' --fail --silent http://127.0.0.1:18081/v1/messages/count_tokens \
      -H 'content-type: application/json' -H 'x-api-key: mock-provider-key' --data '{}'
  fi
  wait_for "PostgreSQL" container exec agentdesktop-walkthrough-postgres \
    psql -U agentdesktop -d agentdesktop -c 'SELECT 1'

  run_in_stack \
    --name agentdesktop-walkthrough-enrollment \
    --user 0 \
    --volume "$walkthrough_dir/certs:/certs:ro,Z" \
    --env DATABASE_URL='postgres://agentdesktop:agentdesktop@127.0.0.1:5432/agentdesktop?sslmode=disable' \
    --env OAUTH_ISSUER="$issuer" \
    --env OAUTH_AUDIENCE=agentdesktop \
    --env OAUTH_SCOPE=agentgateway.invoke \
    --env ADMIN_OAUTH_SCOPE=agentdesktop.enrollment.admin \
    --env ADMIN_UI_OAUTH_CLIENT_ID=agentdesktop-test \
    --env ADMIN_UI_AUTHORIZATION_ENDPOINT="${admin_oauth_origin}authorize" \
    --env ADMIN_UI_TOKEN_ENDPOINT="${admin_oauth_origin}token" \
    --env ADMIN_UI_LISTEN_ADDRESS=0.0.0.0:8091 \
    --env ORGANIZATION_ID=11111111-1111-4111-8111-111111111111 \
    --env 'ORGANIZATION_NAME=Walkthrough Organization' \
    --env CA_SIGNER_BACKEND=file \
    --env CA_CERTIFICATE_PATH=/certs/enrollment-ca.crt \
    --env CA_PRIVATE_KEY_PATH=/certs/enrollment-ca.key \
    --env SERVER_TLS_CERTIFICATE_PATH=/certs/enrollment-server.crt \
    --env SERVER_TLS_PRIVATE_KEY_PATH=/certs/enrollment-server.key \
    --env SSL_CERT_FILE=/certs/process-ca-bundle.crt \
    --env MTLS_TRUST_DOMAIN=agentdesktop.test \
    --env LISTEN_ADDRESS=0.0.0.0:8090 \
    "$control_plane_image" -migrate \
    >/dev/null

  wait_for "enrollment service" curl --noproxy '*' --fail --silent \
    --cacert "$walkthrough_dir/certs/gateway-server-ca.crt" \
    https://127.0.0.1:8090/healthz

  local gateway_config=/walkthrough/agentgateway.yaml
  local gateway_environment=()
  if [[ "$provider_mode" == anthropic ]]; then
    gateway_config=/walkthrough/agentgateway-anthropic.yaml
    gateway_environment=(--env ANTHROPIC_API_KEY)
  else
    gateway_environment=(--env ANTHROPIC_API_KEY=mock-provider-key)
  fi

  run_in_stack \
    --name agentdesktop-walkthrough-gateway \
    --user 0 \
    --workdir /walkthrough \
    --volume "$walkthrough_dir:/walkthrough:ro,Z" \
    --env OIDC_ISSUER="$issuer" \
    --env OIDC_AUDIENCE=agentdesktop \
    --env OIDC_JWKS_URL="${issuer}jwks" \
    --env SSL_CERT_FILE=/walkthrough/certs/process-ca-bundle.crt \
    "${gateway_environment[@]}" \
    "$gateway_image" -f "$gateway_config" \
    >/dev/null

  wait_for "Agent Gateway" curl --noproxy '*' --fail --silent http://127.0.0.1:15021/healthz/ready
  trap - ERR

  cat <<EOF
Managed walkthrough infrastructure is ready.
Container engine: $container_engine
Provider: $provider_mode

No host trust was changed. Agent Desktop commands use:
  SSL_CERT_FILE=$walkthrough_dir/certs/process-ca-bundle.crt
  AGENTDESKTOP_IDENTITY_DIR=$walkthrough_dir/certs/identity
  AGENTDESKTOP_CREDENTIAL_STORAGE=file

Stop and delete everything with:
  $0 stop
EOF
}

status() {
  if [[ "$container_engine" == podman ]]; then
    container pod ps --filter "name=$pod"
    container ps --filter "pod=$pod" --format 'table {{.Names}}\t{{.Status}}'
  else
    container ps --filter "label=$stack_label" --format 'table {{.Names}}\t{{.Status}}'
  fi
}

case "${1:-start}" in
  start)
    start mock
    ;;
  start-anthropic)
    start anthropic
    ;;
  stop)
    remove_stack
    remove_runtime_state
    ;;
  status)
    status
    ;;
  *)
    echo "usage: $0 [start|start-anthropic|status|stop]" >&2
    exit 2
    ;;
esac
