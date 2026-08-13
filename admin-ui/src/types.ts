export type EnrollmentStatus = "pending" | "issuing" | "approved" | "rejected";

export interface Settings {
  serverUrl: string;
}

export interface ServerInfo {
  organizationName: string;
  serverUrl: string;
}

export interface Bootstrap {
  settings: Settings;
  server: ServerInfo | null;
  signedIn: boolean;
  connectionError: string | null;
  version: string;
  platform: string;
}

export interface EnrollmentRecord {
  enrollmentId: string;
  status: EnrollmentStatus;
  subject: string;
  username: string | null;
  deviceName: string | null;
  publicKeyFingerprint: string;
  createdAt: string;
  updatedAt: string;
  deviceId: string | null;
}

export interface EnrollmentList {
  status: EnrollmentStatus;
  enrollments: EnrollmentRecord[];
  limited: boolean;
}

export interface FleetSummary {
  pendingEnrollments: number;
  issuingEnrollments: number;
  approvedEnrollments: number;
  rejectedEnrollments: number;
  activeDevices: number;
  revokedDevices: number;
  certificatesExpiring24H: number;
  renewals24H: number;
  generatedAt: string;
}

export interface AdministrativeDevice {
  deviceId: string;
  deviceName: string | null;
  status: "active" | "revoked";
  subject: string;
  username: string | null;
  createdAt: string;
  revokedAt: string | null;
  currentCertificateSerialNumber: string | null;
  currentCertificateNotAfter: string | null;
  certificateCount: number;
  renewalCount: number;
}

export interface DiscoveryConfigSource {
  scope: "user" | "managed";
  source: string;
  format: "json" | "toml";
  status: "parsed" | "invalid" | "oversized" | "symlink_skipped";
  sections: string[];
}

export interface DiscoveryMCPServer {
  name: string;
  scope: "user" | "managed";
  transport: "stdio" | "http" | "sse" | "unknown";
}

export interface DiscoveryNamedResource {
  name: string;
  scope: "user" | "shared";
}

export interface DiscoveryPlugin extends DiscoveryNamedResource {
  state: "enabled" | "configured" | "unknown";
}

export type AgentID = "claude-code" | "claude-desktop" | "codex-cli" | "openclaw" | "vscode-copilot";

export interface DiscoveryAgent {
  id: AgentID;
  installed: boolean;
  version: string | null;
  running: "detected" | "not_detected" | "unknown";
  evidence: string[];
  configSources: DiscoveryConfigSource[];
  mcpServers: DiscoveryMCPServer[];
  skills: DiscoveryNamedResource[];
  plugins: DiscoveryPlugin[];
}

export interface DeviceDiscoveryReport {
  deviceId: string;
  receivedAt: string;
  schemaVersion: number;
  collectorVersion: string;
  platform: "macos" | "windows";
  projectScopes: "not_scanned";
  partial: boolean;
  agents: DiscoveryAgent[];
  issues: Array<{ agentId: string | null; code: string }>;
}

export type InventoryKind = "agent" | "mcp" | "skill" | "plugin";

export interface InventoryCounts {
  activeDevices: number;
  reportingDevices: number;
  agents: number;
  mcpServers: number;
  skills: number;
  plugins: number;
}

export interface InventoryAsset {
  kind: InventoryKind;
  key: string;
  version: string | null;
  detail: string | null;
  deviceCount: number;
  runningCount: number;
}

export interface InventoryPage {
  counts: InventoryCounts;
  kind: InventoryKind;
  assets: InventoryAsset[];
  total: number;
  limit: number;
  offset: number;
  generatedAt: string;
}

export interface InventoryDevice {
  deviceId: string;
  deviceName: string | null;
  subject: string;
  username: string | null;
  status: "active";
  reportReceivedAt: string | null;
}

export interface InventoryDevicePage {
  devices: InventoryDevice[];
  total: number;
  limit: number;
  offset: number;
}

export interface DiscoveryRescanResult {
  requested: number;
  requestedAt: string;
}

export interface AgentPolicyRule {
  agentId: AgentID;
  action: "allow" | "deny";
}

export interface AgentPolicy {
  schemaVersion: 1;
  rules: AgentPolicyRule[];
  configured: boolean;
  enforcement: "not_available";
  updatedBy: string | null;
  updatedAt: string;
}

export interface DeviceList {
  devices: AdministrativeDevice[];
  limited: boolean;
}

export interface ApprovalResult {
  enrollmentId: string;
  status: "approved";
  deviceId: string;
  notAfter: string;
}

export interface RevocationResult {
  deviceId: string;
  status: "revoked";
  revokedAt: string;
}

export type EnrollmentAction = "approve" | "reject" | "revoke";
