#!/bin/sh
set -eu

exec agentdesktop daemon \
  --user \
  --config /demo/config.yaml \
  --state-dir "$HOME/.local/state/agentdesktop" \
  --socket "$HOME/.local/state/agentdesktop/agentdesktop.sock"