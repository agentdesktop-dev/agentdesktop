import {
  CardHeader,
  friendlyBrowser,
  friendlyTool,
  ModelRuntimeInventory,
  ToolIcon,
  ToolInventory,
  UsageBreakdownTable,
  UsageMetrics,
  UsageNotes,
} from "@agentdesktop/ui";
import {
  ArrowLeft,
  Box,
  ChartColumn,
  ChevronRight,
  CircleAlert,
  Code2,
  Cpu,
  Trash2,
} from "lucide-react";
import { useEffect, useRef } from "react";

import { PacBadge, PacComplianceBadge } from "../components/BrowserPac";
import {
  ConfigBadge,
  DefinitionList,
  OnlineBadge,
} from "../components/DeviceTable";
import {
  formatDate,
  formatTime,
  formatTimeMilliseconds,
  friendlyOs,
} from "../format";
import { Link } from "../router";
import type { DeviceDetail, ProgramStatus, UsageReport } from "../types";

export interface DeviceViewProps {
  deleteError: string | null;
  deleteOpen: boolean;
  deleting: boolean;
  device: DeviceDetail;
  onDeleteCancel: () => void;
  onDeleteConfirm: () => void;
  onDeleteRequest: () => void;
  /** This device's last 30 days, when the controller has usage. */
  usage?: UsageReport | null;
}

export function DeviceView({
  deleteError,
  deleteOpen,
  deleting,
  device,
  onDeleteCancel,
  onDeleteConfirm,
  onDeleteRequest,
  usage,
}: DeviceViewProps) {
  const modelCount = device.model_runtimes.reduce(
    (total, runtime) => total + runtime.models.length,
    0,
  );
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
      {device.programs_reported !== null && (
        <section className="card table-card">
          <CardHeader
            title="Managed programs"
            description="Outcome of the latest apply, per program"
          />
          {!device.programs_reported ? (
            <p className="empty-state compact">
              Per-program status is not reported by this agent version
            </p>
          ) : device.programs.length ? (
            <div className="event-list">
              {device.programs.map((program) => (
                <div className="event-row" key={program.program}>
                  <span className="event-source">
                    <ToolIcon kind={program.program} />
                    <strong>{friendlyTool(program.program)}</strong>
                  </span>
                  <span className="program-state">
                    <ProgramBadge state={program.state} />
                    {program.detail && (
                      <code title={program.detail}>{program.detail}</code>
                    )}
                  </span>
                  <time>
                    {program.revision !== device.config_revision
                      ? `r${program.revision} · `
                      : ""}
                    {formatTime(program.updated_at)}
                  </time>
                </div>
              ))}
            </div>
          ) : (
            <p className="empty-state compact">No managed programs</p>
          )}
        </section>
      )}
      {usage && (usage.enabled || usage.totals.requests > 0) ? (
        <DeviceUsage deviceId={device.id} usage={usage} />
      ) : null}
      <BrowserPacCard device={device} />
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
      <section className="card table-card">
        <CardHeader
          title="Local models"
          description={`${modelCount} model${modelCount === 1 ? "" : "s"} reported`}
        />
        {device.model_runtimes.length ? (
          <div className="model-runtime-inventory">
            {device.model_runtimes.map((runtime) => (
              <ModelRuntimeInventory key={runtime.kind} runtime={runtime} />
            ))}
          </div>
        ) : (
          <div className="empty-inline">
            <Cpu size={20} />
            <span>No local models have been reported by this device.</span>
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
          onClick={onDeleteRequest}
        >
          <Trash2 size={14} /> Delete device
        </button>
      </section>
      {deleteOpen && (
        <DeleteDeviceDialog
          hostname={device.hostname}
          deleting={deleting}
          error={deleteError}
          onCancel={onDeleteCancel}
          onConfirm={onDeleteConfirm}
        />
      )}
    </div>
  );
}

function DeviceUsage({
  deviceId,
  usage,
}: {
  deviceId: string;
  usage: UsageReport;
}) {
  const tools = usage.breakdowns.tool ?? [];
  const models = usage.breakdowns.model ?? [];
  return (
    <section className="card table-card device-usage">
      <CardHeader
        title="Usage"
        description="Last 30 days"
        action={
          <Link
            href={`/usage?${new URLSearchParams({ range: "30d", device: deviceId })}`}
            className="text-link"
          >
            View in Usage <ChevronRight size={14} />
          </Link>
        }
      />
      {usage.totals.requests ? (
        <>
          <UsageMetrics totals={usage.totals} />
          <UsageNotes totals={usage.totals} />
          <UsageBreakdownTable
            dimension="tool"
            label="Usage by tool"
            rows={tools}
          />
          <UsageBreakdownTable
            dimension="model"
            label="Usage by model"
            rows={models}
          />
        </>
      ) : (
        <div className="empty-inline">
          <ChartColumn size={20} />
          <span>No LLM gateway usage in the last 30 days.</span>
        </div>
      )}
    </section>
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

function friendlyEvent(kind: string) {
  const names: Record<string, string> = {
    "session.new": "New session",
    "tool.use": "Tool use",
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

const programStateLabels: Record<ProgramStatus["state"], string> = {
  applied: "Applied",
  unchanged: "Unchanged",
  removed: "Removed",
  conflict: "Conflict",
  inactive: "Inactive",
  blocked: "Blocked",
  opted_out: "Opted out",
  failed: "Failed",
  unspecified: "Unknown",
};

const programStateDescriptions: Partial<
  Record<ProgramStatus["state"], string>
> = {
  opted_out: "The device user opted out of this program",
};

function BrowserPacCard({ device }: { device: DeviceDetail }) {
  const pac = device.browser_pac;
  return (
    <section className="card table-card">
      <CardHeader
        title={
          pac.compliance ? "Browser PAC (compliance check)" : "Browser PAC"
        }
        description={
          pac.compliance
            ? "Compares the PAC configured in browser policy with the fleet's expected PAC URL. Agentdesktop does not configure browsers."
            : "The proxy auto-config (PAC) URL set by browser policy"
        }
      />
      {pac.state === "not_reported" ? (
        <p className="empty-state compact">
          No browser policy reported by this agent
        </p>
      ) : (
        <div className="event-list">
          <div className="event-row">
            <span className="event-source">
              <strong>Device</strong>
            </span>
            <span className="program-state">
              <PacBadge pac={pac} />
              {pac.url && <code title={pac.url}>{pac.url}</code>}
            </span>
            <time>
              {pac.compliance && (
                <PacComplianceBadge compliance={pac.compliance} />
              )}
            </time>
          </div>
          {(pac.sha256 || pac.fetch_error) && (
            <div className="event-row">
              <span className="event-source">
                <span>
                  <strong>PAC identity</strong>
                  <small>SHA-256 of what the device fetched</small>
                </span>
              </span>
              <span className="program-state">
                {pac.sha256 ? (
                  <code title={`SHA-256 ${pac.sha256}`}>
                    {pac.sha256.slice(0, 16)} · {pac.size} bytes
                  </code>
                ) : (
                  <>
                    <span className="badge warning">PAC not reachable</span>
                    <code title={pac.fetch_error ?? undefined}>
                      {pac.fetch_error}
                    </code>
                  </>
                )}
              </span>
              <time />
            </div>
          )}
          {!pac.url &&
            device.pac_fetches.map((fetch) => (
              <div className="event-row" key={fetch.url}>
                <span className="event-source">
                  <span>
                    <strong>PAC identity</strong>
                    <small title={fetch.url}>{fetch.url}</small>
                  </span>
                </span>
                <span className="program-state">
                  {fetch.sha256 ? (
                    <code title={`SHA-256 ${fetch.sha256}`}>
                      {fetch.sha256.slice(0, 16)} · {fetch.size} bytes
                    </code>
                  ) : (
                    <>
                      <span className="badge warning">PAC not reachable</span>
                      <code title={fetch.error}>{fetch.error}</code>
                    </>
                  )}
                </span>
                <time />
              </div>
            ))}
          {pac.compliance?.reason && (
            <div className="event-row">
              <span className="event-source">
                <strong>Not compliant because</strong>
              </span>
              <span className="program-state">
                <code title={pac.compliance.reason}>
                  {pac.compliance.reason}
                </code>
              </span>
              <time />
            </div>
          )}
          {device.browser_pacs.map((browser) => (
            <div
              className="event-row"
              key={`${browser.browser}:${browser.packaging}`}
            >
              <span className="event-source">
                <span>
                  <strong>{friendlyBrowser(browser.browser)}</strong>
                  {browser.packaging && <small>{browser.packaging}</small>}
                </span>
              </span>
              <span className="program-state">
                <code
                  title={
                    browser.detail ??
                    (browser.source ? `From ${browser.source}` : undefined)
                  }
                >
                  {browser.pac_url ?? browser.detail ?? "No PAC in policy"}
                </code>
                {browser.recommended && (
                  <span className="badge warning">Recommended</span>
                )}
                {browser.locked && (
                  <span className="badge neutral">Locked</span>
                )}
              </span>
              <time />
            </div>
          ))}
        </div>
      )}
    </section>
  );
}

function ProgramBadge({ state }: { state: ProgramStatus["state"] }) {
  const tone =
    state === "conflict" || state === "failed" || state === "blocked"
      ? "danger"
      : state === "inactive"
        ? "warning"
        : "neutral";
  return (
    <span className={`badge ${tone}`} title={programStateDescriptions[state]}>
      {programStateLabels[state] ?? state}
    </span>
  );
}
