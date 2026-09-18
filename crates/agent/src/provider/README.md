# Provider support

Features currently implemented by agentdesktop.

Grok Build managed configuration requires system mode. Linux and macOS use
`/etc/grok/managed_config.toml`; Windows uses the system drive's
`\etc\grok\managed_config.toml`. `--user` rejects `programs.grok` because
Grok's own configuration sync can delete or replace the user-level managed
file.

Pi stores configuration in `~/.pi/agent` (or `PI_CODING_AGENT_DIR`). `--user`
merges `providers.agentdesktop` into `models.json` and sets the default model
in `settings.json`. MCP inventory comes from `pi-mcp-adapter` config files
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
