#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"
umask 077
rm -rf certs
mkdir certs

openssl ecparam -name prime256v1 -genkey -noout -out certs/enrollment-ca.key
openssl req -x509 -new -sha256 -days 30 \
  -key certs/enrollment-ca.key \
  -subj '/CN=Agent Desktop Walkthrough Enrollment CA' \
  -addext 'basicConstraints=critical,CA:TRUE' \
  -addext 'keyUsage=critical,keyCertSign,cRLSign' \
  -out certs/enrollment-ca.crt

issue_certificate() {
  local name="$1"
  local subject="$2"
  local extensions="$3"

  openssl ecparam -name prime256v1 -genkey -noout -out "certs/${name}.key"
  openssl req -new -sha256 \
    -key "certs/${name}.key" \
    -subj "$subject" \
    -out "certs/${name}.csr"
  openssl x509 -req -sha256 -days 7 \
    -in "certs/${name}.csr" \
    -CA certs/enrollment-ca.crt \
    -CAkey certs/enrollment-ca.key \
    -CAcreateserial \
    -extfile <(printf '%s\n' "$extensions") \
    -out "certs/${name}.crt"
  rm "certs/${name}.csr"
}

issue_certificate \
  enrollment-server \
  '/CN=enrollment.agentdesktop.test' \
  $'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:enrollment.agentdesktop.test,DNS:localhost,IP:127.0.0.1'

issue_certificate \
  gateway-authorizer \
  '/CN=Agent Gateway Walkthrough Authorizer' \
  $'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=clientAuth\nsubjectAltName=URI:spiffe://agentdesktop.test/service/agentgateway'

openssl ecparam -name prime256v1 -genkey -noout -out certs/gateway-server-ca.key
openssl req -x509 -new -sha256 -days 30 \
  -key certs/gateway-server-ca.key \
  -subj '/CN=Agent Gateway Walkthrough Server CA' \
  -addext 'basicConstraints=critical,CA:TRUE' \
  -addext 'keyUsage=critical,keyCertSign,cRLSign' \
  -out certs/gateway-server-ca.crt

openssl ecparam -name prime256v1 -genkey -noout -out certs/gateway-server.key
openssl req -new -sha256 \
  -key certs/gateway-server.key \
  -subj '/CN=gateway.agentdesktop.test' \
  -out certs/gateway-server.csr
openssl x509 -req -sha256 -days 7 \
  -in certs/gateway-server.csr \
  -CA certs/gateway-server-ca.crt \
  -CAkey certs/gateway-server-ca.key \
  -CAcreateserial \
  -extfile <(printf '%s\n' $'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:gateway.agentdesktop.test,DNS:localhost,IP:127.0.0.1') \
  -out certs/gateway-server.crt
rm certs/gateway-server.csr certs/*.srl

chmod 600 certs/*.key
printf 'Generated walkthrough certificates in %s/certs\n' "$PWD"