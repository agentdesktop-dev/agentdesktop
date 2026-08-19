import { friendlyTool, ToolIcon } from "@agentdesktop/ui";
import { Check, ChevronRight, CircleAlert } from "lucide-react";

import { friendlyOs } from "../format";
import { navigate } from "../router";
import type { Device } from "../types";
import { EmptyDevices } from "./ViewStates";

export function DeviceTable({
  devices,
  compact = false,
}: {
  devices: Device[];
  compact?: boolean;
}) {
  return (
    <section
      className="table-scroll"
      // biome-ignore lint/a11y/noNoninteractiveTabindex: Horizontal overflow must be keyboard scrollable.
      tabIndex={0}
      aria-label="Devices table"
    >
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
    </section>
  );
}

export { EmptyDevices };

export function DefinitionList({
  entries,
}: {
  entries: Array<[string, string]>;
}) {
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

export function OnlineBadge({ timestamp }: { timestamp: number | null }) {
  const online = timestamp !== null && Date.now() / 1000 - timestamp <= 90;
  return (
    <span className={`badge ${online ? "success" : "neutral"}`}>
      <span className="mini-dot" />
      {online ? "Online" : "Offline"}
    </span>
  );
}

export function ConfigBadge({
  state,
  revision,
}: {
  state: number | null;
  revision: number | null;
}) {
  if (state === 2) {
    return (
      <span className="badge danger">
        <CircleAlert size={13} /> Failed{revision ? ` · r${revision}` : ""}
      </span>
    );
  }
  if (state === 1) {
    return (
      <span className="badge success">
        <Check size={13} /> Applied{revision ? ` · r${revision}` : ""}
      </span>
    );
  }
  return <span className="badge neutral">Not reported</span>;
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
