#!/usr/bin/env bash
set -euo pipefail

readonly network="agentgateway-edge-smoke"

podman rm --force agentgateway-edge-connector agentgateway-edge-gateway >/dev/null 2>&1 || true
podman network rm "$network" >/dev/null 2>&1 || true
