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

      <section className="stat-grid card" aria-label="Browser PAC">
        {data.browser_pac.expected_pac_url ? (
          <>
            <StatCard
              label="PAC compliant"
              value={data.browser_pac.compliant}
            />
            <StatCard
              label="PAC not compliant"
              value={data.browser_pac.not_compliant}
            />
            <StatCard label="PAC absent" value={data.browser_pac.absent} />
          </>
        ) : (
          <>
            <StatCard label="PAC found" value={data.browser_pac.found} />
            <StatCard label="PAC absent" value={data.browser_pac.absent} />
            <StatCard label="PAC differs" value={data.browser_pac.differs} />
          </>
        )}
        <StatCard
          label="PAC not reported"
          value={data.browser_pac.not_reported}
        />
      </section>

      {data.browser_pac.versions.length > 0 && (
        <section className="card table-card">
          <CardHeader
            title="PAC versions"
            description="Devices grouped by the SHA-256 of the PAC they fetched. More than one version per URL means devices get different PACs."
          />
          <div className="event-list">
            {data.browser_pac.versions.map((version) => (
              <div
                className="event-row"
                key={`${version.url}-${version.sha256 ?? "unreachable"}`}
              >
                <span className="event-source">
                  <code title={version.url}>{version.url}</code>
                </span>
                <span className="program-state">
                  {version.sha256 ? (
                    <code title={`SHA-256 ${version.sha256}`}>
                      {version.sha256.slice(0, 12)}
                    </code>
                  ) : (
                    <span className="badge warning">Not fetched</span>
                  )}
                </span>
                <time>
                  {version.devices} device{version.devices === 1 ? "" : "s"}
                </time>
              </div>
            ))}
          </div>
        </section>
      )}

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
