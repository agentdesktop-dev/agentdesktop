#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/../../.." && pwd)
vm_root=$root/tests/vm/windows
artifacts=$vm_root/.artifacts
state=$vm_root/.state
base=$artifacts/base/windows-11-enterprise-base.qcow2
base_vars=$artifacts/base/efivars.fd
overlay=$state/agentdesktop.qcow2
runtime_vars=$state/efivars.fd
pidfile=$state/qemu.pid
serial_log=$state/serial.log

ssh_port=${WINDOWS_VM_SSH_PORT:-2223}
memory=${WINDOWS_VM_MEMORY_MB:-8192}
cpus=${WINDOWS_VM_CPUS:-4}
host_forwards=${WINDOWS_VM_HOST_FORWARDS:-8000:8000,18080:18080,8090:8090,8443:8443,15008:15008,15021:15021}

usage() {
  cat <<'EOF'
Usage: tests/vm/windows/vm.sh COMMAND [ARGS]

Commands:
  check                 Validate host tools and configuration
  build                 Build the immutable Windows 11 Enterprise base
  reset                 Replace disposable disk and UEFI state
  start [--display]     Start headless by default, or open a GTK display
  wait                  Wait until guest SSH is ready
  ssh [COMMAND ...]     Open a shell or run a command in Windows
  copy SOURCE [DEST]    Copy a host file or directory into Windows
  stop                  Power off the guest and stop QEMU if necessary
  status                Show VM state and stable endpoints
  clean                 Stop and remove disposable runtime state

Build environment:
  WINDOWS_ISO           Official Windows 11 Enterprise evaluation ISO path
  WINDOWS_ISO_SHA256    Expected lowercase or uppercase SHA-256 hex digest

Runtime environment:
  WINDOWS_VM_SSH_PORT       Host SSH port (default: 2223)
  WINDOWS_VM_MEMORY_MB      Guest memory in MiB (default: 8192)
  WINDOWS_VM_CPUS           Guest CPU count (default: 4)
  WINDOWS_VM_HOST_FORWARDS  Comma-separated GUEST_PORT:HOST_PORT mappings
EOF
}

require() {
  command -v "$1" >/dev/null 2>&1 || {
    printf 'required command not found: %s\n' "$1" >&2
    exit 1
  }
}

running() {
  test -f "$pidfile" || return 1
  pid=$(cat "$pidfile")
  kill -0 "$pid" 2>/dev/null
}

prepare_askpass() {
  mkdir -p "$state"
  askpass=$state/askpass.sh
  printf '%s\n' '#!/bin/sh' 'printf "%s\n" agentdesktop' > "$askpass"
  chmod 0700 "$askpass"
  printf '%s\n' "$askpass"
}

ssh_guest() {
  require setsid
  require ssh
  askpass=$(prepare_askpass)
  DISPLAY=agentdesktop-windows-vm \
    SSH_ASKPASS=$askpass \
    SSH_ASKPASS_REQUIRE=force \
    setsid -w ssh \
      -o ConnectTimeout=2 \
      -o LogLevel=ERROR \
      -o StrictHostKeyChecking=no \
      -o UserKnownHostsFile="$state/known_hosts" \
      -p "$ssh_port" \
      agentdesktop@127.0.0.1 "$@"
}

copy_guest() {
  test "$#" -ge 1 && test "$#" -le 2 || {
    usage >&2
    exit 2
  }
  require scp
  require setsid
  askpass=$(prepare_askpass)
  destination=${2:-.}
  DISPLAY=agentdesktop-windows-vm \
    SSH_ASKPASS=$askpass \
    SSH_ASKPASS_REQUIRE=force \
    setsid -w scp -r \
      -o ConnectTimeout=2 \
      -o LogLevel=ERROR \
      -o StrictHostKeyChecking=no \
      -o UserKnownHostsFile="$state/known_hosts" \
      -P "$ssh_port" \
      "$1" "agentdesktop@127.0.0.1:$destination"
}

network_backend() {
  backend="user,id=net0,hostname=agentdesktop-windows,hostfwd=tcp:127.0.0.1:${ssh_port}-:22"
  old_ifs=$IFS
  IFS=,
  for mapping in $host_forwards; do
    case "$mapping" in
      *:*) ;;
      *) printf 'invalid WINDOWS_VM_HOST_FORWARDS mapping: %s\n' "$mapping" >&2; exit 1 ;;
    esac
    guest_port=${mapping%%:*}
    host_port=${mapping#*:}
    case "$guest_port$host_port" in
      ''|*[!0-9]*) printf 'invalid WINDOWS_VM_HOST_FORWARDS mapping: %s\n' "$mapping" >&2; exit 1 ;;
    esac
    backend="$backend,guestfwd=tcp:10.0.2.100:${guest_port}-cmd:socat STDIO TCP:127.0.0.1:${host_port}"
  done
  IFS=$old_ifs
  printf '%s\n' "$backend"
}

stop() {
  if running; then
    ssh_guest shutdown.exe /s /t 0 >/dev/null 2>&1 || true
    pid=$(cat "$pidfile")
    attempt=0
    while kill -0 "$pid" 2>/dev/null && test "$attempt" -lt 30; do
      sleep 1
      attempt=$((attempt + 1))
    done
    kill "$pid" 2>/dev/null || true
  fi
  rm -f "$pidfile"
}

start() {
  display=none
  if test "${1:-}" = --display; then
    display=gtk
  elif test "$#" -ne 0; then
    usage >&2
    exit 2
  fi
  test -f "$overlay" && test -f "$runtime_vars" || {
    printf 'runtime state not found; run tests/vm/windows/vm.sh reset first\n' >&2
    exit 1
  }
  if running; then
    printf 'Windows VM already running with PID %s\n' "$(cat "$pidfile")"
    return
  fi
  require qemu-system-x86_64
  require socat

  if test -r /dev/kvm; then
    accelerator=kvm
    cpu=host
  else
    accelerator=tcg
    cpu=max
  fi

  qemu-system-x86_64 \
    -name agentdesktop-windows \
    -machine "q35,accel=$accelerator" \
    -cpu "$cpu" \
    -smp "$cpus" \
    -m "$memory" \
    -drive "if=pflash,format=raw,readonly=on,file=/usr/share/edk2/ovmf/OVMF_CODE.fd" \
    -drive "if=pflash,format=raw,file=$runtime_vars" \
    -drive "file=$overlay,if=ide,format=qcow2,discard=unmap" \
    -device e1000,netdev=net0 \
    -netdev "$(network_backend)" \
    -display "$display" \
    -serial "file:$serial_log" \
    -pidfile "$pidfile" \
    -daemonize
  printf 'Windows VM started: SSH localhost:%s; host services: 10.0.2.100\n' "$ssh_port"
}

wait_ssh() {
  attempt=0
  while test "$attempt" -lt 300; do
    running || {
      printf 'QEMU exited before SSH became ready; see %s\n' "$serial_log" >&2
      exit 1
    }
    if ssh_guest exit 0 >/dev/null 2>&1; then
      printf 'Windows SSH ready on localhost:%s\n' "$ssh_port"
      return
    fi
    sleep 1
    attempt=$((attempt + 1))
  done
  printf 'timed out waiting for Windows SSH; see %s\n' "$serial_log" >&2
  exit 1
}

command=${1:-}
test -n "$command" || { usage; exit 2; }
shift

case "$command" in
  check)
    for tool in packer qemu-img qemu-system-x86_64 socat setsid ssh; do require "$tool"; done
    test -f /usr/share/edk2/ovmf/OVMF_CODE.fd || { printf 'OVMF firmware not found\n' >&2; exit 1; }
    printf 'Windows VM host preflight passed\n'
    ;;
  build)
    require packer
    test -n "${WINDOWS_ISO:-}" && test -f "$WINDOWS_ISO" || { printf 'WINDOWS_ISO must name the downloaded evaluation ISO\n' >&2; exit 1; }
    case "${WINDOWS_ISO_SHA256:-}" in
      *[!0-9a-fA-F]*|'') printf 'WINDOWS_ISO_SHA256 must be a SHA-256 hex digest\n' >&2; exit 1 ;;
    esac
    test "${#WINDOWS_ISO_SHA256}" -eq 64 || { printf 'WINDOWS_ISO_SHA256 must contain 64 hex characters\n' >&2; exit 1; }
    accelerator=kvm; cpu_model=host
    test -r /dev/kvm || { accelerator=tcg; cpu_model=max; }
    rm -rf "$artifacts/base"
    packer init "$vm_root/packer"
    packer build \
      -var "accelerator=$accelerator" \
      -var "cpu_model=$cpu_model" \
      -var "iso_url=$WINDOWS_ISO" \
      -var "iso_checksum=sha256:$WINDOWS_ISO_SHA256" \
      "$vm_root/packer"
    ;;
  reset)
    require qemu-img
    stop
    test -f "$base" && test -f "$base_vars" || { printf 'base image not found; build it first\n' >&2; exit 1; }
    rm -rf "$state"
    mkdir -p "$state"
    qemu-img create -f qcow2 -F qcow2 -b "$base" "$overlay"
    cp "$base_vars" "$runtime_vars"
    ;;
  start) start "$@" ;;
  wait) wait_ssh ;;
  ssh) ssh_guest "$@" ;;
  copy) copy_guest "$@" ;;
  stop) stop ;;
  status)
    if running; then printf 'running pid=%s ssh=127.0.0.1:%s\n' "$(cat "$pidfile")" "$ssh_port"; else printf 'stopped\n'; fi
    printf 'host services: 10.0.2.100; forwards: %s\n' "$host_forwards"
    ;;
  clean) stop; rm -rf "$state" ;;
  -h|--help|help) usage ;;
  *) usage >&2; exit 2 ;;
esac