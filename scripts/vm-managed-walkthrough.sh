#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
state=$root/target/vm-managed-walkthrough
runtime=$state/runtime
vm=$root/tests/vm/vm.sh
issuer=https://host.test:18080/
gateway=https://host.test:4000/
install_root=/home/agentedge/.local/lib/agentgateway-edge

usage() {
  cat <<'EOF'
Usage: scripts/vm-managed-walkthrough.sh COMMAND

Commands:
  prepare          Prepare the currently running clean VM
  prepare --reset  Reset the VM, open its desktop, then prepare it
  status           Show walkthrough services and VM status
  stop             Stop host-side walkthrough services
EOF
}

require() {
  command -v "$1" >/dev/null 2>&1 || {
    printf 'required command not found: %s\n' "$1" >&2
    exit 1
  }
}

stop_service() {
  name=$1
  pid_file=$runtime/$name.pid
  test -f "$pid_file" || return 0
  pid=$(cat "$pid_file")
  kill "$pid" 2>/dev/null || true
  rm -f "$pid_file"
}

stop_services() {
  stop_service gateway
  stop_service authority
  stop_service provider
}

start_service() {
  name=$1
  shift
  nohup "$@" >"$runtime/$name.log" 2>&1 &
  pid=$!
  printf '%s\n' "$pid" >"$runtime/$name.pid"
}

wait_for_url() {
  attempts=0
  while test "$attempts" -lt 200; do
    if "$@" >/dev/null 2>&1; then
      return 0
    fi
    attempts=$((attempts + 1))
  done
  return 1
}

create_certificates() {
  mkdir -p "$runtime"
  openssl req -x509 -newkey rsa:2048 -nodes \
    -subj '/CN=Agent Gateway Edge Walkthrough CA' \
    -keyout "$runtime/ca.key" \
    -out "$runtime/ca.crt" \
    -days 2 >/dev/null 2>&1
  openssl req -newkey rsa:2048 -nodes \
    -subj '/CN=host.test' \
    -keyout "$runtime/host.test.key" \
    -out "$runtime/host.test.csr" >/dev/null 2>&1
  cat >"$runtime/host.test.ext" <<'EOF'
subjectAltName=DNS:host.test
extendedKeyUsage=serverAuth
keyUsage=digitalSignature,keyEncipherment
EOF
  openssl x509 -req \
    -in "$runtime/host.test.csr" \
    -CA "$runtime/ca.crt" \
    -CAkey "$runtime/ca.key" \
    -CAcreateserial \
    -extfile "$runtime/host.test.ext" \
    -out "$runtime/host.test.crt" \
    -days 2 >/dev/null 2>&1
}

start_services() {
  stop_services
  create_certificates
  start_service provider env \
    MOCK_ANTHROPIC_HOST=127.0.0.1 \
    MOCK_ANTHROPIC_PORT=18081 \
    node "$root/container/mock-anthropic.mjs"
  start_service authority env \
    AGENTGATEWAY_EDGE_FAKE_ISSUER="$issuer" \
    AGENTGATEWAY_EDGE_FAKE_LISTEN_HOST=127.0.0.1 \
    AGENTGATEWAY_EDGE_FAKE_PORT=18080 \
    AGENTGATEWAY_EDGE_FAKE_TLS_KEY="$runtime/host.test.key" \
    AGENTGATEWAY_EDGE_FAKE_TLS_CERTIFICATE="$runtime/host.test.crt" \
    AGENTGATEWAY_EDGE_FAKE_AUTO_APPROVE=1 \
    node "$root/tests/fixtures/fake-authorization-server.mjs"
  start_service gateway env \
    AGENTGATEWAY_EDGE_FAKE_LISTEN_HOST=127.0.0.1 \
    AGENTGATEWAY_EDGE_FAKE_PORT=4000 \
    AGENTGATEWAY_EDGE_FAKE_TLS_KEY="$runtime/host.test.key" \
    AGENTGATEWAY_EDGE_FAKE_TLS_CERTIFICATE="$runtime/host.test.crt" \
    AGENTGATEWAY_EDGE_FAKE_PROVIDER=http://127.0.0.1:18081/ \
    node "$root/tests/fixtures/fake-managed-gateway.mjs"

  wait_for_url curl --silent --fail \
    --cacert "$runtime/ca.crt" \
    --resolve host.test:18080:127.0.0.1 \
    "${issuer}.well-known/oauth-authorization-server" || {
      printf 'walkthrough authority did not start; see %s\n' "$runtime/authority.log" >&2
      exit 1
    }
  wait_for_url curl --silent --output /dev/null --write-out '%{http_code}' \
    --cacert "$runtime/ca.crt" \
    --resolve host.test:4000:127.0.0.1 \
    "$gateway" || {
      printf 'walkthrough gateway did not start; see %s\n' "$runtime/gateway.log" >&2
      exit 1
    }
  wait_for_url curl --silent --fail \
    --request POST \
    --header 'content-type: application/json' \
    --data '{"model":"walkthrough","max_tokens":1,"messages":[]}' \
    http://127.0.0.1:18081/v1/messages || {
      printf 'walkthrough provider did not start; see %s\n' "$runtime/provider.log" >&2
      exit 1
    }
}

write_organization() {
  cat >"$state/organization.json" <<EOF
{
  "format_version": 1,
  "organization": {
    "id": "walkthrough",
    "display_name": "Walkthrough Organization",
    "support_url": "https://host.test:18080/support"
  },
  "identity": {
    "issuer": "$issuer",
    "client_id": "agentgateway-edge-test",
    "audience": "agentgateway-edge",
    "scope": "agentgateway.invoke"
  },
  "gateway": {
    "url": "$gateway"
  }
}
EOF
}

prepare_vm() {
  "$vm" status | grep -q '^running ' || {
    printf 'VM is not running; use prepare --reset\n' >&2
    exit 1
  }
  "$vm" ssh "command -v claude >/dev/null" || {
    printf 'VM base does not contain Claude Code; rebuild it with tests/vm/vm.sh build\n' >&2
    exit 1
  }
  "$vm" ssh "test ! -e '$install_root' && test ! -e /home/agentedge/.config/agentgateway-edge-connector/identity" || {
    printf 'VM contains prior connector state; use prepare --reset for a clean walkthrough\n' >&2
    exit 1
  }

  write_organization
  "$root/scripts/build-managed-installer.sh" \
    "$state/organization.json" \
    "$state/agentgateway-edge-installer"
  "$vm" ssh install -d -m 0755 /home/agentedge/Downloads
  "$vm" copy "$runtime/ca.crt" /home/agentedge/Downloads/agentgateway-edge-walkthrough-ca.crt
  "$vm" copy "$state/agentgateway-edge-installer" /home/agentedge/Downloads/agentgateway-edge-installer
  "$vm" ssh \
    "sudo install -m 0644 /home/agentedge/Downloads/agentgateway-edge-walkthrough-ca.crt /etc/pki/ca-trust/source/anchors/agentgateway-edge-walkthrough-ca.crt && sudo update-ca-trust && chmod +x /home/agentedge/Downloads/agentgateway-edge-installer && /home/agentedge/Downloads/agentgateway-edge-installer install --yes"

  cat <<'EOF'

Managed walkthrough is ready.

In the Fedora desktop:
  1. Open Terminal.
  2. Run:
     ~/.local/lib/agentgateway-edge/bin/agentgateway-edge-connector connect-agents
  3. Complete the browser sign-in and return to Terminal.
  4. Approve the separate Claude Code settings prompt.
  5. Launch `claude` normally and ask it to reply with exactly SMOKE_OK.

The identity authority auto-approves this test device after sign-in.
Run `scripts/vm-managed-walkthrough.sh stop` when finished.
EOF
}

status() {
  "$vm" status
  for name in provider authority gateway; do
    pid_file=$runtime/$name.pid
    if test -f "$pid_file" && kill -0 "$(cat "$pid_file")" 2>/dev/null; then
      printf '%s: running pid=%s\n' "$name" "$(cat "$pid_file")"
    else
      printf '%s: stopped\n' "$name"
    fi
  done
}

command=${1:-}
case "$command" in
  prepare)
    test "$#" -le 2 || {
      usage >&2
      exit 2
    }
    require curl
    require node
    require openssl
    mkdir -p "$runtime"
    if test "${2:-}" = --reset; then
      "$vm" reset
      "$vm" start --display
      "$vm" wait
    elif test "$#" -ne 1; then
      usage >&2
      exit 2
    fi
    start_services
    prepare_vm
    ;;
  status)
    test "$#" -eq 1 || {
      usage >&2
      exit 2
    }
    status
    ;;
  stop)
    test "$#" -eq 1 || {
      usage >&2
      exit 2
    }
    stop_services
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac