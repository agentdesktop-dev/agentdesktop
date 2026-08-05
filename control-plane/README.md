# Enrollment control plane

This Go module is the production backend boundary for managed Agent Desktop enrollment. It validates a standard OAuth bearer token, derives the user from validated `iss` and `sub` claims, validates a signed P-256 CSR, and transactionally persists a pending enrollment in PostgreSQL. A separately scoped administrator token can claim one pending enrollment and issue a short-lived client certificate with authority-controlled SPIFFE identity.

The runtime currently uses a protected local CA key through a narrow issuer interface. This is suitable for development and single-instance deployment, not the final production key boundary; production should replace it with KMS, HSM, `step-ca`, Vault PKI, or a cloud private CA adapter. The service renews valid active device certificates but does not yet recover expired certificates or expose its persisted revocation state to Agent Gateway. Device private keys are generated and retained by Agent Desktop and must never be submitted to this service or stored in PostgreSQL.

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
export ORGANIZATION_ID=3fdba0e6-8c2f-47a8-8202-78d38a32ad9f
export ORGANIZATION_NAME='Example Organization'
export CA_CERTIFICATE_PATH="$PWD/development-ca.crt"
export CA_PRIVATE_KEY_PATH="$PWD/development-ca.key"
export MTLS_TRUST_DOMAIN=devices.example.com
export SERVER_TLS_CERTIFICATE_PATH="$PWD/development-server.crt"
export SERVER_TLS_PRIVATE_KEY_PATH="$PWD/development-server.key"
go run ./cmd/enrollment-server -migrate
```

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

The listener defaults to `https://127.0.0.1:8090` and requires direct TLS 1.3 configuration. Connections may omit a client certificate for initial OAuth enrollment. When a client presents a certificate, the service verifies it against `CA_CERTIFICATE_PATH`; untrusted presented certificates fail during the TLS handshake. This optional verified certificate is the authentication boundary for renewal and does not replace OAuth user identity.

Submit a validated OAuth bearer token and PEM CSR:

```http
POST /v1/enrollments
Authorization: Bearer <access-token>
Content-Type: application/json

{"csr":"-----BEGIN CERTIFICATE REQUEST-----\n..."}
```

The service returns `202 Accepted` with a server-generated enrollment ID and the validated public-key fingerprint. CSR subject and SAN values are untrusted and will not determine certificate identity during issuance.

Approve a pending enrollment with a token carrying `ADMIN_OAUTH_SCOPE`:

```http
POST /v1/admin/enrollments/{enrollment_id}/approve
Authorization: Bearer <administrator-access-token>
```

The response contains the authority-assigned device ID and public certificate chain. The issued leaf has only client-auth extended usage and one SPIFFE URI in the form `spiffe://<trust-domain>/organization/<organization-id>/device/<device-id>`. A second approval returns `409 enrollment_not_pending`. If the CA call fails or the process exits after claiming the enrollment, the claim remains `issuing` for reconciliation; it is not reset to `pending` because CA failure can be ambiguous.

Administrators can list bounded organization-scoped enrollment metadata and reject a pending request without reading CSR or certificate bytes:

```http
GET /v1/admin/enrollments?status=pending
Authorization: Bearer <administrator-access-token>

POST /v1/admin/enrollments/{enrollment_id}/reject
Authorization: Bearer <administrator-access-token>
```

The list defaults to `pending`, accepts `pending`, `issuing`, `approved`, or `rejected`, and returns at most 100 records ordered oldest first. Rejection is an audited exact `pending` to `rejected` transition. Unknown, foreign-organization, and already-transitioned enrollment IDs return the same `409 enrollment_not_pending` response.

Approved list records include the authority-assigned device ID. An administrator can revoke that device:

```http
POST /v1/admin/devices/{device_id}/revoke
Authorization: Bearer <administrator-access-token>
```

Revocation atomically marks the active device and all its unrevoked certificates with the same revocation time and records a `device.revoked` audit event. Unknown, foreign-organization, and already-revoked device IDs return the same `409 device_not_active` response. Agent Gateway consumption of this state is not yet implemented.

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