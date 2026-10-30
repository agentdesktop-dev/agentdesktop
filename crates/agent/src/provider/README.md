# Provider support

Features currently implemented by agentdesktop.

A fleet policy can include programs for both daemon modes. Programs the current
mode cannot manage are skipped and reported as **inactive**, with a reason in
configuration status and dry-run output. Supported programs continue applying.
Skipped programs' files and ownership metadata are left untouched, even when
those programs are removed from the policy. Cleanup runs in the mode that manages
the program. Claude Desktop and Grok Build require system mode; Pi, Copilot CLI,
and VS Code require user mode.

VS Code managed configuration (Copilot Chat custom models in the user's
`chatLanguageModels.json`, or the Copilot Chat API override in the user's
`settings.json` under `copilotChat: githubModels`) and GitHub Copilot CLI
managed configuration (the user's `providers.json`) require user mode; all of
these files point at the daemon's loopback LLM proxy. The Copilot CLI's MCP and skills inventory under
`~/.copilot` is read by the VS Code adapter.
Grok Build managed configuration requires system mode. Linux and macOS use
`/etc/grok/managed_config.toml`; Windows uses the system drive's
`\etc\grok\managed_config.toml`. User daemons skip `programs.grok` because
Grok's own configuration sync can delete or replace the user-level managed
file.

Pi stores configuration in `~/.pi/agent` (or `PI_CODING_AGENT_DIR`, with `~`
expanded to the user's home). Management requires `--user`; system daemons
skip `programs.pi` because Pi does not read `/etc/pi/agent`. In user mode,
authenticated gateway configuration also requires a running daemon and
rejects `--once`.
Pi executes its credential helper on every request. The daemon caches
controller-issued credentials for up to 60 seconds per allowed client,
gateway, audience, and enrolled device, with a 30-second expiry margin and
single-flight fetches. OIDC and subscription flows retain their own refresh
behavior. This removes repeated controller round trips; Pi still launches
the helper for each request.
User mode merges `providers.agentdesktop` into `models.json` and sets the
default model in `settings.json`. Model map keys determine catalog IDs, and
gateway URLs override per-model endpoints, using each model's API dialect.
The gateway owns the entire `providers.agentdesktop.models` catalog while
enabled; stale entries are replaced, and the prior catalog is restored on
cleanup. Other providers and user additions are preserved. On Unix,
`models.json` is written with owner-only permissions (`0600`), and
reconciliation repairs a looser mode on the managed file; cleanup keeps the
file's current mode. Dry runs report the `models.json` action and path
without its contents, which may include the user's own API keys.
Commented `models.json` files are accepted and rewritten as standard JSON,
preserving user configuration values. `settings.json` requires standard JSON.
Discovery recognizes npm's Windows command shims as well as symlinked launchers,
verifying the Pi package manifest without executing the launcher.
MCP inventory defaults to Pi's built-in user `mcp.json` and the working
directory's `.pi/mcp.json` only when saved project trust (or
`defaultProjectTrust: always`) permits it. Session-only trust cannot be
inferred. Disabling `builtin:mcp` excludes these sources. When an enabled,
installed `pi-mcp-adapter` is detected in Pi's package or extension settings,
or its auto-loaded extension directories, discovery instead uses adapter files
(`~/.pi/agent/mcp.json`, the working directory's `.pi/mcp.json` and
`.mcp.json`, and shared `~/.config/mcp/mcp.json` / `~/.agents/mcp.json` /
`~/.agents/mcp/mcp.json`). Ancestor project files are included only when a
user-global config sets `settings.ancestorConfigRoots` to an existing
directory under the home that contains the working directory. Both `.agents`
paths are supported by the [adapter's configuration loader](https://github.com/nicobailon/pi-mcp-adapter/blob/33bdc38d8dd3802f2b51ba1fe37e30ed7ae6a29a/config.ts#L15-L19).
`PI_MCP_CONFIG_MODE=exclusive` applies only to the adapter and inventories
only the Pi agent `mcp.json`.
Skill inventory reads `~/.pi/agent/skills`, `~/.agents/skills`, the working
directory's `.pi/skills`, and `.agents/skills` from the working directory
through the git repository root.
Host-specific Cursor/Claude
files are not scanned until the adapter imports them into a Pi-owned file.

✅ Implemented · ◯ Not implemented · — Not applicable

| Feature | [Claude Code](claude_code/) | [Claude Desktop](claude_desktop/) | [Codex](codex/) | [OpenCode](opencode/) | [VS Code](vscode/) | [Ollama](ollama/) | [Cursor](cursor/) | [Grok Build](grok/) | [GitHub Copilot CLI](copilot/) | [Pi](pi/) |
| --- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| Installation and version discovery | ✅ | ✅ | ✅ | ✅ | ✅ | — | ✅ | ✅ | ✅ | ✅ |
| Local model discovery | — | — | — | — | — | ✅ | — | — | — | — |
| MCP server discovery | ✅ | ✅ | ✅ | ◯ | ✅ | — | ✅ | ✅ | — | ✅ |
| Skill discovery | ✅ | ◯ | ✅ | ◯ | ✅ | — | ✅ | ✅ | — | ✅ |
| Managed configuration | ✅ | ✅ | ✅ | ✅ | ✅ | ◯ | ◯ | ✅ | ✅ | ✅ |
| LLM gateway routing | ✅ | ✅ | ✅ | ✅ | ✅ | — | ◯ | ✅ | ✅ | ✅ |
| Gateway credentials | ✅ | ✅ | ✅ | ✅ | ✅ | — | ◯ | ✅ | ✅ | ✅ |
| Sandbox configuration | ✅ | ◯ | ✅ | ◯ | ◯ | — | ◯ | ◯ | ◯ | ◯ |
| Tool-use telemetry | ✅ | ◯ | ◯ | ◯ | ◯ | — | ◯ | ◯ | ◯ | ◯ |
| Session-start telemetry | ✅ | ◯ | ◯ | ◯ | ◯ | — | ◯ | ◯ | ◯ | ◯ |
| Sandboxed launch (Windows) | ✅ | ◯ | ◯ | ◯ | ◯ | — | ◯ | ◯ | ◯ | ◯ |

- ◯ describes a gap in agentdesktop; it does not mean the upstream provider
  cannot support the feature.
- Managed configuration includes dry runs and removal of agentdesktop-owned
  settings (no dry run for the GitHub Copilot CLI providers file and the VS
  Code chat language models and settings files: their loopback proxy cannot
  run in one-shot mode). Claude Code also supports merging into user settings; Claude Desktop
  requires system-managed settings.
- Discovery is best effort; versions may be unavailable. Ollama discovers models
  through its running local API. Grok Build versions require its optional
  `version.json` update cache; fresh installs may have no discovered version.

See [provider integration tests](../../tests/README.md) for the Docker suite.
