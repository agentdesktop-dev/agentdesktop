import { invoke } from "@tauri-apps/api/core";

import type {
  Bootstrap,
  ClaudeSnapshot,
  ConnectorSnapshot,
  ManagedDeviceSnapshot,
  ManagedPage,
  Settings
} from "./types";

const managedPreview = new URLSearchParams(window.location.search).get("preview") === "managed";

const browserPreview: Bootstrap = {
  settings: {
    openOnStartup: true
  },
  version: "0.1.0",
  platform: "Browser preview",
  managesProviderCredentials: !managedPreview,
  providerCredentialConfigured: managedPreview
};

const browserConnector: ConnectorSnapshot = {
  state: "ready",
  detail: null,
  runtime: {
    version: "0.1.0",
    mode: managedPreview ? "managed" : "standalone",
    gateway: "reachable",
    identity: managedPreview ? "ready" : "not-required",
    inFlight: 0,
    maxInFlight: 128,
    connectTimeoutMs: 5000,
    requestTimeoutMs: 30000,
    shutdownTimeoutMs: 10000,
    platform: {
      os: "macos",
      nativeGateway: true,
      transparentCapture: false,
      trustInstallation: false,
      secretService: false,
      protectedFileCredentials: false
    },
    metrics: {
      requests: 0,
      upstreamResponses: 0,
      identityFailures: 0,
      overloadRejections: 0,
      upstreamTimeouts: 0,
      upstreamFailures: 0
    }
  }
};

let browserClaude: ClaudeSnapshot = {
  state: "not-connected",
  installed: true,
  canConnect: true,
  detail: "Claude Code can be routed through the local connector."
};

const browserManagedDevice: ManagedDeviceSnapshot = managedPreview
  ? {
      configured: true,
      organizationName: "Northstar Labs",
      supportUrl: "https://support.example/agent-desktop",
      adminUrl: "https://enrollment.example/admin/",
      session: "ready",
      enrollment: "approved",
      enrollmentId: "5b32bfeb-8b97-4662-b33f-365243fb2541",
      enrollmentCreatedAt: new Date(Date.now() - 5 * 24 * 60 * 60 * 1000).toISOString(),
      deviceId: "26c702b2-ed3d-436c-b79b-d58335c9d2dd",
      publicKeyFingerprint: "ke7lP4CgEAJ3hLKr7h6PKY_D83DM8rwQ2jKx9baYMB8",
      certificate: {
        serialNumber: "58410729813620315",
        notBefore: new Date(Date.now() - 5 * 24 * 60 * 60 * 1000).toISOString(),
        notAfter: new Date(Date.now() + 4 * 24 * 60 * 60 * 1000).toISOString()
      },
      detail: null
    }
  : {
      configured: false,
      organizationName: null,
      supportUrl: null,
      adminUrl: null,
      session: "not-configured",
      enrollment: "not-configured",
      enrollmentId: null,
      enrollmentCreatedAt: null,
      deviceId: null,
      publicKeyFingerprint: null,
      certificate: null,
      detail: null
    };

function isTauriRuntime(): boolean {
  return "__TAURI_INTERNALS__" in window;
}

export async function getBootstrap(): Promise<Bootstrap> {
  if (!isTauriRuntime()) return browserPreview;
  return invoke<Bootstrap>("get_bootstrap");
}

export async function saveSettings(settings: Settings): Promise<Settings> {
  if (!isTauriRuntime()) {
    browserPreview.settings = settings;
    return settings;
  }
  return invoke<Settings>("save_settings", { settings });
}

export async function getConnectorStatus(): Promise<ConnectorSnapshot> {
  if (!isTauriRuntime()) return browserConnector;
  return invoke<ConnectorSnapshot>("get_connector_status");
}

export async function getClaudeStatus(): Promise<ClaudeSnapshot> {
  if (!isTauriRuntime()) return browserClaude;
  return invoke<ClaudeSnapshot>("get_claude_status");
}

export async function getManagedDeviceStatus(): Promise<ManagedDeviceSnapshot> {
  if (!isTauriRuntime()) return browserManagedDevice;
  return invoke<ManagedDeviceSnapshot>("get_managed_device_status");
}

export async function openManagedPage(page: ManagedPage): Promise<void> {
  if (!isTauriRuntime()) {
    const url = page === "support" ? browserManagedDevice.supportUrl : browserManagedDevice.adminUrl;
    if (url) window.open(url, "_blank", "noopener,noreferrer");
    return;
  }
  return invoke<void>("open_managed_page", { page });
}

export async function connectClaude(apiKey?: string): Promise<ClaudeSnapshot> {
  if (!isTauriRuntime()) {
    if (apiKey) browserPreview.providerCredentialConfigured = true;
    browserClaude = {
      state: "connected",
      installed: true,
      canConnect: false,
      detail: "Claude Code is configured to use Agent Desktop."
    };
    return browserClaude;
  }
  return invoke<ClaudeSnapshot>("connect_claude", { apiKey });
}