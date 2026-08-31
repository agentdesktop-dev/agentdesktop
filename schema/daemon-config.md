# Daemon Configuration Schema

|Field|Type|Description|
|-|-|-|
|`controller`|object|Controller connection settings. Omit this field to run without fleet management.|
|`controller.address`|string|HTTPS address of the controller's fleet API.|
|`controller.caCertificatePath`|string|Path to a PEM-encoded CA certificate used to verify the controller.<br><br>Omit this field to use the operating system's trusted certificate roots.|
|`controller.heartbeatInterval`|string|Interval between device heartbeats. Defaults to `30s`.|
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
|`programs.openCode`|object|OpenCode managed configuration.|
|`programs.openCode.managedConfig`|object|Arbitrary values written to OpenCode's system-managed configuration.|
|`programs.openCode.model`|string|Model ID selected from `models` when using the LLM gateway.<br><br>This is required when a top-level `llmGateway` is configured.|
|`programs.openCode.models`|object|Models exposed by the managed LLM gateway provider, keyed by model ID.<br><br>Each value is an arbitrary OpenCode model configuration object. At least<br>one model is required when a top-level `llmGateway` is configured.|
|`programs.openCode.useLlmGateway`|boolean|Whether this program uses the top-level LLM gateway.|
|`telemetry`|object|Telemetry collected from managed developer tools.|
|`telemetry.events`|[]enum|Event names to collect. `tool.use.input` implies `tool.use` and includes tool arguments.<br>Possible values: `session.new`, `tool.use`, `tool.use.input`.|
