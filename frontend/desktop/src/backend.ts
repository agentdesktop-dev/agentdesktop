import { invoke } from "@tauri-apps/api/core";

import type {
  AccessReport,
  Bootstrap,
  ConnectorSnapshot,
  DeviceUsage,
  Discovery,
  ManagedDeviceSnapshot,
  NetworkRuleChange,
  ProgramChoice,
  ProgramManagement,
  Settings,
  UsageRange,
} from "./types";

export async function desktopReady(): Promise<void> {
  return invoke<void>("desktop_ready");
}

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

export async function getAccessReport(): Promise<AccessReport> {
  return invoke<AccessReport>("get_access_report");
}

export async function applyNetworkRuleChange(
  request: NetworkRuleChange,
): Promise<AccessReport> {
  return invoke<AccessReport>("apply_network_rule_change", { request });
}

export async function openAccessSource(path: string): Promise<void> {
  return invoke<void>("open_access_source", { path });
}

export async function getDeviceUsage(
  range: UsageRange,
  utcOffsetMinutes: number,
): Promise<DeviceUsage> {
  return invoke<DeviceUsage>("get_device_usage", { range, utcOffsetMinutes });
}

export async function logoutManagedDevice(): Promise<void> {
  return invoke<void>("logout_managed_device");
}

export async function getProgramManagement(): Promise<ProgramManagement> {
  return invoke<ProgramManagement>("get_program_management");
}

export async function setProgramChoice(
  choice: ProgramChoice,
): Promise<ProgramManagement> {
  return invoke<ProgramManagement>("set_program_choice", { choice });
}

export async function setupManagedDevice(): Promise<ManagedDeviceSnapshot> {
  return invoke<ManagedDeviceSnapshot>("setup_managed_device");
}
