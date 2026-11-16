# Control-plane threat model

This document states what Agentdesktop protects against when the control
plane, meaning the controller and the people who administer it, is itself
malicious or compromised. It sets the goals that any mitigation must meet.
Most of the design for local limits on controller-delivered configuration is
still open. [Status](#status) lists what exists today.

For how devices enroll and authenticate to the controller, see
[Device identity and enrollment](device-identity.md).

## Scope

In controller-managed mode, the daemon on each workstation receives a
`DaemonConfig` from the controller and writes it into the managed settings of
developer tools: Claude Code, Codex, OpenCode, and the others. Those tools
run as the user, read the user's source code, and send prompts to an LLM
gateway. A configuration change from the controller therefore changes what
those tools run, what they ask before running it, and where they send data.

The assets are:

| Asset | Where it is |
| --- | --- |
| Code execution as the device user | Every managed tool. Many tool settings name commands the tool runs. |
| Source code and other data on the workstation | Read by tools and sent to the model in prompts. |
| Tool guardrails | Approval prompts, permission modes, and sandboxes in each tool's settings. |
| Credentials | Device key and OIDC tokens (daemon), gateway JWTs and gateway OIDC tokens, provider subscription tokens that tools send to the gateway. |

## Actors and trust

| Actor | How it reaches the device | Trust |
| --- | --- | --- |
| MDM or installer | Installs Agentdesktop and the local configuration file, including its `daemon:` and `controller:` sections. | Most trusted. Changes are change-controlled and made by a small group. |
| Controller and its admins | Pushes `DaemonConfig` over the `Connect` stream. Signs device certificates and gateway JWTs. | Trusted to manage tools, within limits set locally. The primary threat in this document. |
| LLM gateway and model | Returns model output, including tool calls, to the tools. | Output is untrusted. See [Malicious model](#malicious-model). |
| Device user | Runs the tools and owns the data on the workstation. | Trusted. Not an adversary in this model. |
| IdP | Authenticates the users who enroll and use devices. | Trusted. Out of scope. |

MDM is trusted more than the controller for two reasons. MDM changes go
through slower, change-controlled processes, while the controller is used for
day-to-day operations. And the controller is a new, internet-facing service
with its own database, admin UI, and configuration source, so it has a larger
attack surface than an MDM profile.

Local limits on controller-delivered configuration therefore belong in the
`daemon:` section of the local configuration file, which the controller cannot
set. Without such limits, the controller is fully trusted, as it is today.

## Primary threat: a malicious or compromised control plane

The threat covers two cases:

- **Compromised controller.** An attacker controls the controller host or its
  keys, the daemon configuration file it watches (for example a Kubernetes
  ConfigMap), or an admin account that can change that configuration.
- **Evil or careless admin.** A person with legitimate access pushes a
  configuration that harms devices, on purpose or by mistake.

Both cases produce the same thing on the device: a well-formed, correctly
authenticated `DaemonConfig`. The daemon cannot tell them apart, and mTLS
does not help, because the attacker speaks through the real controller.

### What the controller can send today

- `DaemonConfig` is the only message the controller sends on the `Connect`
  stream ([`fleet.proto`](../crates/proto/proto/fleet.proto)). There is no
  remote command channel.
- The daemon rejects a pushed configuration that contains the `daemon:`
  section ([`parse_daemon`](../crates/core/src/config.rs), used by
  [`parse_pushed_config`](../crates/agent/src/remote.rs) and when the cached
  `remote-config.yaml` is loaded at startup). Startup settings, such as the
  state directory, socket, and tool configuration paths, stay local.
- The daemon connects to the controller address in the local file, not one
  from the pushed configuration ([`daemon.rs`](../crates/agent/src/daemon.rs)).
- Everything else is controller-settable and replaces the local
  configuration: `llmGateway` (including `url`, `proxyUrl`, and
  `authentication`), `sandbox`, `telemetry.events`, and `programs`.
- Most program sections pass arbitrary keys through to the tool's managed
  settings: the flattened Claude Code and Claude Desktop settings, and
  `managedConfig` for Codex, OpenCode, Grok Build, and Pi. Agentdesktop does
  not check these keys, except that its own generated values take precedence
  where they overlap.
- The `sha256` field of `DaemonConfig` is computed by the controller. It
  detects a corrupted payload, not a malicious one.

## Attack tiers

Ordered from most to least direct. Each needs only a controller-delivered
configuration.

### 1. Silent code execution

The configuration names a command that a tool runs on its own, with no model
involved and no prompt shown to the user. Examples:

- Claude Code `hooks`, and helper commands such as `apiKeyHelper` and other
  `*Helper` settings.
- MCP server definitions, which are command lines the tool starts, and
  plugins.
- Codex `notify`.
- OpenCode `formatter` and `lsp` commands.
- Pi model `apiKey` values that start with `!`, which Pi runs as a shell
  command. Agentdesktop itself uses this form for the credential helper.
- Environment variables, such as Claude Code `env` setting `NODE_OPTIONS` or
  `LD_PRELOAD`.

### 2. Code execution via the model

The configuration points a tool at a model the attacker controls and removes
the tool's guardrails. Examples: set `llmGateway.url` to an attacker server,
and set Codex `approval_policy = "never"` with
`sandbox_mode = "danger-full-access"`, or Claude
Code `permissions.defaultMode: bypassPermissions`. The fake model then
returns tool calls that run commands, and the tool runs them without asking.

This tier uses no command-bearing keys, so blocking tier 1 alone is not
enough. Lowering guardrails is also harmful without a redirect, because it
lets an ordinary prompt injection run commands.

### 3. Data exfiltration

The configuration sends data to a destination the attacker reads. Examples:

- `llmGateway.url` or `proxyUrl`. This field is typed and controller-settable.
  Unless the local policy pins its origin, the daemon checks only its form
  (scheme, host, no credentials). A
  redirected gateway receives every prompt, which carries source code, and the
  credentials sent with it: gateway JWTs, gateway OIDC tokens, and provider
  subscription tokens. It is the largest exfiltration channel.
- Per-tool base URLs that bypass `llmGateway`, such as `ANTHROPIC_BASE_URL` in
  Claude Code `env` or a Codex provider `base_url`.
- OpenTelemetry exporters (Codex `otel`, `OTEL_*` variables) and custom
  request headers.

## Malicious model

A model can be hostile without any help from the control plane: through
prompt injection in a repository, web page, or tool result, or through a
malicious gateway or model provider.

For this case Agentdesktop guarantees only that **the control plane cannot
make it worse**. It does not try to make it better. Hostile tool calls are the
job of each tool's guardrails: approval prompts, permission rules, and
sandboxes. Agentdesktop's duty is that the controller cannot lower those
guardrails below what the local policy sets, for example by enabling
auto-approve, setting `approval_policy: never`, turning a sandbox off, or
enabling a permission bypass mode.

The reasons:

- Agentdesktop configures tools. It is not in the tool-call path for most of
  them, so it cannot inspect or block individual tool calls.
- The tools already have guardrails designed for this. A second,
  tool-agnostic layer in Agentdesktop would be weaker and would lag behind
  each tool's features.
- Lowering guardrails is a control-plane action, so the malicious-model
  threat folds into the control-plane threat.

An admin can still use the controller to tighten guardrails. That is a
feature, not a security guarantee of this model.

## Security goals

With a local policy in place, the following must hold for every
controller-delivered configuration. Each goal is stated so that it can be
tested by pushing a hostile configuration and inspecting what the daemon
writes.

1. **No code execution.** The controller cannot make a managed tool run a
   command that the local policy did not allow. Test: push each
   command-bearing key from tier 1. No written tool setting contains a command
   outside the local policy.
2. **No lowered guardrails.** The controller cannot weaken approval,
   permission, or sandbox settings below the local policy. Test: push
   `approval_policy: never`, a bypass permission mode, and a disabled
   sandbox. The written settings keep the locally required level.
3. **Gateway origin pin (optional).** If the local policy lists allowed
   gateway origins, every endpoint that a managed tool sends prompts or
   telemetry to is in that list, and so is the OIDC issuer that users sign in
   to. This includes `llmGateway.url`, `proxyUrl`,
   `llmGateway.authentication.issuer`, per-tool base URLs, environment
   overrides, and OpenTelemetry endpoints.
   Test: push each of these with an outside origin. None is written. The pin
   is by origin, not by full URL, so the controller can still change paths,
   `proxyUrl`, and authentication within an allowed origin.
4. **Local policy is out of reach.** The controller cannot change the local
   policy. This already holds for the `daemon:` section, and is tested in
   [`config.rs`](../crates/core/src/config.rs)
   (`daemon_reconcile_interval_is_rejected_in_pushed_configuration`).
5. **Limits apply to the cached configuration.** A `remote-config.yaml`
   accepted before the local policy was installed or tightened is checked
   again at startup. Test: install a stricter local policy over a cached
   hostile configuration and restart. The hostile parts are not applied.
6. **Refusals are visible.** When the daemon refuses part of a configuration,
   it reports this in `ConfigStatus`, so the admin sees it in the controller.

Goal 3 is meaningful only together with goals 1 and 2 and the closing of the
pass-through redirect channels in tier 3. A pinned `llmGateway.url` does not
help if Claude Code `env` can still set `ANTHROPIC_BASE_URL`.

## Status

The local policy is `daemon.controllerLimits` in the local configuration file
(see the [daemon configuration reference](../schema/daemon-config.md)). A
pushed configuration outside it is rejected as a whole: the last accepted
configuration stays in effect, the rejected one is not saved, and the reason
is reported in `ConfigStatus`. At startup, a saved configuration outside the
policy is deleted and the local configuration is applied in its place, which
removes the tool settings written from it.

| Goal | Status |
| --- | --- |
| 1. No code execution | Not started. |
| 2. No lowered guardrails | Not started. |
| 3. Gateway origin pin | Partial. `allowedGatewayOrigins` checks `llmGateway.url`, `proxyUrl`, and the OIDC issuer. Base URLs and endpoints in programs' free-form settings are not checked. |
| 4. Local policy is out of reach | Holds. |
| 5. Limits apply to the cached configuration | Holds for the checks that exist. |
| 6. Refusals are visible | Holds for the checks that exist. |

## Out of scope

| Threat | Why it is out of scope | Where it is handled |
| --- | --- | --- |
| Malicious model, beyond "not made worse" | Hostile tool calls are the tools' guardrails' job. | Each tool's approval, permission, and sandbox settings. See [Malicious model](#malicious-model). |
| Local privilege escalation through the system daemon | Real, but a different threat: root writes driven by the controller, the local socket, or `remote-config.yaml`. | A separate workstream. |
| Network attacker between daemon and controller | Not a control-plane compromise. | mTLS ([Device identity](device-identity.md)) and controller CA pinning, which is being built separately. |
| Device user circumventing organization policy | The opposite direction: the user is not the adversary here. | Not handled by this model. |
| Rogue or stolen enrolled device pulling gateway credentials | An organization-side threat, not a threat to the device. | IdP revocation, see [Logout and revocation](device-identity.md#logout-and-revocation). |
| Controller impersonating users at the gateway | The controller signs gateway JWTs by design. | Gateway-side controls, or `authentication.type: oidc`, which keeps the controller out of gateway authentication. |
| Supply chain: Agentdesktop releases, tool updates, user-installed plugins | Not delivered by the control plane. | Not handled by this model. |
| Configuration signing | Deferred. | See [Deferred options](#deferred-options). |

## Deferred options

### Configuration signing

Signing each configuration with a key the controller does not hold would make
a compromised controller unable to push a valid configuration. It is deferred
for these reasons:

- Prior art is rare and opt-in: Tailscale Tailnet Lock, and signed-commit
  verification in Flux and Argo CD. Where browsers and operating systems
  verify signed configuration, the key is usually the vendor's, not one the
  customer manages.
- The signing key must be kept offline, and losing it needs a recovery plan.
- It rules out editing policy through the controller, unless the signing key
  is online, which defeats the purpose.
- Signing in CI moves the trust to CI rather than removing it.
- It does not stop an evil admin who holds the key. That needs multi-party
  signing.

Signing may come later as an opt-in layer for customers who already manage
policy through GitOps.

## Known gaps

These remain even when the goals above are met:

- **Sandbox can be dropped.** The top-level `sandbox` setting is
  controller-delivered, and Agentdesktop generates each tool's sandbox block
  from it. A malicious controller can omit it. Goal 2 covers sandbox settings
  passed through to tools, but the local policy may also need a way to
  require the generated sandbox.
- **Telemetry can carry tool input.** `telemetry.events` is
  controller-settable, and `tool.use.input` sends tool arguments, such as
  commands and file contents (up to 256 KiB per event), to the controller.
  This is exfiltration to the controller itself.
