import type {
  ControllerSettings,
  DaemonConfigDocument,
  Device,
  DeviceDetail,
  Overview,
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
};

export const failedDevice: Device = {
  ...macDevice,
  id: "device-failed-13572468",
  hostname: "design-windows",
  os: "windows",
  architecture: "x86_64",
  config_state: 2,
  config_error: "The managed configuration contains an unsupported key.",
};

export const devices = [macDevice, linuxDevice, failedDevice];

export const overview: Overview = {
  total_devices: 3,
  online_devices: 2,
  offline_devices: 1,
  config_failures: 1,
  active_revision: 4,
  recent_devices: devices,
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
};

export const emptyDeviceDetail: DeviceDetail = {
  ...deviceDetail,
  discoveries: [],
  recent_events: [],
};

export const controllerSettings: ControllerSettings = {
  fleet_listen: "0.0.0.0:8080",
  admin_listen: "127.0.0.1:8081",
  oidc_enabled: true,
  tls_enabled: true,
  gateway_jwt_enabled: true,
};

export const activeDaemonConfig: DaemonConfigDocument = {
  inferenceGateway: {
    url: "https://gateway.example.internal",
    authentication: { type: "controllerJwt", audience: "agentgateway" },
  },
  telemetry: { events: ["session.new", "tool.use.input"] },
  programs: {
    claudeCode: { permissions: { defaultMode: "plan" } },
    openCode: { useInferenceGateway: false, autoupdate: false },
  },
};
