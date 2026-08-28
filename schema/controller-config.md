# Controller Configuration Schema

|Field|Type|Description|
|-|-|-|
|`adminListen`|string|Loopback address on which the controller management UI listens.|
|`allowInsecureDev`|boolean|Permit a non-HTTPS OIDC issuer for isolated local development.<br><br>This escape hatch is only appropriate for isolated local development.|
|`daemonConfig`|object|Daemon configuration distributed to enrolled devices.|
|`daemonConfig.path`|string|Path to the watched YAML configuration distributed to enrolled devices.<br><br>Relative paths are resolved from the controller configuration directory.<br>Valid file changes are published to connected devices automatically.|
|`daemonConfig.revision`|integer|Monotonically increasing revision assigned to the daemon configuration.|
|`daemonConfig.configMap`|object|Writable configuration stored in a dedicated Kubernetes ConfigMap.<br>Kubernetes ConfigMap containing writable fleet configuration.|
|`daemonConfig.configMap.dataKey`|string|Data key containing daemon configuration YAML.|
|`daemonConfig.configMap.name`|string|Name of the ConfigMap.|
|`daemonConfig.configMap.namespace`|string|Namespace containing the ConfigMap.|
|`daemonConfig.configMap.revisionKey`|string|Data key containing the positive numeric fleet revision.|
|`databaseUrl`|string|SQLite or PostgreSQL URL used for controller state.|
|`fleetListen`|string|Address on which the device-facing gRPC fleet API listens.|
|`gatewayJwt`|object|LLM gateway JWT signing settings.|
|`gatewayJwt.issuer`|string|Issuer claim placed in generated JWTs.|
|`gatewayJwt.keyId`|string|Key identifier placed in generated JWT headers.|
|`gatewayJwt.lifetime`|string|Lifetime of generated JWTs. Defaults to `5m`.|
|`gatewayJwt.privateKey`|string|Path to the PEM-encoded RSA private signing key.<br><br>Relative paths are resolved from the controller configuration directory.|
|`oidc`|object|OpenID Connect settings used for device enrollment and authorization.|
|`oidc.clientId`|string|Public OpenID Connect client identifier.|
|`oidc.issuer`|string|Exact OpenID Connect issuer URL.|
|`oidc.redirectUri`|string|Redirect URI registered for the native enrollment client.|
|`tls`|string|TLS identities used by the device-facing fleet API.<br><br>A string selects a directory containing `controller.pem`,<br>`controller-key.pem`, `device-ca.pem`, and `device-ca-key.pem`.|
|`tls.certificate`|string|Path to the PEM-encoded TLS certificate chain.<br><br>Relative paths are resolved from the controller configuration directory.|
|`tls.clientCaCertificate`|string|PEM CA roots used to verify issued device client certificates.|
|`tls.clientCaKey`|string|PEM private key used to issue device certificates from `clientCaCertificate`.<br><br>Enrolled daemons generate their own private key and send a CSR.|
|`tls.key`|string|Path to the PEM-encoded TLS private key.<br><br>Relative paths are resolved from the controller configuration directory.|
