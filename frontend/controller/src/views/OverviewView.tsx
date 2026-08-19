import { CardHeader } from "@agentdesktop/ui";
import { ChevronRight } from "lucide-react";

import { DeviceTable, EmptyDevices } from "../components/DeviceTable";
import { Link } from "../router";
import type { Overview } from "../types";

export function OverviewView({ data }: { data: Overview }) {
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
