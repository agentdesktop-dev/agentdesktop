# VS Code Copilot through Agentgateway

Routes local VS Code Copilot chat and Agent mode through a loopback
Agentgateway listener, without a controller:

| Client | Gateway | Authentication |
| --- | --- | --- |
| Local VS Code Copilot chat and Agent mode | `127.0.0.1:4002` → GitHub Copilot | VS Code's existing Copilot bearer, passed through unchanged |

This route requires an existing Copilot sign-in. It does not use daemon-issued
gateway credentials, BYOK model routing, or provider API keys. No analytics,
database, or cost reporting is enabled.

## Start the gateway

Prerequisites:

- Docker Compose and `agentdesktop` on `PATH`.
- VS Code with a working GitHub Copilot sign-in.

From the repository root:

```sh
docker compose -f examples/vscode/compose.yaml up -d
```

[compose.yaml](compose.yaml) pins AGW **1.5.0** and publishes only loopback
port 4002. Do not run another local example on the same port.

## Configure VS Code

[config.yaml](config.yaml) points VS Code's built-in Copilot at the listener:

```yaml
programs:
  vscode:
    useLlmGateway: true
    copilotProxyUrl: http://127.0.0.1:4002/v1
```

```sh
agentdesktop daemon --config examples/vscode/config.yaml --user --dry-run
agentdesktop daemon --config examples/vscode/config.yaml --user
```

Agentdesktop merges only the Copilot endpoint overrides into the configured
**VS Code User settings** file:

```json
{
  "github.copilot.advanced": {
    "debug.overrideProxyUrl": "http://127.0.0.1:4002/v1",
    "debug.overrideCapiUrl": "http://127.0.0.1:4002"
  }
}
```

This requires `--user`. Set `daemon.vscode.config` in the local configuration
file to select another User settings file explicitly; Agentdesktop does not
automatically find the active profile or configure all profiles, workspaces,
Remote sessions, or system settings.
Unrelated settings and comments are preserved. Removing `programs.vscode` or
setting `useLlmGateway: false` restores Agentdesktop-owned overrides, without
replacing user edits made afterward.

Reload VS Code after reconciliation. Copilot keeps its existing sign-in,
model picker and provider; no Custom Endpoint provider or daemon credential
is added. AGW forwards the Copilot bearer to `api.githubcopilot.com`.
Both `/models` and `/v1/models` are rewritten to GitHub's native `/models`
endpoint because AGW 1.5.0's generic AI backend does not implement model
discovery. Inference uses the Copilot backend.

`copilotProxyUrl` must end in `/v1` and use HTTPS or a loopback HTTP host,
without credentials, query, or fragment. Keep port 4002 local. This passthrough
covers local requests carrying the Copilot bearer, not Agent Host or standalone
CLI sessions that use server-side authentication. The endpoint overrides are
advanced debug settings, not a stable managed policy. AGW documents them for
Copilot Business and Enterprise; pin and retest the VS Code and AGW versions
before deployment.

## Stop

Remove or disable `programs.vscode`, let the daemon reconcile, then reload
VS Code before stopping the gateway. Stop the daemon and the example stack:

```sh
docker compose -f examples/vscode/compose.yaml down
```
