import {
  CardHeader,
  defaultUsageMetric,
  formatCount,
  UsageBreakdownTable,
  UsageChart,
  type UsageDimension,
  type UsageGroup,
  type UsageMetricKind,
  UsageMetrics,
  UsageNotes,
  UsageSegmented,
  usageGroupName,
} from "@agentdesktop/ui";
import { ChartColumn, X } from "lucide-react";
import { useState } from "react";

import { ErrorState, PageSkeleton } from "../components/ViewStates";
import type { UsageRange, UsageReport } from "../types";
import { type UsageFilters, usageDimensions } from "../usage";

const ranges: ReadonlyArray<{ value: UsageRange; label: string }> = [
  { value: "24h", label: "24h" },
  { value: "7d", label: "7d" },
  { value: "30d", label: "30d" },
  { value: "90d", label: "90d" },
];

const metrics: ReadonlyArray<{ value: UsageMetricKind; label: string }> = [
  { value: "cost", label: "Cost" },
  { value: "tokens", label: "Tokens" },
  { value: "requests", label: "Requests" },
];

const dimensionNames: Record<UsageDimension, string> = {
  user: "User",
  device: "Device",
  tool: "Tool",
  model: "Model",
  provider: "Provider",
};

type SelectDimension = "user" | "device";

const filterSelects: ReadonlyArray<{
  dimension: SelectDimension;
  all: string;
}> = [
  { dimension: "user", all: "All users" },
  { dimension: "device", all: "All devices" },
];

export interface UsageViewProps {
  dimension: UsageDimension;
  error: string | null;
  filters: UsageFilters;
  loading: boolean;
  onDimensionChange: (dimension: UsageDimension) => void;
  onFiltersChange: (filters: UsageFilters) => void;
  onRangeChange: (range: UsageRange) => void;
  range: UsageRange;
  report: UsageReport | null;
}

export function UsageView({
  dimension,
  error,
  filters,
  loading,
  onDimensionChange,
  onFiltersChange,
  onRangeChange,
  range,
  report,
}: UsageViewProps) {
  const [metric, setMetric] = useState<UsageMetricKind | null>(null);
  // Users and devices have their own selects.
  const filterEntries = usageDimensions.flatMap(({ value }) => {
    const key = filters[value];
    return key === undefined || value === "user" || value === "device"
      ? []
      : [{ dimension: value, key }];
  });
  const rows = report?.breakdowns[dimension] ?? [];
  const chartMetric =
    metric ?? (report ? defaultUsageMetric(report.totals) : "cost");

  function filterName(filterDimension: UsageDimension, key: string) {
    const group = report?.breakdowns[filterDimension]?.find(
      (row) => row.key === key,
    );
    return usageGroupName(filterDimension, group ?? { key });
  }

  /** Options by name, unattributed usage last, including the chosen one. */
  function choices(selectDimension: SelectDimension) {
    const options = report?.filterOptions[selectDimension] ?? [];
    const chosen = filters[selectDimension];
    const named = options.map((option) => ({
      key: option.key,
      name: usageGroupName(selectDimension, option),
    }));
    if (chosen !== undefined && !options.some(({ key }) => key === chosen)) {
      named.push({ key: chosen, name: filterName(selectDimension, chosen) });
    }
    return named.sort(
      (left, right) =>
        Number(!left.key) - Number(!right.key) ||
        left.name.localeCompare(right.name),
    );
  }

  function choose(selectDimension: SelectDimension, key: string | undefined) {
    const next = { ...filters };
    if (key === undefined) delete next[selectDimension];
    else next[selectDimension] = key;
    // Device options belong to the previous user.
    if (selectDimension === "user") delete next.device;
    onFiltersChange(next);
  }

  function select(group: UsageGroup) {
    onFiltersChange({ ...filters, [dimension]: group.key });
  }

  function clear(filterDimension: UsageDimension) {
    const next = { ...filters };
    delete next[filterDimension];
    onFiltersChange(next);
  }

  return (
    <div className="stack">
      <section className="welcome-row">
        <div>
          <h2>Fleet usage</h2>
          <p>
            {report
              ? `${formatCount(report.activeUsers)} ${report.activeUsers === 1 ? "user" : "users"} on ${formatCount(report.activeDevices)} ${report.activeDevices === 1 ? "device" : "devices"}`
              : "LLM tokens and cost recorded by the gateway."}
          </p>
        </div>
        <UsageSegmented
          label="Period"
          onChange={onRangeChange}
          options={ranges}
          value={range}
        />
      </section>
      <fieldset className="usage-filter-bar">
        <legend className="usage-visually-hidden">Filters</legend>
        {filterSelects.map(({ dimension: selectDimension, all }) => {
          const options = choices(selectDimension);
          const chosen = options.findIndex(
            ({ key }) => key === filters[selectDimension],
          );
          return (
            <label className="usage-filter" key={selectDimension}>
              <span>{dimensionNames[selectDimension]}</span>
              <select
                onChange={(event) =>
                  choose(
                    selectDimension,
                    event.target.value === ""
                      ? undefined
                      : options[Number(event.target.value)]?.key,
                  )
                }
                value={chosen < 0 ? "" : String(chosen)}
              >
                <option value="">{all}</option>
                {options.map((option, index) => (
                  <option key={option.key} value={String(index)}>
                    {option.name}
                  </option>
                ))}
              </select>
            </label>
          );
        })}
        {filterEntries.length ? (
          <ul className="usage-filters">
            {filterEntries.map((filter) => (
              <li key={filter.dimension}>
                <span>
                  {dimensionNames[filter.dimension]}:{" "}
                  <strong>{filterName(filter.dimension, filter.key)}</strong>
                </span>
                <button
                  aria-label={`Remove ${dimensionNames[filter.dimension].toLowerCase()} filter`}
                  onClick={() => clear(filter.dimension)}
                  type="button"
                >
                  <X size={14} />
                </button>
              </li>
            ))}
          </ul>
        ) : null}
      </fieldset>
      {!report ? (
        loading ? (
          <PageSkeleton />
        ) : (
          <ErrorState message={error} />
        )
      ) : (
        <>
          {!report.enabled ? (
            <section className="local-notice">
              <div>
                <strong>Usage collection is off</strong>
                <span>
                  Set usage.tokenFile in the controller configuration and send
                  agentgateway access logs to the fleet API.
                </span>
              </div>
            </section>
          ) : null}
          <UsageMetrics
            previous={report.previousTotals}
            totals={report.totals}
          />
          <UsageNotes totals={report.totals} />
          <section className="card">
            <CardHeader
              title="Over time"
              description={
                report.bucketSeconds < 86400 ? "Per hour" : "Per day"
              }
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
              bucketSeconds={report.bucketSeconds}
              buckets={report.buckets}
              metric={chartMetric}
            />
          </section>
          <section className="card table-card">
            <CardHeader
              title="Breakdown"
              description={`${formatCount(rows.length)} ${(rows.length === 1 ? dimensionNames[dimension] : `${dimensionNames[dimension]}s`).toLowerCase()}`}
              action={
                <UsageSegmented
                  label="Group by"
                  onChange={onDimensionChange}
                  options={usageDimensions}
                  value={dimension}
                />
              }
            />
            {rows.length ? (
              <UsageBreakdownTable
                dimension={dimension}
                label={`Usage by ${dimensionNames[dimension].toLowerCase()}`}
                onSelect={filters[dimension] === undefined ? select : undefined}
                rows={rows}
              />
            ) : (
              <div className="empty-inline">
                <ChartColumn size={20} />
                <span>No usage recorded in this period.</span>
              </div>
            )}
          </section>
        </>
      )}
    </div>
  );
}
