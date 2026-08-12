# Local scenario

This scenario runs AgentDesktop with Dex, Claude Code, and Agentgateway.

Generate the controller signing key and start the controller

```console
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 \
  -out /tmp/agentdesktop-gateway-jwt.pem

make ui
cargo run --bin agentdesktop-controller -- \
  --config examples/claude/controller.yaml
```

Start supporting services (Dex IDP and Agentgateway):
```console
docker compose -f examples/claude/compose.yaml up -d 
```

Run the local daemon. Note: typically this would be run on a different machine; for this example we run the controller and daemon together.
```console
./scripts/run-agentdesktopd-root \
  --config examples/claude/agentdesktopd.yaml
```

Sign in with `admin@example.com` / `password`. The controller UI is at
<http://127.0.0.1:8080>.
