#!/usr/bin/env bash
set -euo pipefail

if (($# != 0)); then
  echo "usage: $0" >&2
  exit 2
fi

readonly root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly walkthrough="$root_dir/scripts/managed-walkthrough.sh"
readonly runtime="$root_dir/target/managed-e2e"
readonly identity_dir="$runtime/identity"
readonly connector_log="$runtime/connector.log"
readonly login_log="$runtime/login.log"
readonly issuer=https://localhost:18080/
readonly enrollment=https://localhost:8090/
readonly gateway=https://127.0.0.1:8443/
readonly trust_bundle="$root_dir/examples/managed-walkthrough/certs/process-ca-bundle.crt"
readonly request='{"model":"claude-sonnet-5","max_tokens":64,"messages":[{"role":"user","content":"Reply with exactly SMOKE_OK"}]}'
connector_pid=
login_pid=

cleanup() {
  local exit_code=$?
  trap - EXIT INT TERM
  if [[ -n "$login_pid" ]]; then
    kill "$login_pid" >/dev/null 2>&1 || true
    wait "$login_pid" >/dev/null 2>&1 || true
  fi
  if [[ -n "$connector_pid" ]]; then
    kill "$connector_pid" >/dev/null 2>&1 || true
    wait "$connector_pid" >/dev/null 2>&1 || true
  fi
  if ((exit_code != 0)); then
    [[ -f "$login_log" ]] && { echo "login log:" >&2; cat "$login_log" >&2; }
    [[ -f "$connector_log" ]] && { echo "connector log:" >&2; cat "$connector_log" >&2; }
    while read -r container_name; do
      echo "$container_name log:" >&2
      podman logs "$container_name" >&2 || true
    done < <(podman ps --all --filter pod=agentdesktop-managed-walkthrough --format '{{.Names}}')
  fi
  "$walkthrough" stop >/dev/null 2>&1 || true
  exit "$exit_code"
}
trap cleanup EXIT INT TERM

require() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "required command not found: $1" >&2
    exit 1
  }
}

wait_for_text() {
  local description=$1
  local pattern=$2
  local file=$3
  for _ in {1..100}; do
    grep -q "$pattern" "$file" 2>/dev/null && return 0
    sleep 0.05
  done
  echo "$description did not complete" >&2
  return 1
}

wait_for_url() {
  local description=$1
  local url=$2
  for _ in {1..100}; do
    curl --fail --silent "$url" >/dev/null 2>&1 && return 0
    sleep 0.05
  done
  echo "$description did not become ready" >&2
  return 1
}

for command in cargo curl jq podman; do
  require "$command"
done

rm -rf "$runtime"
mkdir -p "$runtime"

echo "Building current Agent Desktop..."
cargo build --manifest-path "$root_dir/Cargo.toml" --bin agentdesktop
readonly connector="$root_dir/target/debug/agentdesktop"

echo "Starting managed infrastructure..."
"$walkthrough" start

export SSL_CERT_FILE="$trust_bundle"
export AGENTDESKTOP_CREDENTIAL_STORAGE=file
export AGENTDESKTOP_IDENTITY_DIR="$identity_dir"

echo "Completing user login..."
"$connector" identity login \
  --issuer "$issuer" \
  --client-id agentdesktop-test \
  --audience agentdesktop \
  --scope agentgateway.invoke \
  --gateway-origin "$gateway" \
  --no-open >"$login_log" 2>&1 &
login_pid=$!
wait_for_text "authorization URL" '^authorization URL: ' "$login_log"
authorization_url=$(sed -n 's/^authorization URL: //p' "$login_log" | head -n 1)
curl --fail --silent --show-error --location --cacert "$trust_bundle" "$authorization_url" >/dev/null
wait "$login_pid"
login_pid=

echo "Requesting and approving enrollment..."
"$connector" identity enroll-request \
  --issuer "$issuer" \
  --enrollment-url "$enrollment" \
  --gateway-origin "$gateway"
admin_token=$(curl --fail --silent --cacert "$trust_bundle" "${issuer}admin-token" | jq -er .access_token)
pending=$(curl --fail --silent --cacert "$trust_bundle" \
  --header "Authorization: Bearer $admin_token" \
  "${enrollment}v1/admin/enrollments?status=pending")
enrollment_id=$(jq -er '.enrollments | select(length == 1) | .[0].enrollment_id' <<<"$pending")
approval=$(curl --fail-with-body --silent --show-error --cacert "$trust_bundle" \
  --request POST \
  --header "Authorization: Bearer $admin_token" \
  "${enrollment}v1/admin/enrollments/${enrollment_id}/approve")
device_id=$(jq -er .device_id <<<"$approval")
"$connector" identity enroll-status \
  --issuer "$issuer" \
  --enrollment-url "$enrollment" \
  --gateway-origin "$gateway"

echo "Starting certificate-authenticated connector..."
"$connector" serve \
  --mode managed \
  --upstream "$gateway" \
  --native-target native.agentdesktop.internal:18443 \
  --identity-issuer "$issuer" \
  --enrollment-url "$enrollment" \
  --listen 127.0.0.1:8080 \
  --status-listen 127.0.0.1:8081 >"$connector_log" 2>&1 &
connector_pid=$!
wait_for_url "Agent Desktop" http://127.0.0.1:8081/_agentdesktop/status

response=$(curl --fail-with-body --silent --show-error \
  --header 'content-type: application/json' \
  --header 'anthropic-version: 2023-06-01' \
  --header 'x-api-key: connector-placeholder' \
  --data "$request" \
  http://127.0.0.1:8080/v1/messages)
jq -e '.content[] | select(.type == "text" and .text == "SMOKE_OK")' <<<"$response" >/dev/null

echo "Revoking enrolled device..."
revocation=$(curl --fail-with-body --silent --show-error --cacert "$trust_bundle" \
  --request POST \
  --header "Authorization: Bearer $admin_token" \
  "${enrollment}v1/admin/devices/${device_id}/revoke")
jq -e '.device_id == $device and .status == "revoked"' --arg device "$device_id" \
  <<<"$revocation" >/dev/null

echo "Managed E2E passed"