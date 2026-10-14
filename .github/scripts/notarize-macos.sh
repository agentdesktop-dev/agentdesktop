#!/bin/bash
set -euo pipefail

: "${APPLE_API_KEY:?Set the App Store Connect team API private key GitHub secret}"
: "${APPLE_API_KEY_ID:?Set the App Store Connect API key ID GitHub variable}"
: "${APPLE_API_ISSUER:?Set the App Store Connect API issuer ID GitHub variable}"

umask 077
key="${RUNNER_TEMP}/agentdesktop-notary.p8"
trap 'rm -f "$key"' EXIT
printf '%s\n' "$APPLE_API_KEY" > "$key"
credentials=(--key "$key" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER")

result="$(xcrun notarytool submit "$1" "${credentials[@]}" \
  --wait --timeout 1h --output-format json)" || true
echo "$result"
id="$(jq -r '.id // empty' <<< "$result" 2> /dev/null)" || true
if [[ -n "$id" ]]; then
  # The log also lists warnings for accepted submissions.
  xcrun notarytool log "$id" "${credentials[@]}" || true
fi
outcome="$(jq -r '.status // empty' <<< "$result" 2> /dev/null)" || true
if [[ "$outcome" != Accepted ]]; then
  echo "Notarization of $1 finished with status '${outcome:-unknown}'" >&2
  exit 1
fi
