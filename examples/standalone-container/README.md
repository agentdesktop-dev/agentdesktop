# Disposable standalone container demo

This demo runs Agentdesktop, Claude Code, Dex, and Agentgateway in containers.
Agentdesktop and Claude Code use a disposable Docker volume, so it does not
create or alter your host Agentdesktop state, `~/.claude/settings.json`, or
Claude Code credentials.

It requires Linux Docker Engine with Compose host networking, Docker image
build support, and an Anthropic API key. Docker images, a named volume, and
loopback-only ports are the only host-side resources it creates.

## Run the demo

From the repository root, start the stack:

```sh
export ANTHROPIC_API_KEY=sk-ant-...
docker compose -f examples/standalone-container/compose.yaml up --build -d
```

The first build compiles Agentdesktop and installs Claude Code in the demo
image. Wait until Dex and Agentgateway are ready, then start Claude Code in the
isolated container:

```sh
docker compose -f examples/standalone-container/compose.yaml exec agentdesktop claude
```

Claude Code requests a gateway credential. The daemon prints a URL similar to
`http://127.0.0.1:51327/`; open it in the host browser and select **Continue**.
Sign in to Dex with `admin@example.com` and `password`. The browser callback
reaches the daemon through Linux host networking and Claude Code then uses the
local Agentgateway at `http://127.0.0.1:4001`.

The Compose services publish only these host-loopback ports: Dex on `5557`,
Agentgateway on `4001`, and the temporary OIDC callback on `51327` while a
login is in progress. Do not expose these development services beyond the host.

## Remove all demo state

Stop the containers and remove their isolated Claude Code and Agentdesktop
state:

```sh
docker compose -f examples/standalone-container/compose.yaml down --volumes
```

The callback flow uses Linux Docker host networking. For macOS or Windows,
use the VM walkthrough from the `yuvalk/main` branch instead