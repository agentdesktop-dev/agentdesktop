#!/bin/sh
set -eu

for _ in $(seq 1 180); do
    if node -e 'fetch("http://127.0.0.1:18080/api/v1/settings").then(response => process.exit(response.ok ? 0 : 1)).catch(() => process.exit(1))'; then
        exec agentdesktop daemon \
            --user \
            --config /demo/agentdesktop.yaml \
            --state-dir "$HOME/.local/state/agentdesktop" \
            --socket "$HOME/.local/state/agentdesktop/agentdesktop.sock"
    fi
    sleep 1
done

echo "timed out waiting for the managed controller" >&2
exit 1