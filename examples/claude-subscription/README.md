# Claude subscription through Agentdesktop

This user-local example builds on [`examples/claude`](../claude) but includes
the small set of copied and modified configuration needed to run independently.
It does not run a controller or use an Anthropic API key.

Start Dex and Agentgateway with this example's Compose file:

```console
docker compose -f examples/claude-subscription/compose.yaml up -d
```

Run Agentdesktop directly as the current user:

```console
agentdesktop daemon \
  --config examples/claude-subscription/config.yaml \
  --user
```

Sign in to the required organization identity, then connect or skip the
optional model-provider subscription on the Agentdesktop checklist. Start
Claude Code after the daemon finishes applying its configuration:

```console
claude
```

When connected, Agentdesktop supplies this composite credential:

```text
agentdesktop:<subscription-token>:<oidc-token>
```

The Agentgateway policy extracts the OIDC token for caller validation and
replaces the upstream Authorization header with the subscription token. The
example uses permissive JWT mode for local experimentation; do not treat it as
a production authentication policy.
