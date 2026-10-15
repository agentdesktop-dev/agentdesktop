import {
  emptyUsageTotals,
  type UsageBucket,
  type UsageGroup,
  type UsageTotals,
} from "@agentdesktop/ui";

import type {
  ControllerSettings,
  DaemonConfigDocument,
  Device,
  DeviceDetail,
  Overview,
  UsageReport,
} from "../types";

const now = Math.floor(Date.now() / 1000);

export const macDevice: Device = {
  id: "device-mac-12345678",
  hostname: "dev-mac",
  os: "macos",
  architecture: "arm64",
  agent_version: "0.1.0",
  created_at: now - 86400 * 14,
  last_seen_at: now - 12,
  enrolled_by_issuer: "https://issuer.example",
  enrolled_by_subject: "developer@example.com",
  config_revision: 4,
  config_state: 1,
  config_error: null,
  config_updated_at: now - 45,
  discovery_count: 2,
  installed_tools: ["vscode", "claude-code", "codex"],
  browser_pac: {
    state: "found",
    url: "https://pac.corp.example/ai-egress.pac",
    sha256: "3f9a2c71d04be8a6c5e1f27d9b03a4c8e6f1d2a7b9c0e3f4a5b6c7d8e9f0a1b2",
    size: 435,
    fetch_error: null,
    compliance: { state: "compliant", reason: null },
  },
};

export const linuxDevice: Device = {
  ...macDevice,
  id: "device-linux-87654321",
  hostname: "build-linux",
  os: "linux",
  architecture: "x86_64",
  last_seen_at: null,
  config_revision: null,
  config_state: null,
  config_updated_at: null,
  installed_tools: ["opencode"],
  browser_pac: {
    state: "absent",
    url: null,
    sha256: null,
    size: null,
    fetch_error: null,
    compliance: { state: "not_compliant", reason: "no PAC in browser policy" },
  },
};

export const failedDevice: Device = {
  ...macDevice,
  id: "device-failed-13572468",
  hostname: "design-windows",
  os: "windows",
  architecture: "x86_64",
  config_state: 2,
  config_error: "The managed configuration contains an unsupported key.",
  browser_pac: {
    state: "not_reported",
    url: null,
    sha256: null,
    size: null,
    fetch_error: null,
    compliance: { state: "not_reported", reason: null },
  },
};

export const devices = [macDevice, linuxDevice, failedDevice];

export const overview: Overview = {
  total_devices: 3,
  online_devices: 2,
  offline_devices: 1,
  config_failures: 1,
  active_revision: 4,
  recent_devices: devices,
  browser_pac: {
    found: 1,
    absent: 1,
    differs: 0,
    not_reported: 1,
    expected_pac_url: "https://pac.corp.example/ai-egress.pac",
    compliant: 1,
    not_compliant: 1,
    versions: [
      {
        url: "https://pac.corp.example/ai-egress.pac",
        sha256:
          "3f9a2c71d04be8a6c5e1f27d9b03a4c8e6f1d2a7b9c0e3f4a5b6c7d8e9f0a1b2",
        devices: 1,
      },
    ],
  },
};

const deviceMcpServers = [
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
];

const deviceSkills = [
  {
    path: "/Users/developer/.agents/skills/release/SKILL.md",
    frontMatter: {
      name: "Release workflow",
      description:
        "Coordinates versioning, validation, release notes, and rollout across repositories.",
    },
  },
  {
    path: "/Users/developer/.agents/skills/incident-response/SKILL.md",
    frontMatter: {
      name: "Incident response",
      description:
        "Triages production incidents and prepares evidence for the incident commander.",
    },
  },
  {
    path: "/Users/developer/.agents/skills/architecture-review/SKILL.md",
    frontMatter: {
      name: "Architecture review",
      description:
        "Reviews module boundaries, coupling, and operational tradeoffs.",
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
        "Builds staged migrations with compatibility gates and rollback points.",
    },
  },
  {
    path: "/Users/developer/.agents/skills/performance-diagnosis/SKILL.md",
    frontMatter: {
      name: "Performance diagnosis",
      description:
        "Captures profiles and validates improvements against a reproducible baseline.",
    },
  },
  {
    path: "/Users/developer/.agents/skills/accessibility-review/SKILL.md",
    frontMatter: {
      name: "Accessibility review",
      description:
        "Audits keyboard flow, semantics, contrast, reflow, and screen reader behavior.",
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
        "Plans and validates framework upgrades while minimizing unrelated churn.",
    },
  },
  {
    path: "/Users/developer/.agents/skills/documentation/SKILL.md",
    frontMatter: {
      name: "Documentation maintenance",
      description:
        "Keeps setup guides, examples, and architecture references current.",
    },
  },
  {
    path: "/Users/developer/.agents/skills/onboarding/SKILL.md",
    frontMatter: {
      name: "Onboarding review",
      description:
        "Tests first-run workflows and improves defaults and recovery guidance.",
    },
  },
];

export const deviceDetail: DeviceDetail = {
  ...macDevice,
  programs: [
    {
      program: "claude-code",
      state: "applied",
      detail: "",
      revision: 12,
      updated_at: now - 60,
    },
    {
      program: "vscode",
      state: "unchanged",
      detail: "",
      revision: 12,
      updated_at: now - 60,
    },
    {
      program: "copilot",
      state: "removed",
      detail: "",
      revision: 12,
      updated_at: now - 60,
    },
  ],
  programs_reported: true,
  discoveries: [
    {
      kind: "vscode",
      version: "1.104.0",
      path: "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
      mcp_servers: deviceMcpServers,
      skills: deviceSkills,
    },
    {
      kind: "claude-code",
      version: "2.1.3",
      path: "/Users/developer/.local/bin/claude",
      mcp_servers: deviceMcpServers.slice(0, 3),
      skills: deviceSkills.slice(0, 5),
    },
  ],
  model_runtimes: [
    {
      kind: "ollama",
      models: [{ name: "gemma3:4b" }, { name: "qwen3:8b" }],
    },
  ],
  pac_fetches: [
    {
      url: "https://pac.corp.example/ai-egress.pac",
      sha256:
        "3f9a2c71d04be8a6c5e1f27d9b03a4c8e6f1d2a7b9c0e3f4a5b6c7d8e9f0a1b2",
      size: 435,
    },
  ],
  browser_pacs: [
    {
      browser: "chrome",
      packaging: "system",
      pac_url: "https://pac.corp.example/ai-egress.pac",
      recommended: false,
      locked: false,
      source: "/Library/Managed Preferences/com.google.Chrome.plist",
      detail: null,
    },
    {
      browser: "firefox",
      packaging: "system",
      pac_url: "https://pac.corp.example/ai-egress.pac",
      recommended: false,
      locked: true,
      source: "/Library/Managed Preferences/org.mozilla.firefox.plist",
      detail: null,
    },
  ],
  recent_events: [
    {
      id: "event-session",
      timestamp_unix_ms: (now - 15) * 1000,
      event_type: "session.new",
      payload: { clientId: "vscode", sessionId: "session-123" },
    },
    {
      id: "event-tool",
      timestamp_unix_ms: (now - 30) * 1000,
      event_type: "tool.use",
      payload: {
        clientId: "claude-code",
        toolName: "Read",
        toolUseId: "tool-456",
        toolInput: { path: "/workspace/src/main.rs" },
      },
    },
  ],
};

export const failedDeviceDetail: DeviceDetail = {
  ...deviceDetail,
  ...failedDevice,
  programs: [
    {
      program: "claude-code",
      state: "blocked",
      detail: "not applied: vscode conflicted",
      revision: 12,
      updated_at: now - 60,
    },
    {
      program: "copilot",
      state: "inactive",
      detail:
        "local LLM proxy not available; see llmProxy.error in daemon-info, or set daemon.llmProxy.listen; the managed entries stay and the tool fails until the proxy is back (whenProxyUnavailable: failClosed)",
      revision: 12,
      updated_at: now - 60,
    },
    {
      program: "vscode",
      state: "conflict",
      detail:
        "settings at /Users/developer/Library/Application Support/Code/User/settings.json: conflicting or invalid existing configuration",
      revision: 12,
      updated_at: now - 60,
    },
    {
      program: "codex",
      state: "failed",
      detail: "multiple providers plan to modify /etc/codex/config.toml",
      revision: 12,
      updated_at: now - 60,
    },
    {
      program: "grok",
      state: "unspecified",
      detail: "",
      revision: 12,
      updated_at: now - 60,
    },
  ],
};

export const oldAgentDeviceDetail: DeviceDetail = {
  ...deviceDetail,
  programs: [],
  programs_reported: false,
};

export const emptyDeviceDetail: DeviceDetail = {
  ...deviceDetail,
  discoveries: [],
  model_runtimes: [],
  browser_pacs: [],
  pac_fetches: [],
  browser_pac: {
    state: "not_reported",
    url: null,
    sha256: null,
    size: null,
    fetch_error: null,
    compliance: null,
  },
  recent_events: [],
  programs: [],
  programs_reported: true,
};

export const controllerSettings: ControllerSettings = {
  fleet_listen: "0.0.0.0:8080",
  admin_listen: "127.0.0.1:8081",
  oidc_enabled: true,
  tls_enabled: true,
  gateway_jwt_enabled: true,
  usage_enabled: true,
};

const day = 86400;
const startOfToday = (() => {
  const date = new Date();
  date.setHours(0, 0, 0, 0);
  return Math.floor(date.getTime() / 1000);
})();

function usageTotals(totals: Partial<UsageTotals>): UsageTotals {
  return { ...emptyUsageTotals, ...totals };
}

function sumUsage(items: Array<{ totals: UsageTotals }>): UsageTotals {
  const sum = { ...emptyUsageTotals };
  for (const { totals } of items) {
    for (const key of Object.keys(sum) as Array<keyof UsageTotals>) {
      sum[key] += totals[key];
    }
  }
  return sum;
}

const dailyCost = [4.12, 6.8, 5.35, 9.9, 7.45, 3.2, 8.15];

export const usageBuckets: UsageBucket[] = dailyCost.map((cost, index) => ({
  startUnixSeconds: startOfToday - (dailyCost.length - 1 - index) * day,
  totals: usageTotals({
    requests: Math.round(cost * 41),
    errors: index === 3 ? 4 : 0,
    inputTokens: Math.round(cost * 205_000),
    outputTokens: Math.round(cost * 19_500),
    cacheReadTokens: Math.round(cost * 140_000),
    costUsd: cost,
    subscriptionCostUsd: Math.round(cost * 38) / 100,
    subscriptionRequests: Math.round(cost * 9),
    unpricedRequests: index === dailyCost.length - 1 ? 12 : 0,
  }),
}));

const usageTools: UsageGroup[] = [
  {
    key: "claude-code",
    totals: usageTotals({
      requests: 1102,
      inputTokens: 6_420_000,
      outputTokens: 512_000,
      costUsd: 21.84,
      subscriptionCostUsd: 17.4,
      subscriptionRequests: 405,
    }),
  },
  {
    key: "codex",
    totals: usageTotals({
      requests: 401,
      inputTokens: 2_210_000,
      outputTokens: 240_000,
      costUsd: 12.1,
    }),
  },
  {
    key: "vscode-copilot",
    totals: usageTotals({
      requests: 260,
      inputTokens: 1_190_000,
      outputTokens: 96_000,
      costUsd: 6.03,
    }),
  },
  {
    key: "",
    totals: usageTotals({
      requests: 12,
      inputTokens: 61_000,
      outputTokens: 4_200,
      unpricedRequests: 12,
    }),
  },
];

export const usageReport: UsageReport = {
  enabled: true,
  range: "7d",
  fromUnixSeconds: usageBuckets[0].startUnixSeconds,
  toUnixSeconds: startOfToday + day,
  bucketSeconds: day,
  totals: sumUsage(usageBuckets),
  previousTotals: usageTotals({
    requests: 1630,
    inputTokens: 8_100_000,
    outputTokens: 742_000,
    costUsd: 38.4,
    subscriptionCostUsd: 14.1,
    subscriptionRequests: 300,
  }),
  activeUsers: 3,
  activeDevices: 4,
  buckets: usageBuckets,
  breakdowns: {
    user: [
      {
        key: "user-maria",
        label: "maria@example.com",
        totals: usageTotals({
          requests: 941,
          inputTokens: 5_300_000,
          outputTokens: 420_000,
          costUsd: 19.6,
          subscriptionCostUsd: 17.4,
          subscriptionRequests: 405,
        }),
      },
      {
        key: "developer@example.com",
        label: "developer@example.com",
        totals: usageTotals({
          requests: 822,
          inputTokens: 4_520_000,
          outputTokens: 428_000,
          costUsd: 20.37,
        }),
      },
      {
        key: "",
        totals: usageTotals({
          requests: 12,
          inputTokens: 61_000,
          outputTokens: 4_200,
          unpricedRequests: 12,
        }),
      },
    ],
    device: [
      {
        key: macDevice.id,
        label: macDevice.hostname,
        totals: usageTotals({
          requests: 1210,
          inputTokens: 6_900_000,
          outputTokens: 560_000,
          costUsd: 26.2,
          subscriptionCostUsd: 17.4,
          subscriptionRequests: 405,
        }),
      },
      {
        key: linuxDevice.id,
        label: linuxDevice.hostname,
        totals: usageTotals({
          requests: 553,
          inputTokens: 2_920_000,
          outputTokens: 288_000,
          costUsd: 13.77,
        }),
      },
      {
        key: "5f0c2a8e-2d6b-4f43-9a51-3c8e0f7d2b19",
        totals: usageTotals({
          requests: 12,
          inputTokens: 61_000,
          outputTokens: 4_200,
          unpricedRequests: 12,
        }),
      },
    ],
    tool: usageTools,
    model: [
      {
        key: "claude-sonnet-4-5-20250929",
        totals: usageTotals({
          requests: 1102,
          inputTokens: 6_420_000,
          outputTokens: 512_000,
          costUsd: 21.84,
          subscriptionCostUsd: 17.4,
          subscriptionRequests: 405,
        }),
      },
      {
        key: "gpt-5-codex",
        totals: usageTotals({
          requests: 661,
          inputTokens: 3_400_000,
          outputTokens: 336_000,
          costUsd: 18.13,
        }),
      },
      {
        key: "llama-3.3-70b",
        totals: usageTotals({
          requests: 12,
          inputTokens: 61_000,
          outputTokens: 4_200,
          unpricedRequests: 12,
        }),
      },
    ],
    provider: [
      {
        key: "anthropic",
        totals: usageTotals({
          requests: 1102,
          inputTokens: 6_420_000,
          outputTokens: 512_000,
          costUsd: 21.84,
          subscriptionCostUsd: 17.4,
          subscriptionRequests: 405,
        }),
      },
      {
        key: "openai",
        totals: usageTotals({
          requests: 673,
          inputTokens: 3_461_000,
          outputTokens: 340_200,
          costUsd: 18.13,
          unpricedRequests: 12,
        }),
      },
    ],
  },
  filterOptions: {
    user: [
      { key: "user-maria", label: "maria@example.com" },
      { key: "developer@example.com", label: "developer@example.com" },
      { key: "user-sam", label: "sam@example.com" },
      { key: "" },
    ],
    device: [
      { key: macDevice.id, label: macDevice.hostname },
      { key: linuxDevice.id, label: linuxDevice.hostname },
      { key: "5f0c2a8e-2d6b-4f43-9a51-3c8e0f7d2b19" },
    ],
  },
};

export const emptyUsageReport: UsageReport = {
  ...usageReport,
  totals: emptyUsageTotals,
  previousTotals: emptyUsageTotals,
  activeUsers: 0,
  activeDevices: 0,
  buckets: usageBuckets.map((bucket) => ({
    ...bucket,
    totals: emptyUsageTotals,
  })),
  breakdowns: {
    user: [],
    device: [],
    tool: [],
    model: [],
    provider: [],
  },
  filterOptions: { user: [], device: [] },
};

export const disabledUsageReport: UsageReport = {
  ...emptyUsageReport,
  enabled: false,
};

export const deviceUsageReport: UsageReport = {
  ...usageReport,
  range: "30d",
  totals: sumUsage(usageTools),
  breakdowns: {
    tool: usageTools,
    model: usageReport.breakdowns.model,
  },
};

export const activeDaemonConfig: DaemonConfigDocument = {
  llmGateway: {
    url: "https://gateway.example.internal",
    authentication: { type: "controllerJwt", audience: "agentgateway" },
  },
  telemetry: { events: ["session.new", "tool.use.input"] },
  programs: {
    claudeCode: { permissions: { defaultMode: "plan" } },
    openCode: { useLlmGateway: false, autoupdate: false },
  },
};

export const copilotFailOpenDaemonConfig: DaemonConfigDocument = {
  llmGateway: {
    url: "https://gateway.example.internal",
    proxyUrl: "https://gateway.example.internal/copilot-proxy",
    whenProxyUnavailable: "failOpen",
    authentication: { type: "controllerJwt", audience: "agentgateway" },
  },
  programs: {
    copilot: { models: { "gpt-4.1": {} } },
    vscode: { copilotChat: "githubModels" },
  },
};

export const sandboxDaemonConfig: DaemonConfigDocument = {
  sandbox: {
    network: {
      allowedDomains: ["api.github.com", "registry.npmjs.org"],
    },
    filesystem: {
      writable: ["/tmp/build-cache", "/opt/project/output"],
      denied: ["~/.ssh", "~/.aws"],
    },
  },
  programs: {
    claudeCode: {},
    codex: {},
  },
};

export const differsDeviceDetail: DeviceDetail = {
  ...deviceDetail,
  browser_pac: {
    state: "differs",
    url: null,
    sha256: null,
    size: null,
    fetch_error: null,
    compliance: {
      state: "not_compliant",
      reason: "browsers set different PACs",
    },
  },
  browser_pacs: [
    { ...deviceDetail.browser_pacs[0] },
    {
      ...deviceDetail.browser_pacs[1],
      pac_url: "https://old-pac.corp.example/ai-egress.pac",
    },
  ],
  pac_fetches: [
    ...deviceDetail.pac_fetches,
    {
      url: "https://old-pac.corp.example/ai-egress.pac",
      error: "error sending request",
    },
  ],
};
