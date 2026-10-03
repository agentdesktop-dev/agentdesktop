import type { ColorMode } from "@agentdesktop/ui";

export interface Settings {
  openOnStartup: boolean;
  colorMode: ColorMode;
}

export interface Bootstrap {
  settings: Settings;
  version: string;
  platform: string;
}

export interface PlatformCapabilities {
  os: string;
}

/**
 * The daemon's own live connection to the controller, independent of local
 * process health. `null` for a standalone (unmanaged) daemon, which has no
 * controller to connect to.
 */
export interface ControllerConnectionStatus {
  connected: boolean;
  /** Last time the controller stream was observed alive. */
  lastSeenUnixSeconds?: number;
  lastError?: ControllerConnectionError;
}

/**
 * Coarse reason for the most recent controller failure. The daemon keeps the
 * full error chain in its logs and never returns it to the local API.
 */
export type ControllerConnectionError =
  | "unreachable"
  | "identityRejected"
  | "sessionExpired"
  | "localError"
  | "unknown";

export interface DaemonInfo {
  version: string;
  scope: "user" | "system";
  configPath: string;
  stateDirectory: string;
  inventoryInterval: string;
  controller: {
    address: string;
    caCertificatePath: string | null;
    heartbeatInterval: string;
  } | null;
  llmProxy?: {
    /** Bound address when `bound`, configured address otherwise. */
    listen: string;
    bound: boolean;
    clientId: string;
    error?: string;
  };
}

export interface ConnectorRuntime {
  mode: string;
  gateway: string;
  platform: PlatformCapabilities;
  daemon: DaemonInfo;
  controller: ControllerConnectionStatus | null;
}

export interface ConnectorSnapshot {
  state: "ready" | "attention" | "offline";
  detail: string | null;
  runtime: ConnectorRuntime | null;
}

export interface ManagedDeviceSnapshot {
  configured: boolean;
  organizationName: string | null;
  enrollment: string;
  detail: string | null;
}

export interface McpServer {
  name: string;
  transport: string;
  command?: string;
  url?: string;
  enabled: boolean;
  source: string;
}

export interface Skill {
  path: string;
  frontMatter: Record<string, unknown>;
}

export interface DiscoveredAgent {
  kind: string;
  executable: string;
  version: string | null;
  mcpServers?: McpServer[];
  skills?: Skill[];
}

export interface Discovery {
  agents: DiscoveredAgent[];
  modelRuntimes?: Array<{
    kind: string;
    models: Array<{ name: string }>;
  }>;
}
