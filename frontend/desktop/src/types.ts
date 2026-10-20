import type {
  ColorMode,
  UsageBucket,
  UsageGroup,
  UsageTotals,
} from "@agentdesktop/ui";

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
  /** Receiver for usage from a gateway on this machine (`daemon.usage`). */
  usageReceiver?: {
    /** Bound address when `bound`, configured address otherwise. */
    listen: string;
    bound: boolean;
    error?: string;
  };
  /** Sandboxed launch (`agentdesktop run`), when `daemon.containment` is configured. */
  containment?: {
    executor: string;
    available: boolean;
    error?: string;
    managedSettingsSource?: "hklm" | "file" | "unknown" | null;
    managedHooksLocked?: boolean;
    managedMcpLocked?: boolean;
    managedSettingsError?: string;
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

export type UsageRange = "24h" | "7d" | "30d";

/** LLM gateway usage recorded for this device by its controller or daemon. */
export interface DeviceUsage {
  /** False when the controller does not collect usage. */
  enabled: boolean;
  fromUnixSeconds: number;
  toUnixSeconds: number;
  bucketSeconds: number;
  totals: UsageTotals;
  buckets: UsageBucket[];
  byTool: UsageGroup[];
  byModel: UsageGroup[];
  /** Configured tools whose gateway traffic is not metered. */
  unmeteredTools?: string[];
}

/**
 * The configured programs the device user wants Agent Desktop to manage,
 * honored only while the policy allows a user choice. Program IDs are
 * provider IDs such as `claude-code`.
 */
export type ProgramChoice =
  | { mode: "all" }
  | { mode: "none" }
  | { mode: "selected"; programs: string[] };

/** One configured program and its last reconcile outcome. */
export interface ManagedProgram {
  /** Provider ID, for example `claude-code`. */
  program: string;
  /** False when the device user opted out of this program. */
  managed: boolean;
  /**
   * Outcome of the last apply (`applied`, `unchanged`, `opted_out`, …);
   * absent until the program has been applied.
   */
  state?: string;
  detail?: string;
}

/**
 * Which configured programs are managed on this device, and whether the
 * device user may change that.
 */
export interface ProgramManagement {
  userChoiceAllowed: boolean;
  /** The stored choice; ignored while `userChoiceAllowed` is false. */
  choice: ProgramChoice;
  /** Configured programs in reconcile order; empty before a configuration. */
  programs: ManagedProgram[];
}
