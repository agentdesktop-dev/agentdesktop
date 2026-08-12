# Daemon Configuration Schema

|Field|Type|Description|
|-|-|-|
|`controller`|object|Controller connection settings. Omit this field to run without fleet management.|
|`controller.address`|string|HTTP or HTTPS address of the controller's fleet API.|
|`controller.caCertificatePath`|string|Path to a PEM-encoded CA certificate used to verify the controller.<br><br>Omit this field to use the operating system's trusted certificate roots.|
|`controller.heartbeatInterval`|string|Interval between device heartbeats. Defaults to `30s`.|
|`inferenceGateways`|object|Named inference gateways used as the local desired-state baseline.<br><br>Names may contain letters, numbers, `.`, `-`, and `_`.|
|`inferenceGateways.*.authentication`|object|Authentication mechanism used when connecting to this gateway.|
|`inferenceGateways.*.authentication.audience`|string|Audience placed in the issued JWT. This must match the gateway's expected audience.|
|`inferenceGateways.*.authentication.type`|enum|Possible values: `controllerJwt`.|
|`inferenceGateways.*.url`|string|Base HTTP or HTTPS URL of the inference gateway.<br><br>The URL must include a host and cannot include credentials, a query, or a fragment.|
|`programs`|object|Per-program settings used as the local desired-state baseline.|
|`programs.claudeCode`|object|Claude Code managed-settings configuration. Arbitrary keys are passed through directly.|
|`programs.claudeCode.inferenceGateway`|string|Name of an entry in `inferenceGateways` that Claude Code should use.<br><br>Omit this field to manage Claude Code settings without configuring an inference gateway.|
|`programs.codex`|object|Codex managed configuration.|
|`programs.codex.inferenceGateway`|string|Name of an entry in `inferenceGateways` that Codex should use.<br><br>Omit this field to manage general Codex settings without configuring an inference gateway.|
|`programs.codex.managedConfig`|object|Arbitrary values written to Codex's organization-managed TOML configuration.<br><br>Use Codex's native snake_case configuration keys. TOML has no null value,<br>so null values cannot be reconciled.|
|`programs.openCode`|object|OpenCode managed configuration.|
|`programs.openCode.inferenceGateway`|string|Name of an entry in `inferenceGateways` that OpenCode should use.<br><br>Omit this field to manage general OpenCode settings without configuring an inference gateway.|
|`programs.openCode.managedConfig`|object|Arbitrary values written to OpenCode's system-managed configuration.|
|`programs.openCode.model`|string|Model ID selected from `models` when using the inference gateway.<br><br>This is required when `inferenceGateway` is set.|
|`programs.openCode.models`|object|Models exposed by the managed inference-gateway provider, keyed by model ID.<br><br>Each value is an arbitrary OpenCode model configuration object. At least<br>one model is required when `inferenceGateway` is set.|
