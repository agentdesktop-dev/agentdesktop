# Desired Configuration Schema

|Field|Type|Description|
|-|-|-|
|`inferenceGateways`|object|Named inference gateways that managed developer tools can use.<br><br>Names may contain letters, numbers, `.`, `-`, and `_`.|
|`inferenceGateways.*.authentication`|object|Authentication mechanism used when connecting to this gateway.|
|`inferenceGateways.*.authentication.audience`|string|Audience placed in the issued JWT. This must match the gateway's expected audience.|
|`inferenceGateways.*.authentication.type`|enum|Possible values: `controllerJwt`.|
|`inferenceGateways.*.url`|string|Base HTTP or HTTPS URL of the inference gateway.<br><br>The URL must include a host and cannot include credentials, a query, or a fragment.|
|`programs`|object|Per-program settings reconciled on this device.|
|`programs.claudeCode`|object|Claude Code managed-settings configuration.|
|`programs.claudeCode.inferenceGateway`|string|Name of an entry in `inferenceGateways` that Claude Code should use.|
