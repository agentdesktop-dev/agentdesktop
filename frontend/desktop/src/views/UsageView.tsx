import {
  CardHeader,
  defaultUsageMetric,
  UsageBreakdownTable,
  UsageChart,
  type UsageMetricKind,
  UsageMetrics,
  UsageNotes,
  UsageSegmented,
} from "@agentdesktop/ui";
import { AlertCircle, ChartColumn, LoaderCircle } from "lucide-react";
import { useState } from "react";

import type { DeviceUsage, UsageRange } from "../types";

const ranges: ReadonlyArray<{ value: UsageRange; label: string }> = [
  { value: "24h", label: "24h" },
  { value: "7d", label: "7d" },
  { value: "30d", label: "30d" },
];

const metrics: ReadonlyArray<{ value: UsageMetricKind; label: string }> = [
  { value: "cost", label: "Cost" },
  { value: "tokens", label: "Tokens" },
  { value: "requests", label: "Requests" },
];

export interface UsageViewProps {
  error: string | null;
  loading: boolean;
  onRangeChange: (range: UsageRange) => void;
  range: UsageRange;
  usage: DeviceUsage | null;
}

export function UsageView({
  error,
  loading,
  onRangeChange,
  range,
  usage,
}: UsageViewProps) {
  const [metric, setMetric] = useState<UsageMetricKind | null>(null);
  const chartMetric =
    metric ?? (usage ? defaultUsageMetric(usage.totals) : "cost");
  return (
    <div className="page-stack">
      <div className="page-heading usage-heading">
        <div>
          <h2>Gateway usage</h2>
          <p>Tokens and cost of LLM requests from this device.</p>
        </div>
        <UsageSegmented
          label="Period"
          onChange={onRangeChange}
          options={ranges}
          value={range}
        />
      </div>
      {!usage ? (
        loading || !error ? (
          <section className="status-loading" role="status" aria-live="polite">
            <LoaderCircle className="spin" size={22} />
            <h2>Loading usage</h2>
          </section>
        ) : (
          <section className="page-error" role="alert">
            <AlertCircle size={24} />
            <h2>Usage is unavailable</h2>
            <p>{error}</p>
          </section>
        )
      ) : !usage.enabled ? (
        <section className="card">
          <div className="empty-inline">
            <ChartColumn size={20} />
            <div>
              <strong>Usage isn’t recorded</strong>
              <span>
                Your organization’s controller doesn’t collect LLM gateway
                usage.
              </span>
            </div>
          </div>
        </section>
      ) : (
        <>
          {error ? (
            <div className="notice notice-error" role="alert">
              Couldn’t refresh usage. Showing the last result.
            </div>
          ) : null}
          <UsageMetrics totals={usage.totals} />
          <UsageNotes
            totals={usage.totals}
            unmeteredTools={usage.unmeteredTools}
          />
          <section className="card">
            <CardHeader
              title="Over time"
              description={usage.bucketSeconds < 86400 ? "Per hour" : "Per day"}
              action={
                <UsageSegmented
                  label="Chart"
                  onChange={setMetric}
                  options={metrics}
                  value={chartMetric}
                />
              }
            />
            <UsageChart
              bucketSeconds={usage.bucketSeconds}
              buckets={usage.buckets}
              metric={chartMetric}
            />
          </section>
          <section className="card table-card">
            <CardHeader
              title="By tool"
              description={`${usage.byTool.length} tool${usage.byTool.length === 1 ? "" : "s"}`}
            />
            {usage.byTool.length ? (
              <UsageBreakdownTable
                dimension="tool"
                label="Usage by tool"
                rows={usage.byTool}
              />
            ) : (
              <div className="empty-inline">
                <ChartColumn size={20} />
                <span>No usage recorded in this period.</span>
              </div>
            )}
          </section>
          {usage.byModel.length ? (
            <section className="card table-card">
              <CardHeader
                title="By model"
                description={`${usage.byModel.length} model${usage.byModel.length === 1 ? "" : "s"}`}
              />
              <UsageBreakdownTable
                dimension="model"
                label="Usage by model"
                rows={usage.byModel}
              />
            </section>
          ) : null}
        </>
      )}
    </div>
  );
}
