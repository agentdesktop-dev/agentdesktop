# Codex subscription through Agentdesktop

Connect ChatGPT through Agentdesktop's subscription OAuth flow, just like Claude.
Agentdesktop owns subscription sign-in, secure token storage, refresh, and the
Codex credential helper. Codex connects directly to the gateway; no local LLM
proxy, pairing header, or separate `codex login` is needed.

The example runs as the current user. System mode uses the same credential-helper
contract, with credentials belonging to the daemon's account. Update both controller and daemon before distributing
`programs.codex.auth: subscription` to a fleet.

## Run

From the repository root, start the example gateway and identity provider:

```sh
docker compose -f examples/codex-subscription/compose.yaml up -d
agentdesktop daemon --user --config examples/codex-subscription/config.yaml
```

Complete organization sign-in in the browser. The local Dex account is
`admin@example.com` with password `password`. Continue to the optional subscription
step and sign in to ChatGPT. The Agentdesktop page uses port 51327 and OpenAI's
registered callback uses port 1455; keep both available during sign-in. Run Codex
in another terminal after the daemon applies the configuration.

Agentdesktop manages `~/.codex/config.toml` in user mode. A pre-existing file
without Agentdesktop's ownership header is reported as a conflict and is not
overwritten. Migrate the settings you need into `programs.codex.managedConfig`
and resolve the reported conflict before testing; do not delete Codex's separate
login file. A `daemon.codex.config` override must point to a configuration layer
the installed Codex version actually reads, not `requirements.toml`.

The reconciler generates a Responses provider with `requires_openai_auth = false`
and an Agentdesktop credential command. It never writes subscription tokens to
Codex configuration or reads Codex's own login file.

## Credential flow

```text
Codex -> gateway -> ChatGPT Codex backend
```

The helper supplies a composite bearer in `Authorization`:

```text
agentdesktop:<chatgpt-access-token>:<gateway-identity-token>:<chatgpt-account-id>
```

The first three fields match Claude's format; the fourth carries the ChatGPT
account ID. The gateway strictly validates the identity in the third field first,
then restores the provider bearer and `ChatGPT-Account-Id` together with one
header-replacement expression, preserving other request headers.

The example uses Agentgateway 1.6.0's **`llm` configuration**, not a raw HTTP
route or opaque passthrough. Codex sends `/v1/responses` to the LLM listener.
The OpenAI provider's `params.baseUrl` points at
`https://chatgpt.com/backend-api/codex`, so the upstream request reaches
`/backend-api/codex/responses`, not the API-key endpoint at `api.openai.com/v1`.
Normal model routing, LLM policies, and token-usage processing remain active.
Add rate limits or guardrails through the usual LLM configuration as needed.

For controller-issued gateway credentials, authorize client ID `codex` and set
the same subscription configuration in the controller's effective policy. The
controller then marks these requests as subscription-billed. The dedicated LLM
listener also labels its access logs with `agentdesktop.billing: subscription`,
including for OIDC-only deployments. Keep that label when adding an OTLP usage
exporter; exporting to Agentdesktop still requires the usage configuration
described in the main README.

On enrolled devices, put `llmGateway` and `programs` in controller-distributed
policy. Set `llmGateway.url` to the LLM listener's `/v1` base URL. `proxyUrl`, `githubOAuth`,
`daemon.llmProxy.listen`, and `whenProxyUnavailable` are not used for this mode.
If subscription sign-in is skipped, the helper supplies identity alone; this
example's strict LLM authentication policy rejects it rather than using a paid fallback.

## Validation and Production

The integration test runs the real Codex 0.153.4 CLI with synthetic ChatGPT/OIDC
credentials in Agentdesktop's store against a mock gateway, with no Codex login
file and no proxy. It verifies direct helper authentication, the account metadata,
streamed responses, and cleanup. It does not validate a paid ChatGPT
account against OpenAI or certify provider terms. Validate both login-refresh
flows and actual subscription access before rollout.

The example was also checked against a real Agentgateway 1.6.0 LLM pipeline with
a local mock upstream: model mapping, Responses-specific fields, streaming,
provider/account headers, malformed-credential rejection, and Prometheus token
metrics were verified. A fixture reporting 10 input and 5 output tokens produced
those counts in the gateway's LLM metrics. This does not establish live OpenAI
subscription compatibility or provider terms.

This local example intentionally uses loopback HTTP and a demonstration Dex
account. Use HTTPS and your production identity provider outside local testing.
This adapter follows Codex 0.153.4's public OAuth client registration and protocol;
they are provider-controlled and are not a guaranteed third-party API.
Protect credential files with user-only permissions or Windows ACLs,
keep tokens out of logs, and review provider terms and credential-handling policy.

Stop the example services with:

```sh
docker compose -f examples/codex-subscription/compose.yaml down
```
