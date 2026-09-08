# Standalone LLM Gateway And Cost Reporting

This example routes Claude Code, Codex, OpenCode, and VS Code's built-in GitHub
Copilot traffic through a local agentgateway (AGW). Claude Desktop can use the
same stack with a separate system-mode configuration.

AGW forwards requests to Anthropic, OpenAI, or GitHub Copilot, records metadata
and token usage in SQLite, applies matching rates from `costs.json`, and exposes
estimated cost through its analytics API. It does not persist prompts or
completions.

The stack publishes three loopback-only ports:

| Port | Purpose | Authentication |
| --- | --- | --- |
| `4001` | Claude Code, Claude Desktop, Codex, and OpenCode | Dex OIDC token obtained by the agentdesktop credential helper |
| `4002` | VS Code GitHub Copilot proxy | Existing Copilot bearer token forwarded by AGW |
| `15000` | AGW analytics API and UI | Loopback reachability |

Both LLM listeners use the same AGW model catalog and SQLite usage ledger. On
`4001`, GPT model names route to OpenAI and everything else routes to Anthropic,
so Codex uses OpenAI models while the Claude tools use Anthropic.

## Prerequisites

- Docker with Compose
- `agentdesktop` on `PATH`
- At least one supported client
- An Anthropic API key when using Claude Code, Claude Desktop, or OpenCode
- An OpenAI API key when using Codex
- A working GitHub Copilot sign-in in VS Code when testing the VS Code path

## Start The Stack

From the repository root:

```sh
# Set only the keys for the backends you use; unset keys fall back to a placeholder.
export ANTHROPIC_API_KEY=sk-ant-...
export OPENAI_API_KEY=sk-...
docker compose -f examples/standalone-costs/compose.yaml up -d
```

If you want to track costs in the agentdesktop **Usage** view, point the daemon
at AGW's analytics summary endpoint with `llmGateway.usageUrl`. The checked-in
`config.yaml` already includes it:

```yaml
llmGateway:
  url: http://127.0.0.1:4001
  usageUrl: http://127.0.0.1:15000/api/logs/analytics/summary
```

`usageUrl` must be a loopback address. Without it, agentdesktop still routes
traffic through AGW but the Usage view reports no data.

## Configure User-Level Agents

Preview all managed changes in `config.yaml`:

```sh
agentdesktop daemon \
  --config examples/standalone-costs/config.yaml \
  --user \
  --dry-run
```

Then keep the daemon running so the OIDC credential helpers can refresh tokens:

```sh
agentdesktop daemon --config examples/standalone-costs/config.yaml --user
```

The checked-in configuration manages:

- Claude Code through `ANTHROPIC_BASE_URL` and `apiKeyHelper`.
- Codex through an AGW-backed Responses API provider, defaulting to
  `gpt-5.2`.
- OpenCode through an AGW-backed provider and credential plugin.
- VS Code through its built-in GitHub Copilot endpoint overrides.

Launch Claude Code, Codex, or OpenCode normally. Their generated settings point
at `http://127.0.0.1:4001` and obtain the Dex token from agentdesktop.

### VS Code

Agentdesktop preserves the current Copilot sign-in, provider, model picker, and
agent configuration. It adds only these values to the current profile's
`settings.json`:

```json
{
  "github.copilot.advanced": {
    "debug.overrideProxyUrl": "http://127.0.0.1:4002/v1",
    "debug.overrideCapiUrl": "http://127.0.0.1:4002"
  }
}
```

Reload VS Code after the setting changes so existing agent sessions also use
the new endpoints. VS Code sends its existing
short-lived Copilot bearer token to the loopback listener; AGW forwards that
token to `api.githubcopilot.com`. No new API key or Custom Endpoint provider is
added to VS Code.

VS Code uses both CAPI paths such as `/models` and `/responses` and compatibility
paths such as `/v1/models` and `/v1/chat/completions`. Pinned AGW v1.5 does not
implement the `Models` operation on a generic AI backend, so the standalone
config rewrites both model-discovery paths to GitHub's `/models` endpoint.
Inference requests continue through AGW's metered Copilot backend.

The checked-in Copilot catalog prices selectable models, including
`gpt-5.6-sol`. VS Code may also issue background utility-model requests whose
model IDs are not in that catalog; AGW records their calls and tokens without an
estimated cost.

This passthrough covers local Copilot chat and Agent mode requests that carry
the Copilot bearer. Agent Host and Copilot CLI sessions switch to server-side
authentication when the override is set and therefore cannot use bearer
passthrough without a separate AGW-side Copilot credential.

AGW currently documents this override for Copilot Business and Enterprise. The
setting is an advanced debug hook rather than a stable managed endpoint policy,
so pin and retest the VS Code and AGW versions before relying on it in
production.

## View Usage And Cost

After a client completes an LLM request, open agentdesktop and select **Usage**:

```sh
agentdesktop
```

AGW's native analytics page is also available at
<http://127.0.0.1:15000/ui/llm/analytics>.

The displayed amount is an estimate based on matching catalog entries, not an
Anthropic, OpenAI, or GitHub Copilot invoice. `costs.json` holds Anthropic and
OpenAI list rates under the `anthropic` and `openai` providers and, under the
`copilot` provider, a snapshot of the `github-copilot` rates published by
models.dev on 2026-09-04. GitHub Copilot subscription and premium-request
billing are not token-denominated. Requests using a model absent from the
catalog remain visible but unpriced. Only requests routed through this AGW
instance are included.

## Claude Desktop

Claude Desktop reads a system-managed inference configuration, so it cannot be
configured by the user-mode daemon above. Run a second daemon with the dedicated
configuration:

```sh
sudo "$(which agentdesktop)" daemon \
  --config examples/standalone-costs/claude-desktop.yaml
```

Do not run both daemons with the same socket path. Use the normal system daemon
for Claude Desktop when the user-mode daemon is already active.

## Stop The Stack

```sh
docker compose -f examples/standalone-costs/compose.yaml down
```

The named SQLite volume preserves usage. Add `-v` to delete the ledger.
