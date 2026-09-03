import type {
  AccessReport,
  Bootstrap,
  ConnectorSnapshot,
  Discovery,
  ManagedDeviceSnapshot,
} from "../types";

export const bootstrap: Bootstrap = {
  settings: { openOnStartup: true },
  version: "0.1.0",
  platform: "macos",
};

export const standaloneConnector: ConnectorSnapshot = {
  state: "ready",
  detail: null,
  runtime: {
    version: "0.1.0",
    mode: "standalone",
    gateway: "not-configured",
    platform: { os: "macos" },
  },
};

export const managedConnector: ConnectorSnapshot = {
  state: "ready",
  detail: null,
  runtime: {
    version: "0.1.0",
    mode: "managed",
    gateway: "reachable",
    platform: { os: "macos" },
  },
};

export const offlineConnector: ConnectorSnapshot = {
  state: "offline",
  detail: "The Agent Desktop daemon is unavailable.",
  runtime: null,
};

export const unconfiguredDevice: ManagedDeviceSnapshot = {
  configured: false,
  organizationName: null,
  enrollment: "unconfigured",
  detail: null,
};

export const approvedDevice: ManagedDeviceSnapshot = {
  configured: true,
  organizationName: "Acme Engineering",
  enrollment: "approved",
  detail: null,
};

export const pendingDevice: ManagedDeviceSnapshot = {
  configured: true,
  organizationName: "Acme Engineering",
  enrollment: "pending",
  detail: null,
};

export const rejectedDevice: ManagedDeviceSnapshot = {
  configured: true,
  organizationName: "Acme Engineering",
  enrollment: "rejected",
  detail: "Your administrator rejected this enrollment request.",
};

const mcpServers = [
  {
    name: "github-enterprise",
    transport: "stdio",
    command: "/usr/local/bin/github-mcp-server --read-only",
    enabled: true,
    source: "VS Code settings",
  },
  {
    name: "workspace-files",
    transport: "stdio",
    command:
      "npx -y @modelcontextprotocol/server-filesystem /Users/developer/projects",
    enabled: true,
    source: "User settings",
  },
  {
    name: "linear",
    transport: "streamable-http",
    url: "https://mcp.linear.app/mcp",
    enabled: true,
    source: "Organization policy",
  },
  {
    name: "sentry-production",
    transport: "sse",
    url: "https://mcp.sentry.example.internal/events/production",
    enabled: true,
    source: "Workspace settings",
  },
  {
    name: "postgres-readonly",
    transport: "stdio",
    command:
      "uvx postgres-mcp --access-mode=restricted --profile=analytics-replica",
    enabled: false,
    source: "Organization policy",
  },
  {
    name: "kubernetes-staging",
    transport: "stdio",
    command:
      "docker run --rm -i company/kubernetes-mcp:stable --context staging-us-east-1",
    enabled: true,
    source: "Dev container configuration",
  },
  {
    name: "notion-engineering",
    transport: "streamable-http",
    url: "https://mcp.notion.com/engineering-workspace",
    enabled: true,
    source: "User settings",
  },
  {
    name: "design-system",
    transport: "http",
    url: "https://tools.example.internal/mcp/design-system/v2",
    enabled: true,
    source: "Workspace settings",
  },
];

const skills = [
  {
    path: "/Users/developer/.agents/skills/incident-response/SKILL.md",
    frontMatter: {
      name: "Incident response",
      description:
        "Triages production incidents, gathers evidence, and prepares a concise handoff for the incident commander.",
    },
  },
  {
    path: "/Users/developer/.agents/skills/release-coordinator/SKILL.md",
    frontMatter: {
      name: "Release coordinator",
      description:
        "Coordinates versioning, release notes, validation, and rollout across services.",
    },
  },
  {
    path: "/Users/developer/.agents/skills/architecture-review/SKILL.md",
    frontMatter: {
      name: "Architecture review",
      description:
        "Reviews module boundaries, coupling, and operational tradeoffs against documented decisions.",
    },
  },
  {
    path: "/Users/developer/.agents/skills/security-audit/SKILL.md",
    frontMatter: {
      name: "Security audit",
      description:
        "Checks authentication, authorization, secret handling, and dependency risk.",
    },
  },
  {
    path: "/Users/developer/.agents/skills/migration-planner/SKILL.md",
    frontMatter: {
      name: "Migration planner",
      description:
        "Builds staged migration plans with compatibility gates and rollback points.",
    },
  },
  {
    path: "/Users/developer/.agents/skills/performance-diagnosis/SKILL.md",
    frontMatter: {
      name: "Performance diagnosis",
      description:
        "Reproduces regressions, captures profiles, and validates improvements against a baseline.",
    },
  },
  {
    path: "/Users/developer/.agents/skills/accessibility-review/SKILL.md",
    frontMatter: {
      name: "Accessibility review",
      description:
        "Audits keyboard flow, semantics, contrast, reflow, and assistive technology behavior.",
    },
  },
  {
    path: "/Users/developer/.agents/skills/api-design/SKILL.md",
    frontMatter: {
      name: "API design",
      description:
        "Shapes stable contracts, error models, pagination, and compatibility expectations.",
    },
  },
  {
    path: "/Users/developer/.agents/skills/dependency-upgrade/SKILL.md",
    frontMatter: {
      name: "Dependency upgrade",
      description:
        "Plans and validates framework upgrades while minimizing unrelated code churn.",
    },
  },
  {
    path: "/Users/developer/.agents/skills/documentation-maintenance/SKILL.md",
    frontMatter: {
      name: "Documentation maintenance",
      description:
        "Keeps setup guides, examples, and architectural references aligned with current behavior.",
    },
  },
  {
    path: "/Users/developer/.agents/skills/onboarding-review/SKILL.md",
    frontMatter: {
      name: "Onboarding review",
      description:
        "Tests first-run workflows and improves empty states, defaults, and recovery guidance.",
    },
  },
  {
    path: "/Users/developer/.agents/skills/changelog/SKILL.md",
    frontMatter: {
      name: "Changelog writer",
      description:
        "Produces user-focused release notes from merged changes and issue context.",
    },
  },
];

export const populatedDiscovery: Discovery = {
  agents: [
    {
      kind: "vscode",
      executable:
        "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
      version: "1.104.0",
      mcpServers,
      skills,
    },
    {
      kind: "claude-code",
      executable: "/Users/developer/.local/bin/claude",
      version: "2.1.3",
      mcpServers: mcpServers.slice(0, 3),
      skills: skills.slice(0, 6),
    },
    {
      kind: "claude-desktop",
      executable: "/Applications/Claude.app/Contents/MacOS/Claude",
      version: "1.25927.0",
      mcpServers: [],
      skills: [],
    },
    {
      kind: "codex",
      executable: "/opt/homebrew/bin/codex",
      version: "0.42.0",
      mcpServers: mcpServers.slice(6),
      skills: skills.slice(8),
    },
    {
      kind: "opencode",
      executable: "/Users/developer/.local/bin/opencode",
      version: "1.1.1",
      mcpServers: [],
      skills: [],
    },
  ],
  modelRuntimes: [
    {
      kind: "ollama",
      models: [{ name: "gemma3:4b" }, { name: "qwen3:8b" }],
    },
  ],
};

export const emptyDiscovery: Discovery = { agents: [] };

export const populatedAccessReport: AccessReport = {
  generatedAtUnixMs: 1788220800000,
  status: "ready",
  agents: [
    {
      kind: "vscode",
      executable:
        "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
      version: "1.104.0",
      userHome: "/Users/developer",
      capabilities: [
        {
          category: "filesystem",
          resource: "active workspace",
          operations: ["read"],
          decision: "allow",
          enforcement: "harness",
          source: { kind: "default" },
          detail: "VS Code agent workspace access",
        },
        {
          category: "filesystem",
          resource: "active workspace",
          operations: ["write"],
          decision: "ask",
          enforcement: "harness",
          source: { kind: "default" },
          detail: "Write approval depends on the session permission level",
        },
        {
          category: "execution",
          resource: "cargo test",
          operations: ["execute"],
          decision: "allow",
          enforcement: "harness",
          source: {
            kind: "configuration",
            path: "/Users/developer/Library/Application Support/Code/User/settings.json",
          },
          detail: "VS Code terminal auto-approval",
        },
        {
          category: "execution",
          resource: "recorded terminal commands",
          operations: ["execute"],
          decision: "allow",
          enforcement: "sandbox",
          source: { kind: "history" },
          detail: "VS Code recorded sandbox-wrapped terminal execution",
        },
        {
          category: "execution",
          resource: "shell commands",
          operations: ["execute"],
          decision: "ask",
          enforcement: "unknown",
          source: { kind: "default" },
          detail:
            "Terminal containment depends on session and VS Code settings",
        },
        {
          category: "network",
          resource: "URL tools",
          operations: ["connect"],
          decision: "ask",
          enforcement: "harness",
          source: { kind: "default" },
          detail: "Unmatched URL tool requests require approval",
        },
        {
          category: "network",
          resource: "*.githubusercontent.com",
          operations: ["connect"],
          decision: "allow",
          enforcement: "harness",
          source: {
            kind: "configuration",
            path: "/Users/developer/Library/Application Support/Code/User/settings.json",
          },
          rule: {
            id: "vscode-url-githubusercontent",
            mechanism: "vscodeUrlAutoApprove",
          },
          detail: "VS Code URL tool auto-approval",
        },
        {
          category: "network",
          resource: "*.amazon.com",
          operations: ["connect"],
          decision: "allow",
          enforcement: "harness",
          source: {
            kind: "configuration",
            path: "/Users/developer/Library/Application Support/Code/User/settings.json",
          },
          rule: {
            id: "vscode-url-amazon",
            mechanism: "vscodeUrlAutoApprove",
          },
          detail: "VS Code URL tool auto-approval",
        },
        {
          category: "network",
          resource: "*.apple.com",
          operations: ["connect"],
          decision: "allow",
          enforcement: "harness",
          source: {
            kind: "configuration",
            path: "/Users/developer/Library/Application Support/Code/User/settings.json",
          },
          rule: {
            id: "vscode-url-apple",
            mechanism: "vscodeUrlAutoApprove",
          },
          detail: "VS Code URL tool auto-approval",
        },
        {
          category: "network",
          resource: "*.cilium.io",
          operations: ["connect"],
          decision: "allow",
          enforcement: "harness",
          source: {
            kind: "configuration",
            path: "/Users/developer/Library/Application Support/Code/User/settings.json",
          },
          rule: {
            id: "vscode-url-cilium",
            mechanism: "vscodeUrlAutoApprove",
          },
          detail: "VS Code URL tool auto-approval",
        },
        {
          category: "network",
          resource: "api.github.com",
          operations: ["connect"],
          decision: "allow",
          enforcement: "harness",
          source: {
            kind: "configuration",
            path: "/Users/developer/Library/Application Support/Code/User/settings.json",
          },
          rule: {
            id: "vscode-url-api-github",
            mechanism: "vscodeUrlAutoApprove",
          },
          detail: "VS Code URL tool auto-approval",
        },
        {
          category: "externalService",
          resource: "mcp:github",
          operations: ["use"],
          decision: "unknown",
          enforcement: "harness",
          source: {
            kind: "mcp",
            path: "/Users/developer/Library/Application Support/Code/User/mcp.json",
          },
          detail:
            "Configured and enabled; per-tool approval depends on the harness",
        },
        {
          category: "externalService",
          resource: "mcp:linear",
          operations: ["use"],
          decision: "unknown",
          enforcement: "harness",
          source: {
            kind: "mcp",
            path: "/Users/developer/Library/Application Support/Code/User/mcp.json",
          },
          detail:
            "Configured and enabled; per-tool approval depends on the harness",
        },
        {
          category: "externalService",
          resource: "mcp:sentry",
          operations: ["use"],
          decision: "unknown",
          enforcement: "harness",
          source: {
            kind: "mcp",
            path: "/Users/developer/Library/Application Support/Code/User/mcp.json",
          },
          detail:
            "Configured and enabled; per-tool approval depends on the harness",
        },
      ],
      observations: [
        {
          category: "filesystem",
          resource: "/Users/developer/projects/agentdesktop",
          operations: ["read", "write"],
          workspace: "/Users/developer/projects/agentdesktop",
          count: 42,
          sessionCount: 8,
          resourceCount: 31,
          workspaceCount: 1,
          evidenceUpdatedAtUnixMs: 1788134400000,
          confidence: "high",
          source: { kind: "history" },
        },
        {
          category: "execution",
          resource: "cd",
          operations: ["execute"],
          count: 18,
          sessionCount: 7,
          resourceCount: 1,
          workspaceCount: 3,
          evidenceUpdatedAtUnixMs: 1788134400000,
          confidence: "high",
          source: { kind: "history" },
        },
        {
          category: "network",
          resource: "docs.rs",
          operations: ["connect"],
          workspace: "/Users/developer/projects/agentdesktop",
          count: 3,
          sessionCount: 2,
          resourceCount: 1,
          workspaceCount: 1,
          evidenceUpdatedAtUnixMs: 1788134400000,
          confidence: "high",
          source: {
            kind: "history",
            path: "/Users/developer/Library/Application Support/Code/User/workspaceStorage/session/chatSessions/access.jsonl",
          },
        },
        {
          category: "network",
          resource: "localhost",
          operations: ["connect"],
          workspace: "/Users/developer/projects/agentdesktop",
          count: 4,
          sessionCount: 2,
          resourceCount: 1,
          workspaceCount: 1,
          evidenceUpdatedAtUnixMs: 1788134400000,
          confidence: "high",
          source: {
            kind: "history",
            path: "/Users/developer/Library/Application Support/Code/User/workspaceStorage/session/chatSessions/access.jsonl",
          },
        },
      ],
      findings: [
        {
          severity: "warning",
          title: "4 wildcard network rules",
          detail:
            "4 wildcard domain rules each allow every matching subdomain; review the Network rules",
          category: "network",
          source: {
            kind: "configuration",
            path: "/Users/developer/Library/Application Support/Code/User/settings.json",
          },
        },
      ],
      coverage: [
        {
          source: "configuration",
          status: "partial",
          detail:
            "Inspected 1 VS Code settings file; profile and workspace settings are not assessed",
        },
        {
          source: "mcp",
          status: "partial",
          detail:
            "Inspected 3 discovered MCP servers; project-scoped definitions outside the daemon working directory may be omitted",
        },
        {
          source: "history",
          status: "partial",
          detail: "Inspected 41 of 252 history files, bounded to 64 MiB",
        },
      ],
    },
    {
      kind: "claude-code",
      executable: "/Users/developer/.local/bin/claude",
      version: "2.1.250",
      userHome: "/Users/developer",
      capabilities: [
        {
          category: "filesystem",
          resource: "/Users/developer/projects/agentdesktop",
          operations: ["read"],
          decision: "allow",
          enforcement: "harness",
          workspace: "/Users/developer/projects/agentdesktop",
          source: {
            kind: "configuration",
            path: "/Users/developer/.claude.json",
          },
          detail: "Trusted Claude workspace",
        },
        {
          category: "network",
          resource: "*.github.com",
          operations: ["connect"],
          decision: "allow",
          enforcement: "sandbox",
          source: {
            kind: "configuration",
            path: "/Users/developer/.claude/settings.json",
          },
          rule: {
            id: "claude-sandbox-github",
            mechanism: "claudeSandboxDomain",
          },
          detail: "Claude sandbox network rule",
        },
        {
          category: "credential",
          resource: "ANTHROPIC_API_KEY",
          operations: ["use"],
          decision: "unknown",
          enforcement: "none",
          source: {
            kind: "configuration",
            path: "/Users/developer/.claude/settings.json",
          },
          detail: "Environment variable configured; value omitted",
        },
        {
          category: "externalService",
          resource: "mcp:github-enterprise",
          operations: ["use"],
          decision: "unknown",
          enforcement: "harness",
          source: {
            kind: "mcp",
            path: "/Users/developer/.claude.json",
          },
          detail:
            "Configured and enabled; per-tool approval depends on the harness",
        },
      ],
      observations: [
        {
          category: "filesystem",
          resource:
            "/Users/developer/projects/agentdesktop/crates/agent/src/access/configuration.rs",
          operations: ["read"],
          workspace: "/Users/developer/projects/agentdesktop",
          count: 7,
          sessionCount: 3,
          resourceCount: 5,
          workspaceCount: 1,
          evidenceUpdatedAtUnixMs: 1788134400000,
          confidence: "high",
          source: {
            kind: "history",
            path: "/Users/developer/.claude/projects/agentdesktop/session.jsonl",
          },
        },
        {
          category: "network",
          resource: "registry.npmjs.org",
          operations: ["connect"],
          workspace: "/Users/developer/projects/agentdesktop",
          count: 2,
          sessionCount: 2,
          resourceCount: 1,
          workspaceCount: 1,
          evidenceUpdatedAtUnixMs: 1788134400000,
          confidence: "heuristic",
          source: {
            kind: "history",
            path: "/Users/developer/.claude/projects/agentdesktop/session.jsonl",
          },
        },
      ],
      findings: [
        {
          severity: "warning",
          title: "Wildcard network rule",
          detail:
            "*.github.com allows every matching subdomain, not one exact host",
          category: "network",
          source: {
            kind: "configuration",
            path: "/Users/developer/.claude/settings.json",
          },
        },
        {
          severity: "notice",
          title: "Local MCP process",
          detail:
            "github-enterprise starts a local MCP process whose host access is not described by MCP configuration",
          category: "externalService",
          source: {
            kind: "mcp",
            path: "/Users/developer/.claude.json",
          },
        },
      ],
      coverage: [
        {
          source: "configuration",
          status: "partial",
          detail:
            "Inspected 3 Claude configuration files; session and plugin settings may add access",
        },
        {
          source: "mcp",
          status: "partial",
          detail:
            "Inspected 1 discovered MCP server; project-scoped definitions outside the daemon working directory may be omitted",
        },
        {
          source: "history",
          status: "partial",
          detail: "Inspected 96 of 142 history files, bounded to 64 MiB",
        },
      ],
    },
    {
      kind: "claude-desktop",
      executable: "/Applications/Claude.app/Contents/MacOS/Claude",
      version: "1.25927.0",
      userHome: "/Users/developer",
      capabilities: [
        {
          category: "network",
          resource: "hosted web search",
          operations: ["use"],
          decision: "allow",
          enforcement: "harness",
          source: {
            kind: "configuration",
            path: "/Users/developer/Library/Application Support/Claude/claude_desktop_config.json",
          },
          detail: "Claude Desktop Cowork web search enabled",
        },
      ],
      observations: [],
      findings: [],
      coverage: [
        {
          source: "configuration",
          status: "partial",
          detail:
            "Inspected Claude Desktop settings; per-session computer and Cowork grants may not be persisted here",
        },
        {
          source: "mcp",
          status: "partial",
          detail:
            "Inspected 0 discovered MCP servers; project-scoped, custom, or agent-specific definitions may be omitted",
        },
        {
          source: "history",
          status: "partial",
          detail:
            "Inspected 5 local agent-mode artifacts; computer-use grants may be stored elsewhere",
        },
      ],
    },
    {
      kind: "codex",
      executable: "/opt/homebrew/bin/codex",
      version: "0.129.0",
      userHome: "/Users/developer",
      capabilities: [
        {
          category: "filesystem",
          resource: "workspace",
          operations: ["read", "write"],
          decision: "allow",
          enforcement: "sandbox",
          source: {
            kind: "configuration",
            path: "/Users/developer/.codex/config.toml",
          },
          detail: "Codex workspace-write sandbox",
        },
        {
          category: "network",
          resource: "*",
          operations: ["connect"],
          decision: "deny",
          enforcement: "sandbox",
          source: {
            kind: "configuration",
            path: "/Users/developer/.codex/config.toml",
          },
          detail: "Codex workspace-write network policy",
        },
      ],
      observations: [],
      findings: [],
      coverage: [
        {
          source: "configuration",
          status: "partial",
          detail:
            "Inspected 1 Codex configuration file; named profiles and requirements are not assessed",
        },
        {
          source: "mcp",
          status: "partial",
          detail:
            "Inspected 0 discovered MCP servers; project-scoped, custom, or agent-specific definitions may be omitted",
        },
        {
          source: "history",
          status: "unavailable",
          detail: "No local Codex history store was found",
        },
      ],
    },
    {
      kind: "opencode",
      executable: "/Users/developer/.local/bin/opencode",
      version: "1.1.1",
      userHome: "/Users/developer",
      capabilities: [
        {
          category: "filesystem",
          resource: "workspace",
          operations: ["read", "write"],
          decision: "allow",
          enforcement: "harness",
          source: { kind: "default" },
          detail: "OpenCode permits most workspace operations by default",
        },
        {
          category: "execution",
          resource: "*",
          operations: ["execute"],
          decision: "allow",
          enforcement: "none",
          source: { kind: "default" },
          detail: "OpenCode permits shell operations by default",
        },
        {
          category: "network",
          resource: "web tools",
          operations: ["connect", "use"],
          decision: "allow",
          enforcement: "harness",
          source: { kind: "default" },
          detail: "OpenCode permits web tools by default",
        },
      ],
      observations: [],
      findings: [
        {
          severity: "critical",
          title: "Uncontained command execution",
          detail:
            "Commands can run without a declared sandbox boundary. Require approval or configure a sandbox",
          category: "execution",
        },
      ],
      coverage: [
        {
          source: "configuration",
          status: "partial",
          detail:
            "Inspected 1 OpenCode configuration file; custom and agent-specific configuration may add access",
        },
        {
          source: "mcp",
          status: "partial",
          detail:
            "Inspected 0 discovered MCP servers; project-scoped, custom, or agent-specific definitions may be omitted",
        },
        {
          source: "history",
          status: "unsupported",
          detail: "No structured history adapter is available for opencode",
        },
      ],
    },
  ],
};

export const unavailableAccessReport: AccessReport = {
  generatedAtUnixMs: 1788220800000,
  status: "unavailable",
  detail:
    "The calling operating-system user could not be identified for this assessment.",
  agents: [],
};

export const remoteConfig = `organization: Acme Engineering
gateway:
  endpoint: https://gateway.example.internal
policies:
  mode: enforced
`;
