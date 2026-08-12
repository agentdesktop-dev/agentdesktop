export interface Settings {
  openOnStartup: boolean;
}

export interface Bootstrap {
  settings: Settings;
  version: string;
  platform: string;
  managesProviderCredentials: boolean;
  providerCredentialConfigured: boolean;
}

export interface PlatformCapabilities {
  os: string;
  nativeGateway: boolean;
  transparentCapture: boolean;
  trustInstallation: boolean;
  secretService: boolean;
  protectedFileCredentials: boolean;
}

export interface MetricsSnapshot {
  requests: number;
  upstreamResponses: number;
  identityFailures: number;
  overloadRejections: number;
  upstreamTimeouts: number;
  upstreamFailures: number;
}

export interface ConnectorRuntime {
  version: string;
  mode: string;
  gateway: string;
  identity: string;
  inFlight: number | null;
  maxInFlight: number | null;
  connectTimeoutMs: number | null;
  shutdownTimeoutMs: number | null;
  platform: PlatformCapabilities;
  metrics: MetricsSnapshot | null;
}

export interface ConnectorSnapshot {
  state: "ready" | "attention" | "offline";
  detail: string | null;
  runtime: ConnectorRuntime | null;
}

export interface ClaudeSnapshot {
  state: "not-installed" | "not-connected" | "connected" | "conflict";
  installed: boolean;
  canConnect: boolean;
  detail: string;
}

export interface ManagedCertificateSnapshot {
  serialNumber: string;
  notBefore: string;
  notAfter: string;
}

export interface ManagedDeviceSnapshot {
  configured: boolean;
  organizationName: string | null;
  supportUrl: string | null;
  adminUrl: string | null;
  session: string;
  enrollment: string;
  enrollmentId: string | null;
  enrollmentCreatedAt: string | null;
  deviceId: string | null;
  publicKeyFingerprint: string | null;
  certificate: ManagedCertificateSnapshot | null;
  detail: string | null;
}

export type ManagedPage = "support" | "administration";