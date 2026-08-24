import {
  CardHeader,
  friendlyTool,
  ModelRuntimeInventory,
  ToolIcon,
  ToolInventory,
} from "@agentdesktop/ui";
import { ArrowLeft, Box, CircleAlert, Code2, Cpu, Trash2 } from "lucide-react";
import { useEffect, useRef } from "react";

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
import type { DeviceDetail } from "../types";

export interface DeviceViewProps {
  deleteError: string | null;
  deleteOpen: boolean;
  deleting: boolean;
  device: DeviceDetail;
  onDeleteCancel: () => void;
  onDeleteConfirm: () => void;
  onDeleteRequest: () => void;
}

export function DeviceView({
  deleteError,
  deleteOpen,
  deleting,
  device,
  onDeleteCancel,
  onDeleteConfirm,
  onDeleteRequest,
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
