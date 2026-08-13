# Local scenario

This scenario runs Agentdesktop with Dex, Claude Code, and Agentgateway.

Generate all local controller key material in one directory. The TLS directory
shorthand recognizes `controller.pem`, `controller-key.pem`, `device-ca.pem`,
and `device-ca-key.pem`; the same directory also holds the inference-gateway
JWT signing key:

```console
mkdir -p /tmp/agentdesktop-keys

openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 \
  -out /tmp/agentdesktop-keys/gateway-jwt-key.pem

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout /tmp/agentdesktop-keys/device-ca-key.pem \
  -out /tmp/agentdesktop-keys/device-ca.pem \
  -days 365 -sha256 -subj /CN=Agentdesktop-local-device-CA \
  -addext basicConstraints=critical,CA:TRUE \
  -addext keyUsage=critical,keyCertSign,cRLSign

openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout /tmp/agentdesktop-keys/controller-key.pem \
  -out /tmp/agentdesktop-controller.csr \
  -subj /CN=localhost \
  -addext subjectAltName=DNS:localhost,IP:127.0.0.1 \
  -addext extendedKeyUsage=serverAuth

openssl x509 -req -in /tmp/agentdesktop-controller.csr \
  -CA /tmp/agentdesktop-keys/device-ca.pem \
  -CAkey /tmp/agentdesktop-keys/device-ca-key.pem \
  -set_serial 1 -days 30 -sha256 -copy_extensions copy \
  -out /tmp/agentdesktop-keys/controller.pem

rm /tmp/agentdesktop-controller.csr
chmod 600 /tmp/agentdesktop-keys/controller-key.pem \
  /tmp/agentdesktop-keys/device-ca-key.pem \
  /tmp/agentdesktop-keys/gateway-jwt-key.pem
```

Start Dex:

```console
docker compose -f examples/claude/compose.yaml up -d dex
```

Build the UI and start the controller:

```console
make ui
cargo run --bin agentdesktop-controller -- \
  --config examples/claude/controller.yaml
```

Start Agentgateway:

```console
docker compose -f examples/claude/compose.yaml up -d agentgateway
```

Run the local daemon. Note: typically this would be run on a different machine; for this example we run the controller and daemon together.
The helper authorizes the invoking desktop user to access the local API. Direct
system deployments should pass that user's numeric UID with `--local-api-uid`.
```console
./scripts/run-agentdesktop-root \
  --config examples/claude/agentdesktop.yaml
```

Sign in with `admin@example.com` / `password`. The controller UI is at
<http://127.0.0.1:8080>.
