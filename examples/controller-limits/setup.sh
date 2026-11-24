#!/bin/sh
# Creates the lab's keys and enrollment token, and the starting policy and
# local configuration. Runs on every `up`; keeps what already exists.
set -eu

keys=/tmp/agentdesktop-keys
if [ ! -e "$keys/controller.pem" ]; then
  apk add --no-cache bash openssl >/dev/null
  bash /demo/create-keys.sh
fi

if [ ! -e "$keys/enrollment-token" ]; then
  apk add --no-cache openssl >/dev/null
  # An operator's signing key. The controller trusts its public key.
  openssl genpkey -algorithm ed25519 -out "$keys/enrollment-issuer.pem"
  openssl pkey -in "$keys/enrollment-issuer.pem" -pubout \
    -out "$keys/enrollment-issuer.pub.pem"
  b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }
  header=$(printf '{"alg":"EdDSA","typ":"JWT"}' | b64url)
  claims=$(printf '{"iss":"lab","aud":"agentdesktop-lab","jti":"lab-1","iat":%s}' \
    "$(date +%s)" | b64url)
  printf '%s.%s' "$header" "$claims" > /tmp/signing-input
  signature=$(openssl pkeyutl -sign -rawin -inkey "$keys/enrollment-issuer.pem" \
    -in /tmp/signing-input | b64url)
  printf '%s.%s.%s\n' "$header" "$claims" "$signature" > "$keys/enrollment-token"
fi

[ -e /policy/policy.yaml ] || cp /lab/policies/allowed.yaml /policy/policy.yaml
[ -e /device-config/agentdesktop.yaml ] ||
  cp /lab/device/limits-on.yaml /device-config/agentdesktop.yaml
