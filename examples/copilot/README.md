# GitHub Copilot through Agentgateway

This example points the GitHub Copilot CLI and VS Code Copilot Chat at a local
Agentgateway through Agentdesktop, without a controller. It is also the test
guide for the Copilot programs: each step says what to check.

Agentdesktop runs a loopback LLM proxy (`daemon.llmProxy.listen`) and writes the
client files that point the tools at it. The proxy adds the developer's
identity (here a Dex OIDC token) to every request, and the gateway checks it.
Two paths go through the same gateway:

| Path | Clients | Models | Billed by |
| --- | --- | --- | --- |
| The gateway's own models (`llm` in `agentgateway.yaml`) | Copilot CLI (`programs.copilot`), VS Code on own models (`programs.vscode`) | The models the gateway serves | Your provider key |
| GitHub's models (`copilot-proxy` route) | VS Code on GitHub's models (`copilotChat: githubModels`) | GitHub's models, as your Copilot plan and policy allow | The developer's Copilot seat |

## Prerequisites

- Linux with Docker Compose (the example was run on Ubuntu 24.04; macOS and
  Windows paths are covered by unit tests only)
- `agentdesktop` on your `PATH` (see the repository README)
- An OpenAI API key for the gateway's own models
- For step 3: the Copilot CLI (`copilot`); for steps 4 and 5: VS Code with
  GitHub Copilot Chat. The example was run with both signed in to GitHub with a
  Copilot Business seat; without a sign-in it was not tried. Steps 3 and 4
  need the organisation's BYOK policy enabled (see
  [github-side.md](github-side.md)); step 5 needs a Copilot Business or
  Enterprise seat.
- A user account where no other Agentdesktop daemon runs or ran. A daemon
  that ran before left sidecar files next to the files it managed; this
  example's daemon removes what that earlier setup added (for example Claude
  Code or OpenCode entries). Stop the other daemon first, and expect to
  re-apply its configuration afterwards.
- Nothing else listening on 127.0.0.1:4001 (gateway), :4010 (the loopback
  proxy), :5557 (Dex) and :51327 (the login callback)

## 1. Start Dex and the gateway

From the repository root:

```sh
export OPENAI_API_KEY=sk-...
docker compose -f examples/copilot/compose.yaml up -d
curl -sI http://127.0.0.1:4001/ | head -1     # HTTP/1.1 200 OK
```

The gateway fetches Dex's keys at start and restarts until Dex answers: if the
check prints nothing, wait a few seconds and run it again
(`docker compose -f examples/copilot/compose.yaml ps` shows both services
`Up`). Dex has one user, `admin@example.com` with password `password`.

## 2. Run the daemon

```sh
agentdesktop daemon --config examples/copilot/config.yaml
```

`config.yaml` sets `daemon.user: true`, so `--user` is not needed. The daemon
stays in the foreground and logs there; run the checks below in a second
terminal, and stop it with Ctrl+C (later steps restart it this way). There is
no `--dry-run` preview for this example: the loopback proxy cannot run in a
one-shot run.

The first start prints `Open this URL to connect Agentdesktop:` with
`http://127.0.0.1:51327/` and opens a browser; sign in to Dex there. Without a
browser on this machine, open that URL in a browser on the same machine; over
ssh, forward ports 51327 and 5557 first. Later restarts reuse the login. Then:

```sh
agentdesktop --socket "$XDG_RUNTIME_DIR/agentdesktop.sock" status   # ok
curl -s --unix-socket "$XDG_RUNTIME_DIR/agentdesktop.sock" http://localhost/v1/daemon-info
```

`llmProxy.bound` is `true` and `listen` is `127.0.0.1:4010`. The daemon log has
`applied reconciliation change` lines for `~/.copilot/providers.json` and the VS
Code `chatLanguageModels.json` (`~/.config/Code/User/`). Both files are
owner-only (600) and carry a pairing value that the proxy requires on every
request; other local users and browser pages cannot use the proxy without it.
The daemon keeps a small sidecar next to each file
(`.providers.json.agentdesktop`, `.chatLanguageModels.json.agentdesktop`) that
records what it added, so it can take out exactly that later.

## 3. Copilot CLI on the gateway's models

```sh
copilot -p "Reply with exactly one word: PONG" --model agentdesktop/gpt-4.1
```

The answer comes from the gateway: its log (`docker compose -f
examples/copilot/compose.yaml logs agentgateway`) shows the request with
`user="admin@example.com"`. `~/.copilot/providers.json` holds the providers
`agentdesktop` (OpenAI-shaped) and `agentdesktop-anthropic`; your own providers
in that file are kept. The CLI reads the file at start: restart a running
session after the first apply.

## 4. VS Code on the gateway's models

Restart VS Code, open Copilot Chat and pick `gpt-4.1-mini (agentdesktop)` in the
model picker. A prompt answers through the gateway (same log line as above).
Your own entries in `chatLanguageModels.json` are kept.

## 5. VS Code on GitHub's models (Copilot Business or Enterprise)

Replace the `vscode` block in `config.yaml` with `copilotChat: githubModels`
(the comment there shows it) and restart the daemon (Ctrl+C, then the command
of step 2). Then:

- The user `settings.json` gains `github.copilot.advanced.debug.overrideCapiUrl`
  pointing at `http://127.0.0.1:4010/vscode-copilot-capi/<pairing>` and two
  entries in `settingsSync.ignoredSettings`, so Settings Sync does not carry the
  override to other machines. Nothing else in the file changes: comments, key
  order and formatting stay. The `agentdesktop` entry in
  `chatLanguageModels.json` is removed.
- Restart VS Code. The picker shows GitHub's models (Auto, and the models your
  organisation's policy enables). A chat turn answers; the gateway log shows
  `POST /copilot-proxy/chat/completions` with status 200 and the Dex user, and
  an agent-mode turn opens `GET /copilot-proxy/responses` with status 101 (the
  gateway logs it when the tunnel closes).

This step was run against Solo Enterprise for AgentGateway with the same route;
with the OSS gateway image of this example the route configuration is accepted
(`--validate-only`) but the step has not been run end to end yet. The route
targets `api.business.githubcopilot.com`. Other plans use another host, which
VS Code's Copilot Chat log shows (the `_ping` requests): change both
occurrences in `agentgateway.yaml` (`urlRewrite.authority.full` and the backend
`host`) and restart the gateway (`docker compose -f
examples/copilot/compose.yaml restart agentgateway`).

While the daemon is not running, or runs without the proxy under the default
`llmGateway.whenProxyUnavailable: failClosed`, the override stays and Copilot
Chat fails instead of reaching GitHub past the gateway; with `failOpen` a
daemon without the proxy removes it and VS Code uses GitHub directly. The
failing requests still carry VS Code's Copilot tokens to the loopback port:
use this on single-user machines, and switch back to own models (or stop VS
Code) before removing the daemon.

## 6. Drift, conflicts and removal

Each item starts from the `config.yaml` of step 4 (own models); undo each
change before the next.

- **Drift.** With `daemon.reconcileInterval` set (one minute in this example;
  the tick is off when it is left out), delete `~/.copilot/providers.json`:
  within one interval it is back with the same managed entries (your own
  entries were in the deleted file and do not come back). `chmod 644` on a
  managed file: back to 600. When nothing changed, the daemon writes nothing.
- **Conflict.** Put `// x` on the first line of `chatLanguageModels.json`. The
  daemon refuses to rewrite a file it cannot parse and logs `program
  configuration outcome` with `state="conflict"` and the path for `vscode`, and
  `state="blocked"` (`not applied: vscode conflicted`) for the other programs
  that had changes; it writes no managed file on the device until the comment
  is gone (VS Code's `settings.json` is the exception that accepts comments and
  trailing commas). Remove the comment: within one interval everything applies
  again (without the tick: at the next restart).
- **Proxy gone.** Remove `daemon.llmProxy` and restart: the log says
  `state="inactive"` with the reason, and the Copilot and VS Code entries stay
  (`llmGateway.whenProxyUnavailable: failClosed`, the default), so the CLI's
  `agentdesktop/gpt-4.1` fails instead of answering. Add
  `whenProxyUnavailable: failOpen` under `llmGateway` in `config.yaml` and
  restart: the entries are removed.
- **Removal.** Remove a program from `programs` and restart: its entries are
  taken out, your own entries stay, and a file the daemon created is deleted
  when nothing else is left.

## 7. With a controller

The same programs can be delivered by the controller instead of the local
file; see the managed quickstart in the repository README. The controller's
device page then lists each managed program with its outcome (applied,
unchanged, removed, conflict, inactive, blocked, failed), and `GET
/api/v1/devices/{id}` returns the same list. With a controller, the gateway
checks the controller's short-lived JWT (issuer `agentdesktop-controller`)
instead of Dex, and the proxy uses the client IDs `copilot-cli` and
`vscode-copilot`, which must be in `llmGateway.authentication.allowedClientIds`.

## 8. GitHub-side settings

The Copilot plan, the BYOK policy, model policies, default models and egress
rules are set by the GitHub organisation or enterprise owner; see
[github-side.md](github-side.md).

## 9. Clean up

Stop the daemon (Ctrl+C). To take the managed entries out, remove `programs`
from `config.yaml` and start the daemon once more (or delete the files if the
daemon created them). Then `docker compose -f examples/copilot/compose.yaml
down`, and remove `~/.local/state/agentdesktop` for a fresh login next time.
Dex keeps its data in memory, so after `down` and `up` the daemon needs a new
login.

## Known limits

- Inline completions and next edit suggestions go directly to GitHub; this
  example routes chat and agent mode only.
- `github.copilot.advanced.debug.overrideCapiUrl` is an undocumented VS Code
  setting; the fallback is VS Code's HTTP proxy setting pointed at the gateway.
- Tested on Linux with VS Code 1.139, Copilot CLI 1.0.88 and a Copilot Business
  seat: steps 1 to 4 and 6 with this example's OSS gateway; step 5 with Solo
  Enterprise for AgentGateway.
