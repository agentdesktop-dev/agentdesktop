# Goose support

Agentdesktop discovers the Goose CLI and can manage a Goose custom provider that sends inference requests through the configured LLM gateway. This integration targets the current declarative custom-provider format in Goose 1.49.

## Install Goose

Follow the [official Goose installation guide](https://block.github.io/goose/docs/getting-started/installation/). For the `postaguest1` validation VM, Goose was installed without running the interactive configurator:

```sh
ssh postaguest1 'curl -fsSL https://github.com/aaif-goose/goose/releases/download/stable/download_cli.sh | CONFIGURE=false bash'
ssh postaguest1 '/Users/ceposta/.local/bin/goose --version'
```

The installer placed Goose at `~/.local/bin/goose` in the guest. Add `~/.local/bin` to `PATH` if it is not already present.

## What is a Goose provider?

A provider is Goose's adapter and configuration for an LLM service. It tells Goose which API protocol to use, where to send inference requests, how to authenticate, which models are available, and whether features such as streaming are supported.

Goose includes providers for well-known services. It also supports declarative custom providers defined by JSON files. Agentdesktop generates a custom provider named `agentdesktop` with:

- The OpenAI-compatible API engine.
- The configured Agentdesktop LLM-gateway URL.
- The selected model ID.
- Streaming chat-completions support.
- A command that obtains a short-lived gateway credential from Agentdesktop.

The generated Goose YAML selects that provider and model:

```yaml
active_provider: agentdesktop
providers:
  agentdesktop:
    configured: true
    enabled: true
    model: company-model
```

When Goose handles a prompt, the resulting flow is:

```text
Goose
  -> runs `agentdesktop credential --client-id goose`
  -> receives a short-lived credential on standard output
  -> sends an authenticated OpenAI-compatible request to the LLM gateway
  -> streams the gateway response back to the user
```

The credential is not stored in the generated Goose files. Goose invokes the command again when it needs to refresh the credential.

## Why user mode is required

Goose loads declarative custom-provider files from the current user's configuration directory:

```text
~/.config/goose/custom_providers/
```

Goose can read general machine-wide settings from `/etc/goose/config.yaml`, but it does not discover custom-provider JSON files from `/etc/goose/custom_providers`. A system daemon could therefore write an `active_provider: agentdesktop` setting while leaving Goose unable to find the corresponding provider definition.

The system daemon also has no single authoritative user to configure. It runs outside an interactive login session, its home directory belongs to the service account or root, and a machine may have zero, one, or several logged-in or configured users. Although a privileged daemon could technically enumerate user profiles and write into them, it would need an explicit targeting policy plus careful file ownership, permission, symlink, multi-user, and cleanup handling. The current implementation intentionally does not guess.

With `--user`, the target is unambiguous: Agentdesktop uses the current process user's home and XDG/AppData configuration directories, writes files owned by that user, and configures the same Goose installation that user invokes.

Using Goose's built-in OpenAI provider might be enough for a simple endpoint with a long-lived static API key. It does not provide this integration's command-based retrieval and refresh of short-lived controller JWT or OIDC credentials. The custom provider is what enables that enterprise authentication flow.

## Configure Agentdesktop

Goose custom providers are user scoped, so run Agentdesktop with `--user` when `programs.goose` is present. The Agentdesktop daemon and Goose must run as the same OS user because Goose loads custom providers from that user's configuration directory. A system-mode daemon rejects Goose configuration with a clear error instead of writing files that Goose will not load.

This means an existing machine-wide Agentdesktop service cannot manage Goose. It can continue managing the other supported harnesses, while Goose uses a user-mode Agentdesktop daemon. Supporting Goose directly from the system service would require safe per-user reconciliation or a system-level custom-provider location in Goose.

For developer laptops, one primary interactive user per workstation is typical, so the user-scoped model is usually a natural fit. Shared labs, jump hosts, VDI/RDS systems, and similar environments may require one managed user daemon per Goose user.

MDM can manage this deployment pattern. For example, macOS can install the binary once and launch `agentdesktop daemon --user` with a `LaunchAgent`; Windows management can use a user-context installation or per-user scheduled task. This removes the need for users to start the daemon manually. Each user daemon currently has its own state directory and may require its own enrollment. A future packaging integration could automate that lifecycle or securely delegate the machine service's identity.

```yaml
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [goose]

programs:
  goose:
    model: company-model
    managedConfig:
      GOOSE_MODE: smart_approve
      GOOSE_TELEMETRY_ENABLED: false
```

Then start the daemon:

```sh
agentdesktop daemon --user --config /path/to/config.yaml
```

`model` is required when Goose uses the top-level `llmGateway`. `useLlmGateway` defaults to `true`; set it to `false` to manage only values under `managedConfig`.

Agentdesktop writes:

- `~/.config/goose/config.yaml`
- `~/.config/goose/custom_providers/agentdesktop.json`
- `~/.config/goose/custom_providers/.agentdesktop.json.owner`

On Windows, the corresponding root is `%APPDATA%\Block\goose\config`.

The provider definition contains the gateway URL, selected model, and an Agentdesktop credential-helper command. It does not persist the short-lived gateway credential. Goose refreshes that credential by invoking:

```sh
agentdesktop --socket <daemon-socket> credential --client-id goose
```

Agentdesktop owns the complete generated Goose YAML and its `agentdesktop.json` provider. If either destination already contains an unowned file, reconciliation stops and leaves it unchanged. Preserve any settings you need under `managedConfig` and intentionally relocate the existing file before retrying. Removing `programs.goose` removes only files marked as owned by Agentdesktop.

Goose is not currently compatible with Agentdesktop's sandbox configuration. A configuration containing both `sandbox` and `programs.goose` is rejected.

## Verify

There are two ways to test the integration:

- For a standalone guest test, copy a newly built Agentdesktop binary to the guest and run it with a local configuration and `daemon --user`. The controller does not participate, so the existing controller does not need to be rebuilt or restarted.
- For a controller-managed fleet test, rebuild and restart the controller so it understands `programs.goose` and serves the updated configuration UI. Also deploy the new Agentdesktop binary to the guest and run its daemon with `--user`. A clean controller reset is not required: preserve its database, certificates, and enrollment state. If the guest previously used only a system daemon, its new user-mode daemon uses a different default state directory and may need to be enrolled once for that user.

Check that the daemon discovers Goose:

```sh
agentdesktop discover
```

Inspect the effective Goose provider and model:

```sh
goose info --verbose
```

Run a headless prompt through the configured gateway:

```sh
goose run --no-session --quiet --text 'Reply with exactly GOOSE_OK'
```

Discovery reports the Goose executable and version without executing it. It also reports secret-free metadata for configured `stdio` and `streamable_http` MCP extensions and discovers skills from Goose's global and project skill directories.

## Validation record

Validated on 2026-09-03 in `postaguest1` (Intel macOS 15.7.9) with Goose 1.49.0:

1. Agentdesktop discovered `/Users/ceposta/.local/bin/goose` as `goose 1.49.0`.
2. User-mode reconciliation generated an isolated Goose YAML and `agentdesktop` custom-provider definition.
3. `goose info --verbose` selected provider `agentdesktop` and model `test-model`.
4. `goose run --no-session --no-profile --quiet` made a streaming OpenAI chat-completions request to a mock bound to `127.0.0.1` inside the guest and returned `GOOSE_GUEST_OK`.
5. The mock observed model `test-model`, `stream: true`, and the submitted prompt.

The mock, temporary Agentdesktop binary, temporary configuration, and test processes were removed afterward. Goose remains installed only in `postaguest1`; the guest's normal `~/.config/goose/config.yaml` was not created or changed by the validation.
