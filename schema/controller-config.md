# Controller Configuration Schema

|Field|Type|Description|
|-|-|-|
|`adminListener`|object|Listener for the controller management UI and API.|
|`adminListener.access`|object|Management authentication, separate from device enrollment. Defaults to local-only access.|
|`adminListener.access.mode`|enum|Unauthenticated access restricted to an IPv4 or IPv6 loopback listener.<br>Possible values: `local`.|
|`adminListener.access.mode`|enum|Possible values: `authenticated`.|
|`adminListener.access.provider`|object|Settings owned and validated by the compiled-in authentication extension.<br>Relative paths inside it are resolved from the controller configuration directory.|
|`adminListener.access.publicUrl`|string|Exact external HTTPS origin, without a path other than `/`.|
|`adminListener.access.transport`|object|How HTTPS reaches the management listener.|
|`adminListener.access.transport.mode`|enum|HTTPS terminates at a trusted proxy; the plaintext listener must not be public.<br>Possible values: `proxy`.|
|`adminListener.access.transport.certificate`|string|PEM certificate chain. Relative paths are resolved from the configuration directory.|
|`adminListener.access.transport.key`|string|PEM private key. Relative paths are resolved from the configuration directory.|
|`adminListener.access.transport.mode`|enum|Possible values: `tls`.|
|`adminListener.limits`|object|Connection limits on the management listener.|
|`adminListener.limits.connectionsPerAddress`|integer|Open management connections allowed from one client address: an IPv4 address, or an IPv6 /64. At the limit, that address's connection idle the longest is closed to admit a new one. Defaults to `256`, the same as `maxConnections`, because behind a proxy or on loopback every connection has the same address.|
|`adminListener.limits.maxConnections`|integer|Open management connections allowed in total. At the limit, the connection idle the longest is closed to admit a new one. Defaults to `256`.|
|`adminListener.listen`|string|Address on which the management listener accepts connections. Defaults to `127.0.0.1:8080`. Local access requires a loopback address.|
|`allowInsecureDev`|boolean|Permit a non-HTTPS OIDC issuer for isolated local development.<br><br>This escape hatch is only appropriate for isolated local development.|
|`daemonConfig`|object|Daemon configuration distributed to enrolled devices.|
|`daemonConfig.database`|object|Store the fleet configuration in the controller database.|
|`daemonConfig.database.seedPath`|string|YAML file imported once when the revision table is empty.<br><br>Relative paths are resolved from the controller configuration directory.|
|`daemonConfig.path`|string|Path to the watched YAML configuration distributed to enrolled devices.<br><br>Relative paths are resolved from the controller configuration directory.<br>Valid file changes are published to connected devices automatically.|
|`daemonConfig.revision`|integer|Monotonically increasing revision assigned to the daemon configuration.<br><br>Applies to file mode only. Older configurations that omit it start at 1.|
|`databaseUrl`|string|SQLite or PostgreSQL URL used for controller state.|
|`enrollmentTokens`|object|Unattended device enrollment with operator-signed tokens. Disabled when unset.|
|`enrollmentTokens.audience`|string|Audience (`aud`) every enrollment token must carry.|
|`enrollmentTokens.issuers`|[]object|Token issuers the controller trusts.|
|`enrollmentTokens.issuers[].name`|string|Issuer (`iss`) of the tokens this key verifies.|
|`enrollmentTokens.issuers[].publicKeyPath`|string|Path to the PEM-encoded public key: ECDSA P-256 or P-384, Ed25519, or<br>RSA of at least 2048 bits. Tokens must be signed with an algorithm of<br>the key's type.<br><br>Relative paths are resolved from the controller configuration directory.|
|`enrollmentTokens.issuers[].revokedIds`|[]string|Token IDs (`jti`) that are no longer accepted. Devices enrolled with<br>one of them are rejected as well.|
|`fleetListener`|object|Listener for the device-facing gRPC fleet API.|
|`fleetListener.limits`|object|Limits on the fleet API that depend on the deployment.|
|`fleetListener.limits.connectionsPerAddress`|integer|Open fleet connections allowed from one client address: an IPv4<br>address, or an IPv6 /64. At the limit, that address's idle connection<br>with the oldest activity is closed to admit a new one; when none is<br>idle, the new one is closed. Raise it when more devices share an address, such as behind a<br>NAT or a load balancer that does not preserve client addresses. Defaults<br>to `4096`.|
|`fleetListener.limits.deviceMessageBurst`|integer|Messages one device may send in a burst on its `Connect` stream, after<br>which it may send one more every `deviceMessageRefill`. Once a device<br>has sent its burst, the controller reads its messages no faster than<br>that, and the device waits to send more. Defaults to `600`.|
|`fleetListener.limits.deviceMessageRefill`|string|Time for a device to get back one message. Defaults to `50ms`.|
|`fleetListener.limits.enrollmentBurst`|integer|Enrollment requests one client address may make in a burst, after which<br>it may make one more every `enrollmentRefill`. Each device sign-in makes<br>at least two: one to read the login configuration and one to enroll.<br>Defaults to `60`.|
|`fleetListener.limits.enrollmentInFlight`|integer|Enrollment requests in progress at once across all clients. Enrollment<br>calls the identity provider. Defaults to `64`.|
|`fleetListener.limits.enrollmentRefill`|string|Time for a client address to get back one enrollment request. Defaults<br>to `1s`.|
|`fleetListener.limits.maxConnections`|integer|Open fleet connections allowed in total. At the limit, the idle<br>connection with the oldest activity (its last request starting or<br>ending, or else its being accepted) is closed to admit a new one; when<br>none is idle, the new one is closed. A connection is idle unless a<br>client with a device certificate has a request or `Connect` stream open<br>on it. Defaults to `50000`.|
|`fleetListener.limits.maxMessageBytes`|integer|Largest message, in bytes, that a device and the controller may send<br>each other. Requests from clients without a device certificate, such as<br>enrollment, stay limited to 64 KiB. Devices accept at most 4 MiB, so<br>raising this only raises what devices may send. Defaults to `4194304`<br>(4 MiB).|
|`fleetListener.listen`|string|Address on which the fleet listener accepts connections. Defaults to `127.0.0.1:8443`; devices on other hosts need a non-loopback address.|
|`gatewayJwt`|object|LLM gateway JWT signing settings.|
|`gatewayJwt.issuer`|string|Issuer claim placed in generated JWTs.|
|`gatewayJwt.keyId`|string|Key identifier placed in generated JWT headers.|
|`gatewayJwt.lifetime`|string|Lifetime of generated JWTs. Defaults to `5m`.|
|`gatewayJwt.privateKey`|string|Path to the PEM-encoded RSA private signing key.<br><br>Relative paths are resolved from the controller configuration directory.|
|`gatewayListener`|object|Listener that LLM gateways use, started when `gatewayJwt` is set. It serves only `/.well-known/jwks.json`, the keys gateways use to verify controller-issued JWTs.<br><br>No other listener serves the keys, so a NetworkPolicy or firewall can allow gateways this port and block the management API, which can read the device inventory and delete devices. Gateways that export usage also need the fleet port.|
|`gatewayListener.limits`|object|Connection limits on the gateway listener.|
|`gatewayListener.limits.connectionsPerAddress`|integer|Open gateway connections allowed from one client address: an IPv4 address, or an IPv6 /64. At the limit, that address's connection idle the longest is closed to admit a new one. Raise it when many gateway replicas share an address. Defaults to `64`.|
|`gatewayListener.limits.maxConnections`|integer|Open gateway connections allowed in total. At the limit, the connection idle the longest is closed to admit a new one. Defaults to `1024`.|
|`gatewayListener.listen`|string|Address on which the gateway listener accepts connections. Defaults to `127.0.0.1:8444`.|
|`gatewayListener.tls`|boolean|Serve HTTPS with the controller certificate from `tls` instead of plain HTTP. Use HTTPS unless gateways reach the listener over loopback or a trusted network: whoever can change the keys a gateway fetches can forge credentials it accepts.|
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
