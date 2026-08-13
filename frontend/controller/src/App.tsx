import {
  CardHeader,
  friendlyTool,
  ToolIcon,
  ToolInventory,
} from "@agentdesktop/ui";
import {
  ArrowLeft,
  Box,
  Check,
  ChevronRight,
  CircleAlert,
  Code2,
  Copy,
  Gauge,
  Laptop,
  Plus,
  RefreshCw,
  Search,
  Settings,
  SlidersHorizontal,
  Trash2,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import agentdesktopIcon from "../../ui/assets/app-icon.svg";

type Device = {
  id: string;
  hostname: string;
  os: string;
  architecture: string;
  agent_version: string;
  created_at: number;
  last_seen_at: number | null;
  enrolled_by_issuer: string;
  enrolled_by_subject: string;
  config_revision: number | null;
  config_state: number | null;
  config_error: string | null;
  config_updated_at: number | null;
  discovery_count: number;
  installed_tools: string[];
};

type DeviceDetail = Device & {
  discoveries: Array<{
    kind: string;
    version: string;
    path: string;
    mcp_servers?: Array<{
      name: string;
      transport: string;
      command?: string;
      url?: string;
      enabled: boolean;
      source: string;
    }>;
    skills?: Array<{
      path: string;
      frontMatter: Record<string, unknown>;
    }>;
  }>;
  recent_events: Array<{
    id: string;
    timestamp_unix_ms: number;
    event_type: string;
    payload: {
      clientId?: string;
      toolName?: string;
      toolUseId?: string;
      toolInput?: unknown;
      sessionId?: string;
    };
  }>;
};

type Overview = {
  total_devices: number;
  online_devices: number;
  offline_devices: number;
  config_failures: number;
  active_revision: number | null;
  recent_devices: Device[];
};

type ControllerSettings = {
  fleet_listen: string;
  admin_listen: string;
  oidc_enabled: boolean;
  tls_enabled: boolean;
  gateway_jwt_enabled: boolean;
};

type AgentKind = "claudeCode" | "claudeDesktop" | "codex" | "openCode";

type AgentDraft = {
  kind: AgentKind;
  useGateway: boolean;
  settings: string;
};

type DaemonConfigDocument = {
  inferenceGateway?: {
    url: string;
    authentication?: {
      type: string;
      audience?: string;
    };
  };
  telemetry?: {
    events?: string[];
  };
  programs?: Partial<Record<AgentKind, Record<string, unknown>>>;
};

type ActiveDaemonConfig = {
  config: DaemonConfigDocument | null;
};

const nav = [
  { href: "/", label: "Overview", icon: Gauge },
  { href: "/devices", label: "Devices", icon: Laptop },
  { href: "/configuration", label: "Configuration", icon: SlidersHorizontal },
  { href: "/settings", label: "Settings", icon: Settings },
];

function usePath() {
  const [path, setPath] = useState(window.location.pathname);
  useEffect(() => {
    const update = () => setPath(window.location.pathname);
    window.addEventListener("popstate", update);
    return () => window.removeEventListener("popstate", update);
  }, []);
  return path;
}

function navigate(href: string) {
  window.history.pushState({}, "", href);
  window.dispatchEvent(new PopStateEvent("popstate"));
  window.scrollTo({ top: 0 });
}

function Link({
  href,
  className,
  children,
}: React.PropsWithChildren<{ href: string; className?: string }>) {
  return (
    <a
      href={href}
      className={className}
      onClick={(event) => {
        if (!event.metaKey && !event.ctrlKey && !event.shiftKey) {
          event.preventDefault();
          navigate(href);
        }
      }}
    >
      {children}
    </a>
  );
}

function useApi<T>(path: string) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    setLoading(true);
    fetch(path, { signal: controller.signal })
      .then(async (response) => {
        if (!response.ok) throw new Error(await response.text());
        return response.json() as Promise<T>;
      })
      .then((response) => {
        if (active) setData(response);
      })
      .catch((reason: Error) => {
        if (active && reason.name !== "AbortError")
          setError(reason.message || "Request failed");
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
      controller.abort();
    };
  }, [path]);
  return { data, error, loading };
}

export function App() {
  const path = usePath();
  const pageTitle = path.startsWith("/devices/")
    ? "Device details"
    : (nav.find((item) => item.href === path)?.label ?? "Overview");

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <img className="brand-mark" src={agentdesktopIcon} alt="" />
          <span>Agentdesktop</span>
        </div>
        <nav className="primary-nav" aria-label="Primary navigation">
          {nav.map((item) => {
            const active =
              item.href === "/" ? path === "/" : path.startsWith(item.href);
            return (
              <Link
                href={item.href}
                className={`nav-item ${active ? "active" : ""}`}
                key={item.href}
              >
                <item.icon size={18} />
                <span>{item.label}</span>
              </Link>
            );
          })}
        </nav>
      </aside>

      <main className="main-area">
        <header className="topbar">
          <h1>{pageTitle}</h1>
          <button
            type="button"
            className="refresh-button"
            onClick={() => window.location.reload()}
          >
            <RefreshCw size={14} /> Refresh
          </button>
        </header>
        <div className="page-content">
          {path === "/" && <OverviewPage />}
          {path === "/devices" && <DevicesPage />}
          {path.startsWith("/devices/") && (
            <DevicePage id={decodeURIComponent(path.slice(9))} />
          )}
          {path === "/configuration" && <ConfigurationPage />}
          {path === "/settings" && <SettingsPage />}
          {!nav.some((item) => item.href === path) &&
            !path.startsWith("/devices/") && <NotFound />}
        </div>
      </main>
    </div>
  );
}

function OverviewPage() {
  const query = useApi<Overview>("/api/v1/overview");
  if (query.loading) return <PageSkeleton />;
  if (query.error || !query.data) return <ErrorState message={query.error} />;
  const data = query.data;
  return (
    <div className="stack">
      <section className="welcome-row">
        <div>
          <h2>Fleet overview</h2>
          <p>
            Live health and configuration state across your managed developer
            machines.
          </p>
        </div>
      </section>

      <section className="stat-grid card">
        <StatCard label="Total devices" value={data.total_devices} />
        <StatCard label="Online" value={data.online_devices} />
        <StatCard label="Offline" value={data.offline_devices} />
        <StatCard label="Config failures" value={data.config_failures} />
      </section>

      <div className="overview-grid">
        <section className="card table-card">
          <CardHeader
            title="Recent devices"
            description="Most recently connected machines"
            action={
              <Link href="/devices" className="text-link">
                View all <ChevronRight size={14} />
              </Link>
            }
          />
          {data.recent_devices.length ? (
            <DeviceTable devices={data.recent_devices} compact />
          ) : (
            <EmptyDevices />
          )}
        </section>
        <section className="card config-summary">
          <CardHeader
            title="Daemon configuration"
            description="Controller-wide rollout"
          />
          <div className="config-summary-body">
            <strong>
              {data.active_revision ? `r${data.active_revision}` : "—"}
            </strong>
            <div>
              <h3>
                {data.active_revision
                  ? `Revision ${data.active_revision} active`
                  : "No active configuration"}
              </h3>
              <p>
                {data.active_revision
                  ? "Sent to agents when they connect."
                  : "Start the controller with a daemon config to begin a rollout."}
              </p>
            </div>
          </div>
          <Link href="/configuration" className="config-link">
            View configuration <ChevronRight size={14} />
          </Link>
        </section>
      </div>
    </div>
  );
}

function StatCard({ label, value }: { label: string; value: number }) {
  return (
    <article className="stat-card">
      <strong>{value}</strong>
      <span>{label}</span>
    </article>
  );
}

function DevicesPage() {
  const query = useApi<Device[]>("/api/v1/devices");
  const [search, setSearch] = useState("");
  const filtered = useMemo(
    () =>
      (query.data ?? []).filter((device) =>
        `${device.hostname} ${device.os} ${device.agent_version}`
          .toLowerCase()
          .includes(search.toLowerCase()),
      ),
    [query.data, search],
  );
  return (
    <div className="stack">
      <section className="section-intro">
        <div>
          <h2>Managed devices</h2>
          <p>
            Inventory, connectivity, and rollout state for every enrolled
            machine.
          </p>
        </div>
        <div className="search-box">
          <Search size={16} />
          <input
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="Search devices…"
            aria-label="Search devices"
          />
        </div>
      </section>
      <section className="card table-card">
        {query.loading ? (
          <PageSkeleton rows={5} />
        ) : query.error ? (
          <ErrorState message={query.error} />
        ) : filtered.length ? (
          <DeviceTable devices={filtered} />
        ) : (
          <EmptyDevices searching={Boolean(search)} />
        )}
      </section>
    </div>
  );
}

function DeviceTable({
  devices,
  compact = false,
}: {
  devices: Device[];
  compact?: boolean;
}) {
  return (
    <div className="table-scroll">
      <table>
        <thead>
          <tr>
            <th>Device</th>
            <th>Status</th>
            <th>Platform</th>
            {!compact && <th>Agent</th>}
            <th>Tools</th>
            <th>Configuration</th>
            <th>
              <span className="sr-only">Open</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {devices.map((device) => (
            <tr
              key={device.id}
              onClick={() =>
                navigate(`/devices/${encodeURIComponent(device.id)}`)
              }
            >
              <td>
                <div className="device-cell">
                  <div>
                    <strong>{device.hostname || "Unnamed device"}</strong>
                    <span>{device.id.slice(0, 8)}</span>
                  </div>
                </div>
              </td>
              <td>
                <OnlineBadge timestamp={device.last_seen_at} />
              </td>
              <td>
                <div className="cell-stack">
                  <strong>{friendlyOs(device.os)}</strong>
                  <span>{device.architecture || "Unknown architecture"}</span>
                </div>
              </td>
              {!compact && (
                <td>
                  <span className="mono-soft">
                    {device.agent_version || "—"}
                  </span>
                </td>
              )}
              <td>
                <ToolAvatarGroup kinds={device.installed_tools} />
              </td>
              <td>
                <ConfigBadge
                  state={device.config_state}
                  revision={device.config_revision}
                />
              </td>
              <td>
                <ChevronRight size={16} className="row-arrow" />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function DevicePage({ id }: { id: string }) {
  const query = useApi<DeviceDetail>(
    `/api/v1/devices/${encodeURIComponent(id)}`,
  );
  const [showDelete, setShowDelete] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [deleteError, setDeleteError] = useState<string | null>(null);

  async function deleteDevice() {
    setDeleting(true);
    setDeleteError(null);
    try {
      const response = await fetch(
        `/api/v1/devices/${encodeURIComponent(id)}`,
        {
          method: "DELETE",
        },
      );
      if (!response.ok) throw new Error(await response.text());
      navigate("/devices");
    } catch (error) {
      setDeleteError(error instanceof Error ? error.message : "Delete failed");
      setDeleting(false);
    }
  }

  if (query.loading) return <PageSkeleton />;
  if (query.error || !query.data) return <ErrorState message={query.error} />;
  const device = query.data;
  return (
    <div className="stack">
      <Link href="/devices" className="back-link">
        <ArrowLeft size={15} /> Back to devices
      </Link>
      <section className="device-hero">
        <div className="device-identity">
          <div>
            <h2>{device.hostname}</h2>
            <OnlineBadge timestamp={device.last_seen_at} />
          </div>
          <p>{device.id}</p>
        </div>
      </section>
      <div className="detail-grid">
        <section className="card detail-card">
          <CardHeader
            title="Device information"
            description="Reported by the local agent"
          />
          <DefinitionList
            entries={[
              ["Operating system", friendlyOs(device.os)],
              ["Architecture", device.architecture || "Unknown"],
              ["Agent version", device.agent_version || "Unknown"],
              ["Last seen", formatTime(device.last_seen_at)],
              ["Enrolled", formatDate(device.created_at)],
            ]}
          />
        </section>
        <section className="card detail-card">
          <CardHeader
            title="Configuration state"
            description="Latest reconciliation reported by the agent"
          />
          <div className="config-state-large">
            <ConfigBadge
              state={device.config_state}
              revision={device.config_revision}
            />
          </div>
          <DefinitionList
            entries={[
              ["Last update", formatTime(device.config_updated_at)],
              [
                "Desired revision",
                device.config_revision
                  ? `Revision ${device.config_revision}`
                  : "Not reported",
              ],
            ]}
          />
          {device.config_error && (
            <div className="error-callout">
              <CircleAlert size={16} />
              <span>{device.config_error}</span>
            </div>
          )}
        </section>
      </div>
      <section className="card table-card">
        <CardHeader
          title="Recent activity"
          description={`${device.recent_events.length} recent telemetry event${device.recent_events.length === 1 ? "" : "s"}`}
        />
        {device.recent_events.length ? (
          <div className="event-list">
            {device.recent_events.map((event) => (
              <div className="event-row" key={event.id}>
                <span className="event-source">
                  <ToolIcon kind={event.payload.clientId ?? ""} />
                  <span>
                    <strong>
                      {event.payload.toolName ??
                        friendlyEvent(event.event_type)}
                    </strong>
                    <small>
                      {friendlyTool(event.payload.clientId ?? "Unknown")}
                    </small>
                  </span>
                </span>
                <code title={telemetryDetail(event.payload)}>
                  {telemetryDetail(event.payload)}
                </code>
                <time
                  dateTime={new Date(event.timestamp_unix_ms).toISOString()}
                >
                  {formatTimeMilliseconds(event.timestamp_unix_ms)}
                </time>
              </div>
            ))}
          </div>
        ) : (
          <div className="empty-inline">
            <Code2 size={20} />
            <span>No telemetry has been reported by this device.</span>
          </div>
        )}
      </section>
      <section className="card table-card">
        <CardHeader
          title="Discovered developer tools"
          description={`${device.discoveries.length} installation${device.discoveries.length === 1 ? "" : "s"} reported`}
        />
        {device.discoveries.length ? (
          <div className="tool-inventory">
            {device.discoveries.map((item) => (
              <ToolInventory
                key={`${item.kind}-${item.path}`}
                discovery={item}
              />
            ))}
          </div>
        ) : (
          <div className="empty-inline">
            <Box size={20} />
            <span>No tools have been reported by this device.</span>
          </div>
        )}
      </section>
      <section className="card detail-card">
        <CardHeader
          title="Enrollment identity"
          description="Identity captured when this device joined the fleet"
        />
        <DefinitionList
          entries={[
            ["Issuer", device.enrolled_by_issuer || "Enrollment token"],
            ["Subject", device.enrolled_by_subject || "Device credential"],
          ]}
        />
      </section>
      <section className="danger-zone">
        <div>
          <h3>Delete device</h3>
          <p>
            Remove this device, its credential, inventory, telemetry, and
            configuration status.
          </p>
        </div>
        <button
          type="button"
          className="destructive-button"
          onClick={() => setShowDelete(true)}
        >
          <Trash2 size={14} /> Delete device
        </button>
      </section>
      {showDelete && (
        <DeleteDeviceDialog
          hostname={device.hostname}
          deleting={deleting}
          error={deleteError}
          onCancel={() => {
            if (!deleting) {
              setShowDelete(false);
              setDeleteError(null);
            }
          }}
          onConfirm={deleteDevice}
        />
      )}
    </div>
  );
}

function DeleteDeviceDialog({
  hostname,
  deleting,
  error,
  onCancel,
  onConfirm,
}: {
  hostname: string;
  deleting: boolean;
  error: string | null;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    dialog.current?.showModal();
    return () => dialog.current?.close();
  }, []);
  return (
    <dialog
      ref={dialog}
      className="delete-dialog"
      aria-labelledby="delete-device-title"
      onCancel={(event) => {
        event.preventDefault();
        onCancel();
      }}
    >
      <div className="dialog-body">
        <h2 id="delete-device-title">Delete {hostname || "this device"}?</h2>
        <p>
          This removes the device credential and all controller inventory. A
          running agent will be rejected the next time it connects and must be
          re-enrolled.
        </p>
        {error && <div className="dialog-error">{error}</div>}
      </div>
      <div className="dialog-actions">
        <button
          type="button"
          className="button secondary"
          disabled={deleting}
          onClick={onCancel}
        >
          Cancel
        </button>
        <button
          type="button"
          className="destructive-button"
          disabled={deleting}
          onClick={onConfirm}
        >
          {deleting ? "Deleting…" : "Delete device"}
        </button>
      </div>
    </dialog>
  );
}

function ConfigurationPage() {
  const addAgentMenu = useRef<HTMLDetailsElement>(null);
  const initializedFromController = useRef(false);
  const activeConfig = useApi<ActiveDaemonConfig>("/api/v1/daemon-config");
  const [gateway, setGateway] = useState(true);
  const [gatewayUrl, setGatewayUrl] = useState("https://gateway.example.com");
  const [controllerJwt, setControllerJwt] = useState(true);
  const [audience, setAudience] = useState("agentgateway");
  const [sessionNewTelemetry, setSessionNewTelemetry] = useState(false);
  const [toolUseTelemetry, setToolUseTelemetry] = useState(false);
  const [toolInputTelemetry, setToolInputTelemetry] = useState(false);
  const [agents, setAgents] = useState<AgentDraft[]>([
    { kind: "claudeCode", useGateway: true, settings: "" },
  ]);
  const [copied, setCopied] = useState(false);
  const yaml = daemonConfigYaml({
    gateway,
    gatewayUrl,
    controllerJwt,
    audience,
    sessionNewTelemetry,
    toolUseTelemetry,
    toolInputTelemetry,
    agents,
  });
  const availableAgents = configurableAgents.filter(
    (candidate) => !agents.some((agent) => agent.kind === candidate.kind),
  );

  useEffect(() => {
    if (
      initializedFromController.current ||
      activeConfig.loading ||
      activeConfig.error ||
      !activeConfig.data
    ) {
      return;
    }
    initializedFromController.current = true;
    const config = activeConfig.data.config;
    if (!config) return;

    const inferenceGateway = config.inferenceGateway;
    const events = new Set(config.telemetry?.events ?? []);
    setGateway(Boolean(inferenceGateway));
    if (inferenceGateway) {
      setGatewayUrl(inferenceGateway.url);
      setControllerJwt(
        inferenceGateway.authentication?.type === "controllerJwt",
      );
      setAudience(inferenceGateway.authentication?.audience ?? "agentgateway");
    }
    setSessionNewTelemetry(events.has("session.new"));
    setToolUseTelemetry(events.has("tool.use") || events.has("tool.use.input"));
    setToolInputTelemetry(events.has("tool.use.input"));
    setAgents(agentDrafts(config.programs));
  }, [activeConfig.data, activeConfig.error, activeConfig.loading]);

  useEffect(() => {
    const closeMenu = (event: PointerEvent) => {
      const menu = addAgentMenu.current;
      if (menu?.open && !menu.contains(event.target as Node)) {
        menu.removeAttribute("open");
      }
    };
    document.addEventListener("pointerdown", closeMenu);
    return () => document.removeEventListener("pointerdown", closeMenu);
  }, []);
  async function copyYaml() {
    await navigator.clipboard.writeText(yaml);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1500);
  }

  function updateAgent(kind: AgentKind, update: Partial<AgentDraft>) {
    setAgents((current) =>
      current.map((agent) =>
        agent.kind === kind ? { ...agent, ...update } : agent,
      ),
    );
  }

  function addAgent(selectedAgent: AgentKind) {
    const definition = configurableAgents.find(
      (candidate) => candidate.kind === selectedAgent,
    );
    setAgents((current) => [
      ...current,
      {
        kind: selectedAgent,
        useGateway: true,
        settings: definition?.initialSettings ?? "",
      },
    ]);
  }

  return (
    <div className="stack">
      <section className="section-intro">
        <div>
          <h2>Build a configuration</h2>
          <p>Choose the settings to manage, then copy the generated YAML.</p>
        </div>
      </section>
      <div className="configuration-builder">
        <section className="card wizard-card">
          <details className="wizard-section" open={gateway}>
            <summary className="wizard-section-summary">
              <span className="wizard-section-title">
                <strong>Inference gateway</strong>
                <small>Shared connection settings for managed agents.</small>
              </span>
              <ChevronRight size={15} />
            </summary>
            <div className="wizard-section-content">
              <label className="toggle-row">
                <span>
                  <strong>Enable inference gateway</strong>
                  <small>Agents can opt into these shared settings.</small>
                </span>
                <input
                  type="checkbox"
                  checked={gateway}
                  onChange={(event) => setGateway(event.target.checked)}
                />
              </label>
              {gateway && (
                <div className="form-grid">
                  <label className="field full-width">
                    <span>Gateway URL</span>
                    <input
                      value={gatewayUrl}
                      onChange={(event) => setGatewayUrl(event.target.value)}
                    />
                  </label>
                  <label className="toggle-row compact full-width">
                    <span>
                      <strong>Controller JWT</strong>
                      <small>Use identity-aware short-lived credentials.</small>
                    </span>
                    <input
                      type="checkbox"
                      checked={controllerJwt}
                      onChange={(event) =>
                        setControllerJwt(event.target.checked)
                      }
                    />
                  </label>
                  {controllerJwt && (
                    <label className="field full-width">
                      <span>JWT audience</span>
                      <input
                        value={audience}
                        onChange={(event) => setAudience(event.target.value)}
                      />
                    </label>
                  )}
                </div>
              )}
            </div>
          </details>
          <details
            className="wizard-section"
            open={sessionNewTelemetry || toolUseTelemetry || toolInputTelemetry}
          >
            <summary className="wizard-section-summary">
              <span className="wizard-section-title">
                <strong>Telemetry</strong>
                <small>Select the events to collect.</small>
              </span>
              <ChevronRight size={15} />
            </summary>
            <div className="wizard-section-content">
              <div className="telemetry-options">
                <label className="telemetry-option">
                  <span>
                    <strong>New session</strong>
                    <small>A developer-tool session starts.</small>
                  </span>
                  <code>session.new</code>
                  <input
                    type="checkbox"
                    checked={sessionNewTelemetry}
                    onChange={(event) =>
                      setSessionNewTelemetry(event.target.checked)
                    }
                  />
                </label>
                <label className="telemetry-option">
                  <span>
                    <strong>Tool use</strong>
                    <small>Agent, tool name, and invocation ID.</small>
                  </span>
                  <code>tool.use</code>
                  <input
                    type="checkbox"
                    checked={toolUseTelemetry}
                    onChange={(event) => {
                      setToolUseTelemetry(event.target.checked);
                      if (!event.target.checked) setToolInputTelemetry(false);
                    }}
                  />
                </label>
                <label className="telemetry-option">
                  <span>
                    <strong>Tool input</strong>
                    <small>May contain source code, prompts, or secrets.</small>
                  </span>
                  <code>tool.use.input</code>
                  <input
                    type="checkbox"
                    checked={toolInputTelemetry}
                    onChange={(event) => {
                      setToolInputTelemetry(event.target.checked);
                      if (event.target.checked) setToolUseTelemetry(true);
                    }}
                  />
                </label>
              </div>
            </div>
          </details>
          <details className="wizard-section" open={agents.length > 0}>
            <summary className="wizard-section-summary">
              <span className="wizard-section-title">
                <strong>Agents</strong>
                <small>Add the developer tools you want to manage.</small>
              </span>
              <ChevronRight size={15} />
            </summary>
            <div className="wizard-section-content">
              <div className="agent-list-heading">
                {availableAgents.length > 0 && (
                  <details className="add-agent-menu" ref={addAgentMenu}>
                    <summary className="button secondary">
                      <Plus size={14} /> Add agent
                      <ChevronRight className="menu-chevron" size={13} />
                    </summary>
                    <div className="add-agent-options">
                      {availableAgents.map((agent) => (
                        <button
                          type="button"
                          key={agent.kind}
                          onClick={(event) => {
                            addAgent(agent.kind);
                            event.currentTarget
                              .closest("details")
                              ?.removeAttribute("open");
                          }}
                        >
                          <ToolIcon kind={agent.iconKind} />
                          <span>{agent.label}</span>
                          <Plus size={13} />
                        </button>
                      ))}
                    </div>
                  </details>
                )}
              </div>
              <div className="agent-drafts">
                {agents.map((agent) => {
                  const definition = configurableAgents.find(
                    (candidate) => candidate.kind === agent.kind,
                  );
                  if (!definition) return null;
                  return (
                    <section className="agent-draft" key={agent.kind}>
                      <div className="agent-draft-heading">
                        <span className="tool-cell">
                          <ToolIcon kind={definition.iconKind} />
                          <strong>{definition.label}</strong>
                        </span>
                        <button
                          type="button"
                          className="icon-button"
                          aria-label={`Remove ${definition.label}`}
                          onClick={() =>
                            setAgents((current) =>
                              current.filter(
                                (candidate) => candidate.kind !== agent.kind,
                              ),
                            )
                          }
                        >
                          <Trash2 size={14} />
                        </button>
                      </div>
                      <label className="toggle-row compact">
                        <span>
                          <strong>Use inference gateway</strong>
                          <small>
                            Apply the general gateway settings above.
                          </small>
                        </span>
                        <input
                          type="checkbox"
                          disabled={!gateway}
                          checked={gateway && agent.useGateway}
                          onChange={(event) =>
                            updateAgent(agent.kind, {
                              useGateway: event.target.checked,
                            })
                          }
                        />
                      </label>
                      <label className="field">
                        <span>Additional settings (YAML)</span>
                        <textarea
                          rows={7}
                          spellCheck={false}
                          placeholder={definition.placeholder}
                          value={agent.settings}
                          onChange={(event) =>
                            updateAgent(agent.kind, {
                              settings: event.target.value,
                            })
                          }
                        />
                        <small>
                          Use the agent’s native configuration keys.
                        </small>
                      </label>
                    </section>
                  );
                })}
                {agents.length === 0 && (
                  <p className="agent-empty">No agents added.</p>
                )}
              </div>
            </div>
          </details>
        </section>
        <section className="card output-card">
          <div className="output-heading">
            <div>
              <h3>Generated YAML</h3>
              <p>Copy this into the daemon configuration file.</p>
            </div>
            <button
              type="button"
              className="button secondary"
              onClick={copyYaml}
            >
              {copied ? <Check size={14} /> : <Copy size={14} />}
              {copied ? "Copied" : "Copy"}
            </button>
          </div>
          <pre>
            <code>{yaml}</code>
          </pre>
        </section>
      </div>
    </div>
  );
}

const configurableAgents: Array<{
  kind: AgentKind;
  label: string;
  iconKind: string;
  placeholder: string;
  initialSettings?: string;
}> = [
  {
    kind: "claudeCode",
    label: "Claude Code",
    iconKind: "claude-code",
    placeholder: "permissions:\n  defaultMode: plan",
  },
  {
    kind: "claudeDesktop",
    label: "Claude Desktop",
    iconKind: "claude-desktop",
    placeholder: "isLocalDevMcpEnabled: true",
  },
  {
    kind: "codex",
    label: "Codex",
    iconKind: "codex",
    placeholder: "managedConfig:\n  model_reasoning_effort: high",
  },
  {
    kind: "openCode",
    label: "OpenCode",
    iconKind: "opencode",
    placeholder: "managedConfig:\n  autoupdate: false",
    initialSettings:
      "model: gpt-5.6-terra\nmodels:\n  gpt-5.6-terra:\n    name: GPT 5.6 Terra",
  },
];

function daemonConfigYaml(options: {
  gateway: boolean;
  gatewayUrl: string;
  controllerJwt: boolean;
  audience: string;
  sessionNewTelemetry: boolean;
  toolUseTelemetry: boolean;
  toolInputTelemetry: boolean;
  agents: AgentDraft[];
}) {
  const lines: string[] = [];
  if (options.gateway) {
    lines.push("inferenceGateway:", `  url: ${yamlString(options.gatewayUrl)}`);
    if (options.controllerJwt) {
      lines.push(
        "  authentication:",
        "    type: controllerJwt",
        `    audience: ${yamlString(options.audience)}`,
        "    allowedClientIds: [claude-code, claude-desktop, codex, opencode]",
      );
    }
    lines.push("");
  }
  if (options.sessionNewTelemetry || options.toolUseTelemetry) {
    lines.push("telemetry:", "  events:");
    if (options.sessionNewTelemetry) lines.push("  - session.new");
    if (options.toolUseTelemetry) {
      lines.push(
        `  - ${options.toolInputTelemetry ? "tool.use.input" : "tool.use"}`,
      );
    }
    lines.push("");
  }
  if (options.agents.length === 0) {
    lines.push("programs: {}");
  } else {
    lines.push("programs:");
    for (const agent of options.agents) {
      const settings = agent.settings.trim();
      const disablesGateway = options.gateway && !agent.useGateway;
      if (!settings && !disablesGateway) {
        lines.push(`  ${agent.kind}: {}`);
        continue;
      }
      lines.push(`  ${agent.kind}:`);
      if (disablesGateway) lines.push("    useInferenceGateway: false");
      if (settings) {
        lines.push(...settings.split("\n").map((line) => `    ${line}`));
      }
    }
  }
  return `${lines.join("\n")}\n`;
}

function yamlString(value: string) {
  return JSON.stringify(value);
}

function agentDrafts(programs: DaemonConfigDocument["programs"]): AgentDraft[] {
  if (!programs) return [];
  return configurableAgents.flatMap(({ kind }) => {
    const program = programs[kind];
    if (!program) return [];
    const { useInferenceGateway, ...settings } = program;
    return [
      {
        kind,
        useGateway: useInferenceGateway !== false,
        settings: objectYaml(settings),
      },
    ];
  });
}

function objectYaml(value: Record<string, unknown>) {
  return yamlLines(value, 0).join("\n");
}

function yamlLines(value: unknown, indent: number): string[] {
  const padding = " ".repeat(indent);
  if (Array.isArray(value)) {
    if (value.length === 0) return [`${padding}[]`];
    return value.flatMap((item) => {
      if (isNonEmptyCollection(item)) {
        return [`${padding}-`, ...yamlLines(item, indent + 2)];
      }
      return [`${padding}- ${yamlScalar(item)}`];
    });
  }
  if (value !== null && typeof value === "object") {
    const entries = Object.entries(value);
    if (entries.length === 0) return [`${padding}{}`];
    return entries.flatMap(([key, item]) => {
      const yamlKey = /^[A-Za-z_][A-Za-z0-9_.-]*$/.test(key)
        ? key
        : yamlString(key);
      if (isNonEmptyCollection(item)) {
        return [`${padding}${yamlKey}:`, ...yamlLines(item, indent + 2)];
      }
      return [`${padding}${yamlKey}: ${yamlScalar(item)}`];
    });
  }
  return [`${padding}${yamlScalar(value)}`];
}

function isNonEmptyCollection(value: unknown) {
  return (
    (Array.isArray(value) && value.length > 0) ||
    (value !== null &&
      typeof value === "object" &&
      Object.keys(value).length > 0)
  );
}

function yamlScalar(value: unknown) {
  if (typeof value === "string") return yamlString(value);
  if (value === null) return "null";
  if (typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  if (Array.isArray(value)) return "[]";
  if (typeof value === "object") return "{}";
  return yamlString(String(value));
}

function SettingsPage() {
  const query = useApi<ControllerSettings>("/api/v1/settings");
  if (query.loading) return <PageSkeleton />;
  if (query.error || !query.data) return <ErrorState message={query.error} />;
  const data = query.data;
  return (
    <div className="stack">
      <section className="section-intro">
        <div>
          <h2>Controller settings</h2>
          <p>Runtime capabilities for this controller instance.</p>
        </div>
      </section>
      <section className="settings-list">
        <SettingRow title="Fleet API" description={data.fleet_listen} enabled />
        <SettingRow
          title="Admin UI"
          description={`${data.admin_listen} · loopback only`}
          enabled
        />
        <SettingRow
          title="TLS"
          description="Encrypted fleet transport"
          enabled={data.tls_enabled}
        />
        <SettingRow
          title="OIDC enrollment"
          description="Interactive SSO-based device enrollment"
          enabled={data.oidc_enabled}
        />
        <SettingRow
          title="Gateway JWT issuer"
          description="Short-lived inference gateway credentials"
          enabled={data.gateway_jwt_enabled}
        />
      </section>
      <section className="local-notice">
        <div>
          <strong>Local access only</strong>
          <span>
            The controller rejects admin listener addresses that are not
            loopback interfaces.
          </span>
        </div>
      </section>
    </div>
  );
}

function SettingRow({
  title,
  description,
  enabled,
}: {
  title: string;
  description: string;
  enabled: boolean;
}) {
  return (
    <div className="setting-row">
      <div>
        <strong>{title}</strong>
        <span>{description}</span>
      </div>
      <span className={`badge ${enabled ? "success" : "neutral"}`}>
        {enabled ? "Enabled" : "Disabled"}
      </span>
    </div>
  );
}

function DefinitionList({ entries }: { entries: Array<[string, string]> }) {
  return (
    <dl className="definition-list">
      {entries.map(([term, value]) => (
        <div key={term}>
          <dt>{term}</dt>
          <dd>{value}</dd>
        </div>
      ))}
    </dl>
  );
}

function OnlineBadge({ timestamp }: { timestamp: number | null }) {
  const online = timestamp !== null && Date.now() / 1000 - timestamp <= 90;
  return (
    <span className={`badge ${online ? "success" : "neutral"}`}>
      <span className="mini-dot" />
      {online ? "Online" : "Offline"}
    </span>
  );
}

function ConfigBadge({
  state,
  revision,
}: {
  state: number | null;
  revision: number | null;
}) {
  if (state === 2)
    return (
      <span className="badge danger">
        <CircleAlert size={13} /> Failed{revision ? ` · r${revision}` : ""}
      </span>
    );
  if (state === 1)
    return (
      <span className="badge success">
        <Check size={13} /> Applied{revision ? ` · r${revision}` : ""}
      </span>
    );
  return <span className="badge neutral">Not reported</span>;
}

function EmptyDevices({ searching = false }: { searching?: boolean }) {
  return (
    <div className="empty-state">
      <Laptop size={28} />
      <h3>{searching ? "No matching devices" : "No devices enrolled"}</h3>
      <p>
        {searching
          ? "Try a different hostname, platform, or version."
          : "Devices will appear here after their first enrollment."}
      </p>
    </div>
  );
}

function ErrorState({ message }: { message?: string | null }) {
  return (
    <div className="empty-state error">
      <CircleAlert size={28} />
      <h3>Couldn’t load controller data</h3>
      <p>{message || "The controller returned an unexpected response."}</p>
    </div>
  );
}

function PageSkeleton({ rows = 3 }: { rows?: number }) {
  const skeletonRows = ["first", "second", "third", "fourth", "fifth", "sixth"];
  return (
    <div className="skeleton-card">
      {skeletonRows.slice(0, rows).map((row) => (
        <div className="skeleton-line" key={row} />
      ))}
    </div>
  );
}

function NotFound() {
  return (
    <div className="empty-state">
      <CircleAlert size={28} />
      <h2>Page not found</h2>
      <Link href="/" className="button secondary">
        Return to overview
      </Link>
    </div>
  );
}

function friendlyOs(os: string) {
  const names: Record<string, string> = {
    linux: "Linux",
    macos: "macOS",
    darwin: "macOS",
    windows: "Windows",
  };
  return names[os.toLowerCase()] ?? (os || "Unknown");
}

function friendlyEvent(kind: string) {
  const names: Record<string, string> = {
    "session.new": "New session",
    "tool.use": "Tool use",
    // Preserve display of events stored by older development builds.
    sessionNew: "New session",
    toolUse: "Tool use",
  };
  return names[kind] ?? kind;
}

function telemetryDetail(
  payload: DeviceDetail["recent_events"][number]["payload"],
) {
  if (payload.sessionId) return payload.sessionId;
  const value = payload.toolInput;
  if (value === undefined) return "No input reported";
  const encoded = JSON.stringify(value);
  return encoded.length > 180 ? `${encoded.slice(0, 177)}…` : encoded;
}

function ToolAvatarGroup({ kinds }: { kinds: string[] }) {
  const uniqueKinds = [...new Set(kinds)];
  const visibleKinds = uniqueKinds.slice(0, 4);
  if (!visibleKinds.length) return <span className="no-tools">None</span>;
  return (
    <div className="tool-avatar-group">
      {visibleKinds.map((kind) => (
        <span className="tool-avatar" title={friendlyTool(kind)} key={kind}>
          <ToolIcon kind={kind} />
        </span>
      ))}
      {uniqueKinds.length > visibleKinds.length && (
        <span className="tool-avatar tool-avatar-more">
          +{uniqueKinds.length - visibleKinds.length}
        </span>
      )}
    </div>
  );
}

function formatTime(timestamp: number | null) {
  if (!timestamp) return "Never";
  const delta = Math.max(0, Math.round(Date.now() / 1000 - timestamp));
  if (delta < 60) return `${delta}s ago`;
  if (delta < 3600) return `${Math.floor(delta / 60)}m ago`;
  if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`;
  return `${Math.floor(delta / 86400)}d ago`;
}

function formatTimeMilliseconds(timestamp: number) {
  return formatTime(Math.floor(timestamp / 1000));
}

function formatDate(timestamp: number) {
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(timestamp * 1000));
}
