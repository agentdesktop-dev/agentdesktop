#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
organization=${1:-}
output=${2:-}
template=$root/target/release/agentgateway-edge-managed-installer
caller=$(pwd)

if [ -n "$organization" ]; then
  case $organization in
    /*) ;;
    *) organization=$caller/$organization ;;
  esac
fi
if [ -n "$output" ]; then
  case $output in
    /*) ;;
    *) output=$caller/$output ;;
  esac
fi

cd "$root"
cargo build --release \
  --bin agentgateway-edge-install \
  --bin agentgateway-edge-connector \
  --bin agentgateway-edge-customize

AGENTGATEWAY_EDGE_INSTALLER_MODE=managed \
AGENTGATEWAY_EDGE_PAYLOAD_INSTALLER=$root/target/release/agentgateway-edge-install \
AGENTGATEWAY_EDGE_PAYLOAD_CONNECTOR=$root/target/release/agentgateway-edge-connector \
  cargo build --release --features embedded-installer --bin agentgateway-edge-installer

cp "$root/target/release/agentgateway-edge-installer" "$template"
printf 'built generic managed installer %s\n' "$template"

if [ -n "$organization" ]; then
  test -f "$organization" || {
    printf 'organization bootstrap not found: %s\n' "$organization" >&2
    exit 1
  }
  if [ -z "$output" ]; then
    output=$root/target/release/agentgateway-edge-organization-installer
  fi
  rm -f "$output"
  "$root/target/release/agentgateway-edge-customize" \
    "$template" "$organization" --output "$output"
fi