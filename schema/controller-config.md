# Controller Configuration Schema

|Field|Type|Description|
|-|-|-|
|`adminAccess`|object|Management authentication, separate from device enrollment. Defaults to local-only access.|
|`adminAccess.mode`|enum|Unauthenticated access restricted to an IPv4 or IPv6 loopback listener.<br>Possible values: `local`.|
|`adminAccess.mode`|enum|Possible values: `authenticated`.|
|`adminAccess.provider`|object|Settings owned and validated by the compiled-in authentication extension.<br>Relative paths inside it are resolved from the controller configuration directory.|
|`adminAccess.publicUrl`|string|Exact external HTTPS origin, without a path other than `/`.|
|`adminAccess.transport`|object|How HTTPS reaches the management listener.|
|`adminAccess.transport.mode`|enum|HTTPS terminates at a trusted proxy; the plaintext listener must not be public.<br>Possible values: `proxy`.|
|`adminAccess.transport.certificate`|string|PEM certificate chain. Relative paths are resolved from the configuration directory.|
|`adminAccess.transport.key`|string|PEM private key. Relative paths are resolved from the configuration directory.|
|`adminAccess.transport.mode`|enum|Possible values: `tls`.|
|`adminListen`|string|Address on which the controller management UI listens. Local mode requires loopback.|
|`allowInsecureDev`|boolean|Permit a non-HTTPS OIDC issuer for isolated local development.<br><br>This escape hatch is only appropriate for isolated local development.|
|`daemonConfig`|object|Daemon configuration distributed to enrolled devices.|
|`daemonConfig.path`|string|Path to the watched YAML configuration distributed to enrolled devices.<br><br>Relative paths are resolved from the controller configuration directory.<br>Valid file changes are published to connected devices automatically.|
|`daemonConfig.revision`|integer|Monotonically increasing revision assigned to the daemon configuration.|
|`databaseUrl`|string|SQLite or PostgreSQL URL used for controller state.|
|`fleetLimits`|object|Limits on the device-facing fleet API that depend on the deployment.|
|`fleetLimits.connectionsPerAddress`|integer|Open fleet connections allowed from one client address: an IPv4<br>address, or an IPv6 /64. Raise it when more devices share an address,<br>such as behind a NAT or a load balancer that does not preserve client<br>addresses. Defaults to `4096`.|
|`fleetLimits.deviceMessageBurst`|integer|Messages one device may send in a burst on its `Connect` stream, after<br>which it may send one more every `deviceMessageRefill`. Once a device<br>has sent its burst, the controller reads its messages no faster than<br>that, and the device waits to send more. Defaults to `600`.|
|`fleetLimits.deviceMessageRefill`|string|Time for a device to get back one message. Defaults to `50ms`.|
|`fleetLimits.enrollmentBurst`|integer|Enrollments one client address may start in a burst, after which it may<br>start one more every `enrollmentRefill`. Defaults to `60`.|
|`fleetLimits.enrollmentInFlight`|integer|Enrollment requests in progress at once across all clients. Enrollment<br>calls the identity provider. Defaults to `64`.|
|`fleetLimits.enrollmentRefill`|string|Time for a client address to get back one enrollment. Defaults to `1s`.|
|`fleetLimits.maxConnections`|integer|Open fleet connections allowed in total. At the limit, the connection<br>idle the longest is closed to admit a new one; when none is idle, the<br>new one is closed. Defaults to `50000`.|
|`fleetLimits.maxMessageBytes`|integer|Largest message, in bytes, that a device and the controller may send<br>each other. Requests from clients without a device certificate, such as<br>enrollment, stay limited to 64 KiB. Devices accept at most 4 MiB, so<br>raising this only raises what devices may send. Defaults to `4194304`<br>(4 MiB).|
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
|`usage`|object|Token and cost collection from agentgateway access logs.|
|`usage.tokenFile`|string|Path to a file containing the bearer token agentgateway sends with each export. The token must be at least 32 characters.<br><br>Relative paths are resolved from the controller configuration directory.|
