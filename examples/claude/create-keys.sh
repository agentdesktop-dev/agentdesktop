#!/usr/bin/env bash

set -euo pipefail

key_dir=/tmp/agentdesktop-keys
key_files=(
  controller.pem
  controller-key.pem
  device-ca.pem
  device-ca-key.pem
  gateway-jwt-key.pem
)

if ! command -v openssl >/dev/null 2>&1; then
  echo "openssl is required to generate Agentdesktop development keys." >&2
  exit 1
fi

existing_keys=0
for file in "${key_files[@]}"; do
  if [[ -e "${key_dir}/${file}" ]]; then
    existing_keys=$((existing_keys + 1))
  fi
done

if (( existing_keys > 0 )); then
  if (( existing_keys != ${#key_files[@]} )); then
    echo "${key_dir} contains an incomplete Agentdesktop development key set." >&2
    echo "Remove the directory before generating a new key set." >&2
    exit 1
  fi
  echo "${key_dir} already contains Agentdesktop development keys." >&2
  echo "Remove the directory before generating a new key set." >&2
  exit 1
fi

work_dir="$(mktemp -d)"
trap 'rm -rf "${work_dir}"' EXIT

openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 \
  -out "${work_dir}/gateway-jwt-key.pem"

cat > "${work_dir}/ca.cnf" <<'EOF'
[req]
distinguished_name = subject
x509_extensions = v3_ca
prompt = no

[subject]
CN = Agentdesktop-local-device-CA

[v3_ca]
basicConstraints = critical,CA:TRUE
keyUsage = critical,keyCertSign,cRLSign
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid:always,issuer
EOF

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout "${work_dir}/device-ca-key.pem" \
  -out "${work_dir}/device-ca.pem" \
  -days 365 -sha256 -config "${work_dir}/ca.cnf"

cat > "${work_dir}/controller.cnf" <<'EOF'
[req]
distinguished_name = subject
prompt = no

[subject]
CN = localhost
EOF

cat > "${work_dir}/controller.ext" <<'EOF'
[v3_server]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature,keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:localhost,IP:127.0.0.1
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid,issuer
EOF

openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout "${work_dir}/controller-key.pem" \
  -out "${work_dir}/controller.csr" \
  -config "${work_dir}/controller.cnf"

openssl x509 -req -in "${work_dir}/controller.csr" \
  -CA "${work_dir}/device-ca.pem" \
  -CAkey "${work_dir}/device-ca-key.pem" \
  -set_serial 1 -days 30 -sha256 \
  -extfile "${work_dir}/controller.ext" \
  -extensions v3_server \
  -out "${work_dir}/controller.pem"

install -d -m 700 "${key_dir}"
install -m 600 \
  "${work_dir}/controller-key.pem" \
  "${work_dir}/device-ca-key.pem" \
  "${work_dir}/gateway-jwt-key.pem" \
  "${key_dir}/"
install -m 644 \
  "${work_dir}/controller.pem" \
  "${work_dir}/device-ca.pem" \
  "${key_dir}/"

echo "Generated Agentdesktop development keys in ${key_dir}"