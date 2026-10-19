# Codex subscription through Agentdesktop

Use Codex's existing ChatGPT login for inference while separately authenticating
to the organization's gateway. Agentdesktop manages the Codex provider, local
proxy route, and pairing header; Codex owns ChatGPT sign-in and token refresh.

This requires a user-mode daemon. The Windows system service cannot run the
per-user proxy. Update both the controller and daemon before distributing
`programs.codex.auth: subscription` to a fleet.

## Run

From the repository root, start the example gateway and identity provider:

```sh
docker compose -f examples/codex-subscription/compose.yaml up -d
agentdesktop daemon --user --config examples/codex-subscription/config.yaml
```

Complete organization sign-in in the browser. The local Dex account is
`admin@example.com` with password `password`. Codex must also be signed in to
ChatGPT using its normal `codex login` flow. Run Codex in another terminal after
the daemon applies the configuration.

Agentdesktop manages `~/.codex/config.toml` in user mode. A pre-existing file
without Agentdesktop's ownership header is reported as a conflict and is not
overwritten. Migrate the settings you need into `programs.codex.managedConfig`
and resolve the reported conflict before testing; do not delete Codex's separate
login file. A `daemon.codex.config` override must point to a configuration layer
the installed Codex version actually reads, not `requirements.toml`.

The reconciler generates a provider with `requires_openai_auth = true`, forces
ChatGPT login, disables WebSockets for this HTTP-only route, and writes the
pairing header into an owner-only configuration file. It does not write a
gateway credential command or copy ChatGPT tokens into the file.

## Credential flow

```text
Codex -> local /codex proxy -> gateway -> ChatGPT Codex backend
```

The local proxy puts gateway identity in `Authorization` and Codex's bearer in
`x-llm-token`. The gateway strictly validates organization identity first, then
restores the ChatGPT bearer and strips `x-llm-token`. The gateway forwards the
account-identification headers and maps `/codex-subscription/responses` to
`https://chatgpt.com/backend-api/codex/responses`, not the API-key endpoint at
`api.openai.com/v1`.

For controller-issued gateway credentials, authorize client ID `codex` and set
the same subscription configuration in the controller's effective policy. The
controller then marks these requests as subscription-billed. OIDC-only deployments
must classify subscription traffic using the dedicated gateway route when
collecting usage.

The proxy must be enabled locally with `daemon.llmProxy.listen`; `proxyUrl` and
the program configuration belong in the controller-distributed policy on enrolled
devices. `llmGateway.githubOAuth` is not required for the managed `/codex` route.
When the proxy is unavailable, the existing `whenProxyUnavailable` policy applies:
`failClosed` retains existing managed settings; `failOpen` removes the managed
file. Before the first successful apply there is no managed configuration to keep.

## Validation and Production

The integration test runs the real Codex 0.153.4 CLI with synthetic ChatGPT/OIDC
credentials against a mock gateway and verifies streamed responses, separated
credentials, account headers, and cleanup. It does not validate a paid ChatGPT
account against OpenAI or certify provider terms. Validate both login-refresh
flows and actual subscription access before rollout.

This local example intentionally uses loopback HTTP and a demonstration Dex
account. Use HTTPS and your production identity provider outside local testing.
Protect pairing and credential files with user-only permissions or Windows ACLs,
keep tokens out of logs, and review provider terms and credential-handling policy.

Stop the example services with:

```sh
docker compose -f examples/codex-subscription/compose.yaml down
```
