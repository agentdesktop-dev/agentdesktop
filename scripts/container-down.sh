#!/usr/bin/env bash
set -euo pipefail

readonly root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly network="agentdesktop-smoke"

source "$root_dir/scripts/container-engine.sh"

"$container_engine" rm --force agentdesktop-gateway >/dev/null 2>&1 || true
"$container_engine" rm --force agentdesktop-anthropic-mock >/dev/null 2>&1 || true
"$container_engine" rm --force agentdesktop >/dev/null 2>&1 || true
"$container_engine" network rm "$network" >/dev/null 2>&1 || true
