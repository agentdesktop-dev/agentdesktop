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
|`programs.claudeCode`|object|Claude Code managed-settings configuration.|
|`programs.claudeCode.inferenceGateway`|string|Name of an entry in `inferenceGateways` that Claude Code should use.|
