# Provider support

Features currently implemented by agentdesktop.

Grok Build managed configuration requires system mode. Linux and macOS use
`/etc/grok/managed_config.toml`; Windows uses the system drive's
`\etc\grok\managed_config.toml`. `--user` rejects `programs.grok` because
Grok's own configuration sync can delete or replace the user-level managed
file.

Pi stores configuration in `~/.pi/agent` (or `PI_CODING_AGENT_DIR`, with `~`
expanded to the user's home). Management requires `--user`; system mode
rejects `programs.pi` because Pi does not read `/etc/pi/agent`. Authenticated
gateway configuration also requires a running daemon and rejects `--once`.
User mode merges `providers.agentdesktop` into `models.json` and sets the
default model in `settings.json`. Model map keys determine catalog IDs, and
gateway URLs override per-model endpoints, using each model's API dialect.
Commented `models.json` files are accepted and rewritten as standard JSON,
preserving user configuration values. `settings.json` requires standard JSON.
Discovery recognizes npm's Windows command shims as well as symlinked launchers,
verifying the Pi package manifest without executing the launcher.
MCP inventory comes from `pi-mcp-adapter` config files
(`~/.pi/agent/mcp.json`, `.pi/mcp.json`, `.mcp.json`, and shared
`~/.config/mcp/mcp.json` / `~/.agents/mcp.json`). Host-specific Cursor/Claude
files are not scanned until the adapter imports them into a Pi-owned file.

✅ Implemented · ◯ Not implemented · — Not applicable

| Feature | [Claude Code](claude_code/) | [Claude Desktop](claude_desktop/) | [Codex](codex/) | [OpenCode](opencode/) | [VS Code](vscode/) | [Ollama](ollama/) | [Cursor](cursor/) | [Grok Build](grok/) | [Pi](pi/) |
| --- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| Installation and version discovery | ✅ | ✅ | ✅ | ✅ | ✅ | — | ✅ | ✅ | ✅ |
| Local model discovery | — | — | — | — | — | ✅ | — | — | — |
| MCP server discovery | ✅ | ✅ | ✅ | ◯ | ✅ | — | ✅ | ✅ | ✅ |
| Skill discovery | ✅ | ◯ | ✅ | ◯ | ✅ | — | ✅ | ✅ | ✅ |
| Managed configuration | ✅ | ✅ | ✅ | ✅ | ◯ | ◯ | ◯ | ✅ | ✅ |
| LLM gateway routing | ✅ | ✅ | ✅ | ✅ | ◯ | — | ◯ | ✅ | ✅ |
| Gateway credentials | ✅ | ✅ | ✅ | ✅ | ◯ | — | ◯ | ✅ | ✅ |
| Sandbox configuration | ✅ | ◯ | ✅ | ◯ | ◯ | — | ◯ | ◯ | ◯ |
| Tool-use telemetry | ✅ | ◯ | ◯ | ◯ | ◯ | — | ◯ | ◯ | ◯ |
| Session-start telemetry | ✅ | ◯ | ◯ | ◯ | ◯ | — | ◯ | ◯ | ◯ |

- ◯ describes a gap in agentdesktop; it does not mean the upstream provider
  cannot support the feature.
- Managed configuration includes dry runs and removal of agentdesktop-owned
  settings. Claude Code also supports merging into user settings; Claude Desktop
  requires system-managed settings.
- Discovery is best effort; versions may be unavailable. Ollama discovers models
  through its running local API. Grok Build versions require its optional
  `version.json` update cache; fresh installs may have no discovered version.

See [provider integration tests](../../tests/README.md) for the Docker suite.
