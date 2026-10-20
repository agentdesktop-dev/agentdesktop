import { emptyUsageTotals, type UsageTotals } from "@agentdesktop/ui";

import type {
  Bootstrap,
  ConnectorSnapshot,
  DaemonInfo,
  DeviceUsage,
  Discovery,
  ManagedDeviceSnapshot,
  ProgramManagement,
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
  llmProxy: { listen: "127.0.0.1:18095", bound: true, clientId: "copilot-cli" },
};

export const proxyUnboundDaemonInfo: DaemonInfo = {
  ...standaloneDaemonInfo,
  llmProxy: {
    listen: "127.0.0.1:18095",
    bound: false,
    clientId: "copilot-cli",
    error: "Address already in use (os error 98)",
  },
};

export const usageReceiverDaemonInfo: DaemonInfo = {
  ...standaloneDaemonInfo,
  usageReceiver: { listen: "127.0.0.1:4319", bound: true },
};

export const containmentDaemonInfo: DaemonInfo = {
  ...standaloneDaemonInfo,
  containment: {
    executor: "C:\\Program Files\\mxc\\wxc-exec.exe",
    engine: "mxc",
    available: false,
    error: "executor not found: C:\\Program Files\\mxc\\wxc-exec.exe",
    managedSettingsSource: null,
    managedHooksLocked: false,
    managedMcpLocked: false,
  },
};

export const usageReceiverOffDaemonInfo: DaemonInfo = {
  ...standaloneDaemonInfo,
  usageReceiver: {
    listen: "127.0.0.1:4319",
    bound: false,
    error:
      "read usage token from /Users/developer/.config/agentdesktop/usage-token: No such file or directory (os error 2)",
  },
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
      lastSeenUnixSeconds: Date.now() / 1000,
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
      lastSeenUnixSeconds: Date.now() / 1000 - 900,
      lastError: "unreachable",
    },
  },
} satisfies ConnectorSnapshot;

export const offlineConnector: ConnectorSnapshot = {
  state: "offline",
  detail: "The agentdesktop daemon is unavailable.",
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

const day = 86400;
const startOfToday = (() => {
  const date = new Date();
  date.setHours(0, 0, 0, 0);
  return Math.floor(date.getTime() / 1000);
})();

function usageTotals(totals: Partial<UsageTotals>): UsageTotals {
  return { ...emptyUsageTotals, ...totals };
}

const dailyTokens = [310_000, 0, 820_000, 1_240_000, 560_000, 905_000, 410_000];

export const deviceUsage: DeviceUsage = {
  enabled: true,
  fromUnixSeconds: startOfToday - 6 * day,
  toUnixSeconds: startOfToday + day,
  bucketSeconds: day,
  totals: usageTotals({
    requests: 912,
    errors: 3,
    inputTokens: 3_910_000,
    outputTokens: 335_000,
    cacheReadTokens: 2_600_000,
    costUsd: 9.84,
    subscriptionCostUsd: 14.62,
    subscriptionRequests: 520,
  }),
  buckets: dailyTokens.map((tokens, index) => ({
    startUnixSeconds: startOfToday - (6 - index) * day,
    totals: usageTotals({
      requests: Math.round(tokens / 4700),
      inputTokens: Math.round(tokens * 0.92),
      outputTokens: Math.round(tokens * 0.08),
      costUsd: Math.round(tokens / 4000) / 100,
      subscriptionCostUsd: Math.round(tokens / 2700) / 100,
      subscriptionRequests: Math.round(tokens / 8200),
    }),
  })),
  byTool: [
    {
      key: "claude-code",
      totals: usageTotals({
        requests: 520,
        inputTokens: 2_480_000,
        outputTokens: 210_000,
        subscriptionCostUsd: 14.62,
        subscriptionRequests: 520,
      }),
    },
    {
      key: "codex",
      totals: usageTotals({
        requests: 392,
        inputTokens: 1_430_000,
        outputTokens: 125_000,
        costUsd: 9.84,
      }),
    },
  ],
  byModel: [
    {
      key: "claude-sonnet-4-5-20250929",
      totals: usageTotals({
        requests: 520,
        inputTokens: 2_480_000,
        outputTokens: 210_000,
        subscriptionCostUsd: 14.62,
        subscriptionRequests: 520,
      }),
    },
    {
      key: "gpt-5-codex",
      totals: usageTotals({
        requests: 392,
        inputTokens: 1_430_000,
        outputTokens: 125_000,
        costUsd: 9.84,
      }),
    },
  ],
  unmeteredTools: ["vscode-copilot"],
};

export const noDeviceUsage: DeviceUsage = {
  ...deviceUsage,
  totals: emptyUsageTotals,
  buckets: deviceUsage.buckets.map((bucket) => ({
    ...bucket,
    totals: emptyUsageTotals,
  })),
  byTool: [],
  byModel: [],
  unmeteredTools: [],
};

export const usageNotCollected: DeviceUsage = {
  ...noDeviceUsage,
  enabled: false,
};

/** Every configured program managed, as the device user chose. */
export const allProgramsManaged: ProgramManagement = {
  userChoiceAllowed: true,
  choice: { mode: "all" },
  programs: [
    { program: "claude-code", managed: true, state: "unchanged" },
    { program: "codex", managed: true, state: "applied" },
    {
      program: "vscode",
      managed: true,
      state: "conflict",
      detail: "settings.json already sets a value Agent Desktop manages",
    },
  ],
};

/** The device user manages only Claude Code and VS Code. */
export const selectedProgramsManaged: ProgramManagement = {
  userChoiceAllowed: true,
  choice: { mode: "selected", programs: ["claude-code", "vscode"] },
  programs: [
    { program: "claude-code", managed: true, state: "unchanged" },
    { program: "codex", managed: false, state: "opted_out" },
    { program: "vscode", managed: true, state: "applied" },
  ],
};

/** The device user opted out of every configured program. */
export const noProgramsManaged: ProgramManagement = {
  userChoiceAllowed: true,
  choice: { mode: "none" },
  programs: [
    { program: "claude-code", managed: false, state: "opted_out" },
    { program: "codex", managed: false, state: "opted_out" },
    { program: "vscode", managed: false, state: "opted_out" },
  ],
};

/** The policy does not let the device user choose. */
export const organizationManagedPrograms: ProgramManagement = {
  userChoiceAllowed: false,
  choice: { mode: "all" },
  programs: [
    { program: "claude-code", managed: true, state: "unchanged" },
    { program: "codex", managed: true, state: "applied" },
    { program: "grok", managed: true },
  ],
};

/** No configuration, or one that configures no programs. */
export const noConfiguredPrograms: ProgramManagement = {
  userChoiceAllowed: false,
  choice: { mode: "all" },
  programs: [],
};
