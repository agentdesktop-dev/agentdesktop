#!/usr/bin/env bash
# End-to-end test of device enrollment against Dex and the controller.
#
# Enrolls one headless device with the device authorization flow and one with
# the browser flow, checks the controller's record and gateway credential for
# them, then revokes the user at Dex and checks that the device re-enrolls.
#
# Requires Docker with Compose, and jq. Set BUILD_PROFILE=debug for a faster
# build, and KEEP=1 to leave the services running after the test.

set -euo pipefail

cd "$(dirname "$0")"

project=agentdesktop-enrollment-e2e
network="${project}_default"
curl_image=docker.io/curlimages/curl:8.16.0
dex=http://dex:5556/dex

compose() {
  docker compose --project-name "$project" -f compose.yaml -f compose.e2e.yaml "$@"
}

step() {
  echo "==> $*"
}

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

cleanup() {
  local status=$?
  if (( status != 0 )); then
    compose logs --no-color controller headless headless-browser 2>&1 | tail -n 200 || true
  fi
  if [[ -z "${KEEP:-}" ]]; then
    compose down --volumes --remove-orphans >/dev/null 2>&1 || true
  fi
}

# Runs a command in a curl container joined to `$1`, a Docker network mode.
lab_curl() {
  local network_mode=$1
  shift
  docker run --rm --network "$network_mode" "$curl_image" "$@"
}

# Retries a command until it succeeds or `$1` seconds pass.
wait_for() {
  local timeout=$1 description=$2
  shift 2
  local deadline=$((SECONDS + timeout))
  until "$@" >/dev/null 2>&1; do
    if (( SECONDS >= deadline )); then
      fail "timed out waiting for ${description}"
    fi
    sleep 1
  done
}

enrollment() {
  compose exec -T "$1" agentdesktop-headless enrollment
}

enrollment_status_is() {
  [[ "$(enrollment "$1" | head -n 1)" == "$2" ]]
}

credential() {
  compose exec -T "$1" agentdesktop-headless credential
}

has_sign_in_url() {
  enrollment "$1" | grep -q '^Sign in at: '
}

# Signs in to Dex's password form from network mode `$1`, starting at URL `$2`,
# as user `$3`. Extra arguments are passed to the first request.
sign_in() {
  local network_mode=$1 url=$2 login=$3
  shift 3
  lab_curl "$network_mode" sh -euc '
    jar=$(mktemp)
    url=$1 login=$2
    shift 2
    action=$(curl -sSfL -c "$jar" -b "$jar" "$@" "$url" |
      sed -n "s/.*<form method=\"post\" action=\"\([^\"]*\)\".*/\1/p" |
      sed "s/&amp;/\&/g")
    [ -n "$action" ] || { echo "no sign-in form at $url" >&2; exit 1; }
    curl -sSfL -o /dev/null -c "$jar" -b "$jar" \
      -d "login=$login" -d password=password "http://dex:5556$action"
  ' sh "$url" "$login" "$@"
}

# Prints the JSON payload of a JWT.
jwt_payload() {
  local payload
  payload=$(cut -d. -f2 <<<"$1" | tr '_-' '/+')
  while (( ${#payload} % 4 )); do
    payload+="="
  done
  base64 -d <<<"$payload"
}

devices() {
  lab_curl "container:$(compose ps -q controller)" -sSf http://127.0.0.1:8080/api/v1/devices
}

dex_ready() {
  compose exec -T dex wget -q -O /dev/null "$dex/.well-known/openid-configuration"
}

compose down --volumes --remove-orphans >/dev/null 2>&1 || true
trap cleanup EXIT

step "Building images"
compose build --build-arg BUILD_PROFILE="${BUILD_PROFILE:-release}" \
  controller headless headless-browser
docker pull --quiet "$curl_image" >/dev/null

step "Starting Dex, the controller, and two headless devices"
compose up -d keys dex controller headless headless-browser

step "Device authorization flow: headless as user@example.com"
wait_for 120 "a device code from headless" has_sign_in_url headless
code=$(enrollment headless | sed -n 's/^Code: //p')
[[ -n "$code" ]] || fail "headless printed no user code"
sign_in "$network" "$dex/device/auth/verify_code" user@example.com -d "user_code=$code"
wait_for 30 "headless to enroll" enrollment_status_is headless enrolled

step "Browser flow: headless-browser as admin@example.com"
wait_for 120 "an authorization URL from headless-browser" has_sign_in_url headless-browser
url=$(enrollment headless-browser | sed -n 's/^Sign in at: //p')
[[ "$url" == "$dex/auth?"* ]] || fail "unexpected authorization URL: $url"
sign_in "container:$(compose ps -q headless-browser)" "$url" admin@example.com
wait_for 30 "headless-browser to enroll" enrollment_status_is headless-browser enrolled

step "Controller records each device with the user who enrolled it"
records=$(devices)
jq -e --arg issuer "$dex" '
  (map(select(.hostname == "headless-device"))[0].enrolled_by_subject | . != null and . != "") and
  (map(select(.hostname == "headless-browser-device"))[0].enrolled_by_subject | . != null and . != "") and
  (map(select(.hostname == "headless-device"))[0].enrolled_by_subject !=
   map(select(.hostname == "headless-browser-device"))[0].enrolled_by_subject) and
  all(.[]; .enrolled_by_issuer == $issuer)
' <<<"$records" >/dev/null || fail "unexpected device records: $records"
headless_id=$(jq -r 'map(select(.hostname == "headless-device"))[0].id' <<<"$records")
headless_subject=$(jq -r 'map(select(.hostname == "headless-device"))[0].enrolled_by_subject' <<<"$records")

step "Gateway credential names the user and the device"
# The gateway policy arrives over the controller stream after enrollment.
wait_for 60 "headless to receive its gateway policy" credential headless
claims=$(jwt_payload "$(credential headless)")
jq -e --arg sub "$headless_subject" --arg device "$headless_id" '
  .sub == $sub and .act.sub == $device and .email == "user@example.com"
' <<<"$claims" >/dev/null || fail "unexpected credential claims: $claims"

step "Revoking at Dex makes the device enroll again"
compose restart dex >/dev/null
wait_for 60 "Dex to restart" dex_ready
compose restart headless >/dev/null
wait_for 60 "headless to restart enrollment" has_sign_in_url headless

step "PASS"
