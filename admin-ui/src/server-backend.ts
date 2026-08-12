import type {
  ApprovalResult,
  AgentPolicy,
  AgentPolicyRule,
  Bootstrap,
  DeviceList,
  DeviceDiscoveryReport,
  DiscoveryRescanResult,
  EnrollmentList,
  EnrollmentRecord,
  EnrollmentStatus,
  FleetSummary,
  InventoryAsset,
  InventoryDevicePage,
  InventoryKind,
  InventoryPage,
  RevocationResult,
} from "./types";

const accessTokenKey = "agentdesktop.admin.access_token";
const accessTokenExpiryKey = "agentdesktop.admin.access_token_expiry";
const oauthStateKey = "agentdesktop.admin.oauth_state";
const pkceVerifierKey = "agentdesktop.admin.pkce_verifier";
export const adminSessionExpiredEvent = "agentdesktop:admin-session-expired";
let callbackCompletion: Promise<void> | null = null;

interface AdminConfig {
  organization_name: string;
  authorization_endpoint: string;
  token_endpoint: string;
  client_id: string;
  audience: string;
  scope: string;
}

interface TokenResponse {
  access_token: string;
  expires_in: number;
}

interface WireEnrollmentRecord {
  enrollment_id: string;
  status: EnrollmentStatus;
  subject: string;
  username?: string | null;
  device_name?: string | null;
  public_key_fingerprint: string;
  created_at: string;
  updated_at: string;
  device_id: string | null;
}

interface WireDevice {
  device_id: string;
  device_name?: string | null;
  status: "active" | "revoked";
  subject: string;
  username?: string | null;
  created_at: string;
  revoked_at: string | null;
  current_certificate_serial_number: string | null;
  current_certificate_not_after: string | null;
  certificate_count: number;
  renewal_count: number;
}

interface WireFleetSummary {
  pending_enrollments: number;
  issuing_enrollments: number;
  approved_enrollments: number;
  rejected_enrollments: number;
  active_devices: number;
  revoked_devices: number;
  certificates_expiring_24h: number;
  renewals_24h: number;
  generated_at: string;
}

interface WireDeviceDiscoveryReport {
  device_id: string;
  received_at: string;
  report: {
    schema_version: number;
    collector_version: string;
    platform: "macos";
    coverage: { project_scopes: "not_scanned"; partial: boolean };
    agents: Array<{
      id: "claude-code" | "claude-desktop" | "codex-cli" | "openclaw" | "vscode-copilot";
      installed: boolean;
      version: string | null;
      running: "detected" | "not_detected" | "unknown";
      evidence: string[];
      config_sources: Array<{
        scope: "user" | "managed";
        source: string;
        format: "json" | "toml";
        status: "parsed" | "invalid" | "oversized" | "symlink_skipped";
        sections: string[];
      }>;
      mcp_servers: Array<{ name: string; scope: "user" | "managed"; transport: "stdio" | "http" | "sse" | "unknown" }>;
      skills: Array<{ name: string; scope: "user" | "shared" }>;
      plugins: Array<{ name: string; scope: "user" | "shared"; state: "enabled" | "configured" | "unknown" }>;
    }>;
    issues: Array<{ agent_id?: string; code: string }>;
  };
}

interface WireInventoryPage {
  counts: {
    active_devices: number;
    reporting_devices: number;
    agents: number;
    mcp_servers: number;
    skills: number;
    plugins: number;
  };
  kind: InventoryKind;
  assets: Array<{
    kind: InventoryKind;
    key: string;
    version?: string;
    detail?: string;
    device_count: number;
    running_count: number;
  }>;
  total: number;
  limit: number;
  offset: number;
  generated_at: string;
}

interface WireInventoryDevicePage {
  devices: Array<{
    device_id: string;
    device_name?: string;
    subject: string;
    username?: string;
    status: "active";
    report_received_at: string | null;
  }>;
  total: number;
  limit: number;
  offset: number;
}

interface WireAgentPolicy {
  schema_version: 1;
  rules: Array<{ agent_id: AgentPolicyRule["agentId"]; action: AgentPolicyRule["action"] }>;
  configured: boolean;
  enforcement: "not_available";
  updated_by?: string;
  updated_at?: string;
}

interface WireApprovalResult {
  enrollment_id: string;
  status: "approved";
  device_id: string;
  not_after: string;
}

interface WireRevocationResult {
  device_id: string;
  status: "revoked";
  revoked_at: string;
}

function encode(bytes: Uint8Array): string {
  let value = "";
  for (const byte of bytes) value += String.fromCharCode(byte);
  return btoa(value).replaceAll("+", "-").replaceAll("/", "_").replaceAll("=", "");
}

async function digest(value: string): Promise<string> {
  const bytes = new TextEncoder().encode(value);
  return encode(new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)));
}

function randomValue(size = 32): string {
  const bytes = new Uint8Array(size);
  crypto.getRandomValues(bytes);
  return encode(bytes);
}

function applicationUrl(): URL {
  const url = new URL(".", window.location.href);
  url.search = "";
  url.hash = "";
  return url;
}

function clearSession(): void {
  sessionStorage.removeItem(accessTokenKey);
  sessionStorage.removeItem(accessTokenExpiryKey);
  sessionStorage.removeItem(oauthStateKey);
  sessionStorage.removeItem(pkceVerifierKey);
}

function expireSession(): void {
  clearSession();
  window.dispatchEvent(new Event(adminSessionExpiredEvent));
}

function activeAccessToken(): string | null {
  const token = sessionStorage.getItem(accessTokenKey);
  const expiresAt = Number(sessionStorage.getItem(accessTokenExpiryKey));
  if (!token || !Number.isFinite(expiresAt) || expiresAt <= Date.now()) {
    clearSession();
    return null;
  }
  return token;
}

async function responseError(response: Response): Promise<Error> {
  try {
    const body = await response.json() as { error?: { code?: string } };
    if (body.error?.code) return new Error(body.error.code.replaceAll("_", " "));
  } catch {
    // Fall back to the HTTP status when the response has no API error envelope.
  }
  return new Error(`The enrollment server returned HTTP ${response.status}`);
}

async function fetchConfig(): Promise<AdminConfig> {
  const response = await fetch("/v1/admin/ui-config", {
    headers: { accept: "application/json" }
  });
  if (!response.ok) throw await responseError(response);
  return response.json() as Promise<AdminConfig>;
}

function completeSignIn(config: AdminConfig): Promise<void> {
  if (callbackCompletion) return callbackCompletion;
  const query = new URLSearchParams(window.location.search);
  const code = query.get("code");
  if (!code) return Promise.resolve();

  const expectedState = sessionStorage.getItem(oauthStateKey);
  const verifier = sessionStorage.getItem(pkceVerifierKey);
  if (!expectedState || query.get("state") !== expectedState || !verifier) {
    clearSession();
    return Promise.reject(new Error("Administrator sign-in state did not match"));
  }

  callbackCompletion = exchangeAuthorizationCode(config, code, verifier);
  return callbackCompletion;
}

async function exchangeAuthorizationCode(
  config: AdminConfig,
  code: string,
  verifier: string
): Promise<void> {
  const form = new URLSearchParams({
    grant_type: "authorization_code",
    code,
    client_id: config.client_id,
    redirect_uri: applicationUrl().toString(),
    code_verifier: verifier
  });
  const response = await fetch(config.token_endpoint, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: form
  });
  if (!response.ok) {
    clearSession();
    throw new Error("The identity provider rejected administrator sign-in");
  }
  const tokens = await response.json() as TokenResponse;
  sessionStorage.setItem(accessTokenKey, tokens.access_token);
  sessionStorage.setItem(
    accessTokenExpiryKey,
    String(Date.now() + Math.max(0, tokens.expires_in - 15) * 1000)
  );
  sessionStorage.removeItem(oauthStateKey);
  sessionStorage.removeItem(pkceVerifierKey);
  window.history.replaceState({}, "", applicationUrl());
}

async function api<T>(path: string, init: RequestInit = {}): Promise<T> {
  const token = activeAccessToken();
  if (!token) {
    expireSession();
    throw new Error("Administrator session expired. Sign in again.");
  }
  const response = await fetch(path, {
    ...init,
    headers: {
      accept: "application/json",
      authorization: `Bearer ${token}`,
      ...init.headers
    }
  });
  if (response.status === 401) {
    expireSession();
    throw new Error("Administrator session expired. Sign in again.");
  }
  if (!response.ok) throw await responseError(response);
  return response.json() as Promise<T>;
}

function enrollment(record: WireEnrollmentRecord): EnrollmentRecord {
  return {
    enrollmentId: record.enrollment_id,
    status: record.status,
    subject: record.subject,
    username: record.username ?? null,
    deviceName: record.device_name ?? null,
    publicKeyFingerprint: record.public_key_fingerprint,
    createdAt: record.created_at,
    updatedAt: record.updated_at,
    deviceId: record.device_id
  };
}

export async function getBootstrap(): Promise<Bootstrap> {
  try {
    const config = await fetchConfig();
    await completeSignIn(config);
    return {
      settings: { serverUrl: `${window.location.origin}/` },
      server: {
        organizationName: config.organization_name,
        serverUrl: `${window.location.origin}/`
      },
      signedIn: activeAccessToken() !== null,
      connectionError: null,
      version: "0.1.0",
      platform: "Server-hosted web console"
    };
  } catch (error) {
    return {
      settings: { serverUrl: `${window.location.origin}/` },
      server: null,
      signedIn: false,
      connectionError: error instanceof Error ? error.message : String(error),
      version: "0.1.0",
      platform: "Server-hosted web console"
    };
  }
}

export async function signIn(): Promise<Bootstrap> {
  const config = await fetchConfig();
  const verifier = randomValue(64);
  const state = randomValue();
  sessionStorage.setItem(pkceVerifierKey, verifier);
  sessionStorage.setItem(oauthStateKey, state);

  const authorizationUrl = new URL(config.authorization_endpoint);
  authorizationUrl.searchParams.set("response_type", "code");
  authorizationUrl.searchParams.set("client_id", config.client_id);
  authorizationUrl.searchParams.set("redirect_uri", applicationUrl().toString());
  authorizationUrl.searchParams.set("scope", config.scope);
  authorizationUrl.searchParams.set("audience", config.audience);
  authorizationUrl.searchParams.set("state", state);
  authorizationUrl.searchParams.set("code_challenge", await digest(verifier));
  authorizationUrl.searchParams.set("code_challenge_method", "S256");
  window.location.assign(authorizationUrl);
  return new Promise<Bootstrap>(() => undefined);
}

export async function signOut(): Promise<Bootstrap> {
  clearSession();
  return getBootstrap();
}

export async function listEnrollments(status: EnrollmentStatus): Promise<EnrollmentList> {
  const response = await api<{ enrollments: WireEnrollmentRecord[] }>(
    `/v1/admin/enrollments?status=${encodeURIComponent(status)}`
  );
  return {
    status,
    enrollments: response.enrollments.map(enrollment),
    limited: response.enrollments.length >= 100
  };
}

export async function getFleetSummary(): Promise<FleetSummary> {
  const summary = await api<WireFleetSummary>("/v1/admin/summary");
  return {
    pendingEnrollments: summary.pending_enrollments,
    issuingEnrollments: summary.issuing_enrollments,
    approvedEnrollments: summary.approved_enrollments,
    rejectedEnrollments: summary.rejected_enrollments,
    activeDevices: summary.active_devices,
    revokedDevices: summary.revoked_devices,
    certificatesExpiring24H: summary.certificates_expiring_24h,
    renewals24H: summary.renewals_24h,
    generatedAt: summary.generated_at
  };
}

export async function listDevices(): Promise<DeviceList> {
  const response = await api<{ devices: WireDevice[]; limited: boolean }>("/v1/admin/devices");
  return {
    limited: response.limited,
    devices: response.devices.map((device) => ({
      deviceId: device.device_id,
      deviceName: device.device_name ?? null,
      status: device.status,
      subject: device.subject,
      username: device.username ?? null,
      createdAt: device.created_at,
      revokedAt: device.revoked_at,
      currentCertificateSerialNumber: device.current_certificate_serial_number,
      currentCertificateNotAfter: device.current_certificate_not_after,
      certificateCount: device.certificate_count,
      renewalCount: device.renewal_count
    }))
  };
}

export async function getDeviceDiscoveryReport(deviceId: string): Promise<DeviceDiscoveryReport | null> {
  const token = activeAccessToken();
  if (!token) {
    expireSession();
    throw new Error("Administrator session expired. Sign in again.");
  }
  const response = await fetch(`/v1/admin/devices/${encodeURIComponent(deviceId)}/discovery-report`, {
    headers: { accept: "application/json", authorization: `Bearer ${token}` }
  });
  if (response.status === 401) {
    expireSession();
    throw new Error("Administrator session expired. Sign in again.");
  }
  if (response.status === 404) return null;
  if (!response.ok) throw await responseError(response);
  const stored = await response.json() as WireDeviceDiscoveryReport;
  return {
    deviceId: stored.device_id,
    receivedAt: stored.received_at,
    schemaVersion: stored.report.schema_version,
    collectorVersion: stored.report.collector_version,
    platform: stored.report.platform,
    projectScopes: stored.report.coverage.project_scopes,
    partial: stored.report.coverage.partial,
    agents: stored.report.agents.map((agent) => ({
      id: agent.id,
      installed: agent.installed,
      version: agent.version,
      running: agent.running,
      evidence: agent.evidence,
      configSources: agent.config_sources,
      mcpServers: agent.mcp_servers,
      skills: agent.skills,
      plugins: agent.plugins
    })),
    issues: stored.report.issues.map((issue) => ({ agentId: issue.agent_id ?? null, code: issue.code }))
  };
}

export async function getInventory(kind: InventoryKind, query: string, offset = 0, limit = 25): Promise<InventoryPage> {
  const search = new URLSearchParams({ kind, q: query, offset: String(offset), limit: String(limit) });
  const page = await api<WireInventoryPage>(`/v1/admin/inventory?${search}`);
  const assets: InventoryAsset[] = page.assets.map((asset) => ({
    kind: asset.kind,
    key: asset.key,
    version: asset.version ?? null,
    detail: asset.detail || null,
    deviceCount: asset.device_count,
    runningCount: asset.running_count
  }));
  return {
    counts: {
      activeDevices: page.counts.active_devices,
      reportingDevices: page.counts.reporting_devices,
      agents: page.counts.agents,
      mcpServers: page.counts.mcp_servers,
      skills: page.counts.skills,
      plugins: page.counts.plugins
    },
    kind: page.kind,
    assets,
    total: page.total,
    limit: page.limit,
    offset: page.offset,
    generatedAt: page.generated_at
  };
}

export async function getInventoryDevices(
  asset: InventoryAsset | null,
  query: string,
  offset = 0,
  limit = 50
): Promise<InventoryDevicePage> {
  const search = new URLSearchParams({ q: query, offset: String(offset), limit: String(limit) });
  if (asset) {
    search.set("kind", asset.kind);
    search.set("key", asset.key);
    if (asset.version) search.set("version", asset.version);
    if (asset.detail) search.set("detail", asset.detail);
  }
  const page = await api<WireInventoryDevicePage>(`/v1/admin/inventory/devices?${search}`);
  return {
    devices: page.devices.map((device) => ({
      deviceId: device.device_id,
      deviceName: device.device_name ?? null,
      subject: device.subject,
      username: device.username ?? null,
      status: device.status,
      reportReceivedAt: device.report_received_at
    })),
    total: page.total,
    limit: page.limit,
    offset: page.offset
  };
}

export async function requestDiscoveryRescan(deviceIds: string[] | null): Promise<DiscoveryRescanResult> {
  const result = await api<{ requested: number; requested_at: string }>("/v1/admin/discovery-rescans", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      target_mode: deviceIds ? "selected" : "all_active",
      device_ids: deviceIds ?? []
    })
  });
  return { requested: result.requested, requestedAt: result.requested_at };
}

function agentPolicy(policy: WireAgentPolicy): AgentPolicy {
  return {
    schemaVersion: policy.schema_version,
    rules: policy.rules.map((rule) => ({ agentId: rule.agent_id, action: rule.action })),
    configured: policy.configured,
    enforcement: policy.enforcement,
    updatedBy: policy.updated_by ?? null,
    updatedAt: policy.updated_at ?? ""
  };
}

export async function getAgentPolicy(): Promise<AgentPolicy> {
  return agentPolicy(await api<WireAgentPolicy>("/v1/admin/agent-policy"));
}

export async function putAgentPolicy(rules: AgentPolicyRule[]): Promise<AgentPolicy> {
  const policy = await api<WireAgentPolicy>("/v1/admin/agent-policy", {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      schema_version: 1,
      rules: rules.map((rule) => ({ agent_id: rule.agentId, action: rule.action }))
    })
  });
  return agentPolicy(policy);
}

export async function approveEnrollment(enrollmentId: string): Promise<ApprovalResult> {
  const result = await api<WireApprovalResult>(
    `/v1/admin/enrollments/${encodeURIComponent(enrollmentId)}/approve`,
    { method: "POST" }
  );
  return {
    enrollmentId: result.enrollment_id,
    status: result.status,
    deviceId: result.device_id,
    notAfter: result.not_after
  };
}

export async function rejectEnrollment(enrollmentId: string): Promise<EnrollmentRecord> {
  const result = await api<WireEnrollmentRecord>(
    `/v1/admin/enrollments/${encodeURIComponent(enrollmentId)}/reject`,
    { method: "POST" }
  );
  return enrollment(result);
}

export async function revokeDevice(deviceId: string): Promise<RevocationResult> {
  const result = await api<WireRevocationResult>(
    `/v1/admin/devices/${encodeURIComponent(deviceId)}/revoke`,
    { method: "POST" }
  );
  return {
    deviceId: result.device_id,
    status: result.status,
    revokedAt: result.revoked_at
  };
}