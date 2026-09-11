import {
  CardHeader,
  formatEstimatedCost,
  formatUsageNumber,
  type LlmUsageRange,
  UsageRangePicker,
  usageNumberTitle,
  usageRangeDescriptions,
} from "@agentdesktop/ui";
import { ChartNoAxesCombined, ChevronRight } from "lucide-react";

import { ErrorState, PageSkeleton } from "../components/ViewStates";
import { navigate } from "../router";
import type { LlmDeviceUsage, LlmFleetUsageSummary } from "../types";

export interface UsageViewProps {
  usage: LlmFleetUsageSummary | null;
  range: LlmUsageRange;
  onRangeChange: (range: LlmUsageRange) => void;
  error?: string | null;
  loading?: boolean;
}

export function UsageView({
  usage,
  range,
  onRangeChange,
  error = null,
  loading = false,
}: UsageViewProps) {
  return (
    <div className="stack">
      <section className="section-intro">
        <div>
          <h2>Fleet usage</h2>
          <p>
            Estimated LLM cost, tokens, and calls metered by Agentgateway,
            attributed to the enrolled device that sent each request.
          </p>
        </div>
        <UsageRangePicker
          range={range}
          disabled={loading}
          onChange={onRangeChange}
        />
      </section>
      {loading && !usage ? (
        <PageSkeleton rows={4} />
      ) : error ? (
        <ErrorState message={error} />
      ) : !usage ? (
        <section className="card usage-empty">
          <ChartNoAxesCombined size={20} aria-hidden="true" />
          <div>
            <h2>Usage reporting is not configured</h2>
            <p>
              Set <code>llmGatewayUsageUrl</code> in the controller
              configuration to read Agentgateway analytics.
            </p>
          </div>
        </section>
      ) : (
        <>
          <section className="stat-grid card" aria-busy={loading}>
            <StatCard
              label={`Estimated cost · ${usageRangeDescriptions[range].toLowerCase()}`}
              value={formatEstimatedCost(usage.estimatedCost, usage.currency)}
            />
            <StatCard
              label="Tokens"
              value={formatUsageNumber(usage.totalTokens)}
              title={usageNumberTitle(usage.totalTokens)}
            />
            <StatCard
              label="Calls"
              value={formatUsageNumber(usage.requests)}
              title={usageNumberTitle(usage.requests)}
            />
            <StatCard
              label="Devices with usage"
              value={String(usage.devices.filter((row) => row.deviceId).length)}
            />
          </section>
          <section className="card table-card" aria-busy={loading}>
            <CardHeader
              title="Usage by device"
              description={`${usageRangeDescriptions[range]} · ${usage.from} to ${usage.to}`}
            />
            {usage.devices.length ? (
              <DeviceUsageTable
                currency={usage.currency}
                devices={usage.devices}
              />
            ) : (
              <div className="empty-state">
                <ChartNoAxesCombined size={28} />
                <h3>No usage in this period</h3>
                <p>Requests routed through Agentgateway will appear here.</p>
              </div>
            )}
          </section>
        </>
      )}
    </div>
  );
}

function StatCard({
  label,
  value,
  title,
}: {
  label: string;
  value: string;
  title?: string;
}) {
  return (
    <article className="stat-card">
      <strong title={title}>{value}</strong>
      <span>{label}</span>
    </article>
  );
}

function DeviceUsageTable({
  currency,
  devices,
}: {
  currency: string;
  devices: LlmDeviceUsage[];
}) {
  return (
    <section
      className="table-scroll"
      // biome-ignore lint/a11y/noNoninteractiveTabindex: Horizontal overflow must be keyboard scrollable.
      tabIndex={0}
      aria-label="Usage by device table"
    >
      <table>
        <thead>
          <tr>
            <th>Device</th>
            <th className="numeric">Estimated cost</th>
            <th className="numeric">Tokens</th>
            <th className="numeric">Calls</th>
            <th>
              <span className="sr-only">Open</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {devices.map((row) => {
            const href = row.deviceId
              ? `/devices/${encodeURIComponent(row.deviceId)}`
              : null;
            return (
              <tr
                key={row.deviceId ?? "unattributed"}
                className={href ? undefined : "static-row"}
                role={href ? "link" : undefined}
                tabIndex={href ? 0 : undefined}
                onClick={href ? () => navigate(href) : undefined}
                onKeyDown={
                  href
                    ? (event) => {
                        if (event.key === "Enter" || event.key === " ") {
                          event.preventDefault();
                          navigate(href);
                        }
                      }
                    : undefined
                }
              >
                <td>
                  <div className="device-cell">
                    <div>
                      <strong>
                        {row.hostname ??
                          (row.deviceId ? "Unknown device" : "Unattributed")}
                      </strong>
                      <span>
                        {row.deviceId
                          ? row.deviceId.slice(0, 8)
                          : "No device identity on the gateway credential"}
                      </span>
                    </div>
                  </div>
                </td>
                <td className="numeric usage-cost-cell">
                  {formatEstimatedCost(row.estimatedCost, currency)}
                </td>
                <td
                  className="numeric"
                  title={usageNumberTitle(row.totalTokens)}
                >
                  {formatUsageNumber(row.totalTokens)}
                </td>
                <td className="numeric" title={usageNumberTitle(row.requests)}>
                  {formatUsageNumber(row.requests)}
                </td>
                <td>
                  {href ? (
                    <ChevronRight size={16} className="row-arrow" />
                  ) : null}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </section>
  );
}
