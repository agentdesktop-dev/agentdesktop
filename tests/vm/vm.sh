#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
vm_root=$root/tests/vm
artifacts=$vm_root/.artifacts
state=$vm_root/.state
base=$artifacts/base/fedora-workstation-base.qcow2
overlay=$state/agentedge.qcow2
pidfile=$state/qemu.pid
serial_log=$state/serial.log
monitor=$state/qmp.sock

ssh_port=${VM_SSH_PORT:-2222}
memory=${VM_MEMORY_MB:-8192}
cpus=${VM_CPUS:-4}
host_forwards=${VM_HOST_FORWARDS:-8000:8000,18080:18080,4000:4000,15008:15008,15021:15021}

usage() {
  cat <<'EOF'
Usage: tests/vm/vm.sh COMMAND [ARGS]

Commands:
  check                 Validate host tools and QEMU network configuration
  build                 Build the immutable Fedora Workstation base with Packer
  reset                 Stop the VM and replace its disposable qcow2 overlay
  start [--display]     Start headless by default, or open a GTK display
  wait                  Wait until guest SSH is ready
  ssh [COMMAND ...]     Open a shell or run a command in the guest
  copy SOURCE [DEST]    Copy a host file or directory into the guest
  probe-host            Report which mapped laptop-loopback targets are ready
  stop                  Ask the guest to power off, then stop QEMU if necessary
  status                Show VM state and stable network endpoints
  clean                 Stop and remove disposable runtime state

Environment:
  VM_HOST_FORWARDS      Comma-separated GUEST_PORT:HOST_PORT mappings
  VM_SSH_PORT           Host loopback SSH port (default: 2222)
  VM_MEMORY_MB          Guest memory in MiB (default: 8192)
  VM_CPUS               Guest CPU count (default: 4)

The guest resolves host.test to forwarded host-loopback services and
host.internal to QEMU's stable host gateway at 10.0.2.2.
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

stop() {
  if ! running; then
    rm -f "$pidfile" "$monitor"
    return
  fi

  ssh_guest sudo systemctl poweroff >/dev/null 2>&1 || true
  pid=$(cat "$pidfile")
  attempt=0
  while kill -0 "$pid" 2>/dev/null && test "$attempt" -lt 30; do
    sleep 1
    attempt=$((attempt + 1))
  done
  if kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    wait_attempt=0
    while kill -0 "$pid" 2>/dev/null && test "$wait_attempt" -lt 10; do
      sleep 1
      wait_attempt=$((wait_attempt + 1))
    done
  fi
  rm -f "$pidfile" "$monitor"
}

prepare_askpass() {
  mkdir -p "$state"
  askpass=$state/askpass.sh
  printf '%s\n' '#!/bin/sh' 'printf "%s\n" agentedge' > "$askpass"
  chmod 0700 "$askpass"
  printf '%s\n' "$askpass"
}

ssh_guest() {
  require setsid
  require ssh
  askpass=$(prepare_askpass)
  DISPLAY=agentedge-vm \
    SSH_ASKPASS=$askpass \
    SSH_ASKPASS_REQUIRE=force \
    setsid -w ssh \
      -o ConnectTimeout=2 \
      -o LogLevel=ERROR \
      -o StrictHostKeyChecking=no \
      -o UserKnownHostsFile="$state/known_hosts" \
      -p "$ssh_port" \
      agentedge@127.0.0.1 "$@"
}

copy_guest() {
  test "$#" -ge 1 && test "$#" -le 2 || {
    usage >&2
    exit 2
  }
  require scp
  require setsid
  askpass=$(prepare_askpass)
  destination=${2:-/home/agentedge/}
  DISPLAY=agentedge-vm \
    SSH_ASKPASS=$askpass \
    SSH_ASKPASS_REQUIRE=force \
    setsid -w scp -r \
      -o ConnectTimeout=2 \
      -o LogLevel=ERROR \
      -o StrictHostKeyChecking=no \
      -o UserKnownHostsFile="$state/known_hosts" \
      -P "$ssh_port" \
      "$1" "agentedge@127.0.0.1:$destination"
}

network_backend() {
  backend="user,id=net0,hostname=agentedge-vm,hostfwd=tcp:127.0.0.1:${ssh_port}-:22"
  old_ifs=$IFS
  IFS=,
  for mapping in $host_forwards; do
    case "$mapping" in
      *:*) ;;
      *)
        printf 'invalid VM_HOST_FORWARDS mapping: %s\n' "$mapping" >&2
        exit 1
        ;;
    esac
    guest_port=${mapping%%:*}
    host_port=${mapping#*:}
    case "$guest_port$host_port" in
      ''|*[!0-9]*)
        printf 'invalid VM_HOST_FORWARDS mapping: %s\n' "$mapping" >&2
        exit 1
        ;;
    esac
    backend="$backend,guestfwd=tcp:10.0.2.100:${guest_port}-cmd:socat STDIO TCP:127.0.0.1:${host_port}"
  done
  IFS=$old_ifs
  printf '%s\n' "$backend"
}

start() {
  display=none
  if test "${1:-}" = --display; then
    display=gtk
  elif test "$#" -ne 0; then
    usage >&2
    exit 2
  fi
  test -f "$base" || {
    printf 'base image not found; run tests/vm/vm.sh build first\n' >&2
    exit 1
  }
  if running; then
    printf 'VM already running with PID %s\n' "$(cat "$pidfile")"
    return
  fi
  test -f "$overlay" || reset
  require qemu-system-x86_64
  require socat
  mkdir -p "$state"
  rm -f "$monitor"

  if test -r /dev/kvm; then
    accelerator=kvm
    cpu=host
  else
    accelerator=tcg
    cpu=max
  fi

  qemu-system-x86_64 \
    -name agentedge-e2e \
    -machine "q35,accel=$accelerator" \
    -cpu "$cpu" \
    -smp "$cpus" \
    -m "$memory" \
    -drive "file=$overlay,if=virtio,format=qcow2,discard=unmap" \
    -device virtio-net-pci,netdev=net0 \
    -netdev "$(network_backend)" \
    -device virtio-rng-pci \
    -display "$display" \
    -serial "file:$serial_log" \
    -qmp "unix:$monitor,server=on,wait=off" \
    -pidfile "$pidfile" \
    -daemonize

  printf 'VM started: SSH localhost:%s; host services guest hostname: host.test\n' "$ssh_port"
}

wait_ssh() {
  attempt=0
  while test "$attempt" -lt 300; do
    running || {
      printf 'QEMU exited before SSH became ready; see %s\n' "$serial_log" >&2
      exit 1
    }
    if ssh_guest true >/dev/null 2>&1; then
      printf 'guest SSH ready on localhost:%s\n' "$ssh_port"
      return
    fi
    sleep 1
    attempt=$((attempt + 1))
  done
  printf 'timed out waiting for guest SSH; see %s\n' "$serial_log" >&2
  exit 1
}

probe_host() {
  require socat
  old_ifs=$IFS
  IFS=,
  for mapping in $host_forwards; do
    guest_port=${mapping%%:*}
    host_port=${mapping#*:}
    if socat -T 1 -u /dev/null "TCP:127.0.0.1:$host_port" 2>/dev/null; then
      printf 'host.test:%s reachable\n' "$guest_port"
    else
      printf 'host.test:%s unavailable (host port %s)\n' "$guest_port" "$host_port"
    fi
  done
  IFS=$old_ifs
}

command=${1:-}
test -n "$command" || {
  usage
  exit 2
}
shift

case "$command" in
  check)
    require qemu-img
    require qemu-system-x86_64
    require socat
    require setsid
    require ssh
    test -r /dev/kvm && accelerator=kvm || accelerator=tcg
    ssh_port=0
    printf '%s\n%s\n' \
      '{"execute":"qmp_capabilities"}' \
      '{"execute":"quit"}' |
      qemu-system-x86_64 \
        -machine "none,accel=$accelerator" \
        -display none \
        -nodefaults \
        -netdev "$(network_backend)" \
        -nic none \
        -qmp stdio >/dev/null
    printf 'VM host preflight passed with %s acceleration\n' "$accelerator"
    ;;
  build)
    require packer
    mkdir -p "$artifacts"
    accelerator=kvm
    cpu_model=host
    if ! test -r /dev/kvm; then
      accelerator=tcg
      cpu_model=max
    fi
    packer init "$vm_root/packer"
    packer build -force \
      -var "accelerator=$accelerator" \
      -var "cpu_model=$cpu_model" \
      "$vm_root/packer"
    ;;
  reset)
    require qemu-img
    stop
    test -f "$base" || {
      printf 'base image not found; run tests/vm/vm.sh build first\n' >&2
      exit 1
    }
    mkdir -p "$state"
    rm -f "$overlay" "$serial_log" "$state/known_hosts"
    qemu-img create -f qcow2 -F qcow2 -b "$base" "$overlay"
    ;;
  start)
    start "$@"
    ;;
  wait)
    wait_ssh
    ;;
  ssh)
    ssh_guest "$@"
    ;;
  copy)
    copy_guest "$@"
    ;;
  probe-host)
    probe_host
    ;;
  stop)
    stop
    ;;
  status)
    if running; then
      printf 'running pid=%s ssh=127.0.0.1:%s\n' "$(cat "$pidfile")" "$ssh_port"
    else
      printf 'stopped\n'
    fi
    printf 'guest hostnames: host.test=forwarded-loopback host.internal=10.0.2.2\n'
    printf 'forwards: %s\n' "$host_forwards"
    ;;
  clean)
    stop
    rm -rf "$state"
    ;;
  -h|--help|help)
    usage
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac