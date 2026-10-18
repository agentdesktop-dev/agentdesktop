#!/bin/bash
set -euo pipefail

: "${APPLE_APPLICATION_CERTIFICATE:?Set the base64-encoded Developer ID Application PKCS#12 GitHub secret}"
: "${APPLE_CERTIFICATE_PASSWORD:?Set the Developer ID Application PKCS#12 password GitHub secret}"
: "${APPLE_INSTALLER_CERTIFICATE:?Set the base64-encoded Developer ID Installer PKCS#12 GitHub secret}"
: "${APPLE_INSTALLER_CERTIFICATE_PASSWORD:?Set the Developer ID Installer PKCS#12 password GitHub secret}"

umask 077
keychain="${RUNNER_TEMP}/agentdesktop-signing.keychain-db"
archive="${RUNNER_TEMP}/agentdesktop-signing.p12"
trap 'rm -f "$archive"' EXIT

keychain_password="$(openssl rand -hex 32)"
echo "::add-mask::${keychain_password}"
security create-keychain -p "$keychain_password" "$keychain"
security set-keychain-settings -lut 21600 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"

import_identity() {
  printf '%s' "$1" | base64 --decode > "$archive"
  security import "$archive" -k "$keychain" -P "$2" -T "$3"
}
import_identity "$APPLE_APPLICATION_CERTIFICATE" "$APPLE_CERTIFICATE_PASSWORD" /usr/bin/codesign
import_identity "$APPLE_INSTALLER_CERTIFICATE" "$APPLE_INSTALLER_CERTIFICATE_PASSWORD" /usr/bin/pkgbuild
security set-key-partition-list -S apple-tool:,apple:,codesign: -s \
  -k "$keychain_password" "$keychain" > /dev/null

keychains=()
while IFS= read -r existing; do
  existing="${existing#*\"}"
  existing="${existing%\"*}"
  if [[ -n "$existing" && "$existing" != "$keychain" ]]; then
    keychains+=("$existing")
  fi
done < <(security list-keychains -d user)
security list-keychains -d user -s "$keychain" "${keychains[@]}"

# Validity needs Apple's Developer ID intermediate, which GitHub-hosted runners preinstall.
identities="$(security find-identity -v "$keychain")"
echo "$identities"
application="$(awk '/"Developer ID Application: / { print $2; exit }' <<< "$identities")"
installer="$(awk -F '"' '/"Developer ID Installer: / { print $2; exit }' <<< "$identities")"
: "${application:?APPLE_APPLICATION_CERTIFICATE has no valid Developer ID Application identity}"
: "${installer:?APPLE_INSTALLER_CERTIFICATE has no valid Developer ID Installer identity}"
echo "APPLE_SIGNING_IDENTITY=$application" >> "$GITHUB_ENV"
echo "APPLE_INSTALLER_SIGNING_IDENTITY=$installer" >> "$GITHUB_ENV"
