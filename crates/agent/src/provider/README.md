# Provider support

Features currently implemented by Agentdesktop.

Grok Build managed configuration requires system mode. Linux and macOS use
`/etc/grok/managed_config.toml`; Windows uses the system drive's
`\etc\grok\managed_config.toml`. `--user` rejects `programs.grok` because
Grok's own configuration sync can delete or replace the user-level managed
file.

✅ Implemented · ◯ Not implemented · — Not applicable

| Feature | [Claude Code](claude_code/) | [Claude Desktop](claude_desktop/) | [Codex](codex/) | [OpenCode](opencode/) | [VS Code](vscode/) | [Ollama](ollama/) | [Cursor](cursor/) | [Grok Build](grok/) |
| --- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| Installation and version discovery | ✅ | ✅ | ✅ | ✅ | ✅ | — | ✅ | ✅ |
| Local model discovery | — | — | — | — | — | ✅ | — | — |
| MCP server discovery | ✅ | ✅ | ✅ | ◯ | ✅ | — | ✅ | ✅ |
| Skill discovery | ✅ | ◯ | ✅ | ◯ | ✅ | — | ✅ | ✅ |
| Managed configuration | ✅ | ✅ | ✅ | ✅ | User only | ◯ | ◯ | ✅ |
| LLM gateway routing | ✅ | ✅ | ✅ | ✅ | Local passthrough | — | ◯ | ✅ |
| Gateway credentials | ✅ | ✅ | ✅ | ✅ | Existing Copilot bearer | — | ◯ | ✅ |
| Sandbox configuration | ✅ | ◯ | ✅ | ◯ | ◯ | — | ◯ | ◯ |
| Tool-use telemetry | ✅ | ◯ | ◯ | ◯ | ◯ | — | ◯ | ◯ |
| Session-start telemetry | ✅ | ◯ | ◯ | ◯ | ◯ | — | ◯ | ◯ |

- ◯ describes a gap in Agentdesktop; it does not mean the upstream provider
  cannot support the feature.
- Managed configuration includes dry runs and removal of Agentdesktop-owned
  settings. Claude Code also supports merging into user settings; Claude Desktop
  requires system-managed settings.
- Discovery is best effort; versions may be unavailable. Ollama discovers models
  through its running local API. Grok Build versions require its optional
  `version.json` update cache; fresh installs may have no discovered version.

## VS Code

- `programs.vscode` manages only the configured User settings file, requires
  `--user`, and accepts an explicit `daemon.vscode.config` path. It does not
  automatically select the active profile or configure every profile,
  workspace, Remote session, or system setting.
- The local Copilot proxy uses the existing Copilot bearer, not a daemon-issued
  credential. No daemon JWT or OIDC credential is a usable Copilot upstream
  credential.

See the [VS Code example](../../../../examples/vscode/README.md) for local setup.

See [provider integration tests](../../tests/README.md) for the Docker suite.
