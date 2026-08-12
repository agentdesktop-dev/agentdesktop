#!/usr/bin/env bash
set -euo pipefail

public_host="${1:-}"
source_archive="$HOME/agentdesktop-source.tar.gz"
secrets_file="$HOME/agentdesktop.env"
remote_script="$HOME/agentdesktop-remote-deploy.sh"
install_root=/opt/agentdesktop
managed_dir="$install_root/examples/managed-vm"
ready_file=/var/lib/agentdesktop-bootstrap.ready
stage_dir=""

cleanup() {
  if [[ -n "$stage_dir" ]]; then
    rm -rf "$stage_dir"
  fi
  rm -f "$source_archive" "$secrets_file" "$remote_script"
}
trap cleanup EXIT

if [[ -z "$public_host" || ! "$public_host" =~ ^[A-Za-z0-9.-]+$ ]]; then
  echo "usage: $0 PUBLIC_DNS_NAME" >&2
  exit 2
fi
if [[ ! -f "$source_archive" || ! -f "$secrets_file" ]]; then
  echo "source archive and deployment environment must be uploaded first" >&2
  exit 2
fi

for attempt in $(seq 1 180); do
  if sudo test -f "$ready_file"; then
    break
  fi
  if [[ "$attempt" == 180 ]]; then
    echo "VM bootstrap did not finish within 15 minutes" >&2
    sudo journalctl -u google-startup-scripts.service --no-pager -n 100 || true
    exit 1
  fi
  sleep 5
done

stage_dir="$(mktemp -d)"
tar -xzf "$source_archive" -C "$stage_dir"
test -f "$stage_dir/admin-ui/package.json"
test -f "$stage_dir/control-plane/Dockerfile"
test -f "$stage_dir/examples/managed-vm/compose.yaml"

sudo install -d -m 0755 -o "$(id -u)" -g "$(id -g)" "$install_root"
mkdir -p "$install_root/admin-ui" "$install_root/control-plane" "$managed_dir"
rsync -a --delete \
  --exclude='node_modules/' \
  "$stage_dir/admin-ui/" \
  "$install_root/admin-ui/"
rsync -a --delete "$stage_dir/control-plane/" "$install_root/control-plane/"
rsync -a --delete \
  --exclude='.env' \
  --exclude='runtime/' \
  "$stage_dir/examples/managed-vm/" \
  "$managed_dir/"

if [[ -d "$managed_dir/runtime" ]]; then
  if [[ ! -f "$managed_dir/runtime/organization.json" || ! -f "$managed_dir/runtime/certs/server-ca.crt" ]]; then
    echo "existing managed runtime is incomplete; reset it deliberately before redeploying" >&2
    exit 1
  fi
  existing_host="$(jq -er '.identity.issuer | capture("^https://(?<host>[A-Za-z0-9.-]+):8444/realms/agentdesktop$").host' "$managed_dir/runtime/organization.json")"
  if [[ "$existing_host" != "$public_host" ]]; then
    echo "existing runtime belongs to $existing_host, not $public_host" >&2
    echo "changing the hostname requires a deliberate runtime and enrollment reset" >&2
    exit 1
  fi
fi

install -m 0600 "$secrets_file" "$managed_dir/.env"
if [[ ! -d "$managed_dir/runtime" ]]; then
  bash "$managed_dir/prepare.sh" "$public_host"
fi

sudo docker run --rm \
  --user "$(id -u):$(id -g)" \
  --env HOME=/tmp \
  --volume "$install_root:/workspace" \
  --workdir /workspace/admin-ui \
  docker.io/library/node:22-bookworm-slim \
  sh -c 'npm ci --no-audit --no-fund && npm run build'

cd "$managed_dir"
sudo docker compose config --quiet
sudo docker compose up -d --build
bash ./verify.sh

echo "Server source deployed at $install_root."