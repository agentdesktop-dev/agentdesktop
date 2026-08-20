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
};

export type DeviceDetail = Device & {
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
};

export type ControllerSettings = {
  fleet_listen: string;
  admin_listen: string;
  oidc_enabled: boolean;
  tls_enabled: boolean;
  gateway_jwt_enabled: boolean;
};

export type AgentKind = "claudeCode" | "claudeDesktop" | "codex" | "openCode";

export type AgentDraft = {
  kind: AgentKind;
  useGateway: boolean;
  settings: string;
};

export type DaemonConfigDocument = {
  inferenceGateway?: {
    url: string;
    authentication?: {
      type: string;
      audience?: string;
    };
  };
  telemetry?: {
    events?: string[];
  };
  programs?: Partial<Record<AgentKind, Record<string, unknown>>>;
};

export type ActiveDaemonConfig = {
  config: DaemonConfigDocument | null;
};
