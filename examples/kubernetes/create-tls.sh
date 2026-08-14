#!/usr/bin/env bash

set -euo pipefail

namespace="${1:-agentdesktop}"
work_dir="$(mktemp -d)"
trap 'rm -rf "${work_dir}"' EXIT

if kubectl --namespace "${namespace}" get secret agentdesktop-controller-tls >/dev/null 2>&1; then
  kubectl --namespace "${namespace}" get secret agentdesktop-controller-tls \
    --output jsonpath='{.data.device-ca\.pem}' | openssl base64 -d -A \
    > "${work_dir}/device-ca.pem"
  kubectl --namespace "${namespace}" get secret agentdesktop-controller-tls \
    --output jsonpath='{.data.device-ca-key\.pem}' | openssl base64 -d -A \
    > "${work_dir}/device-ca-key.pem"
  echo "Reusing the existing device CA"
else
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout "${work_dir}/device-ca-key.pem" \
    -out "${work_dir}/device-ca.pem" \
    -days 30 -sha256 -subj /CN=Agentdesktop-Kubernetes-example-device-CA \
    -addext basicConstraints=critical,CA:TRUE \
    -addext keyUsage=critical,keyCertSign,cRLSign
fi

openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout "${work_dir}/controller-key.pem" \
  -out "${work_dir}/controller.csr" \
  -subj /CN=agentdesktop \
  -addext "subjectAltName=DNS:agentdesktop,DNS:agentdesktop.${namespace},DNS:agentdesktop.${namespace}.svc,DNS:agentdesktop.${namespace}.svc.cluster.local" \
  -addext extendedKeyUsage=serverAuth

openssl x509 -req -in "${work_dir}/controller.csr" \
  -CA "${work_dir}/device-ca.pem" \
  -CAkey "${work_dir}/device-ca-key.pem" \
  -set_serial 1 -days 30 -sha256 -copy_extensions copy \
  -out "${work_dir}/controller.pem"

kubectl --namespace "${namespace}" create secret generic agentdesktop-controller-tls \
  --from-file="${work_dir}/controller.pem" \
  --from-file="${work_dir}/controller-key.pem" \
  --from-file="${work_dir}/device-ca.pem" \
  --from-file="${work_dir}/device-ca-key.pem" \
  --dry-run=client --output=yaml | kubectl apply -f -

echo "Created Secret ${namespace}/agentdesktop-controller-tls"

if kubectl --namespace "${namespace}" get deployment agentdesktop >/dev/null 2>&1; then
  kubectl --namespace "${namespace}" rollout restart deployment/agentdesktop
  kubectl --namespace "${namespace}" rollout status deployment/agentdesktop
fi
