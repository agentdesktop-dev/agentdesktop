# Controller limits lab

This scenario shows a device refusing configuration from a compromised
controller. The device's local configuration, which MDM would install, sets
`daemon.controllerLimits.allowedGatewayOrigins`. The controller then pushes a
policy that points the LLM gateway at another origin, and the device reports
it as **Rejected** instead of applying it.
[The control-plane threat model](../../docs/threat-model.md) explains why the
limit is local.

The lab runs Dex, the controller, and one headless device in Docker. The
device enrolls unattended with an
[enrollment token](../../docs/device-identity.md#enrollment-token), so there is
nothing to sign in to. Nothing listens at the gateway URLs: the lab only shows
what the device accepts and writes. The device manages Claude Code, so the
gateway in effect is visible in
`/etc/claude-code/managed-settings.d/50-agentdesktop.json`.

The only host listener is the controller UI at <http://127.0.0.1:18090>.

## Start

From the repository root, point `docker compose` at the lab and start it:

```console
export COMPOSE_FILE="$PWD/examples/controller-limits/compose.yaml"
docker compose up -d --build
```

Every `docker compose` command below then works from any directory in that
shell. In a new terminal, run the `export` line again from the repository root.

The first build compiles the controller and the headless daemon, and takes a
while. To rebuild the device faster while changing the daemon, run
`docker compose build --build-arg BUILD_PROFILE=debug device`. Keep the
controller a release build: a debug build reads its UI from disk at runtime, so
its image serves the API but not the UI.

Open <http://127.0.0.1:18090>. The device `limits-device` shows **Applied**.
Its Claude Code settings point at the allowed gateway:

```console
docker compose exec device cat /etc/claude-code/managed-settings.d/50-agentdesktop.json
```

`ANTHROPIC_BASE_URL` is `https://gateway.example.com/anthropic`.

The commands below use `lab`, a helper that switches the controller's policy
and the device's local configuration between the files in
[`policies/`](policies/) and [`device/`](device/):

```console
alias lab='docker compose run --rm lab'
```

## A push outside the limits

Point the controller's policy at another origin:

```console
lab policy outside
```

The controller pushes the change within a second. The device page shows
**Rejected** with the reason:

```text
configuration is outside the local limits: llmGateway.url origin
https://other.example.com is not in daemon.controllerLimits.allowedGatewayOrigins
```

The overview counts the device under **Config failures**. The device keeps the
configuration it accepted before, so the Claude Code settings still point at
`https://gateway.example.com/anthropic`, and **Managed programs** still lists
Claude Code, under a note that the revision was not applied.

Restore the policy, and the device shows **Applied** again:

```console
lab policy allowed
```

## Limits installed after the fact

A device that already accepted a bad policy, before the limits were installed,
does not apply that policy again once they are.

1. Remove the limits, restart the device, and push the policy outside them.
   With no limits the device trusts the controller and applies it:

   ```console
   lab limits off && docker compose restart device
   lab policy outside
   ```

   The device shows **Applied**, and `ANTHROPIC_BASE_URL` is
   `https://other.example.com/anthropic`.

2. Install the limits, as an MDM rollout would. The daemon reads them at
   startup:

   ```console
   lab limits on && docker compose restart device
   ```

   At startup the saved controller configuration is outside the limits, so the
   daemon applies its local configuration instead. That configuration manages
   no programs, so the Claude Code settings file is removed (fail closed). When
   the controller pushes the same policy on reconnect, the device reports
   **Rejected**, and **Managed programs** shows Claude Code as **Removed**. The log says why:

   ```console
   docker compose exec device sh -c 'grep -h "outside the local limits" /var/lib/agentdesktop/logs/*.log'
   ```

3. The saved configuration is kept, so reverting the limits restores it, even
   while the controller is unreachable:

   ```console
   docker compose stop controller
   lab limits off && docker compose restart device
   docker compose exec device cat /etc/claude-code/managed-settings.d/50-agentdesktop.json
   ```

   `ANTHROPIC_BASE_URL` is `https://other.example.com/anthropic` again.

4. Return to the starting state:

   ```console
   lab policy allowed
   lab limits on
   docker compose start controller
   docker compose up -d --force-recreate controller-ui
   docker compose restart device
   ```

   `controller-ui` shares the controller's network namespace, so it is
   recreated to reach the controller again. Do the same whenever the
   controller restarts or is rebuilt.

## Explore

- The device's local configuration and its saved controller configuration:

  ```console
  docker compose exec device cat /demo/agentdesktop.yaml
  docker compose exec device cat /var/lib/agentdesktop/remote-config.yaml
  ```

- The daemon's log:

  ```console
  docker compose exec device sh -c 'tail -f /var/lib/agentdesktop/logs/*.log'
  ```

- The limit also covers `llmGateway.proxyUrl` and, with
  `authentication.type: oidc`, the OIDC `issuer`. It does not cover URLs in
  programs' free-form settings, such as Claude Code `env`. To try your own
  policy, add it to [`policies/`](policies/), for example `policies/mine.yaml`,
  and run `lab policy mine`. A policy the controller cannot parse is not pushed;
  the controller logs the error and keeps the previous one.

## Reset

Stop the scenario and delete all of its state:

```console
docker compose down --volumes
```

## How it works

- `setup` creates the lab's keys with
  [`create-keys.sh`](../claude/create-keys.sh), an Ed25519 enrollment issuer
  key, and an enrollment token signed with it. It also copies the starting
  policy and local configuration into their volumes.
- The controller trusts the issuer key (`enrollmentTokens` in
  [`controller.yaml`](controller.yaml)) and watches `/policy/policy.yaml`. It
  pushes every valid change to connected devices. The revision stays at 1, as
  set in `controller.yaml`, so the badge always shows `r1`.
- The device reads its local configuration from `/demo/agentdesktop.yaml` and
  enrolls with `/keys/enrollment-token`. It reports each push's outcome to the
  controller: `applied`, `failed` (invalid configuration), or `rejected`
  (refused by `controllerLimits`).
- `lab.sh` writes the new file next to the old one and renames it, so the
  controller sees one complete change.
- The controller's management UI listens only on loopback. The `controller-ui`
  service joins its network namespace and forwards port 18090, as in the
  [enrollment lab](../enrollment/README.md#how-it-works).
