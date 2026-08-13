import { invoke } from "@tauri-apps/api/core";

import type {
  Bootstrap,
  ClaudeSnapshot,
  ConnectorSnapshot,
  ManagedDeviceSnapshot,
  ManagedPage,
  Settings
} from "./types";

export async function getBootstrap(): Promise<Bootstrap> {
  return invoke<Bootstrap>("get_bootstrap");
}

export async function saveSettings(settings: Settings): Promise<Settings> {
  return invoke<Settings>("save_settings", { settings });
}

export async function getConnectorStatus(): Promise<ConnectorSnapshot> {
  return invoke<ConnectorSnapshot>("get_connector_status");
}

export async function getClaudeStatus(): Promise<ClaudeSnapshot> {
  return invoke<ClaudeSnapshot>("get_claude_status");
}

export async function getManagedDeviceStatus(): Promise<ManagedDeviceSnapshot> {
  return invoke<ManagedDeviceSnapshot>("get_managed_device_status");
}

export async function setupManagedDevice(): Promise<ManagedDeviceSnapshot> {
  return invoke<ManagedDeviceSnapshot>("setup_managed_device");
}

export async function openManagedPage(page: ManagedPage): Promise<void> {
  return invoke<void>("open_managed_page", { page });
}

export async function connectClaude(apiKey?: string): Promise<ClaudeSnapshot> {
  return invoke<ClaudeSnapshot>("connect_claude", { apiKey });
}