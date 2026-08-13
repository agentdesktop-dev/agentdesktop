import { invoke } from "@tauri-apps/api/core";

import type {
  Bootstrap,
  ConnectorSnapshot,
  Discovery,
  ManagedDeviceSnapshot,
  Settings,
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

export async function getManagedDeviceStatus(): Promise<ManagedDeviceSnapshot> {
  return invoke<ManagedDeviceSnapshot>("get_managed_device_status");
}

export async function getDiscovery(): Promise<Discovery> {
  return invoke<Discovery>("get_discovery");
}

export async function getRemoteConfig(): Promise<string | null> {
  return invoke<string | null>("get_remote_config");
}

export async function logoutManagedDevice(): Promise<void> {
  return invoke<void>("logout_managed_device");
}

export async function setupManagedDevice(): Promise<ManagedDeviceSnapshot> {
  return invoke<ManagedDeviceSnapshot>("setup_managed_device");
}
