# Disposable managed container demo

This scenario runs an Agentdesktop controller, a simulated enrolled device,
Claude Code, Dex, and Agentgateway in Docker containers. The controller
database, development keys, device identity, OAuth tokens, and Claude Code
configuration are all kept in Docker volumes. It does not write to your host
Agentdesktop state or `~/.claude` configuration.

It requires Linux Docker Engine with Compose host networking and an Anthropic
API key. The stack claims only host-loopback ports: `14000` (Agentgateway),
`5556` (Dex), `18080` (controller dashboard), `18443` (fleet API), and `51327`
only while browser sign-in is active.

## Run the demo

From the repository root:

```sh
export ANTHROPIC_API_KEY=sk-ant-...
docker compose -f examples/managed-container/compose.yaml up --build -d
```

Open the controller dashboard at <http://127.0.0.1:18080>. The simulated device
starts its enrollment flow and logs a local URL. Open that URL in the host
browser, select **Continue**, then sign in to Dex as `admin@example.com` with
password `password`:

```sh
docker compose -f examples/managed-container/compose.yaml logs -f agentdesktop
```

After enrollment, inspect the device and its discovered Claude Code installation
in the controller dashboard. Run Claude Code inside the simulated device with:

```sh
docker compose -f examples/managed-container/compose.yaml exec agentdesktop claude
```

Claude Code receives a controller-signed gateway credential. Its configuration
is stored only in the `device-state` Docker volume.

## Reset the demo

Stop the stack and delete its controller, device, OAuth, Claude Code, and
development-key state:

```sh
docker compose -f examples/managed-container/compose.yaml down --volumes
```

This scenario relies on Linux Docker host networking so the host browser can
reach the simulated device's loopback OIDC callback. Do not expose its
development endpoints beyond the host.