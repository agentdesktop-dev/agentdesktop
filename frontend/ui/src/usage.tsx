import { friendlyTool, ToolIcon } from "./tools";

/** Totals over a set of LLM gateway requests. */
export interface UsageTotals {
  requests: number;
  errors: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  /** API-billed cost in US dollars at the gateway's catalog prices. */
  costUsd: number;
  /** Subscription-billed requests priced at API rates; never part of `costUsd`. */
  subscriptionCostUsd: number;
  subscriptionRequests: number;
  /** Requests for models the gateway has no price for. */
  unpricedRequests: number;
}

export interface UsageBucket {
  startUnixSeconds: number;
  totals: UsageTotals;
}

export interface UsageGroup {
  /** Stored value; empty when the gateway did not attribute the usage. */
  key: string;
  /** User email or device hostname, when known. */
  label?: string;
  totals: UsageTotals;
}

export type UsageDimension = "user" | "device" | "tool" | "provider" | "model";
export type UsageMetricKind = "cost" | "tokens" | "requests";

const dimensionLabels: Record<UsageDimension, string> = {
  user: "User",
  device: "Device",
  tool: "Tool",
  provider: "Provider",
  model: "Model",
};

export const emptyUsageTotals: UsageTotals = {
  requests: 0,
  errors: 0,
  inputTokens: 0,
  outputTokens: 0,
  cacheReadTokens: 0,
  cacheWriteTokens: 0,
  costUsd: 0,
  subscriptionCostUsd: 0,
  subscriptionRequests: 0,
  unpricedRequests: 0,
};

const usd = new Intl.NumberFormat(undefined, {
  style: "currency",
  currency: "USD",
});
// Single requests often cost fractions of a cent.
const usdCents = new Intl.NumberFormat(undefined, {
  style: "currency",
  currency: "USD",
  minimumFractionDigits: 2,
  maximumFractionDigits: 4,
});
const compact = new Intl.NumberFormat(undefined, {
  notation: "compact",
  maximumFractionDigits: 1,
});
const whole = new Intl.NumberFormat(undefined);

export function formatUsd(value: number) {
  if (value > 0 && value < 0.0001) return `<${usdCents.format(0.0001)}`;
  return value > 0 && value < 0.01 ? usdCents.format(value) : usd.format(value);
}

export function formatTokens(value: number) {
  return compact.format(value);
}

export function formatCount(value: number) {
  return whole.format(value);
}

function formatRequests(value: number) {
  return `${formatCount(value)} ${value === 1 ? "request" : "requests"}`;
}

/** Input tokens already include cached tokens. */
export function totalTokens(totals: UsageTotals) {
  return totals.inputTokens + totals.outputTokens;
}

function metricValue(totals: UsageTotals, metric: UsageMetricKind) {
  if (metric === "cost") return totals.costUsd;
  if (metric === "tokens") return totalTokens(totals);
  return totals.requests;
}

function formatMetric(value: number, metric: UsageMetricKind) {
  if (metric === "cost") return formatUsd(value);
  if (metric === "tokens") return formatTokens(value);
  return formatCount(value);
}

/** Cost when there is API spend, tokens otherwise. */
export function defaultUsageMetric(totals: UsageTotals): UsageMetricKind {
  return totals.costUsd > 0 ? "cost" : "tokens";
}

export function friendlyProvider(provider: string) {
  const names: Record<string, string> = {
    anthropic: "Anthropic",
    openai: "OpenAI",
    "aws.bedrock": "Amazon Bedrock",
    "azure.ai.openai": "Azure OpenAI",
    "gcp.gemini": "Gemini",
    "gcp.vertex_ai": "Vertex AI",
    x_ai: "xAI",
    xai: "xAI",
  };
  return names[provider.toLowerCase()] ?? provider;
}

export function usageGroupName(
  dimension: UsageDimension,
  group: Pick<UsageGroup, "key" | "label">,
) {
  if (!group.key) {
    return dimension === "user"
      ? "Unknown user"
      : dimension === "device"
        ? "Unknown device"
        : dimension === "tool"
          ? "Unknown tool"
          : "Unknown";
  }
  if (dimension === "tool") return friendlyTool(group.key);
  if (dimension === "provider") return friendlyProvider(group.key);
  return group.label ?? group.key;
}

function change(current: number, previous: number | undefined) {
  if (!previous) return null;
  const percent = Math.round(((current - previous) / previous) * 100);
  return `${percent > 0 ? "+" : ""}${percent}% vs previous period`;
}

function UsageMetric({
  detail,
  label,
  value,
}: {
  detail?: string | null;
  label: string;
  value: string;
}) {
  return (
    <div className="usage-metric">
      <strong>{value}</strong>
      <span>{label}</span>
      {detail ? <small>{detail}</small> : null}
    </div>
  );
}

/**
 * Headline totals. Subscription usage at API rates is shown on its own,
 * never added to cost.
 */
export function UsageMetrics({
  previous,
  totals,
}: {
  previous?: UsageTotals;
  totals: UsageTotals;
}) {
  const subscription =
    totals.subscriptionRequests > 0 ||
    (previous?.subscriptionRequests ?? 0) > 0;
  return (
    <section className="usage-metrics" aria-label="Usage totals">
      <UsageMetric
        label="Cost"
        value={formatUsd(totals.costUsd)}
        detail={change(totals.costUsd, previous?.costUsd)}
      />
      {subscription ? (
        <UsageMetric
          label="Subscription (API rates)"
          value={formatUsd(totals.subscriptionCostUsd)}
          detail={formatRequests(totals.subscriptionRequests)}
        />
      ) : null}
      <UsageMetric
        label="Tokens"
        value={formatTokens(totalTokens(totals))}
        detail={`${formatTokens(totals.inputTokens)} in · ${formatTokens(totals.outputTokens)} out`}
      />
      <UsageMetric
        label="Requests"
        value={formatCount(totals.requests)}
        detail={
          totals.errors
            ? `${formatCount(totals.errors)} failed`
            : change(totals.requests, previous?.requests)
        }
      />
    </section>
  );
}

function bucketLabel(startUnixSeconds: number, bucketSeconds: number) {
  const midpoint =
    bucketSeconds >= 86400 ? startUnixSeconds + bucketSeconds / 2 : startUnixSeconds;
  const date = new Date(midpoint * 1000);
  return new Intl.DateTimeFormat(
    undefined,
    bucketSeconds < 86400
      ? { hour: "numeric" }
      : { month: "short", day: "numeric" },
  ).format(date);
}

/** Bars of one metric per bucket, oldest first. */
export function UsageChart({
  bucketSeconds,
  buckets,
  metric,
}: {
  bucketSeconds: number;
  buckets: UsageBucket[];
  metric: UsageMetricKind;
}) {
  const values = buckets.map((bucket) => metricValue(bucket.totals, metric));
  const peak = Math.max(0, ...values);
  const peakIndex = values.indexOf(peak);
  const first = buckets[0];
  const last = buckets[buckets.length - 1];
  const middle = buckets[Math.floor(buckets.length / 2)];
  const metricName =
    metric === "cost" ? "Cost" : metric === "tokens" ? "Tokens" : "Requests";
  const summary =
    first && last
      ? `${metricName} from ${bucketLabel(first.startUnixSeconds, bucketSeconds)} to ${bucketLabel(last.startUnixSeconds, bucketSeconds)}${
          peak > 0
            ? `, highest ${formatMetric(peak, metric)} at ${bucketLabel(buckets[peakIndex].startUnixSeconds, bucketSeconds)}`
            : ", none recorded"
        }`
      : `No ${metricName.toLowerCase()} recorded`;
  return (
    <div className="usage-chart">
      <span className="usage-chart-peak" aria-hidden="true">
        {peak > 0 ? formatMetric(peak, metric) : null}
      </span>
      <div
        className={`usage-chart-plot${buckets.length > 40 ? " dense" : ""}`}
        role="img"
        aria-label={summary}
      >
        {buckets.map((bucket, index) => (
          <span
            className="usage-chart-bar"
            key={bucket.startUnixSeconds}
            style={{
              height: peak > 0 ? `${(values[index] / peak) * 100}%` : 0,
            }}
            title={`${bucketLabel(bucket.startUnixSeconds, bucketSeconds)}: ${formatMetric(values[index], metric)}`}
          />
        ))}
      </div>
      {first && last ? (
        <div className="usage-chart-axis" aria-hidden="true">
          <span>{bucketLabel(first.startUnixSeconds, bucketSeconds)}</span>
          {middle && buckets.length > 2 ? (
            <span>{bucketLabel(middle.startUnixSeconds, bucketSeconds)}</span>
          ) : null}
          <span>{bucketLabel(last.startUnixSeconds, bucketSeconds)}</span>
        </div>
      ) : null}
    </div>
  );
}

export function UsageSegmented<T extends string>({
  label,
  onChange,
  options,
  value,
}: {
  label: string;
  onChange: (value: T) => void;
  options: ReadonlyArray<{ value: T; label: string }>;
  value: T;
}) {
  return (
    <fieldset className="usage-segmented">
      <legend className="usage-visually-hidden">{label}</legend>
      {options.map((option) => (
        <button
          aria-pressed={option.value === value}
          key={option.value}
          onClick={() => onChange(option.value)}
          type="button"
        >
          {option.label}
        </button>
      ))}
    </fieldset>
  );
}

/**
 * Usage per value of one dimension, largest spend first. With `onSelect`,
 * names are buttons, for example to filter by that value.
 */
export function UsageBreakdownTable({
  dimension,
  label,
  onSelect,
  rows,
}: {
  dimension: UsageDimension;
  label: string;
  onSelect?: (group: UsageGroup) => void;
  rows: UsageGroup[];
}) {
  const subscription = rows.some((row) => row.totals.subscriptionRequests > 0);
  return (
    <table className="usage-table">
      <caption className="usage-visually-hidden">{label}</caption>
      <thead>
        <tr>
          <th scope="col">{dimensionLabels[dimension]}</th>
          <th scope="col" className="usage-column-optional usage-column-count">
            Requests
          </th>
          <th scope="col" className="usage-column-count">
            Tokens
          </th>
          <th scope="col" className="usage-column-money">
            Cost
          </th>
          {subscription ? (
            <th scope="col" className="usage-column-money">
              Subscription (API rates)
            </th>
          ) : null}
        </tr>
      </thead>
      <tbody>
        {rows.map((row) => {
          const name = usageGroupName(dimension, row);
          const content = (
            <>
              {dimension === "tool" ? <ToolIcon kind={row.key} /> : null}
              <span>{name}</span>
            </>
          );
          return (
            <tr key={row.key}>
              <th scope="row" title={row.label ? row.key : undefined}>
                {onSelect ? (
                  <button
                    className="usage-name"
                    onClick={() => onSelect(row)}
                    type="button"
                  >
                    {content}
                  </button>
                ) : (
                  <span className="usage-name">{content}</span>
                )}
              </th>
              <td className="usage-column-optional">
                {formatCount(row.totals.requests)}
              </td>
              <td>{formatTokens(totalTokens(row.totals))}</td>
              <td>{formatUsd(row.totals.costUsd)}</td>
              {subscription ? (
                <td>
                  {row.totals.subscriptionRequests
                    ? formatUsd(row.totals.subscriptionCostUsd)
                    : "—"}
                </td>
              ) : null}
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}

/** Caveats that change how the totals read. */
export function UsageNotes({
  totals,
  unmeteredTools = [],
}: {
  totals: UsageTotals;
  unmeteredTools?: string[];
}) {
  const notes = [
    totals.unpricedRequests
      ? `Cost excludes ${formatRequests(totals.unpricedRequests)} to models without a gateway price.`
      : null,
    ...unmeteredTools.map(
      (tool) =>
        `${friendlyTool(tool)} uses GitHub's models, which aren't metered.`,
    ),
  ].filter((note): note is string => Boolean(note));
  if (!notes.length) return null;
  return (
    <ul className="usage-notes">
      {notes.map((note) => (
        <li key={note}>{note}</li>
      ))}
    </ul>
  );
}
