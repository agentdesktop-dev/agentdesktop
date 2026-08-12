#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$root/../.." && pwd)"

if [[ "${1:-}" == -h || "${1:-}" == --help ]]; then
  echo "Usage: ./start-client.sh"
  exit 0
fi
if (($# != 0)); then
  echo "Usage: ./start-client.sh" >&2
  exit 2
fi

for path in "$root/runtime/organization.json" "$root/runtime/certs/server-ca.crt"; do
  if [[ ! -f "$path" ]]; then
    echo "managed-VM runtime is missing; run ./prepare.sh agentdesktop.localhost first" >&2
    exit 2
  fi
done
if ! command -v cargo >/dev/null && [[ -x "$HOME/.cargo/bin/cargo" ]]; then
  export PATH="$HOME/.cargo/bin:$PATH"
fi
command -v cargo >/dev/null || {
  echo "cargo is required" >&2
  exit 1
}
command -v npm >/dev/null || {
  echo "npm is required" >&2
  exit 1
}

export SSL_CERT_FILE="$root/runtime/certs/server-ca.crt"
export AGENTDESKTOP_ORGANIZATION_CONFIG="$root/runtime/organization.json"
export AGENTDESKTOP_IDENTITY_DIR="$HOME/.config/agentdesktop-managed-local/identity"
export AGENTDESKTOP_CREDENTIAL_STORAGE=file
export AGENTDESKTOP_DEV_PID_FILE="$root/runtime/desktop.pid"
export NO_PROXY="agentdesktop.localhost,localhost,127.0.0.1,::1${NO_PROXY:+,$NO_PROXY}"
export no_proxy="$NO_PROXY"

exec npm --prefix "$repo_root/ui" run dev:desktop