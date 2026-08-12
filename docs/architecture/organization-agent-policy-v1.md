# Organization agent policy v1

The organization agent policy records desired Allow/Deny state for the five supported managed agents:

- `claude-code`
- `claude-desktop`
- `codex-cli`
- `openclaw`
- `vscode-copilot`

One policy exists per organization. Version 1 requires exactly one rule for every supported agent and accepts only `allow` or `deny`. When no policy has been saved, the API returns an unconfigured default with every agent allowed.

Administrator OAuth protects both operations:

```http
GET /v1/admin/agent-policy

PUT /v1/admin/agent-policy
Content-Type: application/json

{
  "schema_version": 1,
  "rules": [
    {"agent_id":"claude-code","action":"allow"},
    {"agent_id":"claude-desktop","action":"allow"},
    {"agent_id":"codex-cli","action":"deny"},
    {"agent_id":"openclaw","action":"deny"},
    {"agent_id":"vscode-copilot","action":"allow"}
  ]
}
```

Updates replace the organization policy atomically and write an `agent_policy.updated` audit event. The response always includes `enforcement: "not_available"`.

This contract stores desired policy only. Agent Desktop does not yet have a versioned enforcement protocol capable of blocking arbitrary agent execution or egress, and Agent Gateway cannot infer trustworthy local application identity from the current native route. The administrator UI must not claim that Deny is enforced until those boundaries exist and are tested.