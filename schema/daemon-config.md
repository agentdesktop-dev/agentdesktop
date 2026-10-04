# Daemon Configuration Schema

|Field|Type|Description|
|-|-|-|
|`controller`|object|Controller connection settings. Omit this field to run without fleet management.|
|`controller.address`|string|HTTPS address of the controller's fleet API.|
|`controller.caCertificatePath`|string|Path to a PEM-encoded CA certificate used to verify the controller.<br><br>Omit this field to use the operating system's trusted certificate roots.|
|`controller.deviceAuthorization`|boolean|Enroll with the OAuth 2.0 Device Authorization Grant (RFC 8628) instead<br>of a browser redirect to a local callback.<br><br>Use this on hosts without a local browser, such as servers reached over<br>SSH. The daemon logs a verification URL and user code to approve from<br>any other device. The identity provider must support device<br>authorization for the controller's client.|
|`controller.heartbeatInterval`|string|Interval between device heartbeats. Defaults to `30s`.|
|`daemon`|object|Local daemon startup settings. Not accepted in controller-delivered policy.|
|`daemon.claudeCode`|object|Claude Code paths.|
|`daemon.claudeCode.config`|string|Configuration file.|
|`daemon.claudeDesktop`|object|Claude Desktop paths.|
|`daemon.claudeDesktop.config`|string|Configuration file.|
|`daemon.claudeDesktop.credentialHelper`|string|Credential helper path.|
|`daemon.codex`|object|Codex paths.|
|`daemon.codex.config`|string|Configuration file.|
|`daemon.grok`|object|Grok Build paths.|
|`daemon.grok.config`|string|Configuration file.|
|`daemon.llmProxy`|object|Local loopback LLM proxy.|
|`daemon.llmProxy.clientId`|string|Credential policy client ID used by the proxy. Defaults to `vscode`.|
|`daemon.llmProxy.listen`|string|Loopback address to listen on. Unset disables the proxy. Rejected in system mode.|
|`daemon.oidcCallbackListen`|string|Override the OIDC callback bind address.|
|`daemon.openCode`|object|OpenCode paths.|
|`daemon.openCode.config`|string|Configuration file.|
|`daemon.openCode.plugin`|string|Credential plugin path.|
|`daemon.socket`|string|Local API Unix socket or Windows named pipe.|
|`daemon.stateDir`|string|Persistent daemon state directory.|
|`daemon.user`|boolean|Manage the current user’s tool settings instead of system settings.|
|`inventoryInterval`|string|Interval between inventory refreshes. Defaults to `15m`, and must be<br>greater than zero.<br><br>Discovery walks user home directories and developer-tool configuration<br>files, so this trades inventory freshness against local disk activity.|
|`llmGateway`|object|LLM gateway used by managed developer tools.|
|`llmGateway.authentication`|object|Authentication mechanism used when connecting to this gateway.|
|`llmGateway.authentication.allowedClientIds`|[]string|Client identifiers permitted to request credentials for this gateway.|
|`llmGateway.authentication.audience`|string|Audience placed in the issued JWT. This must match the gateway's expected audience.|
|`llmGateway.authentication.type`|enum|Possible values: `controllerJwt`.|
|`llmGateway.authentication.allowInsecure`|boolean|Permit loopback HTTP endpoints for isolated local development.|
|`llmGateway.authentication.clientId`|string|Public OpenID Connect client identifier.|
|`llmGateway.authentication.issuer`|string|Exact OpenID Connect issuer URL.|
|`llmGateway.authentication.redirectUri`|string|Loopback redirect URI registered for the native client.|
|`llmGateway.authentication.scopes`|[]string|Scopes requested during sign-in.|
|`llmGateway.authentication.type`|enum|Possible values: `oidc`.|
|`llmGateway.githubOAuth`|object|GitHub App OAuth used by the local proxy for the x-llm-token header.|
|`llmGateway.githubOAuth.clientId`|string|GitHub App client ID. Required when `source` is `deviceFlow`, where the<br>App must also enable Device Flow. Unused when `source` is `request`.|
|`llmGateway.githubOAuth.source`|enum|Where the GitHub credential comes from.<br>Possible values: `deviceFlow`, `request`.|
|`llmGateway.proxyUrl`|string|Base URL the local LLM proxy forwards to, when it differs from `url`.<br><br>`url` is shared by every program that sets `useLlmGateway`, so it can<br>only carry one path prefix. A gateway that puts each provider behind its<br>own prefix therefore cannot serve both a program and the proxy from one<br>value. Setting this leaves `url` to the programs and gives the proxy its<br>own target. The same rules as for `url` apply.|
|`llmGateway.url`|string|Base HTTP or HTTPS URL of the LLM gateway.<br><br>The URL must include a host and cannot include credentials, a query, or a fragment.|
|`programs`|object|Per-program settings reconciled on this device.|
|`programs.claudeCode`|object|Claude Code managed-settings configuration. Arbitrary keys are passed through directly.|
|`programs.claudeCode.auth`|enum|Upstream authentication used by this agent.<br>Possible values: `subscription`.|
|`programs.claudeCode.useLlmGateway`|boolean|Whether this program uses the top-level LLM gateway.|
|`programs.claudeDesktop`|object|Claude Desktop managed configuration. Arbitrary keys are passed through directly.|
|`programs.claudeDesktop.auth`|enum|Upstream authentication used by this agent.<br>Possible values: `subscription`.|
|`programs.claudeDesktop.useLlmGateway`|boolean|Whether this program uses the top-level LLM gateway.|
|`programs.codex`|object|Codex managed configuration.|
|`programs.codex.managedConfig`|object|Arbitrary values written to Codex's organization-managed TOML configuration.<br><br>Use Codex's native snake_case configuration keys. TOML has no null value,<br>so null values cannot be reconciled.|
|`programs.codex.useLlmGateway`|boolean|Whether this program uses the top-level LLM gateway.|
|`programs.grok`|object|Grok Build managed configuration.|
|`programs.grok.managedConfig`|object|Arbitrary values written to Grok's organization-managed TOML configuration.<br><br>Use Grok's native snake_case configuration keys. TOML has no null value,<br>so null values cannot be reconciled.|
|`programs.grok.model`|string|Catalog ID and API model used when pointing Grok at the LLM gateway.<br><br>This is required when a top-level `llmGateway` is configured. If `models`<br>is empty, agentdesktop creates a catalog entry with this ID.|
|`programs.grok.models`|object|Extra Grok `[model.<id>]` catalog entries, keyed by catalog ID.<br><br>Each value is an arbitrary Grok model object. Generated gateway<br>`base_url` and `auth_provider` values take precedence. When gateway<br>authentication is configured, `api_key` and `env_key` are removed from<br>these entries, including values supplied through `managedConfig`.<br>When this map is non-empty, `model` must name one of its keys.|
|`programs.grok.useLlmGateway`|boolean|Whether this program uses the top-level LLM gateway.|
|`programs.openCode`|object|OpenCode managed configuration.|
|`programs.openCode.managedConfig`|object|Arbitrary values written to OpenCode's system-managed configuration.|
|`programs.openCode.model`|string|Model ID selected from `models` when using the LLM gateway.<br><br>This is required when a top-level `llmGateway` is configured.|
|`programs.openCode.models`|object|Models exposed by the managed LLM gateway provider, keyed by model ID.<br><br>Each value is an arbitrary OpenCode model configuration object. At least<br>one model is required when a top-level `llmGateway` is configured.|
|`programs.openCode.useLlmGateway`|boolean|Whether this program uses the top-level LLM gateway.|
|`sandbox`|object|Local execution sandbox required for managed developer tools.|
|`sandbox.filesystem`|object|Filesystem access available to sandboxed commands.|
|`sandbox.filesystem.denied`|[]string|Paths sandboxed commands may neither read nor modify.|
|`sandbox.filesystem.writable`|[]string|Additional paths sandboxed commands may modify.|
|`sandbox.network`|object|Network destinations available to sandboxed commands.|
|`sandbox.network.allowedDomains`|[]string|Domains sandboxed commands may contact. An empty set disables network access.|
|`telemetry`|object|Telemetry collected from managed developer tools.|
|`telemetry.events`|[]enum|Event names to collect. `tool.use.input` implies `tool.use` and includes tool arguments.<br>Possible values: `session.new`, `tool.use`, `tool.use.input`.|
