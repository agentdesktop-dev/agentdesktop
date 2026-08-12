#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ ! -f "$root/.env" || ! -f "$root/runtime/certs/server-ca.crt" || ! -f "$root/runtime/organization.json" ]]; then
  echo "run ./prepare.sh PUBLIC_DNS_NAME first" >&2
  exit 2
fi

ca="$root/runtime/certs/server-ca.crt"
public_host=$(jq -er '.identity.issuer | capture("^https://(?<host>[A-Za-z0-9.-]+):8444/realms/agentdesktop$").host' "$root/runtime/organization.json")
resolve_identity="--resolve $public_host:8444:127.0.0.1"
resolve_enrollment="--resolve $public_host:8090:127.0.0.1"

curl --fail --silent --show-error --retry 30 --retry-all-errors --retry-delay 1 \
  --noproxy "$public_host" \
  --cacert "$ca" $resolve_identity \
  "https://$public_host:8444/realms/agentdesktop/.well-known/openid-configuration" \
  | jq -e --arg issuer "https://$public_host:8444/realms/agentdesktop" '.issuer == $issuer' >/dev/null
curl --fail --silent --show-error --retry 30 --retry-all-errors --retry-delay 1 \
  --noproxy "$public_host" \
  --cacert "$ca" $resolve_identity --get \
  "https://$public_host:8444/realms/agentdesktop/protocol/openid-connect/auth" \
  --data-urlencode client_id=agentdesktop-admin \
  --data-urlencode "redirect_uri=https://$public_host:8090/admin/" \
  --data-urlencode response_type=code \
  --data-urlencode 'scope=openid agentdesktop.enrollment.admin' \
  --data-urlencode code_challenge=0123456789012345678901234567890123456789012 \
  --data-urlencode code_challenge_method=S256 >/dev/null
curl --fail --silent --show-error --retry 30 --retry-all-errors --retry-delay 1 \
  --noproxy "$public_host" \
  --cacert "$ca" $resolve_enrollment "https://$public_host:8090/healthz" >/dev/null
curl --fail --silent --show-error --retry 30 --retry-all-errors --retry-delay 1 \
  --noproxy '*' \
  http://127.0.0.1:15021/healthz/ready >/dev/null

cat <<EOF
Managed VM stack is healthy.

Enrollment authority: https://$public_host:8090/
OAuth issuer:         https://$public_host:8444/realms/agentdesktop
Agent Gateway:        https://$public_host:8443/
EOF
