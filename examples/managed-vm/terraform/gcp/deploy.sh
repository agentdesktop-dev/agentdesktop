#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../../../.." && pwd)"

usage() {
  cat <<'EOF'
Usage: ./deploy.sh [terraform apply options]

Creates or updates the GCP infrastructure, copies the current local server
source to the VM, starts the managed stack, and downloads the public client
bootstrap. For example:

  ./deploy.sh -auto-approve

Configure Terraform in terraform.tfvars and deployment secrets in deploy.env.
EOF
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
  usage
  exit 0
fi

for command_name in gcloud tar terraform; do
  if ! command -v "$command_name" >/dev/null; then
    echo "$command_name is required" >&2
    exit 1
  fi
done

if [[ -f "$script_dir/deploy.env" ]]; then
  # shellcheck disable=SC1091
  source "$script_dir/deploy.env"
fi

: "${AGENTDESKTOP_ANTHROPIC_API_KEY:?set it in deploy.env or the environment}"
: "${AGENTDESKTOP_KEYCLOAK_ADMIN_PASSWORD:?set it in deploy.env or the environment}"

validate_dotenv_value() {
  local name="$1"
  local value="$2"
  if [[ "$value" == *$'\n'* || "$value" == *$'\r'* || "$value" == *"'"* ]]; then
    echo "$name must not contain a newline, carriage return, or single quote" >&2
    exit 2
  fi
}

validate_dotenv_value AGENTDESKTOP_ANTHROPIC_API_KEY "$AGENTDESKTOP_ANTHROPIC_API_KEY"
validate_dotenv_value AGENTDESKTOP_KEYCLOAK_ADMIN_PASSWORD "$AGENTDESKTOP_KEYCLOAK_ADMIN_PASSWORD"

terraform -chdir="$script_dir" init
terraform -chdir="$script_dir" apply "$@"

project_id="$(terraform -chdir="$script_dir" output -raw project_id)"
zone="$(terraform -chdir="$script_dir" output -raw zone)"
instance_name="$(terraform -chdir="$script_dir" output -raw instance_name)"
public_host="$(terraform -chdir="$script_dir" output -raw public_host)"
public_ip="$(terraform -chdir="$script_dir" output -raw public_ip)"

work_dir="$(mktemp -d "${TMPDIR:-/tmp}/agentdesktop-gcp.XXXXXX")"
source_archive="$work_dir/agentdesktop-source.tar.gz"
secrets_file="$work_dir/agentdesktop.env"

cleanup() {
  rm -rf "$work_dir"
}
trap cleanup EXIT

tar \
  --exclude='*/.DS_Store' \
  --exclude='admin-ui/node_modules' \
  --exclude='control-plane/node_modules' \
  --exclude='examples/managed-vm/.env' \
  --exclude='examples/managed-vm/runtime' \
  --exclude='examples/managed-vm/terraform' \
  -czf "$source_archive" \
  -C "$repo_root" \
  admin-ui control-plane examples/managed-vm

if tar -tzf "$source_archive" | grep -Eq '^examples/managed-vm/(\.env|runtime)(/|$)'; then
  echo "refusing to upload an archive containing managed-vm secrets or runtime state" >&2
  exit 1
fi

cat >"$secrets_file" <<EOF
PUBLIC_HOST='$public_host'
ANTHROPIC_API_KEY='$AGENTDESKTOP_ANTHROPIC_API_KEY'
KEYCLOAK_ADMIN_PASSWORD='$AGENTDESKTOP_KEYCLOAK_ADMIN_PASSWORD'
EOF
chmod 0600 "$secrets_file"

gcloud_options=(
  "--project=$project_id"
  "--zone=$zone"
  --tunnel-through-iap
  --quiet
)

echo "Uploading the current local server source to $instance_name..."
gcloud compute scp \
  "$source_archive" \
  "$instance_name:~/agentdesktop-source.tar.gz" \
  "${gcloud_options[@]}"
gcloud compute scp \
  "$secrets_file" \
  "$instance_name:~/agentdesktop.env" \
  "${gcloud_options[@]}"
gcloud compute scp \
  "$script_dir/remote-deploy.sh" \
  "$instance_name:~/agentdesktop-remote-deploy.sh" \
  "${gcloud_options[@]}"

gcloud compute ssh "$instance_name" \
  "${gcloud_options[@]}" \
  --command="bash ~/agentdesktop-remote-deploy.sh '$public_host'"

bootstrap_dir="${AGENTDESKTOP_BOOTSTRAP_DIR:-$script_dir/client-bootstrap}"
mkdir -p "$bootstrap_dir"
gcloud compute scp \
  "$instance_name:/opt/agentdesktop/examples/managed-vm/runtime/organization.json" \
  "$bootstrap_dir/" \
  "${gcloud_options[@]}"
gcloud compute scp \
  "$instance_name:/opt/agentdesktop/examples/managed-vm/runtime/certs/server-ca.crt" \
  "$bootstrap_dir/" \
  "${gcloud_options[@]}"
chmod 0600 "$bootstrap_dir/organization.json"
chmod 0644 "$bootstrap_dir/server-ca.crt"

cat <<EOF

Agent Desktop managed development stack is running.

Public IP:       $public_ip
Public hostname: $public_host
Admin URL:       https://$public_host:8090/admin/
Client files:    $bootstrap_dir

Confirm that $public_host resolves to $public_ip before connecting a client.
Trust server-ca.crt only on development clients that should access this stack.
EOF