#!/bin/bash
set -euo pipefail

if [[ "$#" -ne 1 ]]; then
  echo "Usage: $0 <archive>" >&2
  exit 2
fi

: "${APPLE_API_KEY:?Set the App Store Connect team API private key GitHub secret}"
: "${APPLE_API_KEY_ID:?Set the App Store Connect API key ID GitHub variable}"
: "${APPLE_API_ISSUER:?Set the App Store Connect API issuer ID GitHub variable}"

umask 077
key="${RUNNER_TEMP}/agentdesktop-notary.p8"
trap 'rm -f "$key"' EXIT
printf '%s\n' "$APPLE_API_KEY" > "$key"
credentials=(--key "$key" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER")

result="$(xcrun notarytool submit "$1" "${credentials[@]}" \
  --no-wait --output-format json)"
printf '%s\n' "$result"
submission_id="$(jq -r '.id // empty' <<< "$result" 2> /dev/null || true)"
if [[ -z "$submission_id" ]]; then
  echo "notarytool did not return a submission ID" >&2
  exit 1
fi
echo "Notarization submission ID: $submission_id"

deadline=$((SECONDS + 7200))
retry_delay=5
while (( SECONDS < deadline )); do
  if result="$(xcrun notarytool info "$submission_id" "${credentials[@]}" \
    --output-format json 2>&1)"; then
    printf '%s\n' "$result"
    outcome="$(jq -r '.status // empty' <<< "$result" 2> /dev/null || true)"
    case "$outcome" in
      Accepted)
        # The log also lists warnings for accepted submissions.
        xcrun notarytool log "$submission_id" "${credentials[@]}" || true
        exit 0
        ;;
      Invalid|Rejected)
        xcrun notarytool log "$submission_id" "${credentials[@]}" || true
        echo "Notarization of $1 finished with status '$outcome'" >&2
        exit 1
        ;;
      "In Progress")
        retry_delay=5
        sleep 60
        ;;
      *)
        echo "Unexpected notarization status '${outcome:-unknown}' for submission $submission_id" >&2
        exit 1
        ;;
    esac
  else
    echo "Could not query notarization submission $submission_id; retrying in ${retry_delay}s:" >&2
    printf '%s\n' "$result" >&2
    sleep "$retry_delay"
    retry_delay=$((retry_delay * 2))
    if (( retry_delay > 60 )); then
      retry_delay=60
    fi
  fi
done

echo "Timed out waiting for notarization submission $submission_id after 2 hours" >&2
exit 1
