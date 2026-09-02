# Claude Code or Claude Desktop through Agentgateway without a controller

This example runs Dex and Agentgateway locally, then configures Agentdesktop to
send Claude traffic through the gateway.
Dex issues the user's OAuth access token directly to Agentdesktop.
Agentgateway verifies that token against Dex's JWKS before forwarding the request to Anthropic.

This example runs without a controller, and is useful for simple configuration on a single machine.

## Prerequisites

- Docker with Compose
- `agentdesktop` and Claude Code on your `PATH`
- An Anthropic API key for Agentgateway's upstream requests

## Run the example

Start Dex and Agentgateway from the repository root:

```sh
export ANTHROPIC_API_KEY=sk-ant-...
docker compose -f examples/standalone/compose.yaml up -d
```

### Claude Code

Next we can run the agentdesktop daemon to configure Claude Code, and then manage authentication to Agentgateway.

First, we can run an optional dry-run to preview the changes:

```sh
$ agentdesktop daemon --config examples/standalone/config.yaml --user --dry-run
Dry run — no files will be changed

UPDATE  Claude Code settings
        /home/john/.claude/settings.json
--- current
+++ proposed
@@ -1,4 +1,12 @@
 {
+  "apiKeyHelper": "'/home/john/.cargo/bin/agentdesktop' --socket '/run/user/1000/agentdesktop.sock' credential --client-id claude-code",
+  "companyAnnouncements": [
+    "Using the local Agentgateway through Agentdesktop"
+  ],
+  "env": {
+    "ANTHROPIC_BASE_URL": "http://127.0.0.1:4001/",
+    "CLAUDE_CODE_API_KEY_HELPER_TTL_MS": "60000"
+  },
   "model": "haiku",
   "permissions": {
     "allow": [

Summary: 1 change, 5 unchanged
```

Then we can run the daemon, which will actually configure Claude Code:

```sh
$ agentdesktop daemon --config examples/standalone/config.yaml --user
```

When we run `claude`, we can now see the message we programmed (`Using the local Agentgateway through Agentdesktop`),
and messages we send will traverse Agentgateway.

## Claude Desktop

In the Claude Code example, we ran with `--user`. This runs the daemon as an unprivileged user.

Claude Desktop, however, requires root configuration.

Uncomment the Claude Desktop configuration in `config.yaml` and save the file.
Then run the daemon as root:

```sh
sudo "$(which agentdesktop)" daemon \
  --config examples/standalone/config.yaml
```
