# Provider support

Features currently implemented by Agentdesktop.

✅ Implemented · ◯ Not implemented · — Not applicable

| Feature | [Claude Code](claude_code/) | [Claude Desktop](claude_desktop/) | [Codex](codex/) | [OpenCode](opencode/) | [VS Code](vscode/) | [Ollama](ollama/) | [Cursor](cursor/) |
| --- | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| Installation and version discovery | ✅ | ✅ | ✅ | ✅ | ✅ | — | ✅ |
| Local model discovery | — | — | — | — | — | ✅ | — |
| MCP server discovery | ✅ | ✅ | ✅ | ◯ | ✅ | — | ✅ |
| Skill discovery | ✅ | ◯ | ✅ | ◯ | ✅ | — | ✅ |
| Managed configuration | ✅ | ✅ | ✅ | ✅ | ◯ | ◯ | ◯ |
| LLM gateway routing | ✅ | ✅ | ✅ | ✅ | ◯ | — | ◯ |
| Gateway credentials | ✅ | ✅ | ✅ | ✅ | ◯ | — | ◯ |
| Sandbox configuration | ✅ | ◯ | ✅ | ◯ | ◯ | — | ◯ |
| Tool-use telemetry | ✅ | ◯ | ◯ | ◯ | ◯ | — | ◯ |
| Session-start telemetry | ✅ | ◯ | ◯ | ◯ | ◯ | — | ◯ |

- ◯ describes a gap in Agentdesktop; it does not mean the upstream provider
  cannot support the feature.
- Managed configuration includes dry runs and removal of Agentdesktop-owned
  settings. Claude Code also supports merging into user settings; Claude Desktop
  requires system-managed settings.
- Discovery is best effort; versions may be unavailable. Ollama discovers models
  through its running local API.

See [provider integration tests](../../tests/README.md) for the Docker suite.
