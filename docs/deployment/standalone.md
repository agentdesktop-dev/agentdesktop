# Standalone Operations

This guide covers a single-user installation where Agent Gateway and the edge connector run as separate processes on one machine. It does not require OAuth, device enrollment, MDM, or a control plane.

## Installation

The standalone release is distributed as one platform-specific executable containing Agent Gateway, the connector, helper commands, and a starter Agent Gateway configuration:

```bash
chmod +x agentdesktop-installer
./agentdesktop-installer
```

The installer displays its destination, service behavior, and network ownership boundary before changing files. It installs for the current user under `$HOME/.local/lib/agentdesktop` by default, starts a user systemd service after confirmation, and verifies that the product is ready before reporting success. Users do not need to run a separate health check. If Claude Code is installed, an interactive installation asks separately before changing its settings.

Use `install --yes` for non-interactive installation; it does not change AI agent settings. Add `--connect-agents` to explicitly permit that change in a script. Use `--no-start` to leave the service disabled and `--root` to select another absolute destination. `--connect-agents` cannot be combined with `--no-start`.

If setup cannot finish, the installer saves an owner-only support report at `$XDG_STATE_HOME/agentdesktop/install-support.txt`, or `$HOME/.local/state/agentdesktop/install-support.txt` when `XDG_STATE_HOME` is unset. The error screen directs the user to [open an issue](https://github.com/agentdesktop-dev/agentdesktop/issues/new) and attach that report. The report contains the installer error, service status, and recent service startup log; it does not include Agent Gateway configuration contents.

The connector application and status listeners and Agent Gateway CONNECT listener are restricted to loopback. Agent Gateway's LLM bind is `mode: internal`, so port `4000` is a socketless CONNECT target rather than an exposed OS listener. Readiness and administration listeners must also be reviewed for local-only host deployment.

Embedded components are compressed independently and verified while extracting. Installation uses a sibling staging tree and atomic activation; upgrades validate the existing manifest and restore the prior bundle if activation fails. This payload integrity check does not replace publisher signing, which remains required before public distribution.

## Ownership

Agent Gateway owns:

- Provider credentials and provider connections.
- AI routing, authorization, rate limits, guardrails, and content inspection.
- Request-level logs, audit records, and their retention.
- Any future TLS inspection configuration and issuing CA keys.

The connector owns:

- Its per-user loopback listener.
- Forwarding opaque application byte streams to Agent Gateway over HTTP/2 CONNECT.
- Optional lifecycle management for a separately installed Agent Gateway executable.
- Connector health and fail-closed flow termination.

The connector has no policy format, provider credential store, request database, or content log.

For a new starter configuration, the standalone installer generates the local inspection CA once in the user-owned Agent Gateway configuration directory. It writes the private key as `0600`, refuses to replace existing CA material, and passes only the resulting file paths to Agent Gateway configuration. Agent Gateway alone uses the key at runtime; the connector service never reads it.

## Files and permissions

Keep the Agent Gateway configuration in a directory accessible only to the user running Agent Gateway. On Unix-like systems, a suitable baseline is:

```bash
install -d -m 0700 "$HOME/.config/agentgateway"
install -m 0600 config.yaml "$HOME/.config/agentgateway/config.yaml"
```

Apply the same restrictions to files referenced by the Agent Gateway configuration. Do not make configuration or credential files readable by other local users. Backups inherit the same sensitivity and should be encrypted and access-controlled.

The connector reads only its own command-line or environment configuration. When connector configuration is stored in a service definition, restrict that file to the user account running the connector.

## Provider credentials

Supply provider credentials only through an Agent Gateway-supported secret source. The repository's development container example expands `ANTHROPIC_API_KEY` inside Agent Gateway configuration; because Agent Desktop supervises Gateway in that container, the parent process environment also contains the variable even though connector code does not read it. Do not treat this smoke setup as a production secret boundary.

Do not put a provider key in:

- Claude Code configuration.
- Connector arguments or environment variables.
- Connector service definitions.

The Claude adapter writes only a local placeholder accepted by Agent Gateway policy. Agent Gateway must supply the real provider credential and prevent the application placeholder from reaching the provider.

Environment variables can be visible to the process owner and privileged diagnostic tools. Prefer the platform secret facility supported by the selected Agent Gateway deployment when one is available. Credential rotation is an Agent Gateway and provider operation; the connector does not cache provider credentials.

## Local endpoints

Run connector endpoints and the Gateway tunnel listener on loopback:

- Connector application path: `127.0.0.1:8080` by default.
- Connector status path: `127.0.0.1:8081` by default.
- Agent Gateway CONNECT path: `127.0.0.1:15008` in the example configuration.

The Agent Gateway LLM bind on port `4000` is internal-only and does not open an OS socket.

Standalone connector validation rejects a non-loopback Agent Gateway upstream and always rejects a non-loopback connector listener. Configure Agent Gateway's listener and administrative or readiness endpoints as local-only for a host installation. Container-only examples may bind readiness endpoints more broadly inside an isolated container network; do not copy that exposure to a host without an explicit access-control decision.

A loopback listener is reachable by other processes running as the same user and may be reachable by other local users depending on operating-system controls. The example Agent Gateway authorization rule protects the mock workflow, but production policy belongs in the user's Agent Gateway configuration.

## Connect Claude Code

The guided installer detects Claude Code after Agent Desktop becomes ready and asks for separate consent before changing it. If accepted, it adds the connector endpoint and local placeholder credential to `~/.claude/settings.json` while preserving unrelated settings. It refuses to replace an existing provider or gateway configuration.

Run the same setup manually with:

```bash
agentdesktop connect-agents
```

The installer owns `~/.local/bin/agentdesktop` as a stable link to the private bundle. It does not edit shell startup files or add directories to `PATH`; environments that do not already include `~/.local/bin` receive an installer warning. The command can be rerun at any time without reinstalling Agent Desktop. Matching settings are reported as already connected; conflicting provider or gateway settings are left unchanged. After connection, launch `claude` normally. Claude Code applies the user settings to terminal and IDE sessions, so Agent Desktop does not install or require a Claude-specific launcher. Requests fail when Agent Gateway is unavailable and do not fall back to Anthropic directly.

Application configuration is routed rather than enforced: a user who can change application settings can bypass it. The standalone Linux `claude` profile provides process-scoped transparent routing; stronger enforcement against local administrators still requires an explicit host or MDM boundary.

Do not configure the same application through `connect-agents` and transparent capture simultaneously. Run normally for connector-assisted native routing, or use `agentdesktop launch --profile claude -- claude` for standalone Linux capture.

## Process lifecycle

The connector supervises a separately installed Agent Gateway executable in standalone mode:

```bash
agentdesktop \
  serve \
  --mode standalone \
  --upstream http://127.0.0.1:15008 \
  --native-target native.agentdesktop.internal:4000 \
  --gateway-binary /usr/local/bin/agentgateway \
  --gateway-config "$HOME/.config/agentgateway/config.yaml"
```

With supervision enabled, the connector starts `agentgateway -f <config>`, waits for the configured upstream to accept TCP connections, stops the child during connector shutdown, and exits if the child exits unexpectedly. It does not install, update, or rewrite Agent Gateway.

The self-contained installer includes a hardened user-systemd unit and enables it by default. If installation used `--no-start`, activate it later or stop it before uninstalling with:

```bash
"$HOME/.local/lib/agentdesktop/bin/agentdesktop-install" \
  service enable --root "$HOME/.local/lib/agentdesktop"
"$HOME/.local/lib/agentdesktop/bin/agentdesktop-install" \
  service disable --root "$HOME/.local/lib/agentdesktop"
```

Enable validates the complete bundle integrity manifest before asking `systemctl --user` to enable and start the generated unit. Disable performs the same validation before stopping and disabling the named unit. Disable the service before uninstalling the bundle; install and uninstall do not implicitly alter the current user session.

Check connector and upstream reachability with:

```bash
curl --fail http://127.0.0.1:8081/_agentdesktop/healthz
```

This health endpoint checks TCP reachability. It does not verify provider credentials, policy correctness, or provider availability.

## Logs and retention

The connector writes lifecycle messages and fatal errors to standard output and standard error. It does not log request or response bodies. The process manager or container engine decides where those streams are stored and how long they are retained.

Agent Gateway controls request logging, policy audit data, and any AI-content-related diagnostics. Review its active configuration before sending sensitive traffic. Also review retention in the process manager, container engine, system journal, Agent Gateway telemetry destination, and backup system.

For a single-user installation:

1. Set an explicit retention policy for service and container logs.
2. Avoid debug logging during normal operation.
3. Treat destination hosts, model names, user identifiers, and policy decisions as potentially sensitive even when bodies are absent.
4. Remove temporary diagnostic output after an incident according to the user's retention policy.

## Data and removal

The connector does not create a persistent data directory. Removing its binary and service definition removes connector-owned state. Remove only configuration and logs that belong to this installation; do not delete user-owned Agent Gateway policy or audit data without explicit confirmation.

When the user explicitly enables inspection trust, the installer adds only the generated local Agent Gateway CA under its SHA-256 fingerprint. `agentdesktop trust remove` and uninstall-related flows remove only matching product-owned trust material and refuse modified anchors or active capture.

Before uninstalling:

1. Stop Claude Code and other configured applications.
2. Disable the installed user service, or stop independently managed connector and Agent Gateway processes.
3. Stop captured application scopes and remove inspection trust if it was installed.
4. Remove application base-URL configuration that points to the connector loopback endpoint.
5. Remove connector service definitions and binaries.
6. Decide separately whether to retain or delete Agent Gateway configuration, credentials, policy, logs, and audit records.
7. Confirm that no application still depends on the local gateway before removing Agent Gateway.

## Verification checklist

- Connector and Agent Gateway listeners are loopback-only.
- Agent Gateway configuration and referenced secret files are user-readable only.
- Provider credentials are available to Agent Gateway, not the connector or application.
- Agent Gateway policy allows intended local clients and denies invalid placeholders.
- Claude Code launched normally completes a request through the connector.
- Stopping Agent Gateway causes requests to fail without direct provider fallback.
- Log destinations and retention periods are known.
- Inspection trust is absent unless explicitly approved; when present, its fingerprint and removal path are known.
