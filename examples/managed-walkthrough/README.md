# Managed native walkthrough

This walkthrough exercises ordinary OAuth user identity and authority-issued mTLS device identity through Agent Desktop and Agent Gateway. It covers browser login, administrator approval, a Claude request, and administrative device revocation. Agent Gateway uses the checked-in configuration only; it needs no source changes. All infrastructure runs in one disposable Podman pod. Agent Desktop and Claude remain host processes, but trust the generated CAs through a process-local bundle; the walkthrough never changes the host trust store or uses `sudo`.

The walkthrough uses the repository's mock OIDC and Anthropic servers. It expects:

- Fedora with Podman, `openssl`, `curl`, `jq`, Rust, and Claude Code.
- Repository root as the current directory unless a command says otherwise.

For a zero-input API-driven verification of the same enrollment, approval, mTLS forwarding, and revocation path, run:

```bash
scripts/managed-e2e.sh
```

The script incrementally rebuilds the current connector, uses cached Podman image layers, exits nonzero on failure, and removes its containers and generated identity state. It is the recommended first managed walkthrough. It proves the direct development topology on the host; it does not exercise Windows WFP, the installed machine/session split, managed transparent capture, or pre-expiry revocation enforcement. The manual steps below and `scripts/vm-managed-walkthrough.sh` remain available for the desktop user and administrator journeys.

Use these fixed local values:

```bash
export OIDC_ISSUER=https://localhost:18080/
export OIDC_AUDIENCE=agentdesktop
export OIDC_JWKS_URL=http://127.0.0.1:18080/jwks
export OIDC_CLIENT_ID=agentdesktop-test
export USER_SCOPE=agentgateway.invoke
export ADMIN_SCOPE=agentdesktop.enrollment.admin
export ORGANIZATION_ID=11111111-1111-4111-8111-111111111111
export ANTHROPIC_BASE_URL=http://127.0.0.1:18081
export ANTHROPIC_API_KEY=mock-provider-key
```

## Start disposable infrastructure

Start OIDC, mock Anthropic, PostgreSQL 17, the enrollment service, and Agent Gateway:

```bash
scripts/managed-walkthrough.sh start
```

The launcher binds every published port to `127.0.0.1`, creates fresh certificates, builds the enrollment image, and waits for every service to become ready. The mock OIDC server performs deterministic test-user and test-admin login when the browser opens. The VM walkthrough exposes its administrator experience on loopback-only `http://localhost:8091/admin/`; production deployments serve this UI over organization-trusted HTTPS.

The mock Anthropic API accepts the fake Gateway-owned API key and returns `SMOKE_OK`; it never contacts Anthropic.

## Log in and enroll

Set the process-local trust bundle, then log in and create the device enrollment:

```bash
export SSL_CERT_FILE="$PWD/examples/managed-walkthrough/certs/process-ca-bundle.crt"
export AGENTDESKTOP_IDENTITY_DIR="$PWD/examples/managed-walkthrough/certs/identity"
export AGENTDESKTOP_CREDENTIAL_STORAGE=file

cargo run -- identity login \
  --issuer "$OIDC_ISSUER" \
  --client-id "$OIDC_CLIENT_ID" \
  --audience "$OIDC_AUDIENCE" \
  --scope "$USER_SCOPE" \
  --gateway-origin https://127.0.0.1:8443

cargo run -- identity enroll-request \
  --issuer "$OIDC_ISSUER" \
  --enrollment-url https://localhost:8090 \
  --gateway-origin https://127.0.0.1:8443
```

Copy the printed enrollment ID, then approve it as the administrator:

```bash
export ADMIN_TOKEN="$(curl --fail --silent "${OIDC_ISSUER}admin-token" | jq -r .access_token)"
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
  --gateway-origin https://127.0.0.1:8443
```

## Start Agent Desktop

Start Agent Desktop with the same process-local CA bundle:

```bash
SSL_CERT_FILE="$PWD/examples/managed-walkthrough/certs/process-ca-bundle.crt" \
cargo run -- serve \
  --mode managed \
  --upstream https://127.0.0.1:8443 \
  --native-target native.agentdesktop.internal:4000 \
  --identity-issuer "$OIDC_ISSUER" \
  --enrollment-url https://localhost:8090
```

Give Claude Code separate consent to use the loopback connector, then make a request:

```bash
ANTHROPIC_BASE_URL=http://127.0.0.1:8080 \
ANTHROPIC_AUTH_TOKEN=connector-placeholder \
claude
```

## Revoke the device

Read `device_id` from the approval or enrollment-status response and revoke it:

```bash
export DEVICE_ID='<approved device ID>'
curl --fail-with-body -X POST \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  "https://localhost:8090/v1/admin/devices/$DEVICE_ID/revoke" | jq
```

The first request returns `SMOKE_OK`. Revocation prevents certificate renewal and records the certificate revocation time. The walkthrough does not publish versioned revocation state or configure Agent Gateway to consume it, so an already-issued certificate remains usable until its short lifetime expires. There is no per-request control-plane authorization callback.

Delete the pod, database, and generated runtime state when finished:

```bash
scripts/managed-walkthrough.sh stop
```