# GitHub Copilot CLI and VS Code through Agentgateway

Two independent routes, without a controller:

| Client | Gateway | Authentication |
| --- | --- | --- |
| `agentdesktop copilot -- …` | `127.0.0.1:4001/v1` → OpenAI or Anthropic | Rotating OIDC credentials from the daemon |
| Local VS Code Copilot chat and Agent mode | `127.0.0.1:4002` → GitHub Copilot | VS Code's existing Copilot bearer, passed through unchanged |

The CLI route uses BYOK and does not require a Copilot subscription. The VS Code
route requires an existing Copilot sign-in. These are **not** interchangeable
authentication mechanisms. No analytics, database, or cost reporting is enabled.

## Start the gateway

Prerequisites:

- Docker Compose and `agentdesktop` on `PATH`.
- For the CLI route: GitHub Copilot CLI **1.0.84+** on `PATH` as `copilot`
  (this example targets 1.0.85), and an OpenAI or Anthropic API key.
- For the VS Code route: VS Code with a working GitHub Copilot sign-in.

From the repository root, set only the key for your CLI model:

```sh
export OPENAI_API_KEY='<openai-api-key>'
# For the Anthropic model instead:
# export ANTHROPIC_API_KEY='<anthropic-api-key>'
docker compose -f examples/copilot/compose.yaml up -d
```

For VS Code alone, neither provider key is needed. Unset keys use an `unused`
placeholder; requests to those backends will fail. Keys remain in the gateway
environment, not the daemon configuration.

[compose.yaml](compose.yaml) pins AGW **1.5.0**, publishes only loopback ports
4001, 4002 and 5557, and reuses the [standalone Dex configuration](../standalone/dex.yaml).
Do not run another local example on the same ports. Dex is an isolated test
identity provider; see the [standalone quickstart](https://agentdesktop.dev/docs/getting-started/standalone/)
for local sign-in. The init service copies only Dex's public signing keys into
a read-only gateway mount. Recreate the stack after restarting Dex so that AGW
receives the new keys; this is not a production key-rotation setup.

## GitHub Copilot CLI

[config.yaml](config.yaml) selects `gpt-5.2` with the default `completions`
wire API. For Anthropic, change `programs.copilotCli.model` to
`claude-haiku-4-5` and keep `wireApi: completions`. Both IDs are declared in
[agentgateway.yaml](agentgateway.yaml).

```sh
agentdesktop daemon --config examples/copilot/config.yaml --user --dry-run
agentdesktop daemon --config examples/copilot/config.yaml --user
```

Complete Dex sign-in and leave the daemon running. In another terminal:

```sh
agentdesktop copilot -- --help
agentdesktop copilot -- -p 'Reply with exactly: pong'
```

The launcher reads the daemon's **effective** configuration (local or
controller-provided), sets OpenAI-compatible BYOK environment variables for
the gateway's `/v1` endpoint, and configures the CLI's native per-request credential
helper that calls `agentdesktop credential --client-id copilot-cli`. This lets
OIDC or fleet credentials rotate while the CLI runs. Use the same socket as
the daemon if you override its default.

Only `agentdesktop copilot -- …` opts into this route. Plain `copilot` stays
unchanged. The launched process ignores the personal provider registry without
overwriting it; existing GitHub login remains intact. Agentdesktop does not
configure native CLI files or manage its sandbox policy.

The required configuration is:

```yaml
programs:
  copilotCli:
    useLlmGateway: true
    model: gpt-5.2
    wireApi: completions
```

`completions` supports OpenAI and Anthropic via AGW translation. Use `responses`
only with a compatible backend; changing the wire API does not make an
Anthropic backend support Responses. In a fleet, the controller's existing
`llmGateway.authentication.allowedClientIds` must explicitly include
`copilot-cli`. Preserve its other entries. The builder includes this ID in
fresh configurations but does not widen an imported allowlist when adding a
tool.

### Using a Copilot subscription upstream

This example's CLI route uses provider API keys, not a Copilot subscription.
To use GitHub Copilot as the AGW upstream instead, provision a separate
**AGW-side `backendAuth: copilot` credential** and a compatible backend route.
The daemon JWT is not a Copilot credential, and the launcher does not reuse
the CLI's GitHub login automatically. Do not point the CLI at the bearer-only
VS Code listener on port 4002.

## VS Code

Use [vscode.yaml](vscode.yaml) instead of the CLI configuration:

```sh
agentdesktop daemon --config examples/copilot/vscode.yaml --user --dry-run
agentdesktop daemon --config examples/copilot/vscode.yaml --user
```

To use both routes with one daemon, add the `programs.vscode` entry from
[vscode.yaml](vscode.yaml) to [config.yaml](config.yaml). Do not start a second
daemon on the same socket.

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

This requires `--user`. Use `--vscode-settings` to select another User settings
file explicitly; Agentdesktop does not automatically find the active profile
or configure all profiles, workspaces, Remote sessions, or system settings.
Unrelated settings and comments are preserved. Removing `programs.vscode` or
setting `useLlmGateway: false` restores Agentdesktop-owned overrides, without
replacing user edits made afterward.

Reload VS Code after reconciliation. Copilot keeps its existing sign-in,
model picker and provider; no Custom Endpoint provider or daemon credential
is added. AGW forwards the Copilot bearer to `api.githubcopilot.com`.
Both `/models` and `/v1/models` are rewritten to GitHub's native `/models`
endpoint because AGW 1.5.0's generic AI backend does not implement model
discovery. Inference uses the Copilot backend.

Keep port 4002 local. This passthrough covers local requests carrying the
Copilot bearer, not Agent Host or standalone CLI sessions that use server-side
authentication. The endpoint overrides are advanced debug settings, not a
stable managed policy. AGW documents them for Copilot Business and Enterprise;
pin and retest the VS Code and AGW versions before deployment.

## Stop

Remove or disable `programs.vscode`, let the daemon reconcile, then reload
VS Code before stopping the gateway. Stop the daemon and the example stack:

```sh
docker compose -f examples/copilot/compose.yaml down -v
```

The only named volume contains public Dex signing keys. CLI provider settings
and GitHub login are not rewritten by the launcher.
