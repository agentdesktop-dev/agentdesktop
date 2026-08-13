#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
runtime="$root/runtime"
public_host="${1:-}"

is_ipv4() {
  local octet
  local -a octets

  IFS=. read -r -a octets <<<"$1"
  [[ "${#octets[@]}" -eq 4 ]] || return 1
  for octet in "${octets[@]}"; do
    [[ "$octet" =~ ^[0-9]{1,3}$ ]] || return 1
    ((10#$octet <= 255)) || return 1
  done
}

if [[ -z "$public_host" || ! "$public_host" =~ ^[A-Za-z0-9.-]+$ || "$public_host" == .* || "$public_host" == *. ]]; then
  echo "usage: $0 SERVER_DNS_NAME_OR_IPV4" >&2
  exit 2
fi
if [[ "$public_host" == *.local ]]; then
  echo ".local is reserved for multicast DNS and is not supported by this example" >&2
  echo "use agentdesktop.localhost for a laptop-local deployment" >&2
  exit 2
fi
if [[ "$public_host" =~ ^[0-9.]+$ ]] && ! is_ipv4 "$public_host"; then
  echo "$public_host is not a valid IPv4 address" >&2
  exit 2
fi
command -v openssl >/dev/null || {
  echo "openssl is required" >&2
  exit 1
}
command -v jq >/dev/null || {
  echo "jq is required" >&2
  exit 1
}

certificate_san="DNS:$public_host"
if is_ipv4 "$public_host"; then
  certificate_san="IP:$public_host"
fi

rm -rf "$runtime"
mkdir -p "$runtime/certs"
umask 077

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout "$runtime/certs/server-ca.key" \
  -out "$runtime/certs/server-ca.crt" \
  -days 365 \
  -subj '/CN=Agent Desktop VM Development Server CA' \
  -addext 'basicConstraints=critical,CA:TRUE' \
  -addext 'keyUsage=critical,keyCertSign,cRLSign'

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout "$runtime/certs/enrollment-ca.key" \
  -out "$runtime/certs/enrollment-ca.crt" \
  -days 365 \
  -subj '/CN=Agent Desktop VM Development Enrollment CA' \
  -addext 'basicConstraints=critical,CA:TRUE' \
  -addext 'keyUsage=critical,keyCertSign,cRLSign'

issue_server_certificate() {
  local name="$1"
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout "$runtime/certs/$name.key" \
    -out "$runtime/certs/$name.csr" \
    -subj "/CN=$public_host" \
    -addext "subjectAltName=$certificate_san"
  openssl x509 -req \
    -in "$runtime/certs/$name.csr" \
    -CA "$runtime/certs/server-ca.crt" \
    -CAkey "$runtime/certs/server-ca.key" \
    -CAcreateserial \
    -out "$runtime/certs/$name.crt" \
    -days 90 \
    -copy_extensions copy \
    -extfile <(printf '%s\n' \
      'basicConstraints=critical,CA:FALSE' \
      'keyUsage=critical,digitalSignature' \
      'extendedKeyUsage=serverAuth' \
      "subjectAltName=$certificate_san")
  rm "$runtime/certs/$name.csr"
}

issue_server_certificate identity-server
issue_server_certificate enrollment-server
issue_server_certificate gateway-server
chmod 0600 "$runtime/certs"/*.key
chmod 0644 "$runtime/certs"/*.crt

cat >"$runtime/organization.json" <<EOF
{
  "format_version": 1,
  "organization": {
    "id": "vm-example",
    "display_name": "VM Example Organization",
    "support_url": "https://$public_host:8090/admin/"
  },
  "identity": {
    "issuer": "https://$public_host:8444/realms/agentdesktop",
    "enrollment_url": "https://$public_host:8090/",
    "client_id": "agentdesktop",
    "audience": "agentdesktop",
    "scope": "agentgateway.invoke"
  },
  "gateway": {
    "url": "https://$public_host:8443/"
  }
}
EOF

jq \
  --arg redirect_uri "https://$public_host:8090/admin/" \
  --arg web_origin "https://$public_host:8090" \
  'if ([.clients[] | select(.clientId == "agentdesktop-admin")] | length) != 1 then
     error("expected exactly one agentdesktop-admin client")
   else
     (.clients[] | select(.clientId == "agentdesktop-admin") | .redirectUris) = [$redirect_uri]
     | (.clients[] | select(.clientId == "agentdesktop-admin") | .webOrigins) = [$web_origin]
   end' \
  "$root/keycloak-realm.json" >"$runtime/keycloak-realm.json"

if [[ ! -f "$root/.env" ]]; then
  cat >"$root/.env" <<EOF
PUBLIC_HOST=$public_host
ANTHROPIC_API_KEY=replace-me
KEYCLOAK_ADMIN_PASSWORD=replace-me
EOF
  chmod 0600 "$root/.env"
  echo "Created $root/.env; replace every replace-me value before startup."
else
  env_update=$(mktemp "$root/.env.XXXXXX")
  awk -v public_host="$public_host" '
    BEGIN { updated = 0 }
    /^PUBLIC_HOST=/ {
      if (!updated) {
        print "PUBLIC_HOST=" public_host
        updated = 1
      }
      next
    }
    { print }
    END {
      if (!updated) print "PUBLIC_HOST=" public_host
    }
  ' "$root/.env" >"$env_update"
  chmod 0600 "$env_update"
  mv "$env_update" "$root/.env"
  echo "Updated PUBLIC_HOST=$public_host in $root/.env; kept existing secrets."
fi

cat <<EOF
Prepared the VM example for $public_host.

Public ports:
  8444  Keycloak OAuth issuer
  8090  enrollment authority and administrator API
  8443  Agent Gateway mTLS CONNECT

Copy these files to each client through a trusted channel:
  $runtime/organization.json
  $runtime/certs/server-ca.crt
EOF
