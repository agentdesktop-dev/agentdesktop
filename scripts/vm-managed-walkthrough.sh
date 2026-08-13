#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
state=$root/target/vm-managed-walkthrough
runtime=$state/runtime
vm=$root/tests/vm/vm.sh
issuer=https://host.test:18080/
gateway=https://host.test:4000/
enrollment=https://host.test:8090/
vm_forwards=443:18444,18080:18080,8090:8090,4000:8443,15021:15021
install_root=/home/agentdesktop/.local/lib/agentdesktop
machine_root=/opt/agentdesktop
machine_stage=/home/agentdesktop/Downloads/agentdesktop-mdm

run_vm() {
  VM_HOST_FORWARDS=$vm_forwards "$vm" "$@"
}

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

stop_services() {
  "$root/scripts/managed-walkthrough.sh" stop
}

start_services() {
  AGENTDESKTOP_WALKTHROUGH_SERVER_DNS=host.test \
  AGENTDESKTOP_WALKTHROUGH_ADMIN_OAUTH_ORIGIN=http://localhost:18082/ \
    "$root/scripts/managed-walkthrough.sh" start
}

write_organization() {
  node - "$root/examples/managed-walkthrough/certs/gateway-server-ca.crt" "$state/organization.json" <<'EOF'
const fs = require("node:fs");
const certificate = fs.readFileSync(process.argv[2], "utf8");
const organization = {
  format_version: 1,
  organization: {
    id: "walkthrough",
    display_name: "Walkthrough Organization",
    support_url: "https://host.test:18080/support",
  },
  identity: {
    issuer: "https://host.test:18080/",
    enrollment_url: "https://host.test:8090/",
    client_id: "agentdesktop-test",
    audience: "agentdesktop",
    scope: "agentgateway.invoke",
  },
  gateway: { url: "https://host.test:4000/" },
  trust: {
    certificate_pem: certificate,
    inspection_scope: "AI application traffic routed through Walkthrough Organization's managed Agent Gateway",
  },
};
fs.writeFileSync(process.argv[3], `${JSON.stringify(organization, null, 2)}\n`);
EOF
}

install_machine_forwarder() {
  run_vm ssh install -d -m 0755 "$machine_stage"
  run_vm copy "$root/target/release/agentdesktop" "$machine_stage/agentdesktop"
  run_vm copy "$root/target/release/agentdesktop-install" "$machine_stage/agentdesktop-install"
  run_vm copy "$root/target/release/agentdesktop-capture-setup" "$machine_stage/agentdesktop-capture-setup"
  run_vm copy "$state/organization.json" "$machine_stage/organization.json"
  run_vm copy "$root/examples/managed-walkthrough/certs/gateway-server-ca.crt" "$machine_stage/organization-ca.crt"
  run_vm ssh "sudo '$machine_stage/agentdesktop-install' managed-install \
    --root '$machine_root' \
    --connector '$machine_stage/agentdesktop' \
    --organization '$machine_stage/organization.json' \
    --control '$machine_stage/agentdesktop-install' \
    --command-link /usr/local/bin/agentdesktop"
  run_vm ssh "sudo '$machine_stage/agentdesktop-capture-setup' system-install \
    --certificate '$machine_stage/organization-ca.crt'"
  run_vm ssh "sudo install -m 0644 \
    '$machine_root/share/systemd/system/agentdesktop-forwarder.service' \
    /etc/systemd/system/agentdesktop-forwarder.service"
  run_vm ssh "sudo systemctl daemon-reload && \
    sudo systemctl enable agentdesktop-forwarder.service && \
    sudo systemctl restart agentdesktop-forwarder.service"
  run_vm ssh "test -S /run/agentdesktop/sessions.sock"
}

prepare_vm() {
  run_vm status | grep -q '^running ' || {
    printf 'VM is not running; use prepare --reset\n' >&2
    exit 1
  }
  run_vm ssh "command -v claude >/dev/null" || {
    printf 'VM base does not contain Claude Code; rebuild it with tests/vm/vm.sh build\n' >&2
    exit 1
  }
  run_vm ssh "test ! -e '$install_root' && test ! -e /home/agentdesktop/.config/agentdesktop/identity" || {
    printf 'VM contains prior connector state; use prepare --reset for a clean walkthrough\n' >&2
    exit 1
  }

  write_organization
  cargo build --release \
    --bin agentdesktop \
    --bin agentdesktop-install \
    --bin agentdesktop-capture-setup
  install_machine_forwarder

  cat <<'EOF'

Managed walkthrough is ready. The root-owned Agent Desktop bundle, machine
forwarder, command, and organization CA were installed over SSH to simulate MDM.

In the Fedora desktop:
  1. Open Terminal.
  2. Run `agentdesktop connect-agents`.
  3. Complete browser sign-in.
  4. For native mode, approve the Claude Code settings change, then launch `claude` normally.
  5. For transparent mode, decline that change and run:
      agentdesktop launch --profile claude -- curl --fail --silent --show-error \
        -H 'content-type: application/json' \
        -d '{"model":"claude-sonnet-5","max_tokens":64,"messages":[{"role":"user","content":"Reply with exactly SMOKE_OK"}]}' \
        https://host.test/v1/messages

Approve the pending device at http://localhost:8091/admin/ on the host.
Run `scripts/vm-managed-walkthrough.sh stop` when finished.
EOF
}

status() {
  run_vm status
  "$root/scripts/managed-walkthrough.sh" status
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
      run_vm reset
      run_vm start --display
      run_vm wait
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