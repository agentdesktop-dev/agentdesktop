# Enrollment control plane

This Go module is the production backend boundary for managed Agent Desktop enrollment. The first implemented increment validates a standard OAuth bearer token, derives the user from validated `iss` and `sub` claims, validates a signed P-256 CSR, and transactionally persists a pending enrollment in PostgreSQL.

It does not yet approve enrollments, issue certificates, renew certificates, recover expired certificates, or expose revocation state to Agent Gateway. Private keys are generated and retained by Agent Desktop; they must never be submitted to this service or stored in PostgreSQL.

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
export ORGANIZATION_ID=3fdba0e6-8c2f-47a8-8202-78d38a32ad9f
export ORGANIZATION_NAME='Example Organization'
go run ./cmd/enrollment-server -migrate
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

Run unit tests:

```bash
go test ./...
go vet ./...
```

Run the PostgreSQL integration test against a disposable or dedicated test database:

```bash
TEST_DATABASE_URL="$DATABASE_URL" go test ./internal/store/postgres -run TestCreatePending
```