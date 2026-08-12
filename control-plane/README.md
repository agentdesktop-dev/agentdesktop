# Enrollment control plane

This Go module is the production backend boundary for managed Agent Desktop enrollment. It validates a standard OAuth bearer token, derives the user from validated `iss` and `sub` claims, validates a signed P-256 CSR, and transactionally persists a pending enrollment in PostgreSQL. A separately scoped administrator token can claim one pending enrollment and issue a short-lived client certificate with authority-controlled SPIFFE identity.

The service supports a development file signer and a production PKCS#11 signer. The PKCS#11 path keeps the enrollment CA private key inside an HSM and uses its P-256 `crypto.Signer` for authority-controlled certificate issuance. The service renews valid active device certificates and recovers the latest certificate for seven days after expiry using OAuth plus enrolled-key proof of possession. It persists device and certificate revocation state, but no versioned publication endpoint or Agent Gateway consumer exists yet. Device private keys are generated and retained by Agent Desktop and must never be submitted to this service or stored in PostgreSQL.

## Local development

Start PostgreSQL:

```bash
podman compose -f control-plane/compose.yaml up -d postgres
```

Run the service from `control-plane/`:

```bash
export DATABASE_URL=postgres://agentdesktop:agentdesktop-development@127.0.0.1:5432/agentdesktop?sslmode=disable
export OAUTH_ISSUER=https://issuer.example/
export OAUTH_AUDIENCE=agentdesktop
export OAUTH_SCOPE=agentgateway.invoke
export ADMIN_OAUTH_SCOPE=agentdesktop.enrollment.admin
export ADMIN_OAUTH_ROLE=agentdesktop-administrator
export ORGANIZATION_ID=3fdba0e6-8c2f-47a8-8202-78d38a32ad9f
export ORGANIZATION_NAME='Example Organization'
export CA_CERTIFICATE_PATH="$PWD/development-ca.crt"
export CA_SIGNER_BACKEND=file
export CA_PRIVATE_KEY_PATH="$PWD/development-ca.key"
export MTLS_TRUST_DOMAIN=devices.example.com
export SERVER_TLS_CERTIFICATE_PATH="$PWD/development-server.crt"
export SERVER_TLS_PRIVATE_KEY_PATH="$PWD/development-server.key"
go run ./cmd/enrollment-server -migrate
```

`ADMIN_OAUTH_ROLE` is optional for compatibility, but production deployments should set it. Administrator requests must then carry both `ADMIN_OAUTH_SCOPE` and that realm role in `realm_access.roles`.

`ISSUANCE_RECONCILIATION_INTERVAL` defaults to `1m`, and `ISSUANCE_RECONCILIATION_GRACE` defaults to `5m`. The worker retries claims that remain `issuing` beyond the grace period using the original enrollment ID, device ID, CSR, and claim time. An external CA adapter must use the enrollment ID as its idempotency key so a timeout or process crash cannot create a second credential.

For local development only, generate a P-256 CA before startup:

```bash
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
	-keyout development-ca.key -out development-ca.crt -days 30 \
	-subj '/CN=Agent Desktop Development Enrollment CA' \
	-addext 'basicConstraints=critical,CA:TRUE' \
	-addext 'keyUsage=critical,keyCertSign,cRLSign'
chmod 0600 development-ca.key

openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
	-keyout development-server.key -out development-server.csr \
	-subj '/CN=localhost' \
	-addext 'subjectAltName=DNS:localhost,IP:127.0.0.1'
openssl x509 -req -in development-server.csr \
	-CA development-ca.crt -CAkey development-ca.key -CAcreateserial \
	-out development-server.crt -days 7 -copy_extensions copy
chmod 0600 development-server.key
rm development-server.csr
```

## Production CA signer

Set `CA_SIGNER_BACKEND=pkcs11`, `CA_PKCS11_CONFIG_PATH`, and `CA_PKCS11_KEY_ID`. The key ID is the non-empty hexadecimal CKA_ID of an existing P-256 CA key pair. The JSON configuration selects exactly one token and contains its user PIN, so it must be a regular file with no group or world permissions:

```json
{
	"Path": "/opt/vendor/lib/libpkcs11.so",
	"TokenLabel": "agentdesktop-enrollment",
	"Pin": "<token-user-pin>",
	"MaxSessions": 8
}
```

The enrollment CA certificate at `CA_CERTIFICATE_PATH` must match that token key. The service fails startup if configuration permissions are broad, the CKA_ID is malformed or absent, the token cannot be opened, or the certificate and token public keys differ. The container is CGO-enabled and must receive the vendor PKCS#11 module and any required runtime libraries through the deployment image or a read-only mount.

The opt-in SoftHSM test exercises the same loader and issuance path:

```bash
TEST_PKCS11_CONFIG_PATH=/path/to/pkcs11.json \
TEST_PKCS11_KEY_ID=01 \
go test -run TestPKCS11SignerIssuesAuthorityControlledCertificate -v ./internal/ca
```

The listener defaults to `https://127.0.0.1:8090` and requires direct TLS 1.3 configuration. Connections may omit a client certificate for initial OAuth enrollment. When a client presents a certificate, the service verifies it against `CA_CERTIFICATE_PATH`; untrusted presented certificates fail during the TLS handshake. This optional verified certificate is the authentication boundary for renewal and does not replace OAuth user identity.

Submit a validated OAuth bearer token and PEM CSR:

```http
POST /v1/enrollments
Authorization: Bearer <access-token>
Content-Type: application/json

{"csr":"-----BEGIN CERTIFICATE REQUEST-----\n...","device_name":"workstation-7"}
```

The service returns `202 Accepted` with a server-generated enrollment ID and the validated public-key fingerprint. `device_name` is optional, client-reported display metadata limited to 128 characters; it is never treated as device identity. CSR subject and SAN values are likewise untrusted and will not determine certificate identity during issuance.

Approve a pending enrollment with a token carrying `ADMIN_OAUTH_SCOPE`:

```http
POST /v1/admin/enrollments/{enrollment_id}/approve
Authorization: Bearer <administrator-access-token>
```

The response contains the authority-assigned device ID and public certificate chain. The issued leaf has only client-auth extended usage and one Agent Gateway-compatible SPIFFE URI in the form `spiffe://<trust-domain>/ns/<organization-id>/sa/user.<user-id>.device.<device-id>`. A second approval returns `409 enrollment_not_pending`. If the CA call fails or the process exits after claiming the enrollment, the claim remains `issuing` for reconciliation; it is not reset to `pending` because CA failure can be ambiguous.

Administrators can list bounded organization-scoped enrollment metadata and reject a pending request without reading CSR or certificate bytes:

```http
GET /v1/admin/enrollments?status=pending
Authorization: Bearer <administrator-access-token>

POST /v1/admin/enrollments/{enrollment_id}/reject
Authorization: Bearer <administrator-access-token>
```

The enrollment list defaults to `pending`, accepts `pending`, `issuing`, `approved`, or `rejected`, and returns at most 100 records ordered oldest first. Rejection is an audited exact `pending` to `rejected` transition. Unknown, foreign-organization, and already-transitioned enrollment IDs return the same `409 enrollment_not_pending` response.

Fleet inventory is aggregated over each device's latest discovery report and remains server-paged for large organizations:

```http
GET /v1/admin/inventory?kind=agent&q=claude&limit=25&offset=0
GET /v1/admin/inventory/devices?kind=agent&key=claude-code&version=2.1.4&limit=50&offset=0
Authorization: Bearer <administrator-access-token>
```

Supported inventory kinds are `agent`, `mcp`, `skill`, and `plugin`. Asset and device pages are limited to 100 records. Device search covers device name, owner display name, immutable subject, and authority-assigned device ID.

Administrators can save one desired Allow/Deny policy for the five supported agents:

```http
GET /v1/admin/agent-policy

PUT /v1/admin/agent-policy
Authorization: Bearer <administrator-access-token>
Content-Type: application/json

{"schema_version":1,"rules":[{"agent_id":"claude-code","action":"allow"},{"agent_id":"claude-desktop","action":"allow"},{"agent_id":"codex-cli","action":"deny"},{"agent_id":"openclaw","action":"deny"},{"agent_id":"vscode-copilot","action":"allow"}]}
```

The policy is organization-scoped and audited. It records desired state only and returns `enforcement: "not_available"`; current clients cannot block arbitrary agent execution. See [Organization agent policy](../docs/architecture/organization-agent-policy-v1.md).

Administrators can force a discovery refresh for every active device or a selected set:

```http
POST /v1/admin/discovery-rescans
Authorization: Bearer <administrator-access-token>
Content-Type: application/json

{"target_mode":"all_active","device_ids":[]}
```

Managed desktops poll `GET /v1/device-reports/current/rescan` every 30 seconds using their current device certificate. A newer report satisfies the request.

Approved list records include the authority-assigned device ID. An administrator can revoke that device:

```http
POST /v1/admin/devices/{device_id}/revoke
Authorization: Bearer <administrator-access-token>
```

Revocation atomically marks the active device and all its unrevoked certificates with the same revocation time and records a `device.revoked` audit event. Unknown, foreign-organization, and already-revoked device IDs return the same `409 device_not_active` response. Revocation immediately blocks renewal. Existing certificates remain valid until expiry because authenticated versioned publication and fail-closed Agent Gateway consumption are not implemented yet. The publication format, freshness contract, and recovery behavior remain explicit design work; do not infer a CRL endpoint from the persisted schema.

Administrators can read organization-scoped fleet statistics and authoritative device lifecycle records:

```http
GET /v1/admin/summary
Authorization: Bearer <administrator-access-token>

GET /v1/admin/devices
Authorization: Bearer <administrator-access-token>
```

The summary reports enrollment counts, active and revoked device counts, current certificates expiring within 24 hours, and successful renewals in the previous 24 hours. The device list returns at most 100 records with client-reported display name, owner subject, authority-assigned device ID, persisted status, enrollment/revocation time, current certificate serial and expiry, certificate generation count, and successful renewal count. Both endpoints are scoped by the authenticated administrator's issuer.

These APIs contain enrollment and certificate lifecycle statistics only. AI requests do not pass through the enrollment authority; request, token, model, cost, and policy statistics remain Agent Gateway telemetry responsibilities.

Agent Gateway independently validates the issued client certificate and derives its bound organizational user and device identity. Managed forwarding carries no OAuth JWT and does not call the enrollment service per request.

An authenticated owner can renew an active device certificate by presenting its current valid certificate and a fresh P-256 CSR:

```http
POST /v1/renewals
Authorization: Bearer <access-token>
Content-Type: application/json

{"csr":"-----BEGIN CERTIFICATE REQUEST-----\n..."}
```

The service requires both OAuth ownership and a TLS-verified authority-controlled device identity. It persists an `issuing` claim before calling the CA, uses the renewal ID as the CA idempotency key, and reconciles interrupted issuance with the original device, CSR, and issuance time. Repeating the same device and public-key fingerprint returns the same claim or completed certificate. Revoked devices, expired or revoked presented certificates, foreign users, and foreign organizations receive `403 device_not_active`.

The authenticated user that created the enrollment can poll it and retrieve the public certificate chain after approval:

```http
GET /v1/enrollments/{enrollment_id}
Authorization: Bearer <user-access-token>
```

Pending responses omit `device_id` and `certificate`. Approved responses include both. Unknown enrollment IDs and enrollments owned by another `(iss, sub)` return the same `404 enrollment_not_found` response. The certificate is usable only with the private key retained by Agent Desktop when it created the CSR.

Run unit tests:

```bash
go test ./...
go vet ./...
```

Run the PostgreSQL integration test against a disposable or dedicated test database:

```bash
TEST_DATABASE_URL="$DATABASE_URL" go test ./internal/store/postgres -run TestCreatePending
```