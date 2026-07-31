#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
agentgateway=${1:-$root/../agentgateway/target/ci/agentgateway}
config=${2:-$root/container/agentgateway-smoke.yaml}

test -x "$agentgateway" || {
  printf 'Agent Gateway binary not found or not executable: %s\n' "$agentgateway" >&2
  exit 1
}
test -f "$config" || {
  printf 'starter config not found: %s\n' "$config" >&2
  exit 1
}

cd "$root"
cargo build --release \
  --bin agentdesktop-install \
  --bin agentdesktop

AGENTDESKTOP_INSTALLER_MODE=standalone \
AGENTDESKTOP_PAYLOAD_INSTALLER=$root/target/release/agentdesktop-install \
AGENTDESKTOP_PAYLOAD_CONNECTOR=$root/target/release/agentdesktop \
AGENTDESKTOP_PAYLOAD_AGENTGATEWAY=$agentgateway \
AGENTDESKTOP_PAYLOAD_CONFIG=$config \
  cargo build --release --features embedded-installer --bin agentdesktop-installer

printf 'built %s\n' "$root/target/release/agentdesktop-installer"