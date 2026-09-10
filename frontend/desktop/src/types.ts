export interface Settings {
  openOnStartup: boolean;
}

export interface Bootstrap {
  settings: Settings;
  version: string;
  platform: string;
}

export interface PlatformCapabilities {
  os: string;
}

export interface ConnectorRuntime {
  version: string;
  mode: string;
  gateway: string;
  platform: PlatformCapabilities;
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

export type AccessCategory =
  | "filesystem"
  | "network"
  | "execution"
  | "externalService"
  | "credential"
  | "browser";

export type AccessOperation = "read" | "write" | "execute" | "connect" | "use";
export type AccessDecision =
  | "allow"
  | "ask"
  | "deny"
  | "autoReview"
  | "unknown";
export type AccessEnforcement = "sandbox" | "harness" | "none" | "unknown";
export type AccessSourceKind = "configuration" | "default" | "mcp" | "history";
export type AccessSeverity = "notice" | "warning" | "critical";
export type AccessRuleMechanism =
  | "vscodeUrlAutoApprove"
  | "claudePermission"
  | "claudeSandboxDomain";
export type NetworkRuleDecision = "allow" | "ask" | "deny";
export type NetworkRuleChange =
  | {
      agentKind: string;
      operation: "add";
      resource: string;
      decision: NetworkRuleDecision;
    }
  | {
      agentKind: string;
      operation: "setDecision";
      ruleId: string;
      decision: NetworkRuleDecision;
    }
  | {
      agentKind: string;
      operation: "remove";
      ruleId: string;
    };
export type AccessCoverageStatus =
  | "complete"
  | "partial"
  | "unavailable"
  | "unsupported";

export interface AccessSource {
  kind: AccessSourceKind;
  path?: string;
}

export interface AccessCapability {
  category: AccessCategory;
  resource: string;
  operations: AccessOperation[];
  decision: AccessDecision;
  enforcement: AccessEnforcement;
  workspace?: string;
  source: AccessSource;
  rule?: {
    id: string;
    mechanism: AccessRuleMechanism;
  };
  detail?: string;
}

export interface AccessObservation {
  category: AccessCategory;
  resource: string;
  operations: AccessOperation[];
  workspace?: string;
  count: number;
  sessionCount: number;
  resourceCount: number;
  workspaceCount: number;
  evidenceUpdatedAtUnixMs?: number;
  confidence: "high" | "heuristic";
  source: AccessSource;
}

export interface AccessFinding {
  severity: AccessSeverity;
  title: string;
  detail: string;
  category: AccessCategory;
  workspace?: string;
  source?: AccessSource;
}

export interface AccessCoverage {
  source: AccessSourceKind;
  status: AccessCoverageStatus;
  detail: string;
}

export interface AgentAccessReport {
  kind: string;
  executable: string;
  version: string | null;
  userHome: string;
  capabilities?: AccessCapability[];
  observations?: AccessObservation[];
  findings?: AccessFinding[];
  coverage?: AccessCoverage[];
}

export interface AccessReport {
  generatedAtUnixMs: number;
  status: "ready" | "unavailable";
  detail?: string;
  agents: AgentAccessReport[];
}
