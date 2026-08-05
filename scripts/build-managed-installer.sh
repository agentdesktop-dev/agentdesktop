#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
organization=${1:-}
output=${2:-}
template=$root/target/release/agentdesktop-managed-installer
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
  --bin agentdesktop-install \
  --bin agentdesktop \
  --bin agentdesktop-capture-setup \
  --bin agentdesktop-customize

AGENTDESKTOP_INSTALLER_MODE=managed \
AGENTDESKTOP_PAYLOAD_INSTALLER=$root/target/release/agentdesktop-install \
AGENTDESKTOP_PAYLOAD_CONNECTOR=$root/target/release/agentdesktop \
AGENTDESKTOP_PAYLOAD_CAPTURE_SETUP=$root/target/release/agentdesktop-capture-setup \
  cargo build --release --features embedded-installer --bin agentdesktop-installer

cp "$root/target/release/agentdesktop-installer" "$template"
printf 'built generic managed installer %s\n' "$template"

if [ -n "$organization" ]; then
  test -f "$organization" || {
    printf 'organization bootstrap not found: %s\n' "$organization" >&2
    exit 1
  }
  if [ -z "$output" ]; then
    output=$root/target/release/agentdesktop-organization-installer
  fi
  rm -f "$output"
  "$root/target/release/agentdesktop-customize" \
    "$template" "$organization" --output "$output"
fi