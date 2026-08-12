#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$root/../.." && pwd)"
identity_root="$HOME/.config/agentdesktop-managed-local"
pid_file="$root/runtime/desktop.pid"
assume_yes=false

usage() {
  cat <<'EOF'
Usage: ./reset-local.sh [--yes]

Reset the laptop-local managed-VM example. This removes:
  - its recorded Agent Desktop development process
  - Agent Desktop-owned Claude Code routing values
  - its Docker containers, network, volumes, and locally built image
  - its file-backed OAuth, enrollment, and device identity
  - its generated certificates, bootstrap, realm, and .env secrets
  - its exact generated CA from the macOS login keychain

Unrelated Claude settings, self-managed local credentials, and shared Docker images
are preserved.
EOF
}

case "${1:-}" in
  "") ;;
  --yes) assume_yes=true ;;
  -h|--help)
    usage
    exit 0
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
if (($# > 1)); then
  usage >&2
  exit 2
fi

if [[ "$assume_yes" == false ]]; then
  usage
  printf '\nContinue? [y/N] '
  read -r answer
  if [[ ! "$answer" =~ ^[Yy]([Ee][Ss])?$ ]]; then
    echo "Reset cancelled; no state was changed."
    exit 0
  fi
fi

if ! command -v cargo >/dev/null && [[ -x "$HOME/.cargo/bin/cargo" ]]; then
  export PATH="$HOME/.cargo/bin:$PATH"
fi
command -v cargo >/dev/null || {
  echo "cargo is required to remove Agent Desktop-owned Claude settings" >&2
  exit 1
}
command -v docker >/dev/null || {
  echo "docker is required" >&2
  exit 1
}

if [[ -f "$pid_file" ]]; then
  read -r desktop_pid <"$pid_file" || true
  if [[ "$desktop_pid" =~ ^[0-9]+$ ]] && kill -0 "$desktop_pid" 2>/dev/null; then
    echo "Stopping the laptop-local Agent Desktop process..."
    kill -TERM "$desktop_pid"
    for _ in {1..50}; do
      if ! kill -0 "$desktop_pid" 2>/dev/null; then
        break
      fi
      sleep 0.1
    done
    if kill -0 "$desktop_pid" 2>/dev/null; then
      echo "Agent Desktop did not stop; stop PID $desktop_pid and run reset again" >&2
      exit 1
    fi
  fi
  rm -f "$pid_file"
fi

if command -v curl >/dev/null \
  && curl --noproxy '*' --fail --silent --max-time 1 \
    http://127.0.0.1:8081/_agentdesktop/status >/dev/null 2>&1; then
  echo "an untracked Agent Desktop process is still running; stop it and run reset again" >&2
  exit 1
fi

echo "Removing Agent Desktop-owned Claude Code settings..."
cargo run --locked --quiet --manifest-path "$repo_root/Cargo.toml" -- disconnect-agents

certificate="$root/runtime/certs/server-ca.crt"
if [[ "$(uname -s)" == Darwin && -f "$certificate" ]]; then
  command -v openssl >/dev/null || {
    echo "openssl is required to identify the generated CA" >&2
    exit 1
  }
  command -v security >/dev/null || {
    echo "the macOS security command is required" >&2
    exit 1
  }
  keychain="$HOME/Library/Keychains/login.keychain-db"
  fingerprint="$(openssl x509 -in "$certificate" -noout -fingerprint -sha256 \
    | cut -d= -f2 | tr -d ':')"
  if security find-certificate -a -Z "$keychain" 2>/dev/null | grep -q "$fingerprint"; then
    echo "Removing the generated CA from the macOS login keychain..."
    while security find-certificate -a -Z "$keychain" 2>/dev/null | grep -q "$fingerprint"; do
      security delete-certificate -t -Z "$fingerprint" "$keychain"
    done
  fi
elif [[ "$(uname -s)" != Darwin && -f "$certificate" ]]; then
  echo "Remove the generated CA from your browser or OS trust store manually." >&2
fi

echo "Removing the laptop-local server stack and data..."
docker compose -f "$root/compose.yaml" down --volumes --remove-orphans --rmi local

if [[ "$identity_root" != "$HOME/.config/agentdesktop-managed-local" ]]; then
  echo "refusing unexpected identity path $identity_root" >&2
  exit 1
fi
rm -rf "$identity_root" "$root/runtime"
rm -f "$root/.env"

echo "Laptop-local managed-VM state reset."