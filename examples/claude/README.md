# Local scenario

This scenario runs Agentdesktop with Dex, Claude Code, and Agentgateway.

From the repository root, generate the development TLS and JWT keys used by the
checked-in configuration:

```console
./examples/claude/create-keys.sh
```

The script creates six files under `/tmp/agentdesktop-keys`: the controller
certificate and private key, the device CA certificate and private key, the
gateway JWT signing key, and the token Agentgateway uses to send token and cost
usage to the controller. It refuses to overwrite an existing key set.

Start Dex:

```console
docker compose -f examples/claude/compose.yaml up -d dex
curl --fail --silent --show-error \
  --retry 10 --retry-all-errors --retry-delay 1 \
  http://127.0.0.1:5556/dex/.well-known/openid-configuration \
  > /dev/null
```

Start the controller:

```console
agentdesktop-controller --config examples/claude/controller.yaml
```

Start Agentgateway:

On Docker Desktop, first enable host networking under **Settings > Resources >
Network**. Linux supports host networking directly.

```console
export ANTHROPIC_API_KEY=...
export AGENTDESKTOP_USAGE_TOKEN="$(cat /tmp/agentdesktop-keys/usage-token)"
docker compose -f examples/claude/compose.yaml up -d agentgateway
```

Agentgateway sends one access-log record per request to the controller, which
counts tokens and cost for each user, device, tool, and model. They appear under
**Usage** in the controller UI and in the desktop app.

Confirm that Agentgateway remains running and answers its reachability route:

```console
docker compose -f examples/claude/compose.yaml ps agentgateway
curl --fail --head http://127.0.0.1:4000/
```

Run the installed local daemon. Typically, this would run on a different
machine; this example runs the controller and daemon together. Because it is
launched with `sudo`, the daemon automatically authorizes the invoking desktop
user to access its local API.

```console
sudo "$(command -v agentdesktop)" daemon \
  --config examples/claude/agentdesktop.yaml
```

Sign in with `admin@example.com` / `password`. The controller UI is at
<http://127.0.0.1:8080>.

After enrollment, open the local desktop UI from another terminal:

```console
agentdesktop
```

## Claude Code

Now that agentdesktop is running, Claude Code can be run.
It will show the configured `Managed by Agentdesktop` and direct traffic through the gateway.

## Stop the scenario

Stop the foreground daemon and controller with Ctrl-C, then stop Dex and
Agentgateway:

```console
docker compose -f examples/claude/compose.yaml down
```
