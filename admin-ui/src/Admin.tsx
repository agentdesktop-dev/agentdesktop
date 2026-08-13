import {
  AlertCircle,
  Bot,
  Boxes,
  Check,
  ChevronLeft,
  ChevronRight,
  CircleUserRound,
  Clock3,
  Code2,
  Inbox,
  KeyRound,
  Laptop2,
  LayoutDashboard,
  LoaderCircle,
  LogOut,
  Ellipsis,
  Network,
  RefreshCw,
  Search,
  Server,
  Settings2,
  Shield,
  ShieldCheck,
  ShieldOff,
  Sparkles,
  SquareTerminal,
  Waypoints,
  Wrench,
  X
} from "lucide-react";
import { startTransition, useDeferredValue, useEffect, useRef, useState, useTransition } from "react";
import { createPortal } from "react-dom";

import {
  adminSessionExpiredEvent,
  approveEnrollment,
  getAgentPolicy,
  getBootstrap,
  getFleetSummary,
  getDeviceDiscoveryReport,
  getInventory,
  getInventoryDevices,
  listDevices,
  listEnrollments,
  putAgentPolicy,
  requestDiscoveryRescan,
  rejectEnrollment,
  revokeDevice,
  signIn,
  signOut
} from "./server-backend";
import type {
  AdministrativeDevice,
  AgentID,
  AgentPolicyRule,
  Bootstrap,
  DeviceDiscoveryReport,
  EnrollmentRecord,
  EnrollmentStatus,
  FleetSummary,
  InventoryAsset,
  InventoryDevice,
  InventoryDevicePage,
  InventoryKind,
  InventoryPage
} from "./types";

type View = "overview" | "inventory" | "policies" | "enrollments" | "devices" | "connection";
type CapabilityTone = "active" | "partial" | "unavailable";
type RecordsByStatus = Record<EnrollmentStatus, EnrollmentRecord[]>;
type LimitsByStatus = Record<EnrollmentStatus, boolean>;
type Confirmation =
  | { action: "approve" | "reject"; record: EnrollmentRecord }
  | { action: "revoke"; device: AdministrativeDevice }
  | null;
type Notice = { tone: "success" | "error"; message: string } | null;
type DiscoveryState =
  | { status: "loading" }
  | { status: "ready"; report: DeviceDiscoveryReport | null }
  | { status: "error"; message: string };

const statuses: EnrollmentStatus[] = ["pending", "issuing", "approved", "rejected"];
const emptyRecords: RecordsByStatus = { pending: [], issuing: [], approved: [], rejected: [] };
const emptyLimits: LimitsByStatus = { pending: false, issuing: false, approved: false, rejected: false };
const emptySummary: FleetSummary = {
  pendingEnrollments: 0,
  issuingEnrollments: 0,
  approvedEnrollments: 0,
  rejectedEnrollments: 0,
  activeDevices: 0,
  revokedDevices: 0,
  certificatesExpiring24H: 0,
  renewals24H: 0,
  generatedAt: ""
};
const administrationRefreshIntervalMs = 5000;
const dateTimeFormat = new Intl.DateTimeFormat(undefined, {
  dateStyle: "medium",
  timeStyle: "short"
});
const numberFormat = new Intl.NumberFormat();
const relativeFormat = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function titleCase(value: string): string {
  return value.charAt(0).toUpperCase() + value.slice(1).replaceAll("-", " ").replaceAll("_", " ");
}

function formatDate(value: string): string {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? "Unavailable" : dateTimeFormat.format(date);
}

function formatRelative(value: string): string {
  const timestamp = Date.parse(value);
  if (Number.isNaN(timestamp)) return "Unavailable";
  const seconds = Math.round((timestamp - Date.now()) / 1000);
  if (Math.abs(seconds) < 60) return relativeFormat.format(seconds, "second");
  const minutes = Math.round(seconds / 60);
  if (Math.abs(minutes) < 60) return relativeFormat.format(minutes, "minute");
  const hours = Math.round(minutes / 60);
  if (Math.abs(hours) < 48) return relativeFormat.format(hours, "hour");
  return relativeFormat.format(Math.round(hours / 24), "day");
}

function compact(value: string | null, start = 8, end = 6): string {
  if (!value) return "Not assigned";
  return value.length > start + end + 1 ? `${value.slice(0, start)}…${value.slice(-end)}` : value;
}

function matches(record: EnrollmentRecord, query: string): boolean {
  if (!query) return true;
  const normalized = query.toLowerCase();
  return [
    record.username ?? "",
    record.subject,
    record.deviceName ?? "",
    record.enrollmentId,
    record.deviceId ?? "",
    record.publicKeyFingerprint
  ].some((value) => value.toLowerCase().includes(normalized));
}

function deviceMatches(device: AdministrativeDevice, query: string): boolean {
  if (!query) return true;
  const normalized = query.toLowerCase();
  return [
    device.deviceName ?? "",
    device.username ?? "",
    device.subject,
    device.deviceId,
    device.currentCertificateSerialNumber ?? ""
  ].some((value) => value.toLowerCase().includes(normalized));
}

function countLabel(count: number, limited: boolean): string {
  return `${count}${limited ? "+" : ""}`;
}

async function loadAdministrationData() {
  const [lists, summary, deviceList] = await Promise.all([
    Promise.all(statuses.map((status) => listEnrollments(status))),
    getFleetSummary(),
    listDevices()
  ]);
  const records = { ...emptyRecords };
  const limits = { ...emptyLimits };
  for (const list of lists) {
    records[list.status] = list.enrollments;
    limits[list.status] = list.limited;
  }
  return { records, limits, summary, deviceList };
}

function StatusBadge({ status }: { status: EnrollmentStatus | "active" | "revoked" }) {
  return <span className={`status-badge status-${status}`}>{titleCase(status)}</span>;
}

function CapabilityBadge({ tone, children }: { tone: CapabilityTone; children: React.ReactNode }) {
  return <span className={`capability-badge capability-${tone}`}>{children}</span>;
}

function Spinner() {
  return <LoaderCircle className="spin" size={16} aria-hidden="true" />;
}

function EmptyState({ title, detail }: { title: string; detail: string }) {
  return (
    <div className="empty-state">
      <span className="empty-icon" aria-hidden="true"><Check size={18} /></span>
      <strong>{title}</strong>
      <span>{detail}</span>
    </div>
  );
}

function SearchField({ value, onChange, label }: { value: string; onChange: (value: string) => void; label: string }) {
  return (
    <label className="search-field">
      <Search size={15} aria-hidden="true" />
      <span className="sr-only">{label}</span>
      <input
        type="search"
        value={value}
        placeholder={label}
        onChange={(event) => onChange(event.target.value)}
      />
    </label>
  );
}

function RowActions({
  record,
  status,
  confirmation,
  busy,
  onConfirm,
  onCancel,
  onSelect
}: {
  record: EnrollmentRecord;
  status: EnrollmentStatus;
  confirmation: Confirmation;
  busy: boolean;
  onConfirm: () => void;
  onCancel: () => void;
  onSelect: (action: "approve" | "reject", record: EnrollmentRecord) => void;
}) {
  if (confirmation && "record" in confirmation && confirmation.record.enrollmentId === record.enrollmentId) {
    return (
      <div className="confirm-actions">
        <button
          type="button"
          className={confirmation.action === "approve" ? "button button-primary" : "button button-danger"}
          disabled={busy}
          onClick={onConfirm}
        >
          {busy ? <Spinner /> : null}
          Confirm {confirmation.action}
        </button>
        <button type="button" className="icon-button" aria-label="Cancel" title="Cancel" disabled={busy} onClick={onCancel}>
          <X size={15} />
        </button>
      </div>
    );
  }
  if (status === "pending") {
    return (
      <div className="row-actions">
        <button type="button" className="button button-quiet danger-text" onClick={() => onSelect("reject", record)}>
          Reject
        </button>
        <button type="button" className="button button-primary" onClick={() => onSelect("approve", record)}>
          Approve
        </button>
      </div>
    );
  }
  return null;
}

function EnrollmentTable({
  records,
  status,
  query,
  confirmation,
  busy,
  onConfirm,
  onCancel,
  onSelect
}: {
  records: EnrollmentRecord[];
  status: EnrollmentStatus;
  query: string;
  confirmation: Confirmation;
  busy: boolean;
  onConfirm: () => void;
  onCancel: () => void;
  onSelect: (action: "approve" | "reject", record: EnrollmentRecord) => void;
}) {
  const filtered = records.filter((record) => matches(record, query));
  if (filtered.length === 0) {
    return <EmptyState title={query ? "No matching records" : `No ${status} enrollments`} detail={query ? "Try another subject, device, or key." : "This queue is clear."} />;
  }
  return (
    <div className="table-scroll">
      <table className="enrollment-table">
        <thead>
          <tr>
            <th>User</th>
            <th>Machine</th>
            <th>Requested</th>
            <th>Key fingerprint</th>
            <th>{status === "approved" || status === "issuing" ? "Device" : "Enrollment"}</th>
            <th>Status</th>
            <th aria-label="Actions" />
          </tr>
        </thead>
        <tbody>
          {filtered.map((record) => {
            return (
              <tr key={record.enrollmentId}>
                <td>
                  <strong>{record.username ?? record.subject}</strong>
                  {record.username ? <small className="row-detail mono" title={record.subject}>{compact(record.subject)}</small> : null}
                </td>
                <td><strong>{record.deviceName ?? "Unnamed machine"}</strong></td>
                <td title={formatDate(record.createdAt)}>{formatRelative(record.createdAt)}</td>
                <td><span className="mono" title={record.publicKeyFingerprint}>{compact(record.publicKeyFingerprint, 9, 7)}</span></td>
                <td><span className="mono" title={record.deviceId ?? record.enrollmentId}>{compact(record.deviceId ?? record.enrollmentId)}</span></td>
                <td><StatusBadge status={record.status} /></td>
                <td className="actions-cell">
                  <RowActions
                    record={record}
                    status={status}
                    confirmation={confirmation}
                    busy={busy}
                    onConfirm={onConfirm}
                    onCancel={onCancel}
                    onSelect={onSelect}
                  />
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

function Overview({
  records,
  summary,
  devices,
  refreshedAt,
  onOpenStatus,
  onOpenDevices
}: {
  records: RecordsByStatus;
  summary: FleetSummary;
  devices: AdministrativeDevice[];
  refreshedAt: Date | null;
  onOpenStatus: (status: EnrollmentStatus) => void;
  onOpenDevices: () => void;
}) {
  const pending = records.pending.slice(0, 5);
  const latestDevices = [...devices]
    .sort((left, right) => Date.parse(right.createdAt) - Date.parse(left.createdAt))
    .slice(0, 5);
  return (
    <>
      <div className="page-heading">
        <div>
          <p className="eyebrow">Agent operations</p>
          <h1>Organization posture</h1>
          <p className="heading-meta">Updated {refreshedAt ? formatRelative(refreshedAt.toISOString()) : "when connected"}</p>
        </div>
      </div>

      <section className="posture-band" aria-label="Management coverage">
        <div><span>Inference policy</span><strong>Gateway-owned</strong></div>
        <div><span>Enrolled endpoints</span><strong>{summary.activeDevices}</strong></div>
        <div><span>Agents</span><strong>Per device</strong></div>
        <div><span>Assignments</span><strong>Draft planning</strong></div>
      </section>

      <section className="metrics-band" aria-label="Enrollment summary">
        <button type="button" onClick={() => onOpenStatus("pending")}>
          <span>Pending review</span>
          <strong>{summary.pendingEnrollments}</strong>
        </button>
        <button type="button" onClick={onOpenDevices}>
          <span>Active devices</span>
          <strong>{summary.activeDevices}</strong>
        </button>
        <button type="button" onClick={onOpenDevices}>
          <span>Revoked devices</span>
          <strong>{summary.revokedDevices}</strong>
        </button>
        <div>
          <span>Expiring in 24h</span>
          <strong>{summary.certificatesExpiring24H}</strong>
        </div>
        <div className="age-metric">
          <span>Renewals in 24h</span>
          <strong>{summary.renewals24H}</strong>
        </div>
      </section>

      <section className="overview-section" aria-labelledby="review-heading">
        <div className="section-heading">
          <div>
            <h2 id="review-heading">Needs review</h2>
            <p>{summary.pendingEnrollments === 0 ? "No pending requests" : `${summary.pendingEnrollments} waiting for a decision`}</p>
          </div>
          <button type="button" className="text-button" onClick={() => onOpenStatus("pending")}>Open queue <ChevronRight size={14} /></button>
        </div>
        {pending.length ? (
          <div className="compact-list">
            {pending.map((record) => (
              <button type="button" key={record.enrollmentId} onClick={() => onOpenStatus("pending")}>
                <span className="avatar" aria-hidden="true"><CircleUserRound size={17} /></span>
                <span><strong>{record.username ?? record.subject}</strong><small>{record.deviceName ?? "Unnamed machine"} · {compact(record.publicKeyFingerprint, 10, 8)}</small></span>
                <time>{formatRelative(record.createdAt)}</time>
                <ChevronRight size={15} aria-hidden="true" />
              </button>
            ))}
          </div>
        ) : <EmptyState title="Review queue clear" detail="New device requests will appear here." />}
      </section>

      <section className="overview-section" aria-labelledby="recent-heading">
        <div className="section-heading">
          <div>
            <h2 id="recent-heading">Recent devices</h2>
            <p>{summary.activeDevices} active · {summary.revokedDevices} revoked</p>
          </div>
          <button type="button" className="text-button" onClick={onOpenDevices}>View devices <ChevronRight size={14} /></button>
        </div>
        {latestDevices.length ? (
          <div className="compact-list">
            {latestDevices.map((device) => (
              <button type="button" key={device.deviceId} onClick={onOpenDevices}>
                <span className="avatar avatar-device" aria-hidden="true"><Laptop2 size={17} /></span>
                <span><strong>{device.deviceName ?? "Unnamed device"}</strong><small>{device.username ?? device.subject} · {compact(device.deviceId)} · {device.status}</small></span>
                <time>{formatRelative(device.createdAt)}</time>
                <ChevronRight size={15} aria-hidden="true" />
              </button>
            ))}
          </div>
        ) : <EmptyState title="No enrolled devices" detail="Approved devices will appear here." />}
      </section>
    </>
  );
}

const inventoryKinds: Array<{ kind: InventoryKind; label: string }> = [
  { kind: "agent", label: "Agents" },
  { kind: "mcp", label: "MCP servers" },
  { kind: "skill", label: "Skills" },
  { kind: "plugin", label: "Plugins" }
];

function InventoryView() {
  const [kind, setKind] = useState<InventoryKind>("agent");
  const [assetQuery, setAssetQuery] = useState("");
  const deferredAssetQuery = useDeferredValue(assetQuery);
  const [assetOffset, setAssetOffset] = useState(0);
  const [inventory, setInventory] = useState<InventoryPage | null>(null);
  const [inventoryError, setInventoryError] = useState<string | null>(null);
  const [assetsLoading, setAssetsLoading] = useState(true);
  const [selectedAsset, setSelectedAsset] = useState<InventoryAsset | null>(null);
  const [deviceQuery, setDeviceQuery] = useState("");
  const deferredDeviceQuery = useDeferredValue(deviceQuery);
  const [deviceOffset, setDeviceOffset] = useState(0);
  const [devicePage, setDevicePage] = useState<InventoryDevicePage | null>(null);
  const [deviceError, setDeviceError] = useState<string | null>(null);
  const [devicesLoading, setDevicesLoading] = useState(true);
  const [expandedDevice, setExpandedDevice] = useState<string | null>(null);
  const [reports, setReports] = useState<Record<string, DiscoveryState>>({});
  const [rescanNotice, setRescanNotice] = useState<Notice>(null);
  const [rescanning, startRescan] = useTransition();

  useEffect(() => {
    let active = true;
    setAssetsLoading(true);
    setInventoryError(null);
    getInventory(kind, deferredAssetQuery, assetOffset)
      .then((page) => {
        if (active) setInventory(page);
      })
      .catch((error: unknown) => {
        if (active) setInventoryError(errorMessage(error));
      })
      .finally(() => {
        if (active) setAssetsLoading(false);
      });
    return () => { active = false; };
  }, [kind, deferredAssetQuery, assetOffset]);

  useEffect(() => {
    let active = true;
    setDevicesLoading(true);
    setDeviceError(null);
    getInventoryDevices(selectedAsset, deferredDeviceQuery, deviceOffset)
      .then((page) => {
        if (active) setDevicePage(page);
      })
      .catch((error: unknown) => {
        if (active) setDeviceError(errorMessage(error));
      })
      .finally(() => {
        if (active) setDevicesLoading(false);
      });
    return () => { active = false; };
  }, [selectedAsset, deferredDeviceQuery, deviceOffset]);

  async function loadReport(deviceId: string) {
    setReports((current) => ({ ...current, [deviceId]: { status: "loading" } }));
    try {
      const report = await getDeviceDiscoveryReport(deviceId);
      setReports((current) => ({ ...current, [deviceId]: { status: "ready", report } }));
    } catch (error) {
      setReports((current) => ({ ...current, [deviceId]: { status: "error", message: errorMessage(error) } }));
    }
  }

  function toggleDevice(deviceId: string) {
    if (expandedDevice === deviceId) {
      setExpandedDevice(null);
      return;
    }
    setExpandedDevice(deviceId);
    if (!reports[deviceId]) void loadReport(deviceId);
  }

  function selectKind(nextKind: InventoryKind) {
    setKind(nextKind);
    setAssetQuery("");
    setAssetOffset(0);
    setSelectedAsset(null);
    setDeviceOffset(0);
  }

  function selectAsset(asset: InventoryAsset) {
    setSelectedAsset(asset);
    setDeviceQuery("");
    setDeviceOffset(0);
    setExpandedDevice(null);
  }

  function forceRescan(deviceIds: string[] | null) {
    setRescanNotice(null);
    startRescan(async () => {
      try {
        const result = await requestDiscoveryRescan(deviceIds);
        setRescanNotice({
          tone: "success",
          message: `Rescan requested for ${numberFormat.format(result.requested)} device${result.requested === 1 ? "" : "s"}. Online desktops poll within 30 seconds.`
        });
      } catch (error: unknown) {
        setRescanNotice({ tone: "error", message: errorMessage(error) });
      }
    });
  }

  const counts = inventory?.counts;
  const reportingPercent = counts?.activeDevices
    ? Math.round((counts.reportingDevices / counts.activeDevices) * 100)
    : 0;

  return (
    <>
      <div className="page-heading">
        <div><p className="eyebrow">Fleet inventory</p><h1>Agents and resources</h1><p className="heading-meta">Installed software and configured resources across managed endpoints</p></div>
        <button className="button button-secondary" type="button" disabled={rescanning} onClick={() => forceRescan(null)}>{rescanning ? <Spinner /> : <RefreshCw size={14} />} Rescan all</button>
      </div>

      {rescanNotice ? <div className={`notice inventory-notice notice-${rescanNotice.tone}`} role="status"><span>{rescanNotice.message}</span></div> : null}

      <section className="inventory-strip" aria-label="Inventory reporting summary">
        <div><span>Reporting</span><strong>{counts ? `${numberFormat.format(counts.reportingDevices)} / ${numberFormat.format(counts.activeDevices)}` : "—"}</strong><small>{counts ? `${reportingPercent}% of active endpoints` : "Loading endpoints"}</small></div>
        <div><span>Agents</span><strong>{counts ? numberFormat.format(counts.agents) : "—"}</strong><small>Versions observed</small></div>
        <div><span>MCP servers</span><strong>{counts ? numberFormat.format(counts.mcpServers) : "—"}</strong><small>Configurations observed</small></div>
        <div><span>Skills + plugins</span><strong>{counts ? numberFormat.format(counts.skills + counts.plugins) : "—"}</strong><small>Resources observed</small></div>
      </section>

      <section className="inventory-section" aria-labelledby="used-assets-heading">
        <div className="section-heading">
          <div><h2 id="used-assets-heading">Most used</h2><p>Ranked by endpoints reporting each item</p></div>
          {assetsLoading ? <Spinner /> : null}
        </div>
        <div className="inventory-toolbar">
          <div className="segmented inventory-tabs" role="tablist" aria-label="Inventory type">
            {inventoryKinds.map((item) => (
              <button className={kind === item.kind ? "active" : ""} type="button" role="tab" aria-selected={kind === item.kind} key={item.kind} onClick={() => selectKind(item.kind)}>
                {item.label}<span>{inventoryKindCount(counts, item.kind)}</span>
              </button>
            ))}
          </div>
          <label className="search-field inventory-search">
            <Search size={15} aria-hidden="true" />
            <span className="sr-only">Search {inventoryKindLabel(kind).toLowerCase()}</span>
            <input value={assetQuery} onChange={(event) => { setAssetQuery(event.target.value); setAssetOffset(0); }} placeholder={`Search ${inventoryKindLabel(kind).toLowerCase()}`} />
          </label>
        </div>
        {inventoryError ? <InventoryError message={inventoryError} /> : inventory?.assets.length ? (
          <div className="inventory-assets" role="table" aria-label={`${inventoryKindLabel(kind)} by endpoint count`}>
            <div className="inventory-asset-head" role="row"><span role="columnheader">Item</span><span role="columnheader">Endpoints</span><span role="columnheader">Fleet</span><span role="columnheader">Observed</span></div>
            {inventory.assets.map((asset) => {
              const selected = selectedAsset && inventoryAssetID(selectedAsset) === inventoryAssetID(asset);
              const fleetPercent = counts?.activeDevices ? Math.round((asset.deviceCount / counts.activeDevices) * 100) : 0;
              return (
                <button className={`inventory-asset-row ${selected ? "selected" : ""}`} type="button" role="row" key={inventoryAssetID(asset)} onClick={() => selectAsset(asset)}>
                  <span className="inventory-asset-name" role="cell"><InventoryAssetIcon asset={asset} /><span><strong>{inventoryAssetName(asset)}</strong><small>{inventoryAssetDetail(asset)}</small></span></span>
                  <strong role="cell">{numberFormat.format(asset.deviceCount)}</strong>
                  <span role="cell">{fleetPercent}%</span>
                  <span role="cell">{asset.kind === "agent" ? `${numberFormat.format(asset.runningCount)} running` : asset.detail ?? "Configured"}</span>
                </button>
              );
            })}
          </div>
        ) : <EmptyState title="No matching inventory" detail="Reports matching this filter will appear here." />}
        <InventoryPager page={inventory} onOffset={setAssetOffset} />
      </section>

      <section className="inventory-section" aria-labelledby="inventory-endpoints-heading">
        <div className="section-heading inventory-device-heading">
          <div>
            <h2 id="inventory-endpoints-heading">{selectedAsset ? `${inventoryAssetName(selectedAsset)} endpoints` : "Enrolled endpoints"}</h2>
            <p>{devicePage ? `${numberFormat.format(devicePage.total)} matching devices` : "Loading devices"}</p>
          </div>
          {selectedAsset ? <button className="text-button" type="button" onClick={() => { setSelectedAsset(null); setDeviceOffset(0); }}>Clear filter <X size={14} /></button> : null}
        </div>
        <div className="inventory-device-tools">
          <label className="search-field inventory-device-search">
            <Search size={15} aria-hidden="true" />
            <span className="sr-only">Search enrolled endpoints</span>
            <input value={deviceQuery} onChange={(event) => { setDeviceQuery(event.target.value); setDeviceOffset(0); }} placeholder="Search device, owner, or ID" />
          </label>
          {devicesLoading ? <Spinner /> : null}
        </div>
        {deviceError ? <InventoryError message={deviceError} /> : devicePage?.devices.length ? (
          <div className="inventory-devices">
            {devicePage.devices.map((device) => {
              const state = reports[device.deviceId];
              const expanded = expandedDevice === device.deviceId;
              return (
                <article className="inventory-device-shell" key={device.deviceId}>
                  <button className="inventory-device" type="button" aria-expanded={expanded} onClick={() => toggleDevice(device.deviceId)}>
                    <span className="avatar avatar-device" aria-hidden="true"><Laptop2 size={17} /></span>
                    <span><strong>{device.deviceName ?? "Unnamed device"}</strong><small>{device.username ?? device.subject} · {compact(device.deviceId)}</small></span>
                    <span className="inventory-device-state">
                      {state?.status === "loading" ? <Spinner /> : <CapabilityBadge tone={device.reportReceivedAt ? "active" : "partial"}>{device.reportReceivedAt ? `Reported ${formatRelative(device.reportReceivedAt)}` : "No report"}</CapabilityBadge>}
                      <ChevronRight className={expanded ? "chevron-open" : ""} size={16} aria-hidden="true" />
                    </span>
                  </button>
                  {expanded ? <DiscoveryReportPanel state={state} onRefresh={() => void loadReport(device.deviceId)} onRescan={() => forceRescan([device.deviceId])} rescanning={rescanning} /> : null}
                </article>
              );
            })}
          </div>
        ) : <EmptyState title="No matching endpoints" detail="Try a different search or inventory filter." />}
        <InventoryPager page={devicePage} onOffset={setDeviceOffset} />
      </section>
    </>
  );
}

function InventoryPager({ page, onOffset }: { page: { total: number; limit: number; offset: number } | null; onOffset: (offset: number) => void }) {
  if (!page || page.total <= page.limit) return null;
  const start = page.offset + 1;
  const end = Math.min(page.offset + page.limit, page.total);
  return (
    <div className="inventory-pager">
      <span>{numberFormat.format(start)}–{numberFormat.format(end)} of {numberFormat.format(page.total)}</span>
      <div>
        <button className="icon-button" type="button" disabled={page.offset === 0} onClick={() => onOffset(Math.max(0, page.offset - page.limit))} aria-label="Previous page" title="Previous page"><ChevronLeft size={15} /></button>
        <button className="icon-button" type="button" disabled={end >= page.total} onClick={() => onOffset(page.offset + page.limit)} aria-label="Next page" title="Next page"><ChevronRight size={15} /></button>
      </div>
    </div>
  );
}

function InventoryError({ message }: { message: string }) {
  return <div className="inventory-load-error"><AlertCircle size={16} aria-hidden="true" /><span>{message}</span></div>;
}

function inventoryKindCount(counts: InventoryPage["counts"] | undefined, kind: InventoryKind): string {
  if (!counts) return "—";
  if (kind === "agent") return numberFormat.format(counts.agents);
  if (kind === "mcp") return numberFormat.format(counts.mcpServers);
  if (kind === "skill") return numberFormat.format(counts.skills);
  return numberFormat.format(counts.plugins);
}

function inventoryKindLabel(kind: InventoryKind): string {
  return inventoryKinds.find((item) => item.kind === kind)?.label ?? "Inventory";
}

function inventoryAssetID(asset: InventoryAsset): string {
  return `${asset.kind}:${asset.key}:${asset.version ?? ""}:${asset.detail ?? ""}`;
}

function inventoryAssetName(asset: InventoryAsset): string {
  return asset.kind === "agent"
    ? discoveryAgentName(asset.key as DeviceDiscoveryReport["agents"][number]["id"])
    : asset.key;
}

function inventoryAssetDetail(asset: InventoryAsset): string {
  if (asset.kind === "agent") return asset.version ? `Version ${asset.version}` : "Version unavailable";
  if (asset.kind === "mcp") return `${asset.detail ?? "unknown"} transport`;
  if (asset.kind === "plugin") return asset.detail ?? "State unknown";
  return "Configured skill";
}

function InventoryAssetIcon({ asset }: { asset: InventoryAsset }) {
  if (asset.kind === "agent") return <span className={`agent-glyph agent-glyph-${asset.key}`} aria-hidden="true"><DiscoveryAgentIcon agentId={asset.key as DeviceDiscoveryReport["agents"][number]["id"]} /></span>;
  const Icon = asset.kind === "mcp" ? Waypoints : asset.kind === "skill" ? Wrench : Boxes;
  return <span className="inventory-resource-glyph" aria-hidden="true"><Icon size={15} /></span>;
}

function DiscoveryReportPanel({ state, onRefresh, onRescan, rescanning }: { state: DiscoveryState | undefined; onRefresh: () => void; onRescan: () => void; rescanning: boolean }) {
  if (!state || state.status === "loading") {
    return <div className="inventory-report-state"><Spinner /><span>Loading latest report</span></div>;
  }
  if (state.status === "error") {
    return <div className="inventory-report-state inventory-report-error"><AlertCircle size={17} aria-hidden="true" /><span>{state.message}</span><button className="icon-button" type="button" onClick={onRefresh} aria-label="Retry discovery report" title="Retry"><RefreshCw size={15} /></button></div>;
  }
  if (!state.report) {
    return <div className="inventory-report-state"><Clock3 size={17} aria-hidden="true" /><span>No discovery report has been received from this device.</span><button className="icon-button" type="button" onClick={onRefresh} aria-label="Refresh discovery report" title="Refresh"><RefreshCw size={15} /></button></div>;
  }
  const report = state.report;
  const agents = report.agents.filter((agent) => agent.installed || agent.evidence.includes("configuration"));
  const platform = report.platform === "windows" ? "Windows" : "macOS";
  return (
    <div className="inventory-report">
      <div className="inventory-report-meta">
        <span><strong>{platform} collector {report.collectorVersion}</strong><small>Received {formatRelative(report.receivedAt)} · Project scopes not scanned</small></span>
        <span className="inventory-report-actions"><CapabilityBadge tone={report.partial ? "partial" : "active"}>{report.partial ? "Partial" : "Complete"}</CapabilityBadge><button className="button button-secondary" type="button" disabled={rescanning} onClick={onRescan}>{rescanning ? <Spinner /> : <RefreshCw size={13} />} Rescan device</button></span>
      </div>
      <div className="agent-inventory-list">
        {agents.length ? agents.map((agent) => (
          <section className="agent-inventory-row" key={agent.id}>
            <div className="agent-inventory-heading">
              <span className="agent-identity">
                <span className={`agent-glyph agent-glyph-${agent.id}`} aria-hidden="true"><DiscoveryAgentIcon agentId={agent.id} /></span>
                <span><strong>{discoveryAgentName(agent.id)}</strong><small>{agent.version ? `Version ${agent.version}` : "Version unavailable"}</small></span>
              </span>
              <span>{agent.installed ? "Installed" : agent.evidence.includes("configuration") ? "Configured" : "Not detected"} · {titleCase(agent.running)}</span>
            </div>
            <DiscoveryResources title="MCP servers" empty="None configured" values={agent.mcpServers.map((server) => `${server.name} · ${server.transport}`)} />
            <DiscoveryResources title="Skills" empty="None found" values={agent.skills.map((skill) => skill.name)} />
            <DiscoveryResources title="Plugins" empty="None found" values={agent.plugins.map((plugin) => `${plugin.name} · ${plugin.state}`)} />
          </section>
        )) : <div className="agent-inventory-empty">No agents detected in the supported locations.</div>}
      </div>
      {report.issues.length ? <p className="inventory-report-note">{report.issues.length} source{report.issues.length === 1 ? "" : "s"} could not be fully inspected.</p> : null}
    </div>
  );
}

function DiscoveryResources({ title, empty, values }: { title: string; empty: string; values: string[] }) {
  return <div className="discovery-resource"><strong>{title}</strong><span>{values.length ? values.join(", ") : empty}</span></div>;
}

function discoveryAgentName(agentId: DeviceDiscoveryReport["agents"][number]["id"]): string {
  return ({ "claude-code": "Claude Code", "claude-desktop": "Claude Desktop", "codex-cli": "Codex CLI", openclaw: "OpenClaw", "vscode-copilot": "VS Code Copilot" })[agentId];
}

function DiscoveryAgentIcon({ agentId }: { agentId: DeviceDiscoveryReport["agents"][number]["id"] }) {
  if (agentId === "claude-code" || agentId === "claude-desktop") return <Sparkles size={16} />;
  if (agentId === "codex-cli") return <SquareTerminal size={16} />;
  if (agentId === "vscode-copilot") return <Code2 size={16} />;
  return <Network size={16} />;
}

function PoliciesView() {
  const agentIDs: AgentID[] = ["claude-code", "claude-desktop", "codex-cli", "openclaw", "vscode-copilot"];
  const [rules, setRules] = useState<AgentPolicyRule[]>(agentIDs.map((agentId) => ({ agentId, action: "allow" })));
  const [configured, setConfigured] = useState(false);
  const [updatedAt, setUpdatedAt] = useState("");
  const [loading, setLoading] = useState(true);
  const [notice, setNotice] = useState<Notice>(null);
  const [isSaving, startSaving] = useTransition();

  useEffect(() => {
    let active = true;
    getAgentPolicy()
      .then((policy) => {
        if (active) {
          setRules(policy.rules);
          setConfigured(policy.configured);
          setUpdatedAt(policy.updatedAt);
        }
      })
      .catch((error: unknown) => {
        if (active) setNotice({ tone: "error", message: errorMessage(error) });
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => { active = false; };
  }, []);

  function setAction(agentId: AgentID, action: AgentPolicyRule["action"]) {
    setRules((current) => current.map((rule) => rule.agentId === agentId ? { ...rule, action } : rule));
  }

  function savePolicy() {
    setNotice(null);
    startSaving(async () => {
      try {
        const policy = await putAgentPolicy(rules);
        setRules(policy.rules);
        setConfigured(policy.configured);
        setUpdatedAt(policy.updatedAt);
        setNotice({ tone: "success", message: "Agent policy saved" });
      } catch (error: unknown) {
        setNotice({ tone: "error", message: errorMessage(error) });
      }
    });
  }

  return (
    <>
      <div className="page-heading">
        <div><p className="eyebrow">Agent access</p><h1>Policy</h1><p className="heading-meta">Choose which managed agents are allowed for this organization</p></div>
      </div>
      <section className="policy-boundary">
        <span className="capability-icon" aria-hidden="true"><Shield size={18} /></span>
        <div><strong>Organization agent policy</strong><span>Allowed agents may use managed routing. Denied agents are recorded as desired policy but endpoint blocking is not available yet.</span></div>
        <CapabilityBadge tone={configured ? "partial" : "unavailable"}>{configured ? "Saved" : "Default allow"}</CapabilityBadge>
      </section>

      {notice ? <div className={`notice policy-notice notice-${notice.tone}`} role="status">{notice.message}</div> : null}

      <section className="agent-policy-list" aria-label="Agent allow and deny policy">
        {rules.map((rule) => (
          <div className="agent-policy-row" key={rule.agentId}>
            <span className={`agent-glyph agent-glyph-${rule.agentId}`} aria-hidden="true"><DiscoveryAgentIcon agentId={rule.agentId} /></span>
            <span><strong>{discoveryAgentName(rule.agentId)}</strong><small>{rule.action === "allow" ? "May use organization-managed routing" : "Desired policy denies this agent"}</small></span>
            <div className="segmented agent-policy-control" role="group" aria-label={`${discoveryAgentName(rule.agentId)} policy`}>
              <button className={rule.action === "allow" ? "active" : ""} type="button" onClick={() => setAction(rule.agentId, "allow")}>Allow</button>
              <button className={rule.action === "deny" ? "active deny" : ""} type="button" onClick={() => setAction(rule.agentId, "deny")}>Deny</button>
            </div>
          </div>
        ))}
      </section>

      <div className="agent-policy-save">
        <span>{loading ? "Loading policy" : updatedAt ? `Last saved ${formatRelative(updatedAt)}` : "Default policy has not been saved"}</span>
        <button className="button button-primary" type="button" disabled={loading || isSaving} onClick={savePolicy}>{isSaving ? <><Spinner /> Saving</> : "Save policy"}</button>
      </div>

      <section className="policy-deployment-boundary" aria-label="Policy deployment status">
        <AlertCircle size={17} aria-hidden="true" />
        <div><strong>Enforcement is not available</strong><span>This saves desired organization policy only. Agent blocking requires a versioned endpoint enforcement contract.</span></div>
      </section>
    </>
  );
}

function EnrollmentView({
  records,
  limits,
  selectedStatus,
  query,
  confirmation,
  busy,
  onStatusChange,
  onQueryChange,
  onConfirm,
  onCancel,
  onSelect
}: {
  records: RecordsByStatus;
  limits: LimitsByStatus;
  selectedStatus: EnrollmentStatus;
  query: string;
  confirmation: Confirmation;
  busy: boolean;
  onStatusChange: (status: EnrollmentStatus) => void;
  onQueryChange: (query: string) => void;
  onConfirm: () => void;
  onCancel: () => void;
  onSelect: (action: "approve" | "reject", record: EnrollmentRecord) => void;
}) {
  return (
    <>
      <div className="page-heading">
        <div><p className="eyebrow">Access workflow</p><h1>Enrollments</h1></div>
        <SearchField value={query} onChange={onQueryChange} label="Search enrollments" />
      </div>
      <div className="segmented" role="tablist" aria-label="Enrollment status">
        {statuses.map((status) => (
          <button
            key={status}
            type="button"
            role="tab"
            aria-selected={selectedStatus === status}
            className={selectedStatus === status ? "active" : ""}
            onClick={() => onStatusChange(status)}
          >
            {titleCase(status)} <span>{countLabel(records[status].length, limits[status])}</span>
          </button>
        ))}
      </div>
      <EnrollmentTable
        records={records[selectedStatus]}
        status={selectedStatus}
        query={query}
        confirmation={confirmation}
        busy={busy}
        onConfirm={onConfirm}
        onCancel={onCancel}
        onSelect={onSelect}
      />
      {limits[selectedStatus] ? <p className="limit-note">Showing the first 100 records returned by the enrollment server.</p> : null}
    </>
  );
}

function DeviceActionsMenu({
  device,
  disabled,
  onRevoke
}: {
  device: AdministrativeDevice;
  disabled: boolean;
  onRevoke: () => void;
}) {
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const [open, setOpen] = useState(false);
  const [position, setPosition] = useState({ top: 0, left: 0 });

  useEffect(() => {
    if (!open) return;

    const positionMenu = () => {
      const trigger = triggerRef.current;
      if (!trigger) return;
      const triggerBounds = trigger.getBoundingClientRect();
      const menuWidth = menuRef.current?.offsetWidth ?? 180;
      const menuHeight = menuRef.current?.offsetHeight ?? 50;
      const gutter = 8;
      const gap = 6;
      const left = Math.min(
        window.innerWidth - menuWidth - gutter,
        Math.max(gutter, triggerBounds.right - menuWidth)
      );
      const below = triggerBounds.bottom + gap;
      const top = below + menuHeight <= window.innerHeight - gutter
        ? below
        : Math.max(gutter, triggerBounds.top - menuHeight - gap);
      setPosition({ top, left });
    };
    const dismiss = (event: PointerEvent) => {
      const target = event.target as Node;
      if (!triggerRef.current?.contains(target) && !menuRef.current?.contains(target)) {
        setOpen(false);
      }
    };
    const handleKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setOpen(false);
        triggerRef.current?.focus();
      }
    };

    positionMenu();
    const frame = window.requestAnimationFrame(() => {
      positionMenu();
      menuRef.current?.querySelector<HTMLButtonElement>("button")?.focus();
    });
    window.addEventListener("resize", positionMenu);
    window.addEventListener("scroll", positionMenu, true);
    document.addEventListener("pointerdown", dismiss);
    document.addEventListener("keydown", handleKey);
    return () => {
      window.cancelAnimationFrame(frame);
      window.removeEventListener("resize", positionMenu);
      window.removeEventListener("scroll", positionMenu, true);
      document.removeEventListener("pointerdown", dismiss);
      document.removeEventListener("keydown", handleKey);
    };
  }, [open]);

  return (
    <>
      <button
        ref={triggerRef}
        type="button"
        className="icon-button action-menu-trigger"
        aria-label={`Actions for ${device.deviceName ?? "device"}`}
        aria-haspopup="menu"
        aria-expanded={open}
        title="Actions"
        disabled={disabled}
        onClick={() => setOpen((current) => !current)}
      >
        <Ellipsis size={18} />
      </button>
      {open ? createPortal(
        <div
          ref={menuRef}
          className="device-actions-dropdown"
          role="menu"
          aria-label={`Actions for ${device.deviceName ?? "device"}`}
          style={position}
        >
          <button
            type="button"
            role="menuitem"
            className="device-actions-item device-actions-item-danger"
            onClick={() => {
              setOpen(false);
              onRevoke();
            }}
          >
            <ShieldOff size={15} />
            <span>Revoke access</span>
          </button>
        </div>,
        document.body
      ) : null}
    </>
  );
}

function DevicesView({
  devices,
  limited,
  query,
  confirmation,
  busy,
  onQueryChange,
  onConfirm,
  onCancel,
  onSelect
}: {
  devices: AdministrativeDevice[];
  limited: boolean;
  query: string;
  confirmation: Confirmation;
  busy: boolean;
  onQueryChange: (query: string) => void;
  onConfirm: () => void;
  onCancel: () => void;
  onSelect: (device: AdministrativeDevice) => void;
}) {
  const filtered = devices.filter((device) => deviceMatches(device, query));
  return (
    <>
      <div className="page-heading">
        <div><p className="eyebrow">Enrollment authority inventory</p><h1>Devices</h1></div>
        <SearchField value={query} onChange={onQueryChange} label="Search devices" />
      </div>
      {filtered.length === 0 ? (
        <EmptyState title={query ? "No matching devices" : "No enrolled devices"} detail={query ? "Try another subject, device, or certificate serial." : "Approved devices will appear here."} />
      ) : (
        <div className="table-scroll device-table-scroll">
          <table className="device-table">
            <thead>
              <tr>
                <th>Device</th>
                <th>Owner</th>
                <th>Enrolled</th>
                <th>Certificate expires</th>
                <th>Certificates</th>
                <th>Renewals</th>
                <th>Status</th>
                <th>Access</th>
              </tr>
            </thead>
            <tbody>
              {filtered.map((device) => {
                const confirming = confirmation && "device" in confirmation && confirmation.device.deviceId === device.deviceId;
                return (
                  <tr key={device.deviceId}>
                    <td data-label="Device">
                      <strong>{device.deviceName ?? "Unnamed device"}</strong>
                      <small className="row-detail mono" title={device.deviceId}>{compact(device.deviceId)}</small>
                      <small className="row-detail device-mobile-meta">
                        {device.username ?? device.subject}
                      </small>
                    </td>
                    <td data-label="Owner">
                      <strong>{device.username ?? device.subject}</strong>
                      {device.username ? <small className="row-detail mono" title={device.subject}>{compact(device.subject)}</small> : null}
                    </td>
                    <td data-label="Enrolled" title={formatDate(device.createdAt)}>{formatRelative(device.createdAt)}</td>
                    <td data-label="Certificate" title={device.currentCertificateNotAfter ? formatDate(device.currentCertificateNotAfter) : "Unavailable"}>
                      {device.currentCertificateNotAfter ? formatRelative(device.currentCertificateNotAfter) : "Unavailable"}
                    </td>
                    <td data-label="Certificates">{device.certificateCount}</td>
                    <td data-label="Renewals">{device.renewalCount}</td>
                    <td data-label="Status"><StatusBadge status={device.status} /></td>
                    <td className="actions-cell" data-label="Access">
                      {confirming ? (
                        <div className="confirm-actions">
                          <button type="button" className="button button-danger" disabled={busy} onClick={onConfirm}>
                            {busy ? <Spinner /> : <ShieldOff size={14} />} Confirm revoke
                          </button>
                          <button type="button" className="icon-button" aria-label="Cancel" title="Cancel" disabled={busy} onClick={onCancel}><X size={15} /></button>
                        </div>
                      ) : device.status === "active" ? (
                        <DeviceActionsMenu
                          device={device}
                          disabled={busy}
                          onRevoke={() => onSelect(device)}
                        />
                      ) : (
                        <span className="muted-action">Revoked {device.revokedAt ? formatRelative(device.revokedAt) : ""}</span>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      {limited ? <p className="limit-note">Showing the first 100 devices returned by the enrollment server.</p> : null}
    </>
  );
}

function ConnectionView({
  bootstrap,
  busy,
  onSignIn,
  onSignOut
}: {
  bootstrap: Bootstrap | null;
  busy: boolean;
  onSignIn: () => void;
  onSignOut: () => void;
}) {
  return (
    <>
      <div className="page-heading"><div><p className="eyebrow">Administration target</p><h1>Connection</h1></div></div>
      <section className="connection-section">
        <div className="connection-state">
          <span className={`connection-mark ${bootstrap?.server ? "connected" : ""}`} aria-hidden="true">
            {bootstrap?.server ? <Server size={19} /> : <AlertCircle size={19} />}
          </span>
          <div>
            <strong>{bootstrap?.server?.organizationName ?? "No enrollment server"}</strong>
            <span>{bootstrap?.server?.serverUrl ?? bootstrap?.connectionError ?? "Configure an administrator endpoint"}</span>
          </div>
          {bootstrap?.server ? <span className="connection-label">Connected</span> : null}
        </div>
      </section>

      {bootstrap?.server ? (
        <section className="connection-section session-section">
          <div>
            <h2>Administrator session</h2>
            <p>{bootstrap.signedIn ? "Authenticated for enrollment administration" : "Sign in with an administrator-scoped organization account"}</p>
          </div>
          {bootstrap.signedIn ? (
            <button type="button" className="button button-secondary" disabled={busy} onClick={onSignOut}><LogOut size={14} /> Sign out</button>
          ) : (
            <button type="button" className="button button-primary" disabled={busy} onClick={onSignIn}><KeyRound size={14} /> Sign in</button>
          )}
        </section>
      ) : null}

      <section className="connection-section build-section">
        <h2>Application</h2>
        <dl>
          <div><dt>Version</dt><dd>{bootstrap?.version ?? "Unavailable"}</dd></div>
          <div><dt>Platform</dt><dd>{bootstrap?.platform ?? "Unavailable"}</dd></div>
          <div><dt>Session storage</dt><dd>Browser session</dd></div>
        </dl>
      </section>
    </>
  );
}

export function Admin() {
  const [view, setView] = useState<View>("overview");
  const [bootstrap, setBootstrap] = useState<Bootstrap | null>(null);
  const [records, setRecords] = useState<RecordsByStatus>(emptyRecords);
  const [limits, setLimits] = useState<LimitsByStatus>(emptyLimits);
  const [summary, setSummary] = useState<FleetSummary>(emptySummary);
  const [devices, setDevices] = useState<AdministrativeDevice[]>([]);
  const [deviceListLimited, setDeviceListLimited] = useState(false);
  const [selectedStatus, setSelectedStatus] = useState<EnrollmentStatus>("pending");
  const [query, setQuery] = useState("");
  const [confirmation, setConfirmation] = useState<Confirmation>(null);
  const [notice, setNotice] = useState<Notice>(null);
  const [refreshedAt, setRefreshedAt] = useState<Date | null>(null);
  const [isRefreshing, startRefreshing] = useTransition();
  const [isActing, startActing] = useTransition();
  const [isConnecting, startConnecting] = useTransition();

  async function fetchData() {
    const data = await loadAdministrationData();
    startTransition(() => {
      setRecords(data.records);
      setLimits(data.limits);
      setSummary(data.summary);
      setDevices(data.deviceList.devices);
      setDeviceListLimited(data.deviceList.limited);
      setRefreshedAt(new Date(data.summary.generatedAt));
    });
  }

  useEffect(() => {
    let active = true;
    getBootstrap()
      .then((nextBootstrap) => {
        if (!active) return;
        setBootstrap(nextBootstrap);
        if (!nextBootstrap.signedIn) setView("connection");
      })
      .catch((error: unknown) => {
        if (active) setNotice({ tone: "error", message: errorMessage(error) });
      });
    return () => { active = false; };
  }, []);

  useEffect(() => {
    const expireSession = () => {
      startTransition(() => {
        setBootstrap((current) => current ? { ...current, signedIn: false } : current);
        setRecords(emptyRecords);
        setLimits(emptyLimits);
        setSummary(emptySummary);
        setDevices([]);
        setDeviceListLimited(false);
        setConfirmation(null);
        setView("connection");
        setNotice({ tone: "error", message: "Administrator session expired. Sign in again." });
      });
    };
    window.addEventListener(adminSessionExpiredEvent, expireSession);
    return () => window.removeEventListener(adminSessionExpiredEvent, expireSession);
  }, []);

  useEffect(() => {
    if (!bootstrap?.signedIn) return;
    let active = true;
    let loading = false;
    const load = (force = false) => {
      if (loading || (!force && document.hidden)) return;
      loading = true;
      loadAdministrationData()
        .then((data) => {
          if (!active) return;
          startTransition(() => {
            setRecords(data.records);
            setLimits(data.limits);
            setSummary(data.summary);
            setDevices(data.deviceList.devices);
            setDeviceListLimited(data.deviceList.limited);
            setRefreshedAt(new Date(data.summary.generatedAt));
          });
        })
        .catch((error: unknown) => {
          if (active) setNotice({ tone: "error", message: errorMessage(error) });
        })
        .finally(() => { loading = false; });
    };
    load(true);
    const interval = window.setInterval(load, administrationRefreshIntervalMs);
    const loadWhenVisible = () => load();
    document.addEventListener("visibilitychange", loadWhenVisible);
    return () => {
      active = false;
      window.clearInterval(interval);
      document.removeEventListener("visibilitychange", loadWhenVisible);
    };
  }, [bootstrap?.signedIn, bootstrap?.server?.serverUrl]);

  function refresh() {
    setNotice(null);
    startRefreshing(async () => {
      try {
        await fetchData();
      } catch (error: unknown) {
        setNotice({ tone: "error", message: errorMessage(error) });
      }
    });
  }

  function openStatus(status: EnrollmentStatus) {
    setSelectedStatus(status);
    setQuery("");
    setView("enrollments");
  }

  function handleSignIn() {
    setNotice(null);
    startConnecting(async () => {
      try {
        const nextBootstrap = await signIn();
        setBootstrap(nextBootstrap);
        setView("overview");
        setNotice({ tone: "success", message: "Administrator sign-in complete" });
      } catch (error: unknown) {
        setNotice({ tone: "error", message: errorMessage(error) });
      }
    });
  }

  function handleSignOut() {
    setNotice(null);
    startConnecting(async () => {
      try {
        setBootstrap(await signOut());
        setRecords(emptyRecords);
        setLimits(emptyLimits);
        setSummary(emptySummary);
        setDevices([]);
        setDeviceListLimited(false);
        setView("connection");
      } catch (error: unknown) {
        setNotice({ tone: "error", message: errorMessage(error) });
      }
    });
  }

  function selectEnrollmentAction(action: "approve" | "reject", record: EnrollmentRecord) {
    setNotice(null);
    setConfirmation({ action, record });
  }

  function selectDeviceAction(device: AdministrativeDevice) {
    setNotice(null);
    setConfirmation({ action: "revoke", device });
  }

  function confirmAction() {
    if (!confirmation) return;
    startActing(async () => {
      try {
        if (confirmation.action === "approve") {
          await approveEnrollment(confirmation.record.enrollmentId);
        } else if (confirmation.action === "reject") {
          await rejectEnrollment(confirmation.record.enrollmentId);
        } else if ("device" in confirmation) {
          await revokeDevice(confirmation.device.deviceId);
        }
        const action = confirmation.action;
        setConfirmation(null);
        setNotice({
          tone: "success",
          message: action === "approve" ? "Device approved" : action === "reject" ? "Enrollment rejected" : "Device access revoked"
        });
        await fetchData();
      } catch (error: unknown) {
        setNotice({ tone: "error", message: errorMessage(error) });
      }
    });
  }

  const signedIn = Boolean(bootstrap?.signedIn);
  const navigation = [
    { id: "overview" as const, label: "Overview", icon: LayoutDashboard },
    { id: "inventory" as const, label: "Inventory", icon: Boxes },
    { id: "policies" as const, label: "Policies", icon: Shield },
    { id: "enrollments" as const, label: "Enrollments", icon: Inbox, count: summary.pendingEnrollments },
    { id: "devices" as const, label: "Devices", icon: Laptop2 },
    { id: "connection" as const, label: "Connection", icon: Settings2 }
  ];

  return (
    <div className="admin-shell">
      <aside className="sidebar">
        <div className="brand">
          <span className="brand-mark" aria-hidden="true"><ShieldCheck size={18} /></span>
          <span><strong>Agent Desktop</strong><small>Administration</small></span>
        </div>
        <nav aria-label="Administration">
          {navigation.map(({ id, label, icon: Icon, count }) => (
            <button
              key={id}
              type="button"
              className={view === id ? "active" : ""}
              aria-current={view === id ? "page" : undefined}
              disabled={!signedIn && id !== "connection"}
              onClick={() => {
                setView(id);
                setQuery("");
                setConfirmation(null);
                setNotice(null);
              }}
            >
              <Icon size={16} />
              <span>{label}</span>
              {count ? <strong>{count}</strong> : null}
            </button>
          ))}
        </nav>
        <div className="sidebar-footer">
          <span className={`server-dot ${bootstrap?.server ? "online" : ""}`} aria-hidden="true" />
          <span>
            <strong>{bootstrap?.server?.organizationName ?? "Not connected"}</strong>
            <small>{signedIn ? "Administrator session" : bootstrap?.server ? "Sign-in required" : "Configure server"}</small>
          </span>
        </div>
      </aside>

      <section className="workspace">
        <header className="topbar">
          <div className="topbar-context">
            <span>{bootstrap?.server?.organizationName ?? "Enrollment administration"}</span>
            {signedIn ? <><span className="divider" /><span>Authenticated</span></> : null}
          </div>
          {signedIn ? (
            <button type="button" className="icon-button" title="Refresh" aria-label="Refresh" disabled={isRefreshing} onClick={refresh}>
              {isRefreshing ? <Spinner /> : <RefreshCw size={16} />}
            </button>
          ) : null}
        </header>

        <main>
          {notice ? (
            <div className={`notice notice-${notice.tone}`} role={notice.tone === "error" ? "alert" : "status"}>
              {notice.tone === "error" ? <AlertCircle size={15} /> : <Check size={15} />}
              <span>{notice.message}</span>
              <button type="button" aria-label="Dismiss" title="Dismiss" onClick={() => setNotice(null)}><X size={14} /></button>
            </div>
          ) : null}

          {!signedIn && view !== "connection" ? null : view === "overview" ? (
            <Overview records={records} summary={summary} devices={devices} refreshedAt={refreshedAt} onOpenStatus={openStatus} onOpenDevices={() => setView("devices")} />
          ) : view === "inventory" ? (
            <InventoryView />
          ) : view === "policies" ? (
            <PoliciesView />
          ) : view === "enrollments" ? (
            <EnrollmentView
              records={records}
              limits={limits}
              selectedStatus={selectedStatus}
              query={query}
              confirmation={confirmation}
              busy={isActing}
              onStatusChange={(status) => { setSelectedStatus(status); setConfirmation(null); }}
              onQueryChange={setQuery}
              onConfirm={confirmAction}
              onCancel={() => setConfirmation(null)}
              onSelect={selectEnrollmentAction}
            />
          ) : view === "devices" ? (
            <DevicesView
              devices={devices}
              limited={deviceListLimited}
              query={query}
              confirmation={confirmation}
              busy={isActing}
              onQueryChange={setQuery}
              onConfirm={confirmAction}
              onCancel={() => setConfirmation(null)}
              onSelect={selectDeviceAction}
            />
          ) : (
            <ConnectionView
              bootstrap={bootstrap}
              busy={isConnecting}
              onSignIn={handleSignIn}
              onSignOut={handleSignOut}
            />
          )}
        </main>
      </section>
    </div>
  );
}
