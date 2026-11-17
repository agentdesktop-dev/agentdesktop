# Provider support

Features currently implemented by agentdesktop.

A fleet policy can include programs for both daemon modes. Programs the current
mode cannot manage are skipped and reported as **inactive**, with a reason in
configuration status and dry-run output. Supported programs continue applying.
Skipped programs' files and ownership metadata are left untouched, even when
those programs are removed from the policy. Cleanup runs in the mode that manages
the program. Claude Desktop and Grok Build require system mode; Pi, Muse Code,
Copilot CLI, and VS Code require user mode.

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

Muse Code stores configuration in `$XDG_CONFIG_HOME/muse` (or `~/.config/muse`).
Management requires `--user`; system daemons skip `programs.muse` because
Muse Code reads `endpoint_transport` credentials only from the user's
`settings.json`. In user mode, authenticated gateway configuration also
requires a running daemon and rejects `--once`.
User mode merges `programs.muse.managedConfig` and `model` into
`settings.json`. With an LLM gateway, the daemon owns the whole
`endpoint_transport` object: `base_url` is the gateway URL ending in `/v1`,
and `auth: "command"` with a `bearer_command` runs the agentdesktop
credential helper (an argument list, not a shell command), or `auth: "none"`
when the gateway has no authentication. The user's own `endpoint_transport`
is restored on cleanup. Muse Code fetches its model catalog from
`/muse-code/models` at the gateway's origin, whatever path `base_url` has, so
the gateway must route that path as well as the model API.
Muse Code rejects a `settings.json` without `schema_version`, so a file the
daemon creates carries `schema_version: 1`, and cleanup keeps the member when
the user added settings to that file. A file with any other schema version is
reported as a conflict and left untouched. On Unix the file is written with
owner-only permissions (`0600`), since it can hold literal MCP headers and
environment values; dry runs report the action and path without its contents.
MCP inventory reads `mcpServers` from the user `settings.json` and from
`.mcp.json` in the working directory and its ancestors. Skill inventory reads
`<config root>/skills`, the user's `~/.agents/skills`, `~/.claude/skills`, and
`~/.codex/skills` (Muse Code loads other agents' user skills), and the working
directory's `.agents/skills` and `.claude/skills` through its ancestors.
Bundled and plugin skills are not inventoried. The version comes from the
launcher's `.muse-version` file, without running the launcher.

✅ Implemented · ◯ Not implemented · — Not applicable

| Feature | [Claude Code](claude_code/) | [Claude Desktop](claude_desktop/) | [Codex](codex/) | [OpenCode](opencode/) | [VS Code](vscode/) | [Ollama](ollama/) | [Cursor](cursor/) | [Grok Build](grok/) | [GitHub Copilot CLI](copilot/) | [Pi](pi/) | [Muse Code](muse/) |
| --- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| Installation and version discovery | ✅ | ✅ | ✅ | ✅ | ✅ | — | ✅ | ✅ | ✅ | ✅ | ✅ |
| Local model discovery | — | — | — | — | — | ✅ | — | — | — | — | — |
| MCP server discovery | ✅ | ✅ | ✅ | ◯ | ✅ | — | ✅ | ✅ | — | ✅ | ✅ |
| Skill discovery | ✅ | ◯ | ✅ | ◯ | ✅ | — | ✅ | ✅ | — | ✅ | ✅ |
| Managed configuration | ✅ | ✅ | ✅ | ✅ | ✅ | ◯ | ◯ | ✅ | ✅ | ✅ | ✅ |
| LLM gateway routing | ✅ | ✅ | ✅ | ✅ | ✅ | — | ◯ | ✅ | ✅ | ✅ | ✅ |
| Gateway credentials | ✅ | ✅ | ✅ | ✅ | ✅ | — | ◯ | ✅ | ✅ | ✅ | ✅ |
| Sandbox configuration | ✅ | ◯ | ✅ | ◯ | ◯ | — | ◯ | ◯ | ◯ | ◯ | ◯ |
| Tool-use telemetry | ✅ | ◯ | ◯ | ◯ | ◯ | — | ◯ | ◯ | ◯ | ◯ | ◯ |
| Session-start telemetry | ✅ | ◯ | ◯ | ◯ | ◯ | — | ◯ | ◯ | ◯ | ◯ | ◯ |

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
