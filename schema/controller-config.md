# Controller Configuration Schema

|Field|Type|Description|
|-|-|-|
|`adminListen`|string|Loopback address on which the controller management UI listens.|
|`allowInsecureDev`|boolean|Permit plaintext remote fleet traffic and non-HTTPS OIDC.<br><br>This escape hatch is only appropriate for isolated local development.|
|`databaseUrl`|string|SQLite or PostgreSQL URL used for controller state.|
|`desiredConfig`|object|Desired configuration distributed to enrolled devices.|
|`desiredConfig.path`|string|Path to the YAML configuration distributed to enrolled devices.<br><br>Relative paths are resolved from the controller configuration directory.|
|`desiredConfig.revision`|integer|Monotonically increasing revision assigned to the desired configuration.|
|`fleetListen`|string|Address on which the device-facing gRPC fleet API listens.|
|`gatewayJwt`|object|Inference-gateway JWT signing settings.|
|`gatewayJwt.issuer`|string|Issuer claim placed in generated JWTs.|
|`gatewayJwt.keyId`|string|Key identifier placed in generated JWT headers.|
|`gatewayJwt.lifetime`|string|Lifetime of generated JWTs. Defaults to `5m`.|
|`gatewayJwt.privateKey`|string|Path to the PEM-encoded RSA private signing key.<br><br>Relative paths are resolved from the controller configuration directory.|
|`oidc`|object|OpenID Connect enrollment settings. Omit to disable new enrollment.|
|`oidc.clientId`|string|Public OpenID Connect client identifier.|
|`oidc.issuer`|string|Exact OpenID Connect issuer URL.|
|`oidc.redirectUri`|string|Redirect URI registered for the native enrollment client.|
|`tls`|object|TLS identity used by the device-facing fleet API.|
|`tls.certificate`|string|Path to the PEM-encoded TLS certificate chain.<br><br>Relative paths are resolved from the controller configuration directory.|
|`tls.key`|string|Path to the PEM-encoded TLS private key.<br><br>Relative paths are resolved from the controller configuration directory.|
