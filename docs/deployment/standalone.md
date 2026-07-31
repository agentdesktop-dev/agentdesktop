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

The connector listener is restricted to loopback. The current Agent Gateway `llm.port` schema accepts only a port and binds a wildcard address; it cannot yet express a loopback address. The QEMU journey is isolated behind QEMU user-mode NAT, but a public host installation requires an address-capable Agent Gateway listener or equivalent local-only transport before this package can claim local-only exposure. Host firewall policy is not a substitute for making that default explicit in the product.

Embedded components are compressed independently and verified while extracting. Installation uses a sibling staging tree and atomic activation; upgrades validate the existing manifest and restore the prior bundle if activation fails. This payload integrity check does not replace publisher signing, which remains required before public distribution.

## Ownership

Agent Gateway owns:

- Provider credentials and provider connections.
- AI routing, authorization, rate limits, guardrails, and content inspection.
- Request-level logs, audit records, and their retention.
- Any future TLS inspection configuration and issuing CA keys.

The connector owns:

- Its per-user loopback listener.
- Forwarding application HTTP traffic to Agent Gateway without interpreting AI bodies.
- Optional lifecycle management for a separately installed Agent Gateway executable.
- Connector health and fail-closed application errors.

The connector has no policy format, provider credential store, request database, or content log.

## Files and permissions

Keep the Agent Gateway configuration in a directory accessible only to the user running Agent Gateway. On Unix-like systems, a suitable baseline is:

```bash
install -d -m 0700 "$HOME/.config/agentgateway"
install -m 0600 config.yaml "$HOME/.config/agentgateway/config.yaml"
```

Apply the same restrictions to files referenced by the Agent Gateway configuration. Do not make configuration or credential files readable by other local users. Backups inherit the same sensitivity and should be encrypted and access-controlled.

The connector reads only its own command-line or environment configuration. When connector configuration is stored in a service definition, restrict that file to the user account running the connector.

## Provider credentials

Supply provider credentials only to Agent Gateway using an Agent Gateway-supported secret source. The repository's real-provider container example expands `ANTHROPIC_API_KEY` inside Agent Gateway configuration and passes that environment variable only to the Agent Gateway container.

Do not put a provider key in:

- Claude Code configuration.
- Connector arguments or environment variables.
- Connector service definitions.

The Claude adapter writes only a local placeholder accepted by Agent Gateway policy. Agent Gateway must supply the real provider credential and prevent the application placeholder from reaching the provider.

Environment variables can be visible to the process owner and privileged diagnostic tools. Prefer the platform secret facility supported by the selected Agent Gateway deployment when one is available. Credential rotation is an Agent Gateway and provider operation; the connector does not cache provider credentials.

## Local endpoints

Run both application-facing endpoints on loopback:

- Agent Gateway native path: `127.0.0.1:4000` by default.
- Connector-assisted path: `127.0.0.1:8080` by default.

Standalone connector validation rejects a non-loopback Agent Gateway upstream and always rejects a non-loopback connector listener. Configure Agent Gateway's listener and administrative or readiness endpoints as local-only for a host installation. Container-only examples may bind readiness endpoints more broadly inside an isolated container network; do not copy that exposure to a host without an explicit access-control decision.

A loopback listener is reachable by other processes running as the same user and may be reachable by other local users depending on operating-system controls. The example Agent Gateway authorization rule protects the mock workflow, but production policy belongs in the user's Agent Gateway configuration.

## Connect Claude Code

The guided installer detects Claude Code after Agent Desktop becomes ready and asks for separate consent before changing it. If accepted, it adds the connector endpoint and local placeholder credential to `~/.claude/settings.json` while preserving unrelated settings. It refuses to replace an existing provider or gateway configuration.

Run the same setup manually with:

```bash
agentdesktop connect-agents
```

The installer owns `~/.local/bin/agentdesktop` as a stable link to the private bundle. It does not edit shell startup files or add directories to `PATH`; environments that do not already include `~/.local/bin` receive an installer warning. The command can be rerun at any time without reinstalling Agent Desktop. Matching settings are reported as already connected; conflicting provider or gateway settings are left unchanged. After connection, launch `claude` normally. Claude Code applies the user settings to terminal and IDE sessions, so Agent Desktop does not install or require a Claude-specific launcher. Requests fail when Agent Gateway is unavailable and do not fall back to Anthropic directly.

Application configuration is routed rather than enforced: a user who can change application settings can bypass it. Enforced routing requires later transparent capture and, where necessary, host firewall or MDM controls.

Do not configure the same application for both a native route and future transparent capture. The current milestone does not implement transparent capture.

## Process lifecycle

Agent Gateway may be started independently, or the connector may supervise a separately installed executable:

```bash
agentdesktop \
  serve \
  --mode standalone \
  --upstream http://127.0.0.1:4000 \
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
curl --fail http://127.0.0.1:8080/_agentdesktop/healthz
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

The current standalone milestone does not install a CA certificate or modify system or application trust. There is therefore no connector-installed trust material to remove. Future transparent inspection must add informed, idempotent trust installation and scoped removal before it is supported.

Before uninstalling:

1. Stop Claude Code and other configured applications.
2. Disable the installed user service, or stop independently managed connector and Agent Gateway processes.
3. Remove application base-URL configuration that points to either loopback endpoint.
4. Remove connector service definitions and binaries.
5. Decide separately whether to retain or delete Agent Gateway configuration, credentials, policy, logs, and audit records.
6. Confirm that no application still depends on the local gateway before removing Agent Gateway.

## Verification checklist

- Connector and Agent Gateway listeners are loopback-only.
- Agent Gateway configuration and referenced secret files are user-readable only.
- Provider credentials are available to Agent Gateway, not the connector or application.
- Agent Gateway policy allows intended local clients and denies invalid placeholders.
- Claude Code launched normally completes a request through the connector.
- Stopping Agent Gateway causes requests to fail without direct provider fallback.
- Log destinations and retention periods are known.
- No CA or trust-store changes are expected for this milestone.
