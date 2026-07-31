#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
gateway=${AGENTGATEWAY_BINARY:-$root/../agentgateway/target/debug/agentgateway}
tmp=$(mktemp -d "${TMPDIR:-/tmp}/agentdesktop-hbone.XXXXXX")
gateway_pid=
relay_pid=
target_pid=
token=$(python3 -c 'import secrets; print(secrets.token_hex(32))')

cleanup() {
  for pid in "$relay_pid" "$gateway_pid" "$target_pid"; do
    if test -n "$pid"; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -rf "$tmp"
}
trap cleanup EXIT INT TERM

wait_tcp() {
  port=$1
  pid=$2
  for attempt in $(seq 1 1000); do
    kill -0 "$pid" 2>/dev/null || return 1
    if python3 -c 'import socket, sys; socket.create_connection(("127.0.0.1", int(sys.argv[1])), 0.05).close()' "$port" 2>/dev/null; then
      return 0
    fi
  done
  return 1
}

test -x "$gateway" || {
  printf 'Agent Gateway binary not found: %s\n' "$gateway" >&2
  exit 1
}

cd "$root"
cargo build --quiet --bin agentdesktop

python3 -m http.server 18080 --bind 127.0.0.1 >"$tmp/target.log" 2>&1 &
target_pid=$!
wait_tcp 18080 "$target_pid"

AGENTDESKTOP_CAPTURE_TOKEN=$token \
  "$gateway" -f container/agentgateway-hbone-smoke.yaml >"$tmp/gateway.log" 2>&1 &
gateway_pid=$!
wait_tcp 15008 "$gateway_pid" || {
  cat "$tmp/gateway.log" >&2
  exit 1
}

printf wrong-token >"$tmp/token"
chmod 0600 "$tmp/token"
target/debug/agentdesktop capture \
  --listen 127.0.0.1:15001 \
  --hbone-endpoint 127.0.0.1:15008 \
  --token-file "$tmp/token" >"$tmp/relay.log" 2>&1 &
relay_pid=$!
wait_tcp 15001 "$relay_pid" || {
  cat "$tmp/relay.log" >&2
  exit 1
}

status=$(curl --silent --output /dev/null --write-out '%{http_code}' --max-time 5 \
  http://127.0.0.1:15001/ \
  --header 'Host: 127.0.0.1:18080')
test "$status" = 403
kill "$relay_pid"
wait "$relay_pid" 2>/dev/null || true
relay_pid=

printf %s "$token" >"$tmp/token"
target/debug/agentdesktop capture \
  --listen 127.0.0.1:15001 \
  --hbone-endpoint 127.0.0.1:15008 \
  --token-file "$tmp/token" >"$tmp/relay.log" 2>&1 &
relay_pid=$!
wait_tcp 15001 "$relay_pid" || {
  cat "$tmp/relay.log" >&2
  exit 1
}

curl --silent --show-error --fail --max-time 5 \
  http://127.0.0.1:15001/ \
  --header 'Host: 127.0.0.1:18080' |
  grep -q 'Directory listing'

grep -q 'listener=captured-http' "$tmp/gateway.log"
grep -q 'endpoint=127.0.0.1:18080' "$tmp/gateway.log"
printf 'real Agent Gateway HBONE interoperability validated\n'