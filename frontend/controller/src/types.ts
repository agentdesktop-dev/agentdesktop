import type {
  UsageBucket,
  UsageDimension,
  UsageGroup,
  UsageTotals,
} from "@agentdesktop/ui";

export type Device = {
  id: string;
  hostname: string;
  os: string;
  architecture: string;
  agent_version: string;
  created_at: number;
  last_seen_at: number | null;
  enrolled_by_issuer: string;
  enrolled_by_subject: string;
  config_revision: number | null;
  config_state: number | null;
  config_error: string | null;
  config_updated_at: number | null;
  discovery_count: number;
  installed_tools: string[];
  browser_pac: DeviceBrowserPac;
};

export type BrowserPacCompliance = {
  state: "compliant" | "not_compliant" | "not_reported";
  reason: string | null;
};

/** The PAC browser policy sets, one value per device. */
export type DeviceBrowserPac = {
  state: "found" | "absent" | "differs" | "not_reported";
  url: string | null;
  /** SHA-256 of the PAC this device fetched from `url`: its identity. */
  sha256: string | null;
  size: number | null;
  fetch_error: string | null;
  /** Null without a fleet compliance profile (browserEgress.compliance). */
  compliance: BrowserPacCompliance | null;
};

export type BrowserPacRecord = {
  browser: string;
  packaging: string;
  pac_url: string | null;
  recommended: boolean;
  locked: boolean;
  source: string | null;
  detail: string | null;
};

export type ProgramStatus = {
  program: string;
  state:
    | "applied"
    | "unchanged"
    | "removed"
    | "conflict"
    | "inactive"
    | "blocked"
    | "failed"
    | "unspecified";
  detail: string;
  revision: number;
  updated_at: number;
};

export type DeviceDetail = Device & {
  programs: ProgramStatus[];
  programs_reported: boolean | null;
  discoveries: Array<{
    kind: string;
    version: string;
    path: string;
    mcp_servers?: Array<{
      name: string;
      transport: string;
      command?: string;
      url?: string;
      enabled: boolean;
      source: string;
    }>;
    skills?: Array<{
      path: string;
      frontMatter: Record<string, unknown>;
    }>;
  }>;
  model_runtimes: Array<{
    kind: string;
    models: Array<{ name: string }>;
  }>;
  browser_pacs: BrowserPacRecord[];
  pac_fetches: Array<{
    url: string;
    sha256?: string;
    size?: number;
    error?: string;
  }>;
  recent_events: Array<{
    id: string;
    timestamp_unix_ms: number;
    event_type: string;
    payload: {
      clientId?: string;
      toolName?: string;
      toolUseId?: string;
      toolInput?: unknown;
      sessionId?: string;
    };
  }>;
};

export type Overview = {
  total_devices: number;
  online_devices: number;
  offline_devices: number;
  config_failures: number;
  active_revision: number | null;
  recent_devices: Device[];
  browser_pac: {
    found: number;
    absent: number;
    differs: number;
    not_reported: number;
    /** Null without a fleet compliance profile. */
    expected_pac_url: string | null;
    compliant: number;
    not_compliant: number;
    /** Devices that found a PAC, grouped by URL and SHA-256 of what they fetched. */
    versions: Array<{ url: string; sha256: string | null; devices: number }>;
  };
};

export type ControllerSettings = {
  fleet_listen: string;
  admin_listen: string;
  oidc_enabled: boolean;
  tls_enabled: boolean;
  gateway_jwt_enabled: boolean;
  usage_enabled: boolean;
};

export type UsageRange = "24h" | "7d" | "30d" | "90d";

/** `GET /api/v1/usage`. Unlike other admin endpoints, this one is camelCase. */
export type UsageReport = {
  enabled: boolean;
  range: UsageRange;
  fromUnixSeconds: number;
  toUnixSeconds: number;
  bucketSeconds: number;
  totals: UsageTotals;
  previousTotals: UsageTotals;
  activeUsers: number;
  activeDevices: number;
  buckets: UsageBucket[];
  breakdowns: Partial<Record<UsageDimension, UsageGroup[]>>;
  /** Users whatever the user and device filters; devices of the chosen user. */
  filterOptions: Record<"user" | "device", UsageFilterOption[]>;
};

export type UsageFilterOption = Pick<UsageGroup, "key" | "label">;

export type AgentKind =
  | "claudeCode"
  | "claudeDesktop"
  | "codex"
  | "openCode"
  | "grok"
  | "copilot"
  | "vscode";

export type AgentDraft = {
  kind: AgentKind;
  useGateway: boolean;
  settings: string;
};

export type SandboxConfigDocument = {
  network?: {
    allowedDomains?: string[];
  };
  filesystem?: {
    writable?: string[];
    denied?: string[];
  };
};

export type DaemonConfigDocument = {
  llmGateway?: {
    url: string;
    proxyUrl?: string;
    whenProxyUnavailable?: "failClosed" | "failOpen";
    authentication?: {
      type: string;
      audience?: string;
    };
  };
  sandbox?: SandboxConfigDocument;
  telemetry?: {
    events?: string[];
  };
  programs?: Partial<Record<AgentKind, Record<string, unknown>>>;
};

export type ActiveDaemonConfig = {
  config: DaemonConfigDocument | null;
};
