import { CardHeader, ToolInventory } from "@agentdesktop/ui";
import {
  AlertCircle,
  Box,
  Check,
  Copy,
  Gauge,
  Laptop,
  LoaderCircle,
  LogOut,
  RefreshCw,
  ShieldCheck,
  Waypoints,
} from "lucide-react";
import type { ErrorInfo, ReactNode } from "react";
import {
  Component,
  startTransition,
  useEffect,
  useState,
  useTransition,
} from "react";
import agentdesktopIcon from "../../ui/assets/app-icon.svg";

import {
  getBootstrap,
  getConnectorStatus,
  getDiscovery,
  getManagedDeviceStatus,
  getRemoteConfig,
  logoutManagedDevice,
  saveSettings,
  setupManagedDevice,
} from "./backend";
import type {
  Bootstrap,
  ConnectorSnapshot,
  Discovery,
  ManagedDeviceSnapshot,
  Settings,
} from "./types";

type View = "home" | "tools";
type Notice = { tone: "success" | "error"; message: string } | null;

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

function gatewayIsConfigured(gateway: string | undefined): boolean {
  return gateway === "reachable" || gateway === "configured";
}

function Definition({
  label,
  value,
}: {
  label: string;
  value: React.ReactNode;
}) {
  return (
    <div className="definition-row">
      <dt>{label}</dt>
      <dd>{value}</dd>
    </div>
  );
}

function EnrollmentWelcome({
  enrollment,
  busy,
  onEnroll,
}: {
  enrollment: ManagedDeviceSnapshot;
  busy: boolean;
  onEnroll: () => void;
}) {
  const waiting =
    enrollment.enrollment === "pending" || enrollment.enrollment === "issuing";
  const failed =
    enrollment.enrollment === "unavailable" ||
    enrollment.enrollment === "rejected";
  return (
    <section className="enrollment-welcome">
      <div
        className={`enrollment-mark ${failed ? "enrollment-mark-error" : ""}`}
      >
        {failed ? <AlertCircle size={28} /> : <ShieldCheck size={28} />}
      </div>
      <p className="eyebrow">Agent Desktop</p>
      <h1>
        {waiting
          ? "Enrollment is in progress"
          : failed
            ? "Enrollment needs attention"
            : "Enroll this device"}
      </h1>
      <p>
        {waiting
          ? "Finish the approval process to connect this device to your organization. Agent Desktop will update automatically when access is ready."
          : failed
            ? (enrollment.detail ??
              "The device could not be enrolled. Try again or contact your administrator.")
            : "Connect this device to your organization to receive managed AI tool configuration, gateway access, and policy updates."}
      </p>
      <button
        className="button button-primary enrollment-action"
        type="button"
        onClick={onEnroll}
        disabled={busy}
      >
        {busy
          ? "Checking enrollment…"
          : waiting
            ? "Check enrollment"
            : failed
              ? "Try again"
              : "Enroll device"}
      </button>
      <small>Sign-in opens securely in your default browser.</small>
    </section>
  );
}

function ToolsPage({ discovery }: { discovery: Discovery | null }) {
  const agents = discovery?.agents ?? [];
  const mcpCount = agents.reduce(
    (total, agent) => total + (agent.mcpServers?.length ?? 0),
    0,
  );
  const skillCount = agents.reduce(
    (total, agent) => total + (agent.skills?.length ?? 0),
    0,
  );
  return (
    <div className="page-stack">
      <div className="page-heading">
        <div>
          <h1>Discovered tools</h1>
          <p>
            Developer tools and capabilities found locally by the Agent Desktop
            daemon.
          </p>
        </div>
      </div>
      <div className="stat-grid">
        <div className="stat-card">
          <strong>{agents.length}</strong>
          <span>Developer tools</span>
        </div>
        <div className="stat-card">
          <strong>{mcpCount}</strong>
          <span>MCP servers</span>
        </div>
        <div className="stat-card">
          <strong>{skillCount}</strong>
          <span>Skills</span>
        </div>
      </div>
      <section className="card table-card">
        <CardHeader
          title="Local inventory"
          description={`${agents.length} installation${agents.length === 1 ? "" : "s"} discovered`}
        />
        {agents.length ? (
          <div className="tool-inventory">
            {agents.map((agent) => (
              <ToolInventory
                key={`${agent.kind}-${agent.executable}`}
                discovery={{
                  kind: agent.kind,
                  version: agent.version,
                  path: agent.executable,
                  mcp_servers: agent.mcpServers,
                  skills: agent.skills,
                }}
              />
            ))}
          </div>
        ) : (
          <div className="empty-inline">
            <Box size={20} />
            <span>No supported tools were discovered on this device.</span>
          </div>
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
  onLogout,
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
  const managed =
    runtime?.mode === "managed" || Boolean(managedDevice?.configured);
  const enrolled = !managed || managedDevice?.enrollment === "approved";
  const daemonReady = connector?.state !== "offline" && Boolean(runtime);
  const ready = daemonReady && enrolled;
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
          <h1>
            {ready
              ? "Agent Desktop is running"
              : "Agent Desktop needs attention"}
          </h1>
          <p>
            {ready
              ? "Your organization settings and local tool inventory are active."
              : (connector?.detail ?? "Review the status below.")}
          </p>
        </div>
      </div>

      <section className="card status-overview">
        {managed ? (
          <div className="status-row">
            <span className="status-row-icon success">
              <ShieldCheck size={17} />
            </span>
            <div>
              <strong>Organization access</strong>
              <span>
                {managedDevice?.organizationName ?? "Managed organization"}
              </span>
            </div>
            <span className={`badge ${enrolled ? "success" : "warning"}`}>
              {enrolled ? "Approved" : humanize(managedDevice?.enrollment)}
            </span>
          </div>
        ) : null}
        <div className="status-row">
          <span
            className={`status-row-icon ${daemonReady ? "success" : "danger"}`}
          >
            <Gauge size={17} />
          </span>
          <div>
            <strong>Local daemon</strong>
            <span>Discovery, configuration, and controller connection</span>
          </div>
          <span className={`badge ${daemonReady ? "success" : "danger"}`}>
            {daemonReady ? "Running" : "Offline"}
          </span>
        </div>
        <div className="status-row">
          <span
            className={`status-row-icon ${gatewayConfigured ? "success" : "neutral"}`}
          >
            <Waypoints size={17} />
          </span>
          <div>
            <strong>Inference gateway</strong>
            <span>Optional routing for managed AI traffic</span>
          </div>
          <span
            className={`badge ${gatewayConfigured ? "success" : "neutral"}`}
          >
            {gatewayConfigured ? "Configured" : "Not configured"}
          </span>
        </div>
        <div className="status-row">
          <span
            className={`status-row-icon ${agents.length ? "success" : "neutral"}`}
          >
            <Laptop size={17} />
          </span>
          <div>
            <strong>Discovered tools</strong>
            <span>
              {agents.length} agent{agents.length === 1 ? "" : "s"} discovered ·{" "}
              {capabilityCount} MCP servers and skills found
            </span>
          </div>
          <span className={`badge ${agents.length ? "success" : "neutral"}`}>
            {agents.length} found
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
                    onClick={() => navigator.clipboard.writeText(remoteConfig)}
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

export function Desktop() {
  const [view, setView] = useState<View>("home");
  const [bootstrap, setBootstrap] = useState<Bootstrap | null>(null);
  const [settings, setSettings] = useState<Settings>(loadingSettings);
  const [connector, setConnector] = useState<ConnectorSnapshot | null>(null);
  const [managedDevice, setManagedDevice] =
    useState<ManagedDeviceSnapshot | null>(null);
  const [discovery, setDiscovery] = useState<Discovery | null>(null);
  const [remoteConfig, setRemoteConfig] = useState<string | null>(null);
  const [notice, setNotice] = useState<Notice>(null);
  const [isRefreshing, startRefreshing] = useTransition();
  const [isManaging, startManaging] = useTransition();
  const [isSaving, startSaving] = useTransition();
  const [isLoggingOut, startLoggingOut] = useTransition();
  const needsEnrollment = Boolean(
    managedDevice?.configured && managedDevice.enrollment !== "approved",
  );

  useEffect(() => {
    let active = true;
    getBootstrap()
      .then((nextBootstrap) => {
        if (!active) return;
        setBootstrap(nextBootstrap);
        setSettings(nextBootstrap.settings);
      })
      .catch((error: unknown) => {
        if (active) setNotice({ tone: "error", message: errorMessage(error) });
      });
    return () => {
      active = false;
    };
  }, []);

  useEffect(() => {
    if (
      !managedDevice ||
      !["pending", "issuing"].includes(managedDevice.enrollment)
    )
      return;
    let active = true;
    let timeout: number | undefined;
    const pollApproval = async () => {
      try {
        const nextManagedDevice = await setupManagedDevice();
        if (active) {
          startTransition(() => {
            setManagedDevice(nextManagedDevice);
          });
          if (nextManagedDevice.enrollment === "approved") {
            setNotice({
              tone: "success",
              message: "Organization access is ready",
            });
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
      Promise.all([
        getConnectorStatus(),
        getManagedDeviceStatus(),
        getDiscovery(),
        getRemoteConfig(),
      ])
        .then(
          ([snapshot, nextManagedDevice, nextDiscovery, nextRemoteConfig]) => {
            if (active) {
              startTransition(() => {
                setConnector(snapshot);
                setManagedDevice(nextManagedDevice);
                setDiscovery(nextDiscovery);
                setRemoteConfig(nextRemoteConfig);
              });
            }
          },
        )
        .catch((error: unknown) => {
          if (active)
            setNotice({ tone: "error", message: errorMessage(error) });
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
        const [
          nextConnector,
          nextManagedDevice,
          nextDiscovery,
          nextRemoteConfig,
        ] = await Promise.all([
          getConnectorStatus(),
          getManagedDeviceStatus(),
          getDiscovery(),
          getRemoteConfig(),
        ]);
        setConnector(nextConnector);
        setManagedDevice(nextManagedDevice);
        setDiscovery(nextDiscovery);
        setRemoteConfig(nextRemoteConfig);
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
            : "Enrollment is starting; continue in your browser when prompted",
        });
        setConnector(await getConnectorStatus());
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
        const [nextConnector, nextManagedDevice, nextRemoteConfig] =
          await Promise.all([
            getConnectorStatus(),
            getManagedDeviceStatus(),
            getRemoteConfig(),
          ]);
        setConnector(nextConnector);
        setManagedDevice(nextManagedDevice);
        setRemoteConfig(nextRemoteConfig);
        setView("home");
        setNotice({
          tone: "success",
          message: "Signed out of the organization on this device",
        });
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
            enrollment: managedDevice.enrollment,
          }
        : null;
      await navigator.clipboard.writeText(
        JSON.stringify({ desktop: bootstrap, connector, managed }, null, 2),
      );
      setNotice({ tone: "success", message: "Diagnostics copied" });
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
            <button
              type="button"
              className={view === "home" ? "active" : ""}
              onClick={() => navigate("home")}
            >
              <Gauge size={18} />
              Status
            </button>
            <button
              type="button"
              className={view === "tools" ? "active" : ""}
              onClick={() => navigate("tools")}
            >
              <Laptop size={18} />
              Tools
            </button>
          </nav>
        ) : null}
      </aside>

      <section className="desktop-main">
        <header className="desktop-page-header">
          <h1>{pageTitle}</h1>
          <button
            className="desktop-refresh"
            type="button"
            onClick={refresh}
            disabled={isRefreshing}
          >
            {isRefreshing ? (
              <LoaderCircle className="spin" size={14} />
            ) : (
              <RefreshCw size={14} />
            )}{" "}
            Refresh
          </button>
        </header>
        <main className="desktop-content">
          {notice ? (
            <div className={`notice notice-${notice.tone}`} role="status">
              {notice.message}
            </div>
          ) : null}

          <PageBoundary key={`${view}-${needsEnrollment}`}>
            {needsEnrollment && managedDevice ? (
              <EnrollmentWelcome
                enrollment={managedDevice}
                busy={isManaging}
                onEnroll={handleManagedSetup}
              />
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
            ) : (
              <ToolsPage discovery={discovery} />
            )}
          </PageBoundary>
        </main>
      </section>
    </div>
  );
}
