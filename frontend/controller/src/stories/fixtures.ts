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

export const deviceDetail: DeviceDetail = {
  ...macDevice,
  discoveries: [
    {
      kind: "vscode",
      version: "1.104.0",
      path: "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
      mcp_servers: [
        {
          name: "github-enterprise",
          transport: "stdio",
          command: "/usr/local/bin/github-mcp-server --read-only",
          enabled: true,
          source: "VS Code settings",
        },
      ],
      skills: [
        {
          path: "/Users/developer/.agents/skills/release/SKILL.md",
          frontMatter: {
            name: "Release workflow",
            description: "Coordinates a release across repositories.",
          },
        },
      ],
    },
    {
      kind: "claude-code",
      version: "2.1.3",
      path: "/Users/developer/.local/bin/claude",
      mcp_servers: [],
      skills: [],
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
