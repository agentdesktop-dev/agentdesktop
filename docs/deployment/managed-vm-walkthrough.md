# Managed Server and Client Walkthrough

This walkthrough deploys the managed Agent Desktop development stack on one Linux VM, enrolls a laptop, approves it from the administrator application, and verifies managed routing and inventory.

An enrollment server alone is not enough. A managed client needs three remote trust boundaries:

```text
Client browser  -> OAuth issuer        :8444
Agent Desktop   -> Enrollment server   :8090
Agent Desktop   -> Agent Gateway       :8443
Administrator   -> Enrollment server   :8090
```

The reference stack also runs PostgreSQL on the VM.

> This is a development deployment. It uses generated private CAs, seeded users, a file-backed enrollment CA, nonstandard public ports, and one VM. Do not expose it as a production service without completing the production replacements at the end.

## Laptop-local variant

The same server stack can run through Docker on a macOS or Linux development laptop. This keeps OAuth, enrollment, Gateway, and PostgreSQL in separate processes, but the server and employee client share one physical device. It is useful for development, not representative of a production trust or availability boundary.

The stack cannot run beside `scripts/managed-walkthrough.sh` because both publish `8090`, `8443`, and `15021`. Stop that disposable fixture and any existing `npm run dev:desktop` process first. Stopping the fixture deletes its disposable certificates and enrollment state.

From the repository root:

```bash
scripts/managed-walkthrough.sh stop
cd examples/managed-vm
./prepare.sh agentdesktop.localhost
```

Use `agentdesktop.localhost` exactly. Do not substitute `agentdesktop.local` (`.local` is reserved for multicast DNS on macOS) or plain `localhost`. OAuth redirect URIs are exact: the administrator page and Keycloak callback must both use `https://agentdesktop.localhost:8090/admin/`.

Edit `.env` and replace the password values. Set a real `ANTHROPIC_API_KEY` for successful Claude responses; a nonempty development placeholder is enough to start and verify the infrastructure, but provider requests will fail.

Before browser sign-in, trust `runtime/certs/server-ca.crt` in the browser or operating-system trust store. On macOS, import it into the login keychain with Keychain Access and explicitly trust it. This browser trust is separate from the process-local `SSL_CERT_FILE` used by Agent Desktop. Remove the development CA when the walkthrough is no longer needed.

Start and verify the stack:

```bash
docker compose config --quiet
docker compose up -d --build
./verify.sh
```

Skip the VM bootstrap-copy step. Install UI dependencies once, then use the local client launcher. It scopes the bootstrap, identity, CA bundle, proxy bypass, and reset PID marker to the desktop process instead of exporting them in your shell:

```bash
npm --prefix ui install
examples/managed-vm/start-client.sh
```

Sign in as `employee` / `employee-change-me`. Open `https://agentdesktop.localhost:8090/admin/` exactly, sign in as `administrator` / `administrator-change-me`, and approve the pending device. Opening the administrator application through `https://localhost:8090/` creates a different OAuth redirect URI and is rejected.

To stop the local stack while preserving its data, run `docker compose down` from `examples/managed-vm`. To remove all laptop-local example state, run from the repository root:

```bash
examples/managed-vm/reset-local.sh
```

The reset asks for confirmation, stops a desktop launched by `start-client.sh`, and removes only the example's Compose containers/volumes/image, file-backed identity, `.env`, generated runtime, exact macOS login-keychain CA fingerprint, and Agent Desktop-owned Claude routing values. It preserves unrelated Claude settings, self-managed local provider credentials, and shared Docker images. Use `--yes` only for an intentional noninteractive reset.

## 1. Prepare the VM

For a GCP-hosted development deployment, the [GCP Terraform helper](../../examples/managed-vm/terraform/gcp/README.md) automates this section, installs the VM prerequisites, and copies the current local server source without requiring a pushed branch. Continue at step 4 after it returns the public client bootstrap.

Use a Linux VM with:

- 4 CPUs, 8 GB RAM, and 30 GB disk or more.
- Docker Engine with the Compose plugin.
- Git, OpenSSL, curl, and jq.
- A public DNS name pointing to the VM, such as `agentdesktop.example.com`.

Allow inbound TCP on these ports:

| Port | Service |
| --- | --- |
| `8444` | Keycloak OAuth issuer |
| `8090` | Enrollment server and administrator API |
| `8443` | Agent Gateway mTLS listener |
| `22` | SSH administration |

PostgreSQL and Gateway readiness remain bound to VM loopback or the private Compose network.

Clone the repository on the VM:

```bash
git clone https://github.com/agentdesktop-dev/agentdesktop.git
cd agentdesktop/examples/managed-vm
```

Generate the development CAs, server certificates, and client bootstrap:

```bash
./prepare.sh agentdesktop.example.com
```

> Run `prepare.sh` only for initial setup or a deliberate full reset. It replaces the generated enrollment CA. Existing enrolled clients will no longer trust newly generated credentials.

Edit `.env` and replace every `replace-me` value:

```dotenv
PUBLIC_HOST=agentdesktop.example.com
ANTHROPIC_API_KEY=sk-ant-...
KEYCLOAK_ADMIN_PASSWORD=choose-a-long-random-value
```

Confirm that `PUBLIC_HOST` exactly matches the hostname passed to `prepare.sh`.

## 2. Start the server stack

Validate and start all services:

```bash
docker compose config --quiet
docker compose up -d --build
./verify.sh
docker compose ps
```

The verification command should print:

```text
Managed VM stack is healthy.
```

The VM now runs:

- Keycloak for employee and administrator OAuth.
- The Go enrollment server and administrator APIs.
- PostgreSQL for organizations, enrollments, devices, certificates, and audit events.
- Agent Gateway for certificate-authenticated AI traffic.

Useful diagnostics:

```bash
docker compose logs --tail=100 keycloak
docker compose logs --tail=100 enrollment-server
docker compose logs --tail=100 agentgateway
```

## 3. Copy the public bootstrap to a laptop

On the VM, only these two files are intended for clients:

```text
examples/managed-vm/runtime/organization.json
examples/managed-vm/runtime/certs/server-ca.crt
```

Never copy a `.key` file from the VM.

On the laptop:

```bash
install -d -m 0700 "$HOME/.config/agentdesktop-vm-example"

scp VM_USER@agentdesktop.example.com:~/agentdesktop/examples/managed-vm/runtime/organization.json \
  "$HOME/.config/agentdesktop-vm-example/organization.json"

scp VM_USER@agentdesktop.example.com:~/agentdesktop/examples/managed-vm/runtime/certs/server-ca.crt \
  "$HOME/.config/agentdesktop-vm-example/server-ca.crt"

chmod 0600 "$HOME/.config/agentdesktop-vm-example/organization.json"
chmod 0644 "$HOME/.config/agentdesktop-vm-example/server-ca.crt"
```

Adjust `VM_USER` and the remote repository path if needed.

## 4. Start Agent Desktop on the laptop

Clone this repository on the laptop and enter it:

```bash
git clone https://github.com/agentdesktop-dev/agentdesktop.git
cd agentdesktop
```

Set the managed development environment:

```bash
export SSL_CERT_FILE="$HOME/.config/agentdesktop-vm-example/server-ca.crt"
export AGENTDESKTOP_ORGANIZATION_CONFIG="$HOME/.config/agentdesktop-vm-example/organization.json"
export AGENTDESKTOP_IDENTITY_DIR="$HOME/.config/agentdesktop-vm-example/identity"
export AGENTDESKTOP_CREDENTIAL_STORAGE=file
```

Start the desktop application:

```bash
npm --prefix ui install
npm --prefix ui run dev:desktop
```

Keep this terminal running. In Agent Desktop:

1. Select **Sign in**.
2. Sign in to Keycloak with the seeded development employee:
   - Username: `employee`
   - Password: `employee-change-me`
3. Agent Desktop generates a P-256 device key locally.
4. Agent Desktop submits a CSR to the enrollment server.
5. The UI displays a pending device enrollment and waits for approval.

The private device key remains on the laptop. The connector does not start managed forwarding until it retrieves and validates the approved certificate.

## 5. Approve the laptop

On an administrator workstation, trust the same organization server CA and open `https://agentdesktop.example.com:8090/admin/`.

Select **Sign in** and use the seeded administrator:

- Username: `administrator`
- Password: `administrator-change-me`

The server requires both the `agentdesktop.enrollment.admin` OAuth scope and the `agentdesktop-administrator` realm role. The seeded employee account cannot administer enrollments.

Open **Enrollments**, review the pending employee request, and select **Approve**. Approval assigns an authority-controlled device ID and issues a short-lived client certificate.

## 6. Verify the client connection

Return to Agent Desktop on the laptop. It polls the enrollment server and should advance through these states:

```text
Organization account: signed in
Device access: approved
Gateway: reachable
Provider access: managed by your organization
```

The local status endpoint should report a ready managed identity:

```bash
curl --fail --silent http://127.0.0.1:8081/_agentdesktop/status | jq
```

Expected fields include:

```json
{
  "mode": "managed",
  "gateway": "reachable",
  "identity": "ready"
}
```

After device approval, wait for Agent Desktop to show that organization routing was applied automatically, then make one Claude Code request. Provider credentials remain on Agent Gateway; the laptop uses only its connector placeholder credential.

If the Anthropic key in the VM `.env` is invalid, the request will fail upstream but still verifies the client-to-Gateway route.

## 7. View client inventory and policy

In Agent Desktop Administration, **Overview** shows:

- Pending enrollment requests.
- Active and revoked devices.
- Certificates expiring within 24 hours.
- Successful certificate renewals in the previous 24 hours.

**Devices** shows each approved laptop separately:

- OAuth subject and authority-assigned device ID.
- Active or revoked state.
- Enrollment and revocation time.
- Current certificate expiry.
- Certificate generations and successful renewals.

The administration navigation also exposes the intended management model:

- **Inventory** shows enrolled endpoints from the authority. Managed macOS reports five known agent families with name, icon, static version, runtime state, configured MCP server names/transports, skills, and plugins. Managed Windows currently reports Claude Code installation/version, fixed user and managed configuration, MCP servers, and skills with runtime state marked unknown. Administrators can request a rescan of all active devices or one expanded device.
- **Policies** stores one desired organization policy with Allow/Deny choices for Claude Code, Claude Desktop, Codex CLI, OpenClaw, and VS Code Copilot. Endpoint enforcement is not implemented yet.

These remaining unavailable states are expected for the current milestone; do not treat them as a deployment failure.

## 8. Add another client

Repeat steps 3 through 6 on another laptop. Each approved enrollment receives a distinct device ID.

To simulate another client on the same development machine, stop the first desktop session and use a different identity directory:

```bash
export AGENTDESKTOP_IDENTITY_DIR="$HOME/.config/agentdesktop-vm-example/identity-client-2"
npm --prefix ui run dev:desktop
```

Do not run two development clients on the same default loopback ports simultaneously.

## 9. Revoke a client

In Agent Desktop Administration:

1. Open **Devices**.
2. Find the device by employee subject or device ID.
3. Select **Revoke access** and confirm.

Revocation immediately blocks certificate renewal and is persisted for fleet statistics. Current revocation publication and Agent Gateway consumption are still incomplete, so an already-issued certificate may remain usable until its natural expiry. Do not claim immediate traffic rejection until that milestone is complete.

## 10. CLI-only alternative

The graphical client is optional. The exact CLI login, enrollment, certificate retrieval, and connector commands are documented in [Remote managed VM example](../../examples/managed-vm/README.md#cli-only-client-path).

## 11. Troubleshooting

### The server stack is unhealthy

```bash
cd agentdesktop/examples/managed-vm
./verify.sh
docker compose ps
docker compose logs --tail=200 SERVICE
```

### OAuth login cannot open or complete

Check public discovery from the laptop:

```bash
curl --fail --cacert "$SSL_CERT_FILE" \
  https://agentdesktop.example.com:8444/realms/agentdesktop/.well-known/oauth-authorization-server | jq
```

The issuer must exactly equal:

```text
https://agentdesktop.example.com:8444/realms/agentdesktop
```

### TLS reports an unknown issuer

Confirm `SSL_CERT_FILE` is an absolute path to the copied `server-ca.crt` and that the hostname matches `organization.json`.

```bash
openssl s_client \
  -connect agentdesktop.example.com:8090 \
  -servername agentdesktop.example.com \
  -CAfile "$SSL_CERT_FILE" </dev/null
```

### Enrollment remains pending

Open the administrator application, refresh **Enrollments**, and approve the matching subject and public-key fingerprint. Inspect enrollment logs:

```bash
docker compose logs --tail=200 enrollment-server
```

### Gateway is unavailable

```bash
ssh VM_USER@agentdesktop.example.com \
  'curl --fail http://127.0.0.1:15021/healthz/ready'
```

Also confirm inbound TCP `8443` is allowed and that `organization.json` uses the correct hostname.

On the server, inspect or restart the Gateway through the deployment boundary rather than the employee client:

```bash
docker compose logs --tail=200 agentgateway
docker compose restart agentgateway
./verify.sh
```

## 12. Stop or reset the VM

Stop services while preserving data:

```bash
cd agentdesktop/examples/managed-vm
docker compose down
```

Delete PostgreSQL and Keycloak state:

```bash
docker compose down --volumes
```

Delete generated certificates and bootstrap only for a deliberate full reset:

```bash
rm -rf runtime .env
```

## Production replacements

Before production use:

- Replace generated server certificates with publicly or organizationally trusted certificates, normally on standard ports.
- Replace seeded Keycloak users with the organization IdP.
- Require a tightly controlled administrator group through `ADMIN_OAUTH_ROLE`.
- Replace the file-backed enrollment CA with PKCS#11/HSM or managed PKI.
- Use managed PostgreSQL, unique credentials, encrypted backups, and restricted private networking.
- Sign desktop and administrator binaries and distribute bootstrap/trust through MDM.
- Add high availability, alerting, backup, and recovery procedures.
- Complete fail-closed revocation consumption before claiming immediate rejection of already-issued certificates.

## Success checklist

- [ ] `./verify.sh` reports the VM stack healthy.
- [ ] The laptop completes employee OAuth login.
- [ ] The administrator sees the pending enrollment.
- [ ] Approval produces an active device in **Devices**.
- [ ] The laptop reports `mode=managed`, `gateway=reachable`, and `identity=ready`.
- [ ] A request reaches Agent Gateway.
- [ ] **Inventory** shows the enrolled endpoint, discovered agents/resources, and supports force-rescan.
- [ ] **Policies** saves Allow/Deny desired state for each supported agent and labels enforcement unavailable.
