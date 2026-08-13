# GCP managed development deployment

This Terraform configuration creates one Google Compute Engine VM for the
[remote managed VM example](../../README.md). It also creates a dedicated VPC,
subnet, static public IPv4 address, client firewall rules, IAP-only SSH access,
and optionally a Cloud DNS `A` record. The VM startup script installs Docker
Engine, Docker Compose, and the host utilities required by the example.

`deploy.sh` copies the current local `admin-ui/`, `control-plane/`, and
`examples/managed-vm/` directories to the VM. The copy includes uncommitted and
untracked source changes, so the branch does not need to be pushed. It excludes
dependencies, `.env`, generated certificates and runtime state, Terraform state,
and repository metadata. Deployment secrets are transferred separately over
IAP and are not stored in Terraform state. The administrator UI is rebuilt in a
disposable Node 22 container before the Go enrollment server is built.

## Development boundary

This creates the repository's development stack, not a production GCP
deployment. It exposes nonstandard TLS ports, uses generated private CAs,
Keycloak development mode and seeded users, one VM, a file-backed enrollment
CA, and local Docker volumes. Restrict `client_source_ranges`, remove the VM
when testing is complete, and apply the production replacements in the
[managed walkthrough](../../../../docs/deployment/managed-vm-walkthrough.md#production-replacements)
before using this architecture for real users.

## Prerequisites

- A GCP project with billing enabled.
- Terraform 1.6 or newer and the Google Cloud CLI on the development machine.
- A public DNS hostname is optional; the default uses the allocated static IPv4
  address directly.
- Permission to enable project services and administer Compute Engine. The
  deploying identity also needs IAP tunnel access and OS Admin Login. Cloud DNS
  administration is required only when Terraform manages the record.

Typical roles for a dedicated development project are:

```text
roles/serviceusage.serviceUsageAdmin
roles/compute.admin
roles/compute.osAdminLogin
roles/iap.tunnelResourceAccessor
roles/dns.admin                         # only for Cloud DNS
```

Authenticate both `gcloud` and Terraform's Application Default Credentials:

```bash
gcloud auth login
gcloud auth application-default login
gcloud config set project YOUR_PROJECT_ID
```

Terraform uses Application Default Credentials, while source upload and IAP SSH
use the active `gcloud` account. Both identities need the roles above; selecting
the same account for both login commands is the simplest setup.

## Configure

From this directory:

```bash
cp terraform.tfvars.example terraform.tfvars
cp deploy.env.example deploy.env
chmod 0600 deploy.env
```

Edit `terraform.tfvars`:

```hcl
project_id = "my-gcp-project"
zone       = "us-central1-a"

instance_name = "agentdesktop-managed"
public_host    = null

client_source_ranges = [
  "YOUR_PUBLIC_IPV4_ADDRESS/32",
]

dns_managed_zone = null
```

`client_source_ranges` controls access to ports `8090`, `8443`, and `8444`.
Do not use `0.0.0.0/0` unless unrestricted Internet access is deliberate for an
isolated test.

With `public_host = null`, Terraform uses the allocated static IPv4 address in
the development certificates, OAuth URLs, and client bootstrap. No DNS record
is required.

To use DNS instead, set `public_host` to the hostname. If it belongs to an
existing Cloud DNS zone in the same project, set `dns_managed_zone` to the
zone's resource name, not its DNS suffix, and Terraform creates the `A` record.
For DNS hosted elsewhere, leave `dns_managed_zone` null and create the record
using the `public_ip` output.

Edit `deploy.env` and replace both placeholders:

```bash
AGENTDESKTOP_ANTHROPIC_API_KEY='sk-ant-...'
AGENTDESKTOP_KEYCLOAK_ADMIN_PASSWORD='a-long-random-value'
```

Both local configuration files are ignored by Git. The password and API key
must not contain newlines or single quotes.

## Deploy

Run the complete infrastructure and source deployment:

```bash
./deploy.sh
```

For a noninteractive Terraform approval:

```bash
./deploy.sh -auto-approve
```

The script performs these operations:

1. Creates or updates the GCP infrastructure.
2. Archives the current local server source, including uncommitted files.
3. Uploads source and secrets through an IAP SSH tunnel.
4. Waits for VM package installation, generates endpoint-bound development
   certificates on first deploy, builds the containers, and verifies the stack.
5. Downloads the public client files into `client-bootstrap/`:
   `organization.json` and `server-ca.crt`.

When `public_host` is set but Cloud DNS is not managed by this configuration,
create the DNS record using:

```bash
terraform output -raw public_ip
terraform output -raw public_host
```

Confirm public resolution before starting Agent Desktop:

```bash
dig +short "$(terraform output -raw public_host)"
```

## Connect a client

Copy `client-bootstrap/organization.json` and
`client-bootstrap/server-ca.crt` to the development client. Never copy a key
from the VM. Trust the development server CA on that client, point
`SSL_CERT_FILE` and `AGENTDESKTOP_ORGANIZATION_CONFIG` at those files, and
continue at [Start Agent Desktop on the laptop](../../../../docs/deployment/managed-vm-walkthrough.md#4-start-agent-desktop-on-the-laptop).

On a Windows development VM that shares this repository, trust the downloaded
CA once for the current user, install the Windows UI dependencies, and use the
GCP launcher:

```powershell
$Bootstrap = Join-Path $PWD 'examples\managed-vm\terraform\gcp\client-bootstrap'
Import-Certificate `
  -FilePath "$Bootstrap\server-ca.crt" `
  -CertStoreLocation 'Cert:\CurrentUser\Root'

npm --prefix ui ci
& .\examples\managed-vm\terraform\gcp\start-client.ps1
```

The launcher validates the bootstrap and CA trust, then derives the OAuth,
enrollment, and Gateway endpoints from `organization.json`. Quit any existing
Agent Desktop tray process before starting it; an already-running process keeps
the organization configuration with which it was launched.

The administrator application is available at the `admin_url` output. The
seeded development accounts remain:

| Workflow | Username | Password |
| --- | --- | --- |
| Laptop enrollment | `employee` | `employee-change-me` |
| Enrollment administration | `administrator` | `administrator-change-me` |

## Redeploy local changes

Run the same command again:

```bash
./deploy.sh -auto-approve
```

The upload reflects the current local files and removes remote source files
that were deleted locally. It preserves generated CAs, enrollments, PostgreSQL
and Keycloak Docker volumes. Changing `public_host` intentionally fails when a
runtime already exists because that change requires replacing certificates,
OAuth callbacks, and enrolled client bootstrap state.

## Diagnostics

Open an IAP SSH session:

```bash
gcloud compute ssh "$(terraform output -raw instance_name)" \
  --project "$(terraform output -raw project_id)" \
  --zone "$(terraform output -raw zone)" \
  --tunnel-through-iap
```

On the VM:

```bash
cd /opt/agentdesktop/examples/managed-vm
sudo docker compose ps
sudo docker compose logs --tail=200 enrollment-server
sudo docker compose logs --tail=200 keycloak
sudo docker compose logs --tail=200 agentgateway
bash ./verify.sh
```

If the initial package installation fails, inspect:

```bash
sudo journalctl -u google-startup-scripts.service --no-pager -n 200
```

## Destroy

Terraform destroy deletes the VM, its Docker volumes and enrollment state, the
static address, firewall rules, subnet, network, and any managed DNS record:

```bash
terraform destroy
```

The enabled Google APIs remain enabled. Local files under `client-bootstrap/`
are ignored but are not deleted automatically.
