import {
  ArrowLeft,
  Box,
  Check,
  ChevronLeft,
  ChevronRight,
  CircleAlert,
  Code2,
  Gauge,
  Laptop,
  RefreshCw,
  Search,
  Server,
  Settings,
  SlidersHorizontal,
  Sparkles,
  Trash2,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import agentdesktopIcon from "../app-icon.svg";
import claudeCodeIcon from "./assets/tool-icons/claude-code.svg";
import codexIcon from "./assets/tool-icons/codex.svg";
import copilotIcon from "./assets/tool-icons/copilot.svg";
import openCodeIcon from "./assets/tool-icons/opencode.svg";

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
};

type Overview = {
  total_devices: number;
  online_devices: number;
  offline_devices: number;
  config_failures: number;
  active_revision: number | null;
  recent_devices: Device[];
};

type Configuration = {
  active: boolean;
  revision: number | null;
  sha256: string | null;
  yaml: string | null;
};

type ControllerSettings = {
  fleet_listen: string;
  admin_listen: string;
  enrollment_token_enabled: boolean;
  oidc_enabled: boolean;
  tls_enabled: boolean;
  gateway_jwt_enabled: boolean;
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
    setLoading(true);
    fetch(path, { signal: controller.signal })
      .then(async (response) => {
        if (!response.ok) throw new Error(await response.text());
        return response.json() as Promise<T>;
      })
      .then(setData)
      .catch((reason: Error) => {
        if (reason.name !== "AbortError")
          setError(reason.message || "Request failed");
      })
      .finally(() => setLoading(false));
    return () => controller.abort();
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
          <span>AgentDesktop</span>
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
        <div className="sidebar-foot">
          <div className="controller-state">
            <span className="status-dot" />
            Controller online
          </div>
          <span>Local administration</span>
        </div>
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
            title="Desired configuration"
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
                  : "Start the controller with a desired config to begin a rollout."}
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
            Remove this device, its credential, inventory, and configuration
            status.
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

function ToolInventory({
  discovery,
}: {
  discovery: DeviceDetail["discoveries"][number];
}) {
  const servers = discovery.mcp_servers ?? [];
  const skills = discovery.skills ?? [];
  const [serverPage, setServerPage] = useState(0);
  const [skillPage, setSkillPage] = useState(0);
  const serverPages = Math.max(1, Math.ceil(servers.length / 5));
  const skillPages = Math.max(1, Math.ceil(skills.length / 5));
  const visibleServerPage = Math.min(serverPage, serverPages - 1);
  const visibleSkillPage = Math.min(skillPage, skillPages - 1);
  const hasCapabilities = servers.length > 0 || skills.length > 0;
  return (
    <details className="tool-inventory-item" open={hasCapabilities}>
      <summary>
        <span className="tool-cell">
          <ToolIcon kind={discovery.kind} />
          <strong>{friendlyTool(discovery.kind)}</strong>
        </span>
        <span className="tool-version">{discovery.version || "Unknown"}</span>
        <code className="tool-path">{discovery.path}</code>
        <span className="capability-counts">
          {servers.length} MCP · {skills.length} skills
        </span>
      </summary>
      <div className="capability-grid">
        <CapabilitySection
          icon={<Server size={14} />}
          title="MCP servers"
          page={visibleServerPage}
          pages={serverPages}
          onPageChange={setServerPage}
        >
          {servers.length ? (
            servers
              .slice(visibleServerPage * 5, visibleServerPage * 5 + 5)
              .map((server) => (
                <div
                  className="capability-row"
                  key={`${server.source}-${server.name}`}
                >
                  <div>
                    <strong>{server.name}</strong>
                    <span>
                      {server.transport}
                      {server.enabled ? "" : " · disabled"}
                    </span>
                  </div>
                  <code>
                    {server.url ?? server.command ?? "No endpoint reported"}
                  </code>
                  <small>{server.source}</small>
                </div>
              ))
          ) : (
            <p className="capability-empty">No MCP servers reported.</p>
          )}
        </CapabilitySection>
        <CapabilitySection
          icon={<Sparkles size={14} />}
          title="Skills"
          page={visibleSkillPage}
          pages={skillPages}
          onPageChange={setSkillPage}
        >
          {skills.length ? (
            skills
              .slice(visibleSkillPage * 5, visibleSkillPage * 5 + 5)
              .map((skill) => (
                <div className="capability-row" key={skill.path}>
                  <div>
                    <strong>
                      {frontMatterText(skill.frontMatter.name) ??
                        "Unnamed skill"}
                    </strong>
                  </div>
                  {frontMatterText(skill.frontMatter.description) && (
                    <p>{frontMatterText(skill.frontMatter.description)}</p>
                  )}
                  <small>{skill.path}</small>
                </div>
              ))
          ) : (
            <p className="capability-empty">No skills reported.</p>
          )}
        </CapabilitySection>
      </div>
    </details>
  );
}

function CapabilitySection({
  icon,
  title,
  page,
  pages,
  onPageChange,
  children,
}: React.PropsWithChildren<{
  icon: React.ReactNode;
  title: string;
  page: number;
  pages: number;
  onPageChange: (page: number) => void;
}>) {
  return (
    <section className="capability-section">
      <div className="capability-heading">
        <h4>
          {icon}
          {title}
        </h4>
        {pages > 1 && (
          <nav className="mini-pager" aria-label={`${title} pages`}>
            <button
              type="button"
              aria-label={`Previous ${title} page`}
              disabled={page === 0}
              onClick={() => onPageChange(page - 1)}
            >
              <ChevronLeft size={12} />
            </button>
            <span>
              {page + 1}/{pages}
            </span>
            <button
              type="button"
              aria-label={`Next ${title} page`}
              disabled={page + 1 === pages}
              onClick={() => onPageChange(page + 1)}
            >
              <ChevronRight size={12} />
            </button>
          </nav>
        )}
      </div>
      {children}
    </section>
  );
}

function frontMatterText(value: unknown) {
  return typeof value === "string" && value.trim() ? value : null;
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
  const query = useApi<Configuration>("/api/v1/configuration");
  if (query.loading) return <PageSkeleton />;
  if (query.error || !query.data) return <ErrorState message={query.error} />;
  const config = query.data;
  return (
    <div className="stack">
      <section className="section-intro">
        <div>
          <h2>Desired configuration</h2>
          <p>
            The controller-wide configuration sent to agents when they connect.
          </p>
        </div>
        <span className={`badge ${config.active ? "success" : "neutral"}`}>
          {config.active ? (
            <>
              <Check size={13} /> Active
            </>
          ) : (
            "Not configured"
          )}
        </span>
      </section>
      <div className="detail-grid">
        <section className="card detail-card">
          <CardHeader
            title="Active revision"
            description="Current rollout source"
          />
          <div className="revision-value">
            {config.revision ? `r${config.revision}` : "—"}
          </div>
          <DefinitionList
            entries={[
              ["Revision", config.revision?.toString() ?? "None"],
              [
                "SHA-256",
                config.sha256
                  ? `${config.sha256.slice(0, 12)}…${config.sha256.slice(-8)}`
                  : "—",
              ],
            ]}
          />
        </section>
        <section className="card notice-card">
          <div>
            <h3>Managed at startup</h3>
            <p>
              This first version is intentionally read-only. Update the desired
              configuration file and restart the controller to publish a
              revision.
            </p>
          </div>
        </section>
      </div>
      <section className="card code-card">
        <CardHeader
          title="Configuration source"
          description="Validated YAML embedded in the active rollout"
        />
        {config.yaml ? (
          <pre>
            <code>{config.yaml}</code>
          </pre>
        ) : (
          <div className="empty-code">
            <h3>No desired configuration</h3>
            <p>
              Start the controller with <code>--desired-config</code> to make a
              configuration active.
            </p>
          </div>
        )}
      </section>
    </div>
  );
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
          title="Enrollment token"
          description="Shared bootstrap token enrollment"
          enabled={data.enrollment_token_enabled}
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

function CardHeader({
  title,
  description,
  action,
}: {
  title: string;
  description: string;
  action?: React.ReactNode;
}) {
  return (
    <div className="card-header">
      <div>
        <h3>{title}</h3>
        <p>{description}</p>
      </div>
      {action}
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

function friendlyTool(kind: string) {
  const names: Record<string, string> = {
    codex: "Codex",
    claude_code: "Claude Code",
    "claude-code": "Claude Code",
    opencode: "OpenCode",
    vscode: "VS Code",
  };
  return names[kind.toLowerCase()] ?? kind;
}

const toolIcons: Record<string, string> = {
  codex: codexIcon,
  "claude-code": claudeCodeIcon,
  claude_code: claudeCodeIcon,
  opencode: openCodeIcon,
  vscode: copilotIcon,
};

function ToolIcon({ kind }: { kind: string }) {
  const icon = toolIcons[kind.toLowerCase()];
  return icon ? (
    <img className="tool-icon" src={icon} alt="" aria-hidden="true" />
  ) : (
    <Code2 className="tool-icon-fallback" size={16} aria-hidden="true" />
  );
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

function formatDate(timestamp: number) {
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(timestamp * 1000));
}
