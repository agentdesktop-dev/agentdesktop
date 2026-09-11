# Fleet usage scenario

This scenario runs the fleet controller, Dex, and agentgateway locally so you
can see LLM usage attributed per enrolled device. Agentgateway meters every
request into a SQLite request log; the controller reads that log through the
gateway's admin API and scopes every device's view to its own traffic.

Ports are chosen so this can run beside the `standalone-costs` scenario:
Dex `5558`, controller `8444` (fleet) / `8081` (admin UI), Agentgateway `4003`
(LLM) / `15001` (admin, loopback only).

Run all commands from the repository root.

## Start the control plane

```console
./examples/claude/create-keys.sh          # writes /tmp/agentdesktop-keys
docker compose -f examples/fleet-costs/compose.yaml up -d dex
cargo run -p agentdesktop-controller -- --config examples/fleet-costs/controller.yaml
```

In another terminal, start agentgateway with provider credentials:

```console
export ANTHROPIC_API_KEY=sk-ant-...
export OPENAI_API_KEY=sk-...                # optional, for Codex models
docker compose -f examples/fleet-costs/compose.yaml up -d agentgateway
curl --fail --head http://127.0.0.1:4003/
```

The controller UI is at <http://127.0.0.1:8081>. **Settings** shows
"LLM usage reports" enabled.

## Enroll a device

Run a daemon in user mode with its own state directory. The flags below keep
every managed file under `target/agentdesktop-fleet` so nothing on the
workstation is touched:

```console
F="$PWD/target/agentdesktop-fleet"; mkdir -p "$F/state" "$F/home/.claude" "$F/managed"
cargo run -p agentdesktop -- daemon --user \
  --socket "$F/state/agentdesktop.sock" \
  --config examples/fleet-costs/agentdesktop.yaml \
  --state-dir "$F/state" \
  --claude-code-settings "$F/home/.claude/settings.json" \
  --claude-desktop-managed-settings "$F/managed/claude-desktop.json" \
  --claude-desktop-credential-helper "$F/managed/claude-desktop-helper" \
  --codex-managed-config "$F/managed/codex.toml" \
  --open-code-managed-config "$F/managed/opencode.jsonc" \
  --open-code-plugin "$F/managed/opencode-plugin.js" \
  --vscode-settings "$F/managed/vscode-settings.json"
```

Sign in at the printed URL with `admin@example.com` / `password`
(`dev@example.com` / `password` is a second user for a second device). After
enrollment the controller pushes `daemon.yaml`, which points Claude Code and
Codex at the gateway with controller-issued JWTs.

Repeat with a different directory (for example `target/agentdesktop-fleet-b`)
to enroll a second device.

## Generate and inspect usage

Send a request as the enrolled device. The credential is a short-lived JWT
carrying the device ID; Agentgateway records it as the `device_id` attribute:

```console
F="$PWD/target/agentdesktop-fleet"
TOKEN=$(curl -sS --unix-socket "$F/state/agentdesktop.sock" \
  'http://d/v1/llm-gateway/credential?client_id=claude-code' | jq -r .credential)
curl -sS http://127.0.0.1:4003/v1/messages \
  -H "authorization: Bearer $TOKEN" -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' -H 'user-agent: claude-cli/2.1.260' \
  -d '{"model":"claude-haiku-4-5","max_tokens":20,"messages":[{"role":"user","content":"Reply with exactly the word: pong"}]}'
```

Then compare the three views:

```console
# This device only, fetched from the controller over mTLS
curl -sS --unix-socket "$F/state/agentdesktop.sock" 'http://d/v1/llm-gateway/usage?range=hour' | jq

# Whole fleet, grouped by device (operator view)
curl -s 'http://127.0.0.1:8081/api/v1/usage?range=hour' | jq

# One device, same model/agent breakdown the desktop shows
DEV=$(curl -s http://127.0.0.1:8081/api/v1/devices | jq -r '.[0].id')
curl -s "http://127.0.0.1:8081/api/v1/devices/$DEV/usage?range=hour" | jq
```

Open **Usage** in the controller UI for the per-device table, or a device page
for its model and agent breakdown with interaction drill-down. Run
`AGENTDESKTOP_SOCKET="$F/state/agentdesktop.sock" agentdesktop` for the
desktop view of the same device.

## Stop the scenario

Stop the daemon(s) and controller with Ctrl-C, then:

```console
docker compose -f examples/fleet-costs/compose.yaml down -v
rm -f /tmp/agentdesktop-fleet-controller.db
```
