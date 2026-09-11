#!/bin/bash
set -euo pipefail

: "${APPLE_APPLICATION_CERTIFICATE:?Set the base64-encoded PKCS#12 GitHub secret}"
: "${APPLE_CERTIFICATE_PASSWORD:?Set the PKCS#12 password GitHub secret}"

umask 077
keychain="${RUNNER_TEMP}/agentdesktop-signing.keychain-db"
certificate="${RUNNER_TEMP}/agentdesktop-signing.pem"
archive="${RUNNER_TEMP}/agentdesktop-signing.p12"
trap 'rm -f "$archive"' EXIT
printf '%s' "$APPLE_APPLICATION_CERTIFICATE" | base64 --decode > "$archive"
openssl pkcs12 -in "$archive" -clcerts -nokeys \
  -passin env:APPLE_CERTIFICATE_PASSWORD -out "$certificate"

keychain_password="$(openssl rand -hex 32)"
echo "::add-mask::${keychain_password}"
security create-keychain -p "$keychain_password" "$keychain"
security set-keychain-settings -lut 21600 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"
security import "$archive" -k "$keychain" -P "$APPLE_CERTIFICATE_PASSWORD" \
  -T /usr/bin/codesign
security set-key-partition-list -S apple-tool:,apple:,codesign: -s \
  -k "$keychain_password" "$keychain" > /dev/null

# Trust our self-signed certificate on this disposable build runner only.
sudo security add-trusted-cert -d -r trustRoot -p codeSign \
  -k "$keychain" "$certificate"
keychains=()
while IFS= read -r existing; do
  existing="${existing#*\"}"
  existing="${existing%\"*}"
  if [[ -n "$existing" && "$existing" != "$keychain" ]]; then
    keychains+=("$existing")
  fi
done < <(security list-keychains -d user)
security list-keychains -d user -s "$keychain" "${keychains[@]}"

# Select the exact certificate, independent of its display name.
identity="$(openssl x509 -in "$certificate" -noout -fingerprint -sha1 | cut -d= -f2 | tr -d ':')"
security find-identity -v -p codesigning "$keychain" | grep -F "$identity"
echo "APPLE_SIGNING_IDENTITY=$identity" >> "$GITHUB_ENV"
