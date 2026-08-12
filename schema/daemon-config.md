# Daemon Configuration Schema

|Field|Type|Description|
|-|-|-|
|`controller`|object|Controller connection settings. Omit this field to run without fleet management.|
|`controller.address`|string|HTTP or HTTPS address of the controller's fleet API.|
|`controller.caCertificatePath`|string|Path to a PEM-encoded CA certificate used to verify the controller.<br><br>Omit this field to use the operating system's trusted certificate roots.|
|`controller.heartbeatInterval`|string|Interval between device heartbeats. Defaults to `30s`.|
|`inferenceGateway`|object|Inference gateway used by managed developer tools.|
|`inferenceGateway.authentication`|object|Authentication mechanism used when connecting to this gateway.|
|`inferenceGateway.authentication.audience`|string|Audience placed in the issued JWT. This must match the gateway's expected audience.|
|`inferenceGateway.authentication.type`|enum|Possible values: `controllerJwt`.|
|`inferenceGateway.url`|string|Base HTTP or HTTPS URL of the inference gateway.<br><br>The URL must include a host and cannot include credentials, a query, or a fragment.|
|`programs`|object|Per-program settings reconciled on this device.|
|`programs.claudeCode`|object|Claude Code managed-settings configuration. Arbitrary keys are passed through directly.|
|`programs.claudeCode.useInferenceGateway`|boolean|Whether this program uses the top-level inference gateway.|
|`programs.codex`|object|Codex managed configuration.|
|`programs.codex.managedConfig`|object|Arbitrary values written to Codex's organization-managed TOML configuration.<br><br>Use Codex's native snake_case configuration keys. TOML has no null value,<br>so null values cannot be reconciled.|
|`programs.codex.useInferenceGateway`|boolean|Whether this program uses the top-level inference gateway.|
|`programs.openCode`|object|OpenCode managed configuration.|
|`programs.openCode.managedConfig`|object|Arbitrary values written to OpenCode's system-managed configuration.|
|`programs.openCode.model`|string|Model ID selected from `models` when using the inference gateway.<br><br>This is required when a top-level `inferenceGateway` is configured.|
|`programs.openCode.models`|object|Models exposed by the managed inference-gateway provider, keyed by model ID.<br><br>Each value is an arbitrary OpenCode model configuration object. At least<br>one model is required when a top-level `inferenceGateway` is configured.|
|`programs.openCode.useInferenceGateway`|boolean|Whether this program uses the top-level inference gateway.|
|`telemetry`|object|Telemetry collected from managed developer tools.|
|`telemetry.events`|[]enum|Event names to collect. `tool.use.input` implies `tool.use` and includes tool arguments.<br>Possible values: `session.new`, `tool.use`, `tool.use.input`.|
