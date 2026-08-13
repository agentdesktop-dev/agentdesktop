import { AlertCircle, Box, Check, Copy, ExternalLink, Gauge, Laptop, LoaderCircle, LogOut, RefreshCw, ShieldCheck, Waypoints } from "lucide-react";
import { Component, startTransition, useEffect, useState, useTransition } from "react";
import type { ErrorInfo, ReactNode } from "react";
import { CardHeader, ToolInventory } from "@agentdesktop/ui";
import agentdesktopIcon from "../../ui/app-icon.svg";

import {
  connectClaude,
  getBootstrap,
  getClaudeStatus,
  getConnectorStatus,
  getDiscovery,
  getManagedDeviceStatus,
  getRemoteConfig,
  logoutManagedDevice,
  openManagedPage,
  saveSettings,
  setupManagedDevice
} from "./backend";
import type {
  Bootstrap,
  ClaudeSnapshot,
  ConnectorRuntime,
  ConnectorSnapshot,
  Discovery,
  ManagedCertificateSnapshot,
  ManagedDeviceSnapshot,
  ManagedPage,
  MetricsSnapshot,
  Settings
} from "./types";

type View = "home" | "tools";
type Notice = { tone: "success" | "error"; message: string } | null;
type StepState = "done" | "waiting" | "error" | "muted";

class PageBoundary extends Component<
  { children: ReactNode },
  { error: string | null }
> {
  state = { error: null };

  static getDerivedStateFromError(error: unknown) {
    return { error: errorMessage(error) };
  }

  componentDidCatch(error: unknown, info: ErrorInfo) {
    console.error("Desktop page failed to render", error, info);
  }

  render() {
    if (this.state.error) {
      return (
        <div className="page-error" role="alert">
          <AlertCircle size={26} />
          <h2>Couldn’t display this page</h2>
          <p>{this.state.error}</p>
        </div>
      );
    }
    return this.props.children;
  }
}

const loadingSettings: Settings = { openOnStartup: true };
const numberFormat = new Intl.NumberFormat();
const dateFormat = new Intl.DateTimeFormat(undefined, { dateStyle: "medium" });
const dateTimeFormat = new Intl.DateTimeFormat(undefined, {
  dateStyle: "medium",
  timeStyle: "short"
});

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function humanize(value: string | undefined): string {
  if (!value) return "Unavailable";
  if (value === "macos") return "macOS";
  return value
    .split("-")
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join(" ");
}

function identityNeedsAttention(identity: string | undefined): boolean {
  return identity === "unavailable" || identity === "not-configured";
}

function identityIsOperational(identity: string | undefined): boolean {
  return identity === "ready" || identity === "refresh-required" || identity === "not-required";
}

function gatewayIsConfigured(gateway: string | undefined): boolean {
  return gateway === "reachable" || gateway === "configured";
}

function formatDate(value: string | null | undefined, includeTime = false): string {
  if (!value) return "Unavailable";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "Unavailable";
  return (includeTime ? dateTimeFormat : dateFormat).format(date);
}

function managedAccountDetail(
  identity: string | undefined,
  managedDevice: ManagedDeviceSnapshot | null
): string {
  switch (identity) {
    case "ready":
      return managedDevice?.organizationName
        ? `Signed in to ${managedDevice.organizationName}`
        : "Organization session is active";
    case "refresh-required":
      return "Session refreshes automatically with the next request";
    case "unavailable":
      return managedDevice?.detail ?? "Managed credentials could not be loaded";
    case "not-configured":
    case "signed-out":
      return "Organization sign-in is required";
    default:
      return "Checking organization sign-in";
  }
}

function certificateState(certificate: ManagedCertificateSnapshot | null): StepState {
  if (!certificate) return "waiting";
  const expiresAt = Date.parse(certificate.notAfter);
  if (Number.isNaN(expiresAt) || expiresAt <= Date.now()) return "error";
  if (expiresAt - Date.now() <= 6 * 60 * 60 * 1000) return "waiting";
  return "done";
}

function managedEnrollmentStep(
  managedDevice: ManagedDeviceSnapshot | null,
  identityReady: boolean
): { state: StepState; detail: string } {
  if (!managedDevice?.configured) {
    return identityReady
      ? { state: "done", detail: "Device credentials are active" }
      : { state: "waiting", detail: "Device approval follows organization sign-in" };
  }
  switch (managedDevice.enrollment) {
    case "approved": {
      if (!managedDevice.certificate) {
        return { state: "done", detail: "Device enrollment is approved" };
      }
      const state = certificateState(managedDevice.certificate);
      if (state === "error") {
        return { state, detail: "Device certificate expired; recovery is required" };
      }
      if (state === "waiting") {
        return { state, detail: "Device certificate renewal is due" };
      }
      return {
        state,
        detail: `Approved · certificate valid until ${formatDate(managedDevice.certificate?.notAfter)}`
      };
    }
    case "pending":
      return { state: "waiting", detail: "Waiting for an administrator to approve this device" };
    case "issuing":
      return { state: "waiting", detail: "Your organization is issuing a device certificate" };
    case "rejected":
      return { state: "error", detail: "This device enrollment was rejected" };
    case "unavailable":
      return { state: "error", detail: managedDevice.detail ?? "Device enrollment is unavailable" };
    default:
      return { state: "waiting", detail: "Device enrollment has not started" };
  }
}

function failureCount(metrics: MetricsSnapshot | null | undefined): number {
  if (!metrics) return 0;
  return (
    metrics.identityFailures +
    metrics.overloadRejections +
    metrics.upstreamTimeouts +
    metrics.upstreamFailures
  );
}

function statusLabel(connector: ConnectorSnapshot | null): string {
  if (!connector) return "Checking";
  if (connector.state === "offline") return "Offline";
  if (connector.state === "attention") return "Needs attention";
  return "Running";
}

function Step({
  state,
  title,
  detail,
  action
}: {
  state: StepState;
  title: string;
  detail: string;
  action?: React.ReactNode;
}) {
  return (
    <li className="setup-step">
      <span className={`step-mark step-mark-${state}`} aria-hidden="true">
        {state === "done" ? <Check size={14} /> : state === "error" ? <AlertCircle size={14} /> : null}
      </span>
      <div className="step-copy">
        <strong>{title}</strong>
        <span>{detail}</span>
      </div>
      {action ? <div className="step-action">{action}</div> : null}
    </li>
  );
}

function Home({
  bootstrap,
  connector,
  claude,
  managedDevice,
  apiKey,
  showCredentialForm,
  isConnecting,
  isManaging,
  onApiKeyChange,
  onCancelCredential,
  onConnect,
  onOpenManagedPage,
  onRequestCredential,
  onSetupManaged
}: {
  bootstrap: Bootstrap | null;
  connector: ConnectorSnapshot | null;
  claude: ClaudeSnapshot | null;
  managedDevice: ManagedDeviceSnapshot | null;
  apiKey: string;
  showCredentialForm: boolean;
  isConnecting: boolean;
  isManaging: boolean;
  onApiKeyChange: (value: string) => void;
  onCancelCredential: () => void;
  onConnect: (apiKey?: string) => void;
  onOpenManagedPage: (page: ManagedPage) => void;
  onRequestCredential: () => void;
  onSetupManaged: () => void;
}) {
  const runtime = connector?.runtime;
  const managed = runtime?.mode === "managed" || Boolean(managedDevice?.configured);
  const providerManaged = !managed && Boolean(bootstrap?.managesProviderCredentials);
  const providerConfigured = managed || Boolean(bootstrap?.providerCredentialConfigured);
  const providerNeedsCredential = providerManaged && !providerConfigured;
  const gatewayReady = gatewayIsConfigured(runtime?.gateway);
  const identity = runtime?.identity ?? managedDevice?.session;
  const identityReady = !managed || identityIsOperational(identity);
  const accountState: StepState = identityReady
    ? "done"
    : identityNeedsAttention(identity)
      ? "error"
      : "waiting";
  const enrollmentStep = managedEnrollmentStep(managedDevice, identityReady);
  const managedAccessReady = !managed || (identityReady && enrollmentStep.state === "done");
  const claudeReady = claude?.state === "connected";
  const managedRoutingReady = Boolean(claude && (!claude.installed || claudeReady));
  const ready =
    connector?.state === "ready" &&
    gatewayReady &&
    identityReady &&
    providerConfigured &&
    (managed ? managedRoutingReady : claudeReady);

  const connectorStep: StepState = managed && !managedAccessReady
    ? "waiting"
    : !connector
      ? "waiting"
      : connector.state === "offline" || !gatewayReady
        ? "error"
        : "done";
  const claudeStep: StepState = !claude
    ? "waiting"
    : claudeReady
      ? "done"
      : claude.state === "conflict"
        ? "error"
        : claude.state === "not-installed"
          ? "muted"
          : "waiting";
  const managedRoutingStep: StepState = !managedAccessReady || !claude
    ? "waiting"
    : claude.state === "conflict"
      ? "error"
      : claude.installed && !claudeReady
        ? "waiting"
        : "done";
  const managedRoutingDetail = !managedAccessReady
    ? "Applies automatically after organization access is approved"
    : !claude
      ? "Checking supported agents"
      : claude.state === "conflict"
        ? "Claude Code has settings that conflict with organization routing"
        : claude.installed && !claudeReady
          ? "Applying organization routing to Claude Code"
          : claudeReady
            ? "Organization routing applied automatically to Claude Code"
            : "Supported agents are configured automatically when detected";

  return (
    <>
      <div className="intro">
        <p className="kicker">
          {managed ? managedDevice?.organizationName ?? "Organization setup" : "Local setup"}
        </p>
        <h1>{ready ? (managed ? "Managed configuration active" : "Agent Desktop is configured") : (managed ? "Action required" : "Finish setup")}</h1>
        <p className="intro-copy">
          {ready
            ? managed
              ? "Agent Desktop is running in the background. Continue using your AI applications normally."
              : "Claude Code will send requests through Agent Gateway."
            : managed
              ? "Complete organization access before using managed AI applications."
              : "Complete the remaining steps, then use Claude Code normally."}
        </p>
      </div>

      <ol className="setup-list">
        {managed ? (
          <>
            <Step
              state={accountState}
              title="Organization account"
              detail={managedAccountDetail(identity, managedDevice)}
              action={
                !identityReady && identity !== "unavailable" ? (
                  <button
                    className="button button-primary"
                    type="button"
                    onClick={onSetupManaged}
                    disabled={isManaging}
                  >
                    {isManaging ? "Signing in…" : "Sign in"}
                  </button>
                ) : accountState === "error" && managedDevice?.supportUrl ? (
                  <button
                    className="button button-secondary"
                    type="button"
                    onClick={() => onOpenManagedPage("support")}
                  >
                    Get help
                  </button>
                ) : undefined
              }
            />
            <Step
              state={enrollmentStep.state}
              title="Device access"
              detail={enrollmentStep.detail}
              action={
                identityReady &&
                ["not-enrolled", "pending", "issuing"].includes(
                  managedDevice?.enrollment ?? "not-enrolled"
                ) ? (
                  <button
                    className={managedDevice?.enrollment === "not-enrolled" ? "button button-primary" : "button button-secondary"}
                    type="button"
                    onClick={onSetupManaged}
                    disabled={isManaging}
                  >
                    {isManaging
                      ? "Checking…"
                      : managedDevice?.enrollment === "not-enrolled"
                        ? "Request access"
                        : "Check status"}
                  </button>
                ) : enrollmentStep.state === "error" && managedDevice?.supportUrl ? (
                  <button
                    className="button button-secondary"
                    type="button"
                    onClick={() => onOpenManagedPage("support")}
                  >
                    Contact support
                  </button>
                ) : undefined
              }
            />
          </>
        ) : null}

        <Step
          state={connectorStep}
          title={managed ? "Organization connection" : "Gateway"}
          detail={
            managed && !managedAccessReady
              ? "Starts after organization access is approved"
              : !connector
                ? "Checking connection"
                : connector.state === "offline"
                  ? "Agent Desktop is not responding"
                  : gatewayReady
                    ? managed
                      ? "Organization gateway is configured"
                      : "Inference gateway is configured"
                    : managed
                      ? "Your organization service is temporarily unavailable"
                      : "Gateway is unavailable"
          }
        />

        {!managed ? (
          <Step
            state={providerConfigured ? "done" : providerManaged ? "waiting" : "muted"}
            title="Provider access"
            detail={providerConfigured ? "Available to Agent Gateway" : providerManaged ? "Anthropic API key required" : "Configured in an external Agent Gateway"}
            action={
              providerNeedsCredential && !showCredentialForm ? (
                <button className="button button-secondary" type="button" onClick={onRequestCredential}>
                  Add key
                </button>
              ) : undefined
            }
          />
        ) : null}

        {showCredentialForm ? (
          <li className="credential-entry">
            <form
              onSubmit={(event) => {
                event.preventDefault();
                onConnect(apiKey);
              }}
            >
              <label htmlFor="provider-api-key">Anthropic API key</label>
              <input
                autoFocus
                id="provider-api-key"
                type="password"
                autoComplete="new-password"
                spellCheck="false"
                required
                minLength={16}
                disabled={isConnecting}
                value={apiKey}
                onChange={(event) => onApiKeyChange(event.target.value)}
              />
              <p>Saved in your system credential store and passed only to Agent Gateway.</p>
              <div className="credential-actions">
                <button className="button button-secondary" type="button" onClick={onCancelCredential} disabled={isConnecting}>
                  Cancel
                </button>
                <button className="button button-primary" type="submit" disabled={isConnecting || apiKey.trim().length < 16}>
                  {isConnecting ? "Saving…" : "Save and connect"}
                </button>
              </div>
            </form>
          </li>
        ) : null}

        {managed ? (
          <Step state={managedRoutingStep} title="Agent routing" detail={managedRoutingDetail} />
        ) : (
          <Step
            state={claudeStep}
            title="Claude Code"
            detail={claude?.detail ?? "Checking installation"}
            action={
              claude?.canConnect && managedAccessReady ? (
                <button
                  className="button button-primary"
                  type="button"
                  onClick={() => (providerNeedsCredential ? onRequestCredential() : onConnect())}
                  disabled={isConnecting || !bootstrap}
                >
                  {isConnecting ? "Connecting…" : "Connect"}
                </button>
              ) : undefined
            }
          />
        )}
      </ol>

      {runtime?.metrics ? (
        <div className="activity-line" aria-label="Activity since connector start">
          <span>{numberFormat.format(runtime.metrics.requests)} flows</span>
          <span>{numberFormat.format(runtime.metrics.upstreamResponses)} completed</span>
          <span>{numberFormat.format(failureCount(runtime.metrics))} failed</span>
          <span>{runtime.inFlight ?? 0} active</span>
        </div>
      ) : null}
    </>
  );
}

function Definition({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <div className="definition-row">
      <dt>{label}</dt>
      <dd>{value}</dd>
    </div>
  );
}

function Availability({ available }: { available: boolean }) {
  return <span className={available ? "available" : "unavailable"}>{available ? "Yes" : "No"}</span>;
}

function IdentifierValue({
  label,
  value,
  compact = false,
  onCopy
}: {
  label: string;
  value: string | null | undefined;
  compact?: boolean;
  onCopy: (label: string, value: string) => void;
}) {
  if (!value) return <>Unavailable</>;
  const display = compact && value.length > 28 ? `${value.slice(0, 14)}…${value.slice(-10)}` : value;
  return (
    <span className="identifier-value">
      <span className="identifier-text" title={value}>{display}</span>
      <button
        className="copy-button"
        type="button"
        title={`Copy ${label}`}
        aria-label={`Copy ${label}`}
        onClick={() => onCopy(label, value)}
      >
        <Copy size={13} />
      </button>
    </span>
  );
}

function CertificateValidity({ certificate }: { certificate: ManagedCertificateSnapshot | null }) {
  if (!certificate) return <>Unavailable</>;
  const expiresAt = Date.parse(certificate.notAfter);
  if (Number.isNaN(expiresAt)) return <>Unavailable</>;
  const expired = expiresAt <= Date.now();
  return (
    <span className={expired ? "state-value state-error" : "state-value state-ready"}>
      {expired ? "Expired" : "Valid until"} {formatDate(certificate.notAfter)}
    </span>
  );
}

function Details({
  bootstrap,
  connector,
  managedDevice,
  settings,
  isSaving,
  onStartupChange,
  onCopy,
  onCopyValue,
  onOpenManagedPage
}: {
  bootstrap: Bootstrap | null;
  connector: ConnectorSnapshot | null;
  managedDevice: ManagedDeviceSnapshot | null;
  settings: Settings;
  isSaving: boolean;
  onStartupChange: (checked: boolean) => void;
  onCopy: () => void;
  onCopyValue: (label: string, value: string) => void;
  onOpenManagedPage: (page: ManagedPage) => void;
}) {
  const runtime = connector?.runtime;
  const platform = runtime?.platform;
  const metrics = runtime?.metrics;
  const managed = runtime?.mode === "managed" || Boolean(managedDevice?.configured);
  const identity = runtime?.identity ?? managedDevice?.session;
  const enrollmentStep = managedEnrollmentStep(managedDevice, identityIsOperational(identity));
  const organizationAccessReady = identityIsOperational(identity) && enrollmentStep.state === "done";

  return (
    <>
      <div className="details-heading">
        <div>
          <p className="kicker">Support information</p>
          <h1>Details</h1>
        </div>
        <button className="button button-secondary" type="button" onClick={onCopy}>
          Copy diagnostics
        </button>
      </div>

      {managed ? (
        <section className="detail-section" aria-labelledby="organization-heading">
          <div className="section-heading">
            <h2 id="organization-heading">Organization access</h2>
            <span className={`state-value ${organizationAccessReady ? "state-ready" : "state-error"}`}>
              {organizationAccessReady ? "Active" : "Attention required"}
            </span>
          </div>
          <dl>
            <Definition label="Organization" value={managedDevice?.organizationName ?? "Managed installation"} />
            <Definition label="User session" value={humanize(identity)} />
            <Definition label="Device enrollment" value={humanize(managedDevice?.enrollment ?? (identity === "ready" ? "approved" : undefined))} />
            <Definition label="Certificate" value={<CertificateValidity certificate={managedDevice?.certificate ?? null} />} />
          </dl>
          {managedDevice?.detail ? <p className="section-note section-note-error">{managedDevice.detail}</p> : null}
          <div className="managed-actions">
            {managedDevice?.supportUrl ? (
              <button className="button button-secondary" type="button" onClick={() => onOpenManagedPage("support")}>
                <ExternalLink size={13} />
                Support
              </button>
            ) : null}
            {managedDevice?.adminUrl ? (
              <button className="button button-secondary" type="button" onClick={() => onOpenManagedPage("administration")}>
                <ExternalLink size={13} />
                Administration
              </button>
            ) : null}
          </div>
        </section>
      ) : null}

      {managed && managedDevice?.enrollmentId ? (
        <details className="detail-section disclosure credential-disclosure">
          <summary>Device credential</summary>
          <dl>
            <Definition
              label="Enrollment ID"
              value={<IdentifierValue label="enrollment ID" value={managedDevice.enrollmentId} onCopy={onCopyValue} />}
            />
            <Definition
              label="Device ID"
              value={<IdentifierValue label="device ID" value={managedDevice.deviceId} onCopy={onCopyValue} />}
            />
            <Definition
              label="Key fingerprint"
              value={<IdentifierValue label="key fingerprint" value={managedDevice.publicKeyFingerprint} compact onCopy={onCopyValue} />}
            />
            <Definition label="Requested" value={formatDate(managedDevice.enrollmentCreatedAt, true)} />
            <Definition label="Certificate issued" value={formatDate(managedDevice.certificate?.notBefore, true)} />
            <Definition
              label="Certificate serial"
              value={<IdentifierValue label="certificate serial" value={managedDevice.certificate?.serialNumber} onCopy={onCopyValue} />}
            />
          </dl>
        </details>
      ) : null}

      <section className="detail-section" aria-labelledby="runtime-heading">
        <h2 id="runtime-heading">Runtime</h2>
        <dl>
          <Definition label="Status" value={statusLabel(connector)} />
          <Definition label="Mode" value={humanize(runtime?.mode)} />
          <Definition
            label={runtime?.mode === "managed" ? "Organization service" : "Gateway"}
            value={runtime?.mode === "managed" ? (gatewayIsConfigured(runtime.gateway) ? "Configured" : "Unavailable") : humanize(runtime?.gateway)}
          />
          <Definition label="Identity" value={humanize(runtime?.identity)} />
          <Definition label="Desktop version" value={bootstrap?.version ?? "Unavailable"} />
          <Definition label="Connector version" value={runtime?.version ?? "Unavailable"} />
        </dl>
      </section>

      {metrics ? (
        <section className="detail-section" aria-labelledby="traffic-heading">
          <div className="section-heading">
            <h2 id="traffic-heading">Local traffic</h2>
            <span className="state-value">Since connector start</span>
          </div>
          <dl>
            <Definition label="Accepted flows" value={numberFormat.format(metrics.requests)} />
            <Definition label="Completed flows" value={numberFormat.format(metrics.upstreamResponses)} />
            <Definition label="Active flows" value={numberFormat.format(runtime?.inFlight ?? 0)} />
            <Definition label="Overload rejections" value={numberFormat.format(metrics.overloadRejections)} />
            <Definition label="Tunnel timeouts" value={numberFormat.format(metrics.upstreamTimeouts)} />
            <Definition label="Tunnel failures" value={numberFormat.format(metrics.upstreamFailures)} />
          </dl>
          <p className="section-note">
            Agent Desktop counts opaque TCP flows. Model requests and tokens are measured by Agent Gateway and shown in administration.
          </p>
        </section>
      ) : null}

      <details className="detail-section disclosure">
        <summary>Advanced diagnostics</summary>
        <dl>
          <Definition label="Operating system" value={humanize(platform?.os ?? bootstrap?.platform)} />
          <Definition label="Maximum in flight" value={runtime?.maxInFlight ?? "Unavailable"} />
          <Definition label="Connect timeout" value={runtime?.connectTimeoutMs ? `${runtime.connectTimeoutMs} ms` : "Unavailable"} />
          <Definition label="Identity failures" value={numberFormat.format(metrics?.identityFailures ?? 0)} />
          <Definition label="Overload rejections" value={numberFormat.format(metrics?.overloadRejections ?? 0)} />
          <Definition label="Upstream timeouts" value={numberFormat.format(metrics?.upstreamTimeouts ?? 0)} />
          <Definition label="Upstream failures" value={numberFormat.format(metrics?.upstreamFailures ?? 0)} />
          <Definition label="Native gateway" value={<Availability available={platform?.nativeGateway ?? false} />} />
          <Definition label="Transparent capture" value={<Availability available={platform?.transparentCapture ?? false} />} />
          <Definition label="Trust installation" value={<Availability available={platform?.trustInstallation ?? false} />} />
        </dl>
      </details>

      <section className="detail-section preference-section" aria-labelledby="preference-heading">
        <div>
          <h2 id="preference-heading">Open window at startup</h2>
          <p>The menu bar app still runs when this is off.</p>
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
      </section>
    </>
  );
}

function EnrollmentWelcome({
  enrollment,
  busy,
  onEnroll
}: {
  enrollment: ManagedDeviceSnapshot;
  busy: boolean;
  onEnroll: () => void;
}) {
  const waiting = enrollment.enrollment === "pending" || enrollment.enrollment === "issuing";
  const failed = enrollment.enrollment === "unavailable" || enrollment.enrollment === "rejected";
  return (
    <section className="enrollment-welcome">
      <div className={`enrollment-mark ${failed ? "enrollment-mark-error" : ""}`}>
        {failed ? <AlertCircle size={28} /> : <ShieldCheck size={28} />}
      </div>
      <p className="eyebrow">Agent Desktop</p>
      <h1>{waiting ? "Enrollment is in progress" : failed ? "Enrollment needs attention" : "Enroll this device"}</h1>
      <p>
        {waiting
          ? "Finish the approval process to connect this device to your organization. Agent Desktop will update automatically when access is ready."
          : failed
            ? enrollment.detail ?? "The device could not be enrolled. Try again or contact your administrator."
            : "Connect this device to your organization to receive managed AI tool configuration, gateway access, and policy updates."}
      </p>
      <button className="button button-primary enrollment-action" type="button" onClick={onEnroll} disabled={busy}>
        {busy ? "Checking enrollment…" : waiting ? "Check enrollment" : failed ? "Try again" : "Enroll device"}
      </button>
      <small>Sign-in opens securely in your default browser.</small>
    </section>
  );
}

function ToolsPage({ discovery }: { discovery: Discovery | null }) {
  const agents = discovery?.agents ?? [];
  const mcpCount = agents.reduce((total, agent) => total + (agent.mcpServers?.length ?? 0), 0);
  const skillCount = agents.reduce((total, agent) => total + (agent.skills?.length ?? 0), 0);
  return (
    <div className="page-stack">
      <div className="page-heading">
        <div><h1>Discovered tools</h1><p>Developer tools and capabilities found locally by the Agent Desktop daemon.</p></div>
      </div>
      <div className="stat-grid">
        <div className="stat-card"><strong>{agents.length}</strong><span>Developer tools</span></div>
        <div className="stat-card"><strong>{mcpCount}</strong><span>MCP servers</span></div>
        <div className="stat-card"><strong>{skillCount}</strong><span>Skills</span></div>
      </div>
      <section className="card table-card">
        <CardHeader title="Local inventory" description={`${agents.length} installation${agents.length === 1 ? "" : "s"} discovered`} />
        {agents.length ? <div className="tool-inventory">{agents.map((agent) => <ToolInventory key={`${agent.kind}-${agent.executable}`} discovery={{ kind: agent.kind, version: agent.version, path: agent.executable, mcp_servers: agent.mcpServers, skills: agent.skills }} />)}</div> : (
          <div className="empty-inline"><Box size={20} /><span>No supported tools were discovered on this device.</span></div>
        )}
      </section>
    </div>
  );
}

function StatusPage({
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
  onLogout
}: {
  bootstrap: Bootstrap | null;
  connector: ConnectorSnapshot | null;
  managedDevice: ManagedDeviceSnapshot | null;
  discovery: Discovery | null;
  remoteConfig: string | null;
  settings: Settings;
  isSaving: boolean;
  isLoggingOut: boolean;
  onStartupChange: (checked: boolean) => void;
  onCopy: () => void;
  onLogout: () => void;
}) {
  const runtime = connector?.runtime;
  const managed = runtime?.mode === "managed" || Boolean(managedDevice?.configured);
  const enrolled = !managed || managedDevice?.enrollment === "approved";
  const daemonReady = connector?.state !== "offline" && Boolean(runtime);
  const ready = daemonReady && enrolled;
  const gatewayConfigured = gatewayIsConfigured(runtime?.gateway);
  const [confirmingLogout, setConfirmingLogout] = useState(false);
  const agents = discovery?.agents ?? [];
  const capabilityCount = agents.reduce(
    (total, agent) => total + (agent.mcpServers?.length ?? 0) + (agent.skills?.length ?? 0),
    0
  );

  return (
    <div className="page-stack status-page">
      <div className="status-hero">
        <span className={`status-hero-icon ${ready ? "ready" : "attention"}`}>
          {ready ? <Check size={22} /> : <AlertCircle size={22} />}
        </span>
        <div>
          <p className="eyebrow">Local device</p>
          <h1>{ready ? "Agent Desktop is running" : "Agent Desktop needs attention"}</h1>
          <p>{ready ? "Your organization settings and local tool inventory are active." : connector?.detail ?? "Review the status below."}</p>
        </div>
      </div>

      <section className="card status-overview">
        {managed ? (
          <div className="status-row">
            <span className="status-row-icon success"><ShieldCheck size={17} /></span>
            <div><strong>Organization access</strong><span>{managedDevice?.organizationName ?? "Managed organization"}</span></div>
            <span className={`badge ${enrolled ? "success" : "warning"}`}>{enrolled ? "Approved" : humanize(managedDevice?.enrollment)}</span>
          </div>
        ) : null}
        <div className="status-row">
          <span className={`status-row-icon ${daemonReady ? "success" : "danger"}`}><Gauge size={17} /></span>
          <div><strong>Local daemon</strong><span>Discovery, configuration, and controller connection</span></div>
          <span className={`badge ${daemonReady ? "success" : "danger"}`}>{daemonReady ? "Running" : "Offline"}</span>
        </div>
        <div className="status-row">
          <span className={`status-row-icon ${gatewayConfigured ? "success" : "neutral"}`}><Waypoints size={17} /></span>
          <div><strong>Inference gateway</strong><span>Optional routing for managed AI traffic</span></div>
          <span className={`badge ${gatewayConfigured ? "success" : "neutral"}`}>{gatewayConfigured ? "Configured" : "Not configured"}</span>
        </div>
        <div className="status-row">
          <span className={`status-row-icon ${agents.length ? "success" : "neutral"}`}><Laptop size={17} /></span>
          <div><strong>Discovered tools</strong><span>{agents.length} agent{agents.length === 1 ? "" : "s"} discovered · {capabilityCount} MCP servers and skills found</span></div>
          <span className={`badge ${agents.length ? "success" : "neutral"}`}>{agents.length} found</span>
        </div>
      </section>

      <details className="card runtime-card">
        <summary>
          <span><strong>Runtime</strong><small>Local application and daemon information</small></span>
          <span>View</span>
        </summary>
        <div className="runtime-card-body">
          <div className="runtime-card-actions">
            <button className="button button-secondary" type="button" onClick={onCopy}><Copy size={13} /> Copy diagnostics</button>
          </div>
          <dl className="runtime-grid">
            <Definition label="Mode" value={humanize(runtime?.mode)} />
            <Definition label="Operating system" value={humanize(runtime?.platform.os ?? bootstrap?.platform)} />
            <Definition label="Desktop version" value={bootstrap?.version ?? "Unavailable"} />
            <Definition label="Daemon version" value={runtime?.version ?? "Unavailable"} />
          </dl>
          <div className="inline-preference">
            <div><strong>Open window at startup</strong><span>The tray application continues running when this is off.</span></div>
            <label className="switch" aria-label="Open window at startup">
              <input type="checkbox" disabled={!bootstrap || isSaving} checked={settings.openOnStartup} onChange={(event) => onStartupChange(event.target.checked)} />
              <span className="switch-track" aria-hidden="true" />
            </label>
          </div>
        </div>
      </details>

      {remoteConfig || (managed && enrolled) ? (
        <details className="card advanced-config">
          <summary>
            <span><strong>Advanced</strong><small>Raw configuration and organization session controls</small></span>
            <span>View</span>
          </summary>
          <div className="advanced-config-body">
            {remoteConfig ? (
              <>
                <div className="advanced-config-heading">
                  <p>This is the exact controller configuration currently persisted and applied by the daemon.</p>
                  <button className="button button-secondary" type="button" onClick={() => navigator.clipboard.writeText(remoteConfig)}><Copy size={13} /> Copy YAML</button>
                </div>
                <pre><code>{remoteConfig}</code></pre>
              </>
            ) : null}

            {managed && enrolled ? (
              <section className="advanced-danger-zone" aria-labelledby="logout-heading">
                <div>
                  <p className="eyebrow">Danger zone</p>
                  <h2 id="logout-heading">Sign out of this organization</h2>
                  <p>Removes this device’s local organization credentials and stops managed access. It does not revoke the device record in the controller.</p>
                </div>
                {confirmingLogout ? (
                  <div className="logout-confirmation" role="group" aria-label="Confirm organization sign out">
                    <strong>Are you sure?</strong>
                    <div>
                      <button className="button button-secondary" type="button" onClick={() => setConfirmingLogout(false)} disabled={isLoggingOut}>Cancel</button>
                      <button className="button button-danger" type="button" onClick={onLogout} disabled={isLoggingOut}>
                        {isLoggingOut ? <LoaderCircle className="spin" size={13} /> : <LogOut size={13} />}
                        {isLoggingOut ? "Signing out…" : "Yes, sign out"}
                      </button>
                    </div>
                  </div>
                ) : (
                  <button className="button button-danger" type="button" onClick={() => setConfirmingLogout(true)}>
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

export function Desktop() {
  const [view, setView] = useState<View>("home");
  const [bootstrap, setBootstrap] = useState<Bootstrap | null>(null);
  const [settings, setSettings] = useState<Settings>(loadingSettings);
  const [connector, setConnector] = useState<ConnectorSnapshot | null>(null);
  const [claude, setClaude] = useState<ClaudeSnapshot | null>(null);
  const [managedDevice, setManagedDevice] = useState<ManagedDeviceSnapshot | null>(null);
  const [discovery, setDiscovery] = useState<Discovery | null>(null);
  const [remoteConfig, setRemoteConfig] = useState<string | null>(null);
  const [notice, setNotice] = useState<Notice>(null);
  const [apiKey, setApiKey] = useState("");
  const [showCredentialForm, setShowCredentialForm] = useState(false);
  const [isRefreshing, startRefreshing] = useTransition();
  const [isConnecting, startConnecting] = useTransition();
  const [isManaging, startManaging] = useTransition();
  const [isSaving, startSaving] = useTransition();
  const [isLoggingOut, startLoggingOut] = useTransition();
  const needsEnrollment = Boolean(managedDevice?.configured && managedDevice.enrollment !== "approved");

  useEffect(() => {
    let active = true;
    Promise.all([getBootstrap(), getClaudeStatus()])
      .then(([nextBootstrap, nextClaude]) => {
        if (!active) return;
        setBootstrap(nextBootstrap);
        setSettings(nextBootstrap.settings);
        setClaude(nextClaude);
      })
      .catch((error: unknown) => {
        if (active) setNotice({ tone: "error", message: errorMessage(error) });
      });
    return () => {
      active = false;
    };
  }, []);

  useEffect(() => {
    if (!managedDevice || !["pending", "issuing"].includes(managedDevice.enrollment)) return;
    let active = true;
    let timeout: number | undefined;
    const pollApproval = async () => {
      try {
        const nextManagedDevice = await setupManagedDevice();
        const nextClaude = nextManagedDevice.enrollment === "approved"
          ? await getClaudeStatus()
          : null;
        if (active) {
          startTransition(() => {
            setManagedDevice(nextManagedDevice);
            if (nextClaude) setClaude(nextClaude);
          });
          if (nextManagedDevice.enrollment === "approved") {
            setNotice({ tone: "success", message: "Organization access is ready" });
          }
        }
      } catch (error: unknown) {
        if (active) setNotice({ tone: "error", message: errorMessage(error) });
      } finally {
        if (active) timeout = window.setTimeout(pollApproval, 5000);
      }
    };
    timeout = window.setTimeout(pollApproval, 5000);
    return () => {
      active = false;
      if (timeout !== undefined) window.clearTimeout(timeout);
    };
  }, [managedDevice?.enrollment]);

  useEffect(() => {
    let active = true;
    const refresh = () => {
      Promise.all([getConnectorStatus(), getClaudeStatus(), getManagedDeviceStatus(), getDiscovery(), getRemoteConfig()])
        .then(([snapshot, nextClaude, nextManagedDevice, nextDiscovery, nextRemoteConfig]) => {
          if (active) {
            startTransition(() => {
              setConnector(snapshot);
              setClaude(nextClaude);
              setManagedDevice(nextManagedDevice);
              setDiscovery(nextDiscovery);
              setRemoteConfig(nextRemoteConfig);
            });
          }
        })
        .catch((error: unknown) => {
          if (active) setNotice({ tone: "error", message: errorMessage(error) });
        });
    };
    refresh();
    const interval = window.setInterval(refresh, 5000);
    const refreshWhenVisible = () => {
      if (!document.hidden) refresh();
    };
    document.addEventListener("visibilitychange", refreshWhenVisible);
    return () => {
      active = false;
      window.clearInterval(interval);
      document.removeEventListener("visibilitychange", refreshWhenVisible);
    };
  }, []);

  function refresh() {
    setNotice(null);
    startRefreshing(async () => {
      try {
        const [nextConnector, nextClaude, nextManagedDevice, nextDiscovery, nextRemoteConfig] = await Promise.all([
          getConnectorStatus(),
          getClaudeStatus(),
          getManagedDeviceStatus(),
          getDiscovery(),
          getRemoteConfig()
        ]);
        setConnector(nextConnector);
        setClaude(nextClaude);
        setManagedDevice(nextManagedDevice);
        setDiscovery(nextDiscovery);
        setRemoteConfig(nextRemoteConfig);
      } catch (error: unknown) {
        setNotice({ tone: "error", message: errorMessage(error) });
      }
    });
  }

  function handleConnect(providerApiKey?: string) {
    setNotice(null);
    startConnecting(async () => {
      try {
        setClaude(await connectClaude(providerApiKey));
        setBootstrap(await getBootstrap());
        setApiKey("");
        setShowCredentialForm(false);
        setNotice({ tone: "success", message: "Claude Code connected" });
      } catch (error: unknown) {
        setNotice({ tone: "error", message: errorMessage(error) });
      }
    });
  }

  function handleManagedSetup() {
    setNotice(null);
    startManaging(async () => {
      try {
        const nextManagedDevice = await setupManagedDevice();
        setManagedDevice(nextManagedDevice);
        const approved = nextManagedDevice.enrollment === "approved";
        setNotice({
          tone: "success",
          message: approved
            ? "Organization access is ready"
            : "Enrollment is starting; continue in your browser when prompted"
        });
        setConnector(await getConnectorStatus());
        setClaude(await getClaudeStatus());
      } catch (error: unknown) {
        setNotice({ tone: "error", message: errorMessage(error) });
      }
    });
  }

  function handleLogout() {
    setNotice(null);
    startLoggingOut(async () => {
      try {
        await logoutManagedDevice();
        const [nextConnector, nextManagedDevice, nextRemoteConfig] = await Promise.all([
          getConnectorStatus(),
          getManagedDeviceStatus(),
          getRemoteConfig()
        ]);
        setConnector(nextConnector);
        setManagedDevice(nextManagedDevice);
        setRemoteConfig(nextRemoteConfig);
        setView("home");
        setNotice({ tone: "success", message: "Signed out of the organization on this device" });
      } catch (error: unknown) {
        setNotice({ tone: "error", message: errorMessage(error) });
      }
    });
  }

  function handleStartupChange(checked: boolean) {
    const previous = settings;
    const next = { ...settings, openOnStartup: checked };
    setSettings(next);
    startSaving(async () => {
      try {
        setSettings(await saveSettings(next));
      } catch (error: unknown) {
        setSettings(previous);
        setNotice({ tone: "error", message: errorMessage(error) });
      }
    });
  }

  async function copyDiagnostics() {
    try {
      const managed = managedDevice
        ? {
            configured: managedDevice.configured,
            organizationName: managedDevice.organizationName,
            session: managedDevice.session,
            enrollment: managedDevice.enrollment,
            certificate: managedDevice.certificate
              ? {
                  notBefore: managedDevice.certificate.notBefore,
                  notAfter: managedDevice.certificate.notAfter
                }
              : null
          }
        : null;
      await navigator.clipboard.writeText(
        JSON.stringify({ desktop: bootstrap, connector, claude, managed }, null, 2)
      );
      setNotice({ tone: "success", message: "Diagnostics copied" });
    } catch (error: unknown) {
      setNotice({ tone: "error", message: errorMessage(error) });
    }
  }

  async function copyValue(label: string, value: string) {
    try {
      await navigator.clipboard.writeText(value);
      setNotice({ tone: "success", message: `${humanize(label)} copied` });
    } catch (error: unknown) {
      setNotice({ tone: "error", message: errorMessage(error) });
    }
  }

  async function handleOpenManagedPage(page: ManagedPage) {
    try {
      setNotice(null);
      await openManagedPage(page);
    } catch (error: unknown) {
      setNotice({ tone: "error", message: errorMessage(error) });
    }
  }

  const pageTitle = needsEnrollment
    ? "Enrollment"
    : view === "home"
      ? "Status"
      : "Discovered tools";

  const navigate = (nextView: View) => {
    setView(nextView);
    setNotice(null);
  };

  return (
    <div className="desktop-shell">
      <aside className="desktop-sidebar">
        <div className="desktop-brand">
          <img src={agentdesktopIcon} alt="" />
          <span>Agentdesktop</span>
        </div>
        {!needsEnrollment ? (
          <nav className="desktop-nav" aria-label="Application">
            <button type="button" className={view === "home" ? "active" : ""} onClick={() => navigate("home")}><Gauge size={18} />Status</button>
            <button type="button" className={view === "tools" ? "active" : ""} onClick={() => navigate("tools")}><Laptop size={18} />Tools</button>
          </nav>
        ) : null}
      </aside>

      <section className="desktop-main">
        <header className="desktop-page-header">
          <h1>{pageTitle}</h1>
          <button className="desktop-refresh" type="button" onClick={refresh} disabled={isRefreshing}>
            {isRefreshing ? <LoaderCircle className="spin" size={14} /> : <RefreshCw size={14} />} Refresh
          </button>
        </header>
        <main className="desktop-content">

          {notice ? <div className={`notice notice-${notice.tone}`} role="status">{notice.message}</div> : null}

        <PageBoundary key={`${view}-${needsEnrollment}`}>
        {needsEnrollment && managedDevice ? (
          <EnrollmentWelcome enrollment={managedDevice} busy={isManaging} onEnroll={handleManagedSetup} />
        ) : view === "home" ? (
          <StatusPage
            bootstrap={bootstrap}
            connector={connector}
            managedDevice={managedDevice}
            discovery={discovery}
            remoteConfig={remoteConfig}
            settings={settings}
            isSaving={isSaving}
            isLoggingOut={isLoggingOut}
            onStartupChange={handleStartupChange}
            onCopy={copyDiagnostics}
            onLogout={handleLogout}
          />
        ) : view === "tools" ? (
          <ToolsPage discovery={discovery} />
        ) : (
          <StatusPage bootstrap={bootstrap} connector={connector} managedDevice={managedDevice} discovery={discovery} remoteConfig={remoteConfig} settings={settings} isSaving={isSaving} isLoggingOut={isLoggingOut} onStartupChange={handleStartupChange} onCopy={copyDiagnostics} onLogout={handleLogout} />
        )}
        </PageBoundary>
        </main>
      </section>
    </div>
  );
}
