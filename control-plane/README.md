# Enrollment control plane

This Go module is the production backend boundary for managed Agent Desktop enrollment. It validates a standard OAuth bearer token, derives the user from validated `iss` and `sub` claims, validates a signed P-256 CSR, and transactionally persists a pending enrollment in PostgreSQL. A separately scoped administrator token can claim one pending enrollment and issue a short-lived client certificate with authority-controlled SPIFFE identity.

The runtime currently uses a protected local CA key through a narrow issuer interface. This is suitable for development and single-instance deployment, not the final production key boundary; production should replace it with KMS, HSM, `step-ca`, Vault PKI, or a cloud private CA adapter. The service does not yet deliver approved certificates to Agent Desktop, renew certificates, recover expired certificates, reconcile interrupted issuance, or expose revocation state to Agent Gateway. Device private keys are generated and retained by Agent Desktop and must never be submitted to this service or stored in PostgreSQL.

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
go run ./cmd/enrollment-server -migrate
```

For local development only, generate a P-256 CA before startup:

```bash
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
	-keyout development-ca.key -out development-ca.crt -days 30 \
	-subj '/CN=Agent Desktop Development Enrollment CA' \
	-addext 'basicConstraints=critical,CA:TRUE' \
	-addext 'keyUsage=critical,keyCertSign,cRLSign'
chmod 0600 development-ca.key
```

The listener defaults to `127.0.0.1:8090`. Production deployment must place it behind authenticated TLS ingress or add direct TLS configuration before exposing it beyond loopback.

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

The response contains the authority-assigned device ID and public certificate chain. The issued leaf has only client-auth extended usage and one SPIFFE URI in the form `spiffe://<trust-domain>/organization/<organization-id>/device/<device-id>`. A second approval returns `409 enrollment_not_pending`.

Run unit tests:

```bash
go test ./...
go vet ./...
```

Run the PostgreSQL integration test against a disposable or dedicated test database:

```bash
TEST_DATABASE_URL="$DATABASE_URL" go test ./internal/store/postgres -run TestCreatePending
```