# Provider support

Features currently implemented by Agentdesktop.

Grok Build managed configuration requires system mode. Linux and macOS use
`/etc/grok/managed_config.toml`; Windows uses the system drive's
`\etc\grok\managed_config.toml`. `--user` rejects `programs.grok` because
Grok's own configuration sync can delete or replace the user-level managed
file.

✅ Implemented · ◯ Not implemented · — Not applicable

| Feature | [Claude Code](claude_code/) | [Claude Desktop](claude_desktop/) | [Codex](codex/) | [OpenCode](opencode/) | [VS Code](vscode/) | GitHub Copilot CLI | [Ollama](ollama/) | [Cursor](cursor/) | [Grok Build](grok/) |
| --- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| Installation and version discovery | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | — | ✅ | ✅ |
| Local model discovery | — | — | — | — | — | — | ✅ | — | — |
| MCP server discovery | ✅ | ✅ | ✅ | ◯ | ✅ | User config | — | ✅ | ✅ |
| Skill discovery | ✅ | ◯ | ✅ | ◯ | ✅ | User config | — | ✅ | ✅ |
| Managed configuration | ✅ | ✅ | ✅ | ✅ | User only | — | ◯ | ◯ | ✅ |
| LLM gateway routing | ✅ | ✅ | ✅ | ✅ | Local passthrough | Launcher only | — | ◯ | ✅ |
| Gateway credentials | ✅ | ✅ | ✅ | ✅ | Existing Copilot bearer | Per-request helper | — | ◯ | ✅ |
| Sandbox configuration | ✅ | ◯ | ✅ | ◯ | ◯ | ◯ | — | ◯ | ◯ |
| Tool-use telemetry | ✅ | ◯ | ◯ | ◯ | ◯ | ◯ | — | ◯ | ◯ |
| Session-start telemetry | ✅ | ◯ | ◯ | ◯ | ◯ | ◯ | — | ◯ | ◯ |

- ◯ describes a gap in Agentdesktop; it does not mean the upstream provider
  cannot support the feature.
- Managed configuration includes dry runs and removal of Agentdesktop-owned
  settings. Claude Code also supports merging into user settings; Claude Desktop
  requires system-managed settings.
- Discovery is best effort; versions may be unavailable. Ollama discovers models
  through its running local API. Grok Build versions require its optional
  `version.json` update cache; fresh installs may have no discovered version.

## VS Code and GitHub Copilot CLI

- **VS Code:** `programs.vscode` manages only the configured User settings file,
  requires `--user`, and accepts an explicit `--vscode-settings` path. It does
  not automatically select the active profile or configure every profile,
  workspace, Remote session, or system setting. The local Copilot proxy uses
  the existing Copilot bearer, not a daemon-issued credential.
- **GitHub Copilot CLI:** inventory ID `copilot-cli`, configuration key
  `programs.copilotCli`. CLI 1.0.84+ supports the explicit
  `agentdesktop copilot -- …` launcher. The effective daemon configuration
  must enable gateway routing and supply a model. `wireApi` defaults to
  `completions`; `responses` requires a compatible backend. A per-request
  `credential --client-id copilot-cli` helper refreshes gateway credentials.
  The launcher ignores the personal provider registry without overwriting it,
  preserves login, and leaves plain `copilot` unchanged. Native CLI
  configuration files are not reconciled.
  Discovery reads MCP definitions and skill front matter from `COPILOT_HOME`
  or the user's `.copilot` directory, not login state or project-specific files.
- OpenAI or Anthropic upstreams do not require a Copilot subscription. A
  Copilot upstream instead needs separately provisioned AGW
  `backendAuth: copilot` credentials; neither a daemon JWT nor the CLI's
  existing login is automatically a usable upstream credential.

See the [Copilot example](../../../../examples/copilot/README.md) for the two
separate authentication paths and local setup.

See [provider integration tests](../../tests/README.md) for the Docker suite.
