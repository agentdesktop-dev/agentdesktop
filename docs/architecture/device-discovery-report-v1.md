# Device discovery report v1

Device discovery reports are privacy-bounded, client-reported inventory metadata for remote managed macOS and Windows devices. They do not participate in Agent Gateway policy decisions and are not a source of cryptographic identity.

## Transport and authorization

The managed desktop sends `PUT /v1/device-reports/current` directly to the enrollment authority over HTTPS using its current managed client certificate. It does not attach an OAuth bearer token, and the JSON body contains no organization, user, device, or certificate identifiers.

The authority derives organization, user, device, and certificate serial from the verified SPIFFE client certificate. A single PostgreSQL statement accepts the report only when all of the following remain true at write time:

- The device belongs to the certificate-derived organization and is active.
- The certificate belongs to that device and organization.
- The certificate is the device's current certificate generation.
- The certificate is within its validity period and is not revoked.
- The certificate-derived user has an approved enrollment for the device.

A stale, revoked, foreign, or wrong-user certificate receives `403 device_not_active` and cannot replace the stored report. TLS authentication failures receive `401 invalid_device_certificate`.

Administrators retrieve a report with `GET /v1/admin/devices/{deviceID}/discovery-report`. This endpoint uses the existing administrator OAuth authorization and organization scope. A missing report, foreign device, and unknown device are all returned as `404 discovery_report_not_found`. Reports are fetched only when a device is inspected; they are not included in the frequently polled device list.

Fleet administration uses two additional organization-scoped, paged endpoints:

```http
GET /v1/admin/inventory?kind=agent&q=claude&limit=25&offset=0
GET /v1/admin/inventory/devices?kind=agent&key=claude-code&version=2.1.4&q=macbook&limit=50&offset=0
```

The first returns active/reporting endpoint counts plus ranked agent-version, MCP-name/transport, skill-name, or plugin-name/state aggregates. The second searches active devices by display name, owner, subject, or UUID and optionally restricts them to one exact inventory asset. Both aggregate directly over each device's latest report in PostgreSQL; the browser does not fetch reports for the whole fleet. Limits are bounded to 100 per page.

## Schema

The request body is limited to 64 KiB, rejects unknown fields, and uses this shape:

```json
{
  "schema_version": 1,
  "collector_version": "0.1.0",
  "platform": "macos",
  "coverage": {
    "project_scopes": "not_scanned",
    "partial": false
  },
  "agents": [{
    "id": "claude-code",
    "installed": true,
    "version": "2.1.4",
    "running": "detected",
    "evidence": ["executable", "configuration"],
    "config_sources": [{
      "scope": "user",
      "source": "claude-user-config",
      "format": "json",
      "status": "parsed",
      "sections": ["mcp"]
    }],
    "mcp_servers": [{
      "name": "github",
      "scope": "user",
      "transport": "stdio"
    }],
    "skills": [{"name": "review-pr", "scope": "user"}],
    "plugins": [{"name": "example@marketplace", "scope": "user", "state": "enabled"}]
  }],
  "issues": []
}
```

Platforms are closed to `macos` and `windows`. Agent IDs are closed to `claude-code`, `claude-desktop`, `codex-cli`, `openclaw`, and `vscode-copilot`. Source, scope, status, section, transport, running-state, and issue-code values are closed enums. An optional version is accepted only from fixed package/application metadata and must begin with a digit and contain no whitespace or shell punctuation. Names and versions are bounded client-reported metadata and must not be interpreted as proof that a resource is reachable, trusted, or effectively enabled.

The GET response wraps the report with server-controlled `device_id` and `received_at` fields. PostgreSQL stores only the latest report for each device. Revocation prevents updates but retains the last report with its receipt time so administrators can recognize it as stale.

## Collection boundary

Collection runs at startup and every 15 minutes while the managed Tauri background app is running. The desktop polls for certificate-authenticated force-rescan requests every 30 seconds and uploads immediately when one is pending. Production freshness across logout or reboot is not guaranteed until platform login/startup lifecycle support exists.

The macOS collector reads only these fixed locations:

| Agent | Installation and configuration evidence |
| --- | --- |
| Claude Code | `~/.local/bin/claude`, `/usr/local/bin/claude`, `/opt/homebrew/bin/claude`, `~/.claude/settings.json`, `~/.claude.json`, `~/.claude/skills`, `/Library/Application Support/ClaudeCode/managed-mcp.json` |
| Claude Desktop | `/Applications/Claude.app`, `~/Applications/Claude.app`, and `~/Library/Application Support/Claude/extensions-installations.json` for installed MCP Extensions |
| Codex CLI | Equivalent fixed executable locations, `~/.codex/config.toml`, `~/.codex/skills`, `~/.agents/skills` |
| OpenClaw | Equivalent fixed executable locations, `~/.openclaw/openclaw.json`, `~/.openclaw/skills`, `~/.openclaw/extensions`, `~/.agents/skills` |
| VS Code Copilot | `/Applications/Visual Studio Code.app`, `~/Applications/Visual Studio Code.app`, `~/.vscode/extensions`, `~/Library/Application Support/Code/User/mcp.json`, user profile `mcp.json` files, and fixed `.copilot`, `.claude`, and `.agents` skill/plugin locations |

The initial Windows collector reports only Claude Code. It reads
`%USERPROFILE%\.local\bin\claude.exe`, `%APPDATA%\npm\claude.cmd`,
`%APPDATA%\npm\node_modules\@anthropic-ai\claude-code\package.json`,
`%USERPROFILE%\.claude\settings.json`, `%USERPROFILE%\.claude.json`,
`%USERPROFILE%\.claude\skills`, and `%ProgramData%\ClaudeCode\managed-mcp.json`.
It does not inspect the process table, so `running` is reported as `unknown`.

The collector does not honor path-changing environment variables, inspect project configuration, or crawl the home directory. Project discovery requires a future explicit managed allowlist.

## Privacy and resilience

- Files are parsed locally with JSON5 or TOML parsers. Config files are capped at 1 MiB.
- Config symlinks and directory symlinks are skipped. Directory entries and every report array are bounded.
- Runtime checks match only exact known process names and discard all command output.
- Reports contain sanitized names, statically read versions, booleans, closed enums, and code-only collection issues.
- Reports never contain filesystem paths, URLs, commands, arguments, headers, environment values, config values, file contents, hostnames, process IDs, or command lines.
- The collector never executes agents, MCP servers, plugins, or skills and never performs reachability probes.
- The enrollment authority does not log request bodies through this implementation.

Agent Gateway remains the sole inference policy and content-inspection boundary. Discovery metadata is informational inventory only.