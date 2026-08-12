# Remote managed VM example

This development reference deploys the managed server-side components on one Linux VM and connects one or more Agent Desktop laptops to them. The same Compose stack can also run on the employee development laptop with `agentdesktop.localhost`.

For a linear operator runbook, start with [Managed Server and Client Walkthrough](../../docs/deployment/managed-vm-walkthrough.md). Its [laptop-local variant](../../docs/deployment/managed-vm-walkthrough.md#laptop-local-variant) documents port conflicts, browser trust, startup, and cleanup. This file is the detailed reference for the example's configuration and security boundaries.

```text
Laptop A ─┐                    ┌─ Keycloak OAuth
Laptop B ─┼─ mTLS CONNECT ────┼─ Enrollment authority ─ PostgreSQL
Laptop C ─┘                    ├─ Agent Gateway
```

**Agent Desktop Administration** provides the operational view for enrolled clients, owners, active/revoked state, current certificate expiry, certificate generations, renewals, enrollment queues, discovered agents/resources, force-rescan requests, and desired agent Allow/Deny policy.

## Development boundary

This is a runnable development deployment, not a production security template. It deliberately uses:

- A file-backed development enrollment CA instead of an HSM or managed PKI.
- Seeded Keycloak users and passwords.
- A single VM and Docker Compose rather than high-availability services.
- A private development server CA copied manually to clients.
- Directly published nonstandard TLS ports.

Production replacements are listed at the end of this guide.

## VM prerequisites

Use a Linux VM with at least 4 CPUs, 8 GB RAM, 30 GB disk, Docker Engine with the Compose plugin, `openssl`, `curl`, and `jq`. Clone this repository onto the VM.

For Google Cloud, the [GCP Terraform development deployment](terraform/gcp/README.md) provisions the VM and networking, installs the prerequisites, and uploads the current local server files. It does not require the local branch or uncommitted changes to be pushed.

Create a DNS `A` or `AAAA` record such as `agentdesktop.example.com` pointing to the VM. Allow inbound TCP:

| Port | Purpose |
| --- | --- |
| `8444` | Keycloak OAuth issuer |
| `8090` | Enrollment authority and administrator API |
| `8443` | Agent Gateway mTLS CONNECT |

PostgreSQL and Gateway readiness remain bound to VM loopback or the private Compose network.

## Prepare the deployment

From this directory on the VM:

```bash
./prepare.sh agentdesktop.localhost
```

The script creates ignored runtime files:

- `runtime/certs/server-ca.crt`: development trust root for public server endpoints.
- `runtime/certs/enrollment-ca.crt`: device certificate authority.
- Server certificates for Keycloak, enrollment, and Agent Gateway.
- `runtime/organization.json`: public Agent Desktop bootstrap.
- `runtime/keycloak-realm.json`: server-only realm import with the public administrator callback.
- `.env`: Compose configuration and secrets.

Edit `.env` and replace every `replace-me` value:

```dotenv
PUBLIC_HOST=agentdesktop.localhost
ANTHROPIC_API_KEY=sk-ant-...
KEYCLOAK_ADMIN_PASSWORD=...
```

Start and verify the stack:

```bash
docker compose up -d --build
./verify.sh
docker compose ps
```

Inspect failures with `docker compose logs SERVICE`, where `SERVICE` is `keycloak`, `enrollment-server`, or `agentgateway`.

## Copy the public client bootstrap

Copy these two files from the VM to each laptop through a trusted channel:

```text
runtime/organization.json
runtime/certs/server-ca.crt
```

Do not copy any `.key` file. Device private keys are generated on each laptop and enrollment/CA private keys remain on the VM.

The checked-in Keycloak realm creates these development users:

| Workflow | Username | Password |
| --- | --- | --- |
| Laptop enrollment | `employee` | `employee-change-me` |
| Enrollment administration | `administrator` | `administrator-change-me` |

Change or remove these users before using the stack beyond an isolated development environment.

## Connect a laptop with the desktop UI

On a development laptop with this repository, Node.js 20+, Rust, and Tauri prerequisites installed, use absolute paths to the two copied files (run from repo root):

```bash
export SSL_CERT_FILE=$PWD/runtime/certs/server-ca.crt
export AGENTDESKTOP_IDENTITY_DIR="$HOME/.config/agentdesktop-vm-example/identity"
export AGENTDESKTOP_CREDENTIAL_STORAGE=file
export AGENTDESKTOP_ORGANIZATION_CONFIG=$PWD/runtime/organization.json

npm --prefix ui install
npm --prefix ui run dev:desktop
```

In Agent Desktop:

1. Select **Sign in**.
2. Sign in to Keycloak as `employee`.
3. Agent Desktop creates a device key and submits a pending enrollment.
4. Leave the application open; it polls for administrator approval.

The connector does not start managed forwarding until the approved certificate has been retrieved and validated.

## Approve the laptop

On an administrator workstation, open `https://agentdesktop.localhost:8090/admin/` in a browser that trusts the organization server CA.

Sign in as `administrator`, open **Enrollments**, and approve the pending `employee` request. The laptop retrieves its certificate, applies the supported Claude Code route automatically, and starts certificate-authenticated forwarding after the Gateway step becomes ready.

Repeat the laptop steps with separate identity directories to simulate multiple clients. Every approval creates a distinct authority-assigned device ID.

## CLI-only client path

The desktop UI is not required. With the same environment variables, run from the repository root:

```bash
cargo run -- identity storage-check \
    --credential-storage file \
    --storage-dir "$AGENTDESKTOP_IDENTITY_DIR"

cargo run -- identity login \
    --issuer https://agentdesktop.example.com:8444/realms/agentdesktop \
    --client-id agentdesktop \
    --audience agentdesktop \
    --scope agentgateway.invoke \
    --gateway-origin https://agentdesktop.example.com:8443

cargo run -- identity enroll-request \
    --issuer https://agentdesktop.example.com:8444/realms/agentdesktop \
    --enrollment-url https://agentdesktop.example.com:8090 \
    --gateway-origin https://agentdesktop.example.com:8443
```

After administrator approval:

```bash
cargo run -- identity enroll-status \
    --issuer https://agentdesktop.example.com:8444/realms/agentdesktop \
    --enrollment-url https://agentdesktop.example.com:8090 \
    --gateway-origin https://agentdesktop.example.com:8443

cargo run -- serve \
    --mode managed \
    --upstream https://agentdesktop.example.com:8443 \
    --native-target native.agentdesktop.internal:4000 \
    --identity-issuer https://agentdesktop.example.com:8444/realms/agentdesktop \
    --enrollment-url https://agentdesktop.example.com:8090 \
    --identity-dir "$AGENTDESKTOP_IDENTITY_DIR"
```

In another terminal, run `cargo run -- connect-agents` to configure Claude Code.

## See client statistics

The server-hosted administrator console reads these administrator-scoped APIs:

```text
GET /v1/admin/summary
GET /v1/admin/devices
```

**Overview** reports:

- Pending enrollment requests.
- Active and revoked devices.
- Current device certificates expiring within 24 hours.
- Successful certificate renewals in the last 24 hours.

**Devices** reports each authority-approved client separately:

- Client-reported device name, authority-assigned device ID, and OAuth subject.
- Active or revoked state.
- Enrollment time and revocation time.
- Current certificate serial and expiry.
- Number of certificate generations and successful renewals.

## Stop or reset

For a laptop-local deployment, the complete ownership-safe reset is:

```bash
./reset-local.sh
```

It stops a client launched by `start-client.sh`, disconnects only Agent Desktop-owned Claude settings, removes the exact generated CA from the macOS login keychain, deletes local identity and `.env`/runtime secrets, and runs the Compose volume reset below. It preserves unrelated user settings and credentials.

Stop services without deleting data:

```bash
docker compose down
```

Delete PostgreSQL and Keycloak state:

```bash
docker compose down --volumes
```

Delete generated certificates and bootstrap only when no enrolled client needs them:

```bash
rm -rf runtime .env
```

## Production replacements

Before production use:

- Use public or organization-managed server certificates and standard ports behind appropriate TCP routing.
- Replace seeded Keycloak users with the organization IdP. The example requires both `agentdesktop.enrollment.admin` and the `agentdesktop-administrator` realm role; map `ADMIN_OAUTH_ROLE` to the equivalent tightly controlled organization role or group.
- Use PKCS#11/HSM or managed PKI instead of `CA_SIGNER_BACKEND=file`.
- Use a managed PostgreSQL service, unique credentials, encrypted backups, and restricted networking.
- Sign Agent Desktop and administrator application artifacts and distribute the bootstrap through MDM.
- Add high availability and alerting for OAuth, enrollment, Gateway, and PostgreSQL.
- Complete and verify Gateway revocation consumption before claiming immediate rejection of existing certificates.
