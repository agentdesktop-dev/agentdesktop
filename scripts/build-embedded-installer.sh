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
  --bin agentgateway-edge-install \
  --bin agentgateway-edge-connector \
  --bin agentgateway-edge-identity

AGENTGATEWAY_EDGE_PAYLOAD_INSTALLER=$root/target/release/agentgateway-edge-install \
AGENTGATEWAY_EDGE_PAYLOAD_CONNECTOR=$root/target/release/agentgateway-edge-connector \
AGENTGATEWAY_EDGE_PAYLOAD_IDENTITY=$root/target/release/agentgateway-edge-identity \
AGENTGATEWAY_EDGE_PAYLOAD_AGENTGATEWAY=$agentgateway \
AGENTGATEWAY_EDGE_PAYLOAD_CONFIG=$config \
  cargo build --release --features embedded-installer --bin agentgateway-edge-installer

printf 'built %s\n' "$root/target/release/agentgateway-edge-installer"