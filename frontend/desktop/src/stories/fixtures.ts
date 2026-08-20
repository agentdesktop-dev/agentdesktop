import type {
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

const skills = Array.from({ length: 7 }, (_, index) => ({
  path: `/Users/developer/.agents/skills/workflow-${index + 1}/SKILL.md`,
  frontMatter: {
    name: `Workflow ${index + 1}`,
    description: "Coordinates a multi-stage engineering workflow.",
  },
}));

export const populatedDiscovery: Discovery = {
  agents: [
    {
      kind: "vscode",
      executable:
        "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
      version: "1.104.0",
      mcpServers: [
        {
          name: "github-enterprise",
          transport: "stdio",
          command: "/usr/local/bin/github-mcp-server --read-only",
          enabled: true,
          source: "VS Code settings",
        },
      ],
      skills,
    },
    {
      kind: "claude-code",
      executable: "/Users/developer/.local/bin/claude",
      version: "2.1.3",
      mcpServers: [],
      skills: skills.slice(0, 2),
    },
    {
      kind: "codex",
      executable: "/opt/homebrew/bin/codex",
      version: "0.42.0",
      mcpServers: [],
      skills: [],
    },
  ],
};

export const emptyDiscovery: Discovery = { agents: [] };

export const remoteConfig = `organization: Acme Engineering
gateway:
  endpoint: https://gateway.example.internal
policies:
  mode: enforced
`;
