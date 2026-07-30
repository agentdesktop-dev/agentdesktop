#!/usr/bin/env bash
set -euo pipefail

readonly root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly network="agentgateway-edge-smoke"

source "$root_dir/scripts/container-engine.sh"

"$container_engine" rm --force agentgateway-edge-gateway >/dev/null 2>&1 || true
"$container_engine" rm --force agentgateway-edge-anthropic-mock >/dev/null 2>&1 || true
"$container_engine" rm --force agentgateway-edge-connector >/dev/null 2>&1 || true
"$container_engine" network rm "$network" >/dev/null 2>&1 || true
