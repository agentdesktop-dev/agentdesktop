# GitHub Copilot: what the GitHub organisation or enterprise owner configures

Agentdesktop points the Copilot CLI and VS Code Copilot Chat at the local LLM proxy, and the proxy sends their traffic through the gateway with the developer's identity attached. Some settings that decide whether that traffic flows, and whether developers can go around it, live on the GitHub side and belong to your GitHub organisation or enterprise owner and your network team. This page lists them per client mode.

## The two VS Code modes and the Copilot CLI

| | Copilot CLI (`programs.copilot`) | VS Code, own models (`copilotChat: ownModels`) | VS Code, GitHub's models (`copilotChat: githubModels`) |
|---|---|---|---|
| Models | The gateway's models | The gateway's models | GitHub's models, as the Copilot plan and model policy allow |
| Billed by | Your model provider, per token | Your model provider, per token | The Copilot seat (included AI credits, then metered) |
| GitHub BYOK policy | Not verified whether it applies | Must stay enabled | Not needed |
| Gateway sees | The user; with a controller also device and client; full request | The user; with a controller also device and client; full request | The user; with a controller also device and client; the request as VS Code sends it to GitHub |

## Settings on the GitHub side

| Item | What to set | Applies to | Verified |
|---|---|---|---|
| Copilot plan | Business or Enterprise seats. BYOK is admin-controllable only on these plans. | all | Business seat, tested |
| BYOK policy | "Bring Your Own Language Model Key in select IDEs" must stay enabled, otherwise VS Code's custom endpoint is switched off for every seat. Whether the Copilot CLI's BYOK follows the same policy is not verified. | own models, CLI | GitHub docs |
| Model availability policy (optional, enforcement) | Hosted chat models "Disabled for every user and agent app" leaves only the gateway models in the VS Code picker, where GitHub's models are greyed out with "Contact your admin". The CLI still offered gpt-4.1 in the lab with the enterprise's default availability disabled; the base model's own status is undocumented. Do not use with `githubModels`, which needs GitHub's models enabled. | own models, CLI | tested |
| CLI default model | Managed settings `{"model": "agentdesktop/<model>"}`: Linux `/etc/github-copilot/managed-settings.json`, macOS `/Library/Application Support/GitHubCopilot/`, Windows `%ProgramFiles%\GitHubCopilot\`. Precedence: native MDM, then server-managed (`.github-private` repository), then the file. A default only: the user's own `config.json` overrides it. Managed settings have no provider, URL or credential keys. | CLI | GitHub docs, tested |
| VS Code default model | `chat.defaultModel` (also the `ChatDefaultModel` enterprise policy), for example `customendpoint/agentdesktop/gpt-4.1-mini`. A default only. Agentdesktop does not write it. | own models | GitHub docs, tested |
| Egress deny (optional, enforcement) | Deny `api.githubcopilot.com`, `api.{individual,business,enterprise}.githubcopilot.com` and the model providers' hosts from developer machines; allow the gateway, `api.github.com`, and the completion hosts `proxy.<plan>.githubcopilot.com` and `copilot-proxy.githubusercontent.com`. With `githubModels`, the gateway (not the device) reaches `api.<plan>.githubcopilot.com`, so the deny on devices still holds. Roll out only after the fleet view shows every device configured. | all | GitHub docs, tested |
| Seat-side observability | The Copilot usage metrics API stays the source for completions and hosted-model use; the gateway sees the traffic routed through it. | all | GitHub docs |
| Not needed | Enterprise custom models ("Editor preview features" policy). | | |

## What each mode leaves outside the gateway

- **Inline completions and next edit suggestions** go straight to `proxy.<plan>.githubcopilot.com` in every mode; Agentdesktop does not route them.
- **GitHub's models mode** relies on `github.copilot.advanced.debug.overrideCapiUrl`, an undocumented Copilot Chat setting. If a VS Code release drops it, the fallback is VS Code's HTTP proxy setting with the gateway as the proxy.
- **Defaults are not enforcement.** A developer can change the default model or remove the managed file entries; only the model policy and egress rules above make the gateway path non-optional.

## Not verified

- Whether the Copilot CLI's BYOK is gated by the VS Code BYOK policy.
- The Enterprise Cloud host (`api.enterprise.githubcopilot.com`) for `githubModels`; tested with a Business seat only.
- macOS and Windows (firewall prompts for the loopback listener, paths beyond unit tests); corporate proxies (`NO_PROXY` for 127.0.0.1).
