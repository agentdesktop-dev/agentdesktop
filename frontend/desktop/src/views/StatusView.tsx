import {
  AlertCircle,
  Check,
  Copy,
  Gauge,
  Laptop,
  LoaderCircle,
  LogOut,
  ShieldCheck,
  Waypoints,
} from "lucide-react";
import type { ReactNode } from "react";
import { useState } from "react";

import type {
  Bootstrap,
  ConnectorSnapshot,
  Discovery,
  ManagedDeviceSnapshot,
  Settings,
} from "../types";

export interface StatusViewProps {
  bootstrap: Bootstrap | null;
  connector: ConnectorSnapshot | null;
  discovery: Discovery | null;
  isLoggingOut: boolean;
  isSaving: boolean;
  managedDevice: ManagedDeviceSnapshot | null;
  onCopy: () => void;
  onCopyRemoteConfig: () => void;
  onLogout: () => void;
  onStartupChange: (checked: boolean) => void;
  remoteConfig: string | null;
  settings: Settings;
}

function humanize(value: string | undefined): string {
  if (!value) return "Unavailable";
  if (value === "macos") return "macOS";
  return value
    .split("-")
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join(" ");
}

function gatewayIsConfigured(gateway: string | undefined): boolean {
  return gateway === "reachable" || gateway === "configured";
}

function Definition({ label, value }: { label: string; value: ReactNode }) {
  return (
    <div className="definition-row">
      <dt>{label}</dt>
      <dd>{value}</dd>
    </div>
  );
}

export function StatusView({
  bootstrap,
  connector,
  managedDevice,
  discovery,
  remoteConfig,
  settings,
  isSaving,
  isLoggingOut,
  onStartupChange,
  onCopy,
  onCopyRemoteConfig,
  onLogout,
}: StatusViewProps) {
  const runtime = connector?.runtime;
  const managed =
    runtime?.mode === "managed" || Boolean(managedDevice?.configured);
  const enrolled = !managed || managedDevice?.enrollment === "approved";
  const daemonReady = connector?.state !== "offline" && Boolean(runtime);
  const statusUnavailable =
    !connector || !discovery || (runtime?.mode === "managed" && !managedDevice);
  const ready = !statusUnavailable && daemonReady && enrolled;
  const gatewayConfigured = gatewayIsConfigured(runtime?.gateway);
  const [confirmingLogout, setConfirmingLogout] = useState(false);
  const agents = discovery?.agents ?? [];
  const capabilityCount = agents.reduce(
    (total, agent) =>
      total + (agent.mcpServers?.length ?? 0) + (agent.skills?.length ?? 0),
    0,
  );

  return (
    <div className="page-stack status-page">
      <div className="status-hero">
        <span className={`status-hero-icon ${ready ? "ready" : "attention"}`}>
          {ready ? <Check size={22} /> : <AlertCircle size={22} />}
        </span>
        <div>
          <p className="eyebrow">Local device</p>
          <h2>
            {statusUnavailable
              ? "Some status is unavailable"
              : ready
                ? "Agent Desktop is running"
                : "Agent Desktop needs attention"}
          </h2>
          <p>
            {statusUnavailable
              ? "Refresh to retry. Available information is shown below."
              : ready
                ? "Your organization settings and local tool inventory are active."
                : (connector?.detail ?? "Review the status below.")}
          </p>
        </div>
      </div>

      <section className="card status-overview">
        {managed ? (
          <div className="status-row">
            <span
              className={`status-row-icon ${managedDevice ? "success" : "neutral"}`}
            >
              <ShieldCheck size={17} />
            </span>
            <div>
              <strong>Organization access</strong>
              <span>
                {managedDevice?.organizationName ?? "Managed organization"}
              </span>
            </div>
            <span
              className={`badge ${managedDevice ? (enrolled ? "success" : "warning") : "neutral"}`}
            >
              {!managedDevice
                ? "Unavailable"
                : enrolled
                  ? "Approved"
                  : humanize(managedDevice.enrollment)}
            </span>
          </div>
        ) : null}
        <div className="status-row">
          <span
            className={`status-row-icon ${!connector ? "neutral" : daemonReady ? "success" : "danger"}`}
          >
            <Gauge size={17} />
          </span>
          <div>
            <strong>Local daemon</strong>
            <span>Discovery, configuration, and controller connection</span>
          </div>
          <span
            className={`badge ${!connector ? "neutral" : daemonReady ? "success" : "danger"}`}
          >
            {!connector ? "Unavailable" : daemonReady ? "Running" : "Offline"}
          </span>
        </div>
        <div className="status-row">
          <span
            className={`status-row-icon ${runtime && gatewayConfigured ? "success" : "neutral"}`}
          >
            <Waypoints size={17} />
          </span>
          <div>
            <strong>LLM gateway</strong>
            <span>Optional routing for managed AI traffic</span>
          </div>
          <span
            className={`badge ${runtime && gatewayConfigured ? "success" : "neutral"}`}
          >
            {!runtime
              ? "Unavailable"
              : gatewayConfigured
                ? "Configured"
                : "Not configured"}
          </span>
        </div>
        <div className="status-row">
          <span
            className={`status-row-icon ${discovery && agents.length ? "success" : "neutral"}`}
          >
            <Laptop size={17} />
          </span>
          <div>
            <strong>Discovered tools</strong>
            <span>
              {discovery
                ? `${agents.length} agent${agents.length === 1 ? "" : "s"} discovered · ${capabilityCount} MCP servers and skills found`
                : "Tool inventory is unavailable"}
            </span>
          </div>
          <span
            className={`badge ${discovery && agents.length ? "success" : "neutral"}`}
          >
            {discovery ? `${agents.length} found` : "Unavailable"}
          </span>
        </div>
      </section>

      <details className="card runtime-card">
        <summary>
          <span>
            <strong>Runtime</strong>
            <small>Local application and daemon information</small>
          </span>
          <span>View</span>
        </summary>
        <div className="runtime-card-body">
          <div className="runtime-card-actions">
            <button
              className="button button-secondary"
              type="button"
              onClick={onCopy}
            >
              <Copy size={13} /> Copy diagnostics
            </button>
          </div>
          <dl className="runtime-grid">
            <Definition label="Mode" value={humanize(runtime?.mode)} />
            <Definition
              label="Operating system"
              value={humanize(runtime?.platform.os ?? bootstrap?.platform)}
            />
            <Definition
              label="Desktop version"
              value={bootstrap?.version ?? "Unavailable"}
            />
            <Definition
              label="Daemon version"
              value={runtime?.version ?? "Unavailable"}
            />
          </dl>
          <div className="inline-preference">
            <div>
              <strong>Open window at startup</strong>
              <span>
                The tray application continues running when this is off.
              </span>
            </div>
            <label className="switch" aria-label="Open window at startup">
              <input
                type="checkbox"
                disabled={!bootstrap || isSaving}
                checked={settings.openOnStartup}
                onChange={(event) => onStartupChange(event.target.checked)}
              />
              <span className="switch-track" aria-hidden="true" />
            </label>
          </div>
        </div>
      </details>

      {remoteConfig || (managed && enrolled) ? (
        <details className="card advanced-config">
          <summary>
            <span>
              <strong>Advanced</strong>
              <small>Raw configuration and organization session controls</small>
            </span>
            <span>View</span>
          </summary>
          <div className="advanced-config-body">
            {remoteConfig ? (
              <>
                <div className="advanced-config-heading">
                  <p>
                    This is the exact controller configuration currently
                    persisted and applied by the daemon.
                  </p>
                  <button
                    className="button button-secondary"
                    type="button"
                    onClick={onCopyRemoteConfig}
                  >
                    <Copy size={13} /> Copy YAML
                  </button>
                </div>
                <pre>
                  <code>{remoteConfig}</code>
                </pre>
              </>
            ) : null}

            {managed && enrolled ? (
              <section
                className="advanced-danger-zone"
                aria-labelledby="logout-heading"
              >
                <div>
                  <p className="eyebrow">Danger zone</p>
                  <h2 id="logout-heading">Sign out of this organization</h2>
                  <p>
                    Removes this device’s local organization credentials and
                    stops managed access. It does not revoke the device record
                    in the controller.
                  </p>
                </div>
                {confirmingLogout ? (
                  <div className="logout-confirmation">
                    <strong>Are you sure?</strong>
                    <div>
                      <button
                        className="button button-secondary"
                        type="button"
                        onClick={() => setConfirmingLogout(false)}
                        disabled={isLoggingOut}
                      >
                        Cancel
                      </button>
                      <button
                        className="button button-danger"
                        type="button"
                        onClick={onLogout}
                        disabled={isLoggingOut}
                      >
                        {isLoggingOut ? (
                          <LoaderCircle className="spin" size={13} />
                        ) : (
                          <LogOut size={13} />
                        )}
                        {isLoggingOut ? "Signing out…" : "Yes, sign out"}
                      </button>
                    </div>
                  </div>
                ) : (
                  <button
                    className="button button-danger"
                    type="button"
                    onClick={() => setConfirmingLogout(true)}
                  >
                    <LogOut size={13} /> Sign out
                  </button>
                )}
              </section>
            ) : null}
          </div>
        </details>
      ) : null}
    </div>
  );
}
