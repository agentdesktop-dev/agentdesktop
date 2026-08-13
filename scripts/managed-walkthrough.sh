#!/usr/bin/env bash
set -euo pipefail

readonly root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly walkthrough_dir="$root_dir/examples/managed-walkthrough"
readonly pod="agentdesktop-managed-walkthrough"
readonly control_plane_image="localhost/agentdesktop-control-plane:managed-walkthrough"
readonly gateway_image="${AGENTGATEWAY_IMAGE:-ghcr.io/agentgateway/agentgateway:v1.4.1}"
readonly server_dns="${AGENTDESKTOP_WALKTHROUGH_SERVER_DNS:-localhost}"
readonly issuer="https://${server_dns}:18080/"
readonly admin_oauth_origin="${AGENTDESKTOP_WALKTHROUGH_ADMIN_OAUTH_ORIGIN:-$issuer}"

container() {
  podman "$@"
}

remove_stack() {
  container pod rm --force "$pod" >/dev/null 2>&1 || true
}

remove_runtime_state() {
  rm -rf "$walkthrough_dir/certs"
}

rollback_stack() {
  echo "walkthrough startup failed; container logs follow" >&2
  local container_name
  while read -r container_name; do
    container logs "$container_name" >&2 || true
  done < <(container ps --all --filter "pod=$pod" --format '{{.Names}}')
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
  done
  echo "$description did not become ready" >&2
  return 1
}

start() {
  remove_stack
  trap 'rollback_stack' ERR
  AGENTDESKTOP_WALKTHROUGH_SERVER_DNS="$server_dns" \
    "$walkthrough_dir/generate-certificates.sh"

  local system_bundle=""
  local candidate
  for candidate in \
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
  container pod create \
    --name "$pod" \
    --add-host "${server_dns}:127.0.0.1" \
    --publish 127.0.0.1:18080:18080 \
    --publish 127.0.0.1:18082:18082 \
    --publish 127.0.0.1:18081:18081 \
    --publish 127.0.0.1:18444:443 \
    --publish 127.0.0.1:8090:8090 \
    --publish 127.0.0.1:8091:8091 \
    --publish 127.0.0.1:8443:8443 \
    --publish 127.0.0.1:15021:15021 \
    >/dev/null

  container run --detach \
    --pod "$pod" \
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

  container run --detach \
    --pod "$pod" \
    --name agentdesktop-walkthrough-anthropic \
    --volume "$root_dir/container/mock-anthropic.mjs:/app/mock-anthropic.mjs:ro,Z" \
    --env MOCK_ANTHROPIC_HOST=0.0.0.0 \
    --env MOCK_ANTHROPIC_PORT=18081 \
    docker.io/library/node:22-alpine \
    node /app/mock-anthropic.mjs \
    >/dev/null

  container run --detach \
    --pod "$pod" \
    --name agentdesktop-walkthrough-captured-target \
    --volume "$root_dir/container/mock-anthropic.mjs:/app/mock-anthropic.mjs:ro,Z" \
    --volume "$walkthrough_dir/certs:/certs:ro,Z" \
    --env MOCK_ANTHROPIC_HOST=0.0.0.0 \
    --env MOCK_ANTHROPIC_PORT=443 \
    --env MOCK_ANTHROPIC_TLS_CERTIFICATE=/certs/gateway-server.crt \
    --env MOCK_ANTHROPIC_TLS_KEY=/certs/gateway-server.key \
    docker.io/library/node:22-alpine \
    node /app/mock-anthropic.mjs \
    >/dev/null

  container run --detach \
    --pod "$pod" \
    --name agentdesktop-walkthrough-postgres \
    --env POSTGRES_USER=agentdesktop \
    --env POSTGRES_PASSWORD=agentdesktop \
    --env POSTGRES_DB=agentdesktop \
    docker.io/library/postgres:17 \
    >/dev/null

  wait_for "mock OIDC" curl --fail --silent \
    --cacert "$walkthrough_dir/certs/gateway-server-ca.crt" \
    --resolve "${server_dns}:18080:127.0.0.1" \
    "${issuer}jwks"
  wait_for "mock Anthropic" curl --fail --silent http://127.0.0.1:18081/v1/messages/count_tokens \
    -H 'content-type: application/json' --data '{}'
  wait_for "captured mock Anthropic" curl --fail --silent \
    --cacert "$walkthrough_dir/certs/gateway-server-ca.crt" \
    --resolve "${server_dns}:18444:127.0.0.1" \
    "https://${server_dns}:18444/v1/messages/count_tokens" \
    -H 'content-type: application/json' --data '{}'
  wait_for "PostgreSQL" container exec agentdesktop-walkthrough-postgres \
    psql -U agentdesktop -d agentdesktop -c 'SELECT 1'

  container run --detach \
    --pod "$pod" \
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

  wait_for "enrollment service" curl --fail --silent \
    --cacert "$walkthrough_dir/certs/gateway-server-ca.crt" \
    https://127.0.0.1:8090/healthz

  container run --detach \
    --pod "$pod" \
    --name agentdesktop-walkthrough-gateway \
    --user 0 \
    --workdir /walkthrough \
    --volume "$walkthrough_dir:/walkthrough:ro,Z" \
    --env OIDC_ISSUER="$issuer" \
    --env OIDC_AUDIENCE=agentdesktop \
    --env OIDC_JWKS_URL="${issuer}jwks" \
    --env SSL_CERT_FILE=/walkthrough/certs/process-ca-bundle.crt \
    --env ANTHROPIC_BASE_URL='http://127.0.0.1:18081' \
    --env ANTHROPIC_API_KEY=mock-provider-key \
    "$gateway_image" -f /walkthrough/agentgateway.yaml \
    >/dev/null

  wait_for "Agent Gateway" curl --fail --silent http://127.0.0.1:15021/healthz/ready
  trap - ERR

  cat <<EOF
Managed walkthrough infrastructure is ready.

No host trust was changed. Agent Desktop commands use:
  SSL_CERT_FILE=$walkthrough_dir/certs/process-ca-bundle.crt
  AGENTDESKTOP_IDENTITY_DIR=$walkthrough_dir/certs/identity
  AGENTDESKTOP_CREDENTIAL_STORAGE=file

Stop and delete everything with:
  $0 stop
EOF
}

status() {
  container pod ps --filter "name=$pod"
  container ps --filter "pod=$pod" --format 'table {{.Names}}\t{{.Status}}'
}

case "${1:-start}" in
  start)
    start
    ;;
  stop)
    remove_stack
    remove_runtime_state
    ;;
  status)
    status
    ;;
  *)
    echo "usage: $0 [start|status|stop]" >&2
    exit 2
    ;;
esac
