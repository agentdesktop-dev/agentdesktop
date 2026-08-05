# Managed native walkthrough

This walkthrough exercises ordinary OAuth user identity and authority-issued mTLS device identity through Agent Desktop and Agent Gateway. It covers browser login, administrator approval, a Claude request, immediate device revocation, and denial of the next request. Agent Gateway uses the checked-in configuration only; it needs no source changes.

The walkthrough expects:

- Fedora with `openssl`, `curl`, `jq`, Rust, Go, PostgreSQL 17, Claude Code, and Agent Gateway built at `../agentgateway/target/debug/agentgateway`.
- An OIDC provider with Authorization Code plus PKCE, an ES256-signed JWT access token, rotating refresh tokens, and separate user and administrator scopes.
- An Anthropic provider credential in `ANTHROPIC_API_KEY`.
- Repository root as the current directory unless a command says otherwise.

Use one issuer, audience, and user scope consistently:

```bash
export OIDC_ISSUER=https://identity.example/
export OIDC_AUDIENCE=https://gateway.agentdesktop.test
export OIDC_JWKS_URL=https://identity.example/.well-known/jwks.json
export OIDC_CLIENT_ID=agentdesktop
export USER_SCOPE=agentgateway.invoke
export ADMIN_SCOPE=agentdesktop.enrollment.admin
export ORGANIZATION_ID=11111111-1111-4111-8111-111111111111
export ADMIN_TOKEN='<administrator access token>'
```

## Prepare trust

Generate disposable development certificates:

```bash
examples/managed-walkthrough/generate-certificates.sh
```

Trust the two server CAs on the walkthrough machine. This affects the system trust store and requires administrator approval:

```bash
sudo cp examples/managed-walkthrough/certs/enrollment-ca.crt \
  /etc/pki/ca-trust/source/anchors/agentdesktop-walkthrough-enrollment-ca.crt
sudo cp examples/managed-walkthrough/certs/gateway-server-ca.crt \
  /etc/pki/ca-trust/source/anchors/agentdesktop-walkthrough-gateway-ca.crt
sudo update-ca-trust
```

## Start the enrollment service

Create an empty PostgreSQL database, then start the service in terminal 1:

```bash
cd control-plane
export DATABASE_URL=postgres://agentdesktop:agentdesktop@127.0.0.1:5432/agentdesktop
export OAUTH_ISSUER="$OIDC_ISSUER"
export OAUTH_AUDIENCE="$OIDC_AUDIENCE"
export OAUTH_SCOPE="$USER_SCOPE"
export ADMIN_OAUTH_SCOPE="$ADMIN_SCOPE"
export ORGANIZATION_ID
export ORGANIZATION_NAME='Walkthrough Organization'
export CA_SIGNER_BACKEND=file
export CA_CERTIFICATE_PATH="$PWD/../examples/managed-walkthrough/certs/enrollment-ca.crt"
export CA_PRIVATE_KEY_PATH="$PWD/../examples/managed-walkthrough/certs/enrollment-ca.key"
export SERVER_TLS_CERTIFICATE_PATH="$PWD/../examples/managed-walkthrough/certs/enrollment-server.crt"
export SERVER_TLS_PRIVATE_KEY_PATH="$PWD/../examples/managed-walkthrough/certs/enrollment-server.key"
export MTLS_TRUST_DOMAIN=agentdesktop.test
export GATEWAY_CLIENT_SPIFFE_ID=spiffe://agentdesktop.test/service/agentgateway
go run ./cmd/enrollment-server -migrate
```

## Log in and enroll

In terminal 2, log in and create the device enrollment:

```bash
cargo run -- identity login \
  --issuer "$OIDC_ISSUER" \
  --client-id "$OIDC_CLIENT_ID" \
  --audience "$OIDC_AUDIENCE" \
  --scope "$USER_SCOPE" \
  --gateway-origin https://localhost:8443

cargo run -- identity enroll-request \
  --issuer "$OIDC_ISSUER" \
  --enrollment-url https://localhost:8090 \
  --gateway-origin https://localhost:8443
```

Copy the printed enrollment ID, then approve it as the administrator:

```bash
export ENROLLMENT_ID='<printed enrollment ID>'
curl --fail-with-body -X POST \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  "https://localhost:8090/v1/admin/enrollments/$ENROLLMENT_ID/approve" | jq
```

Retrieve and persist the approved certificate:

```bash
cargo run -- identity enroll-status \
  --issuer "$OIDC_ISSUER" \
  --enrollment-url https://localhost:8090 \
  --gateway-origin https://localhost:8443
```

## Start the data path

Start Agent Gateway from its configuration directory in terminal 3 so relative certificate paths resolve correctly:

```bash
cd examples/managed-walkthrough
export OIDC_ISSUER OIDC_AUDIENCE OIDC_JWKS_URL ANTHROPIC_API_KEY
../../../agentgateway/target/debug/agentgateway -f agentgateway.yaml
```

Start Agent Desktop in terminal 4:

```bash
cargo run -- serve \
  --mode managed \
  --upstream https://localhost:8443 \
  --identity-issuer "$OIDC_ISSUER" \
  --enrollment-url https://localhost:8090
```

Give Claude Code separate consent to use the loopback connector, then make a request:

```bash
ANTHROPIC_BASE_URL=http://127.0.0.1:8080 \
ANTHROPIC_AUTH_TOKEN=connector-placeholder \
claude
```

## Revoke and verify denial

Read `device_id` from the approval or enrollment-status response and revoke it:

```bash
export DEVICE_ID='<approved device ID>'
curl --fail-with-body -X POST \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  "https://localhost:8090/v1/admin/devices/$DEVICE_ID/revoke" | jq
```

The next Claude request must fail with authorization denied. Agent Gateway does not cache device authorization, so revocation takes effect on the next request without restarting either process.

Remove only the walkthrough trust anchors when finished:

```bash
sudo rm -f \
  /etc/pki/ca-trust/source/anchors/agentdesktop-walkthrough-enrollment-ca.crt \
  /etc/pki/ca-trust/source/anchors/agentdesktop-walkthrough-gateway-ca.crt
sudo update-ca-trust
```