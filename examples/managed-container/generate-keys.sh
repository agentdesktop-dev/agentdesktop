#!/bin/sh
set -eu

apk add --no-cache openssl >/dev/null

if [ -f /keys/controller.pem ]; then
    exit 0
fi

work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT

openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 \
    -out "$work_dir/gateway-jwt-key.pem"

cat > "$work_dir/ca.cnf" <<'EOF'
[req]
distinguished_name = subject
x509_extensions = v3_ca
prompt = no

[subject]
CN = Agentdesktop-managed-container-device-CA

[v3_ca]
basicConstraints = critical,CA:TRUE
keyUsage = critical,keyCertSign,cRLSign
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid:always,issuer
EOF

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout "$work_dir/device-ca-key.pem" \
    -out "$work_dir/device-ca.pem" \
    -days 365 -sha256 -config "$work_dir/ca.cnf"

cat > "$work_dir/controller.cnf" <<'EOF'
[req]
distinguished_name = subject
prompt = no

[subject]
CN = localhost
EOF

cat > "$work_dir/controller.ext" <<'EOF'
[v3_server]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature,keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:localhost,IP:127.0.0.1
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid,issuer
EOF

openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout "$work_dir/controller-key.pem" \
    -out "$work_dir/controller.csr" \
    -config "$work_dir/controller.cnf"

openssl x509 -req -in "$work_dir/controller.csr" \
    -CA "$work_dir/device-ca.pem" \
    -CAkey "$work_dir/device-ca-key.pem" \
    -set_serial 1 -days 30 -sha256 \
    -extfile "$work_dir/controller.ext" \
    -extensions v3_server \
    -out "$work_dir/controller.pem"

cp "$work_dir/controller.pem" "$work_dir/controller-key.pem" \
    "$work_dir/device-ca.pem" "$work_dir/device-ca-key.pem" \
    "$work_dir/gateway-jwt-key.pem" /keys/
chmod 600 /keys/*-key.pem
chmod 644 /keys/controller.pem /keys/device-ca.pem