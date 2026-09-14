import type {
  Bootstrap,
  ConnectorSnapshot,
  DaemonInfo,
  Discovery,
  ManagedDeviceSnapshot,
} from "../types";

export const bootstrap: Bootstrap = {
  settings: { openOnStartup: true, colorMode: "system" },
  version: "0.1.0",
  platform: "macos",
};

export const standaloneDaemonInfo: DaemonInfo = {
  version: "0.1.0",
  scope: "user",
  configPath: "/Users/developer/.config/agentdesktop/config.yaml",
  stateDirectory: "/Users/developer/.local/state/agentdesktop",
  inventoryInterval: "15m",
  controller: null,
};

export const managedDaemonInfo: DaemonInfo = {
  version: "0.1.1",
  scope: "system",
  configPath: "/etc/agentdesktop/config.yaml",
  stateDirectory: "/var/lib/agentdesktop",
  inventoryInterval: "15m",
  controller: {
    address: "https://controller.example.internal:8443/",
    caCertificatePath: "/etc/agentdesktop/controller-ca.pem",
    heartbeatInterval: "30s",
  },
};

export const standaloneConnector = {
  state: "ready",
  detail: null,
  runtime: {
    mode: "standalone",
    gateway: "not-configured",
    platform: { os: "macos" },
    daemon: standaloneDaemonInfo,
    controller: null,
  },
} satisfies ConnectorSnapshot;

export const managedConnector = {
  state: "ready",
  detail: null,
  runtime: {
    mode: "managed",
    gateway: "reachable",
    platform: { os: "macos" },
    daemon: managedDaemonInfo,
    controller: {
      connected: true,
      lastConnectedUnixSeconds: Date.now() / 1000,
    },
  },
} satisfies ConnectorSnapshot;

// A device that is fully healthy locally (daemon running, enrollment
// approved) but whose connection to the controller is currently down —
// the case that motivated a dedicated status row instead of folding this
// into "Local daemon".
export const reconnectingConnector = {
  state: "ready",
  detail: null,
  runtime: {
    mode: "managed",
    gateway: "reachable",
    platform: { os: "macos" },
    daemon: managedDaemonInfo,
    controller: {
      connected: false,
      lastConnectedUnixSeconds: Date.now() / 1000 - 900,
      lastError: "controller connection failed: connection refused",
    },
  },
} satisfies ConnectorSnapshot;

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
      kind: "codex",
      executable: "/opt/homebrew/bin/codex",
      version: "0.42.0",
      mcpServers: mcpServers.slice(6),
      skills: skills.slice(8),
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
