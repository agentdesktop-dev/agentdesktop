import {
  ArrowDown,
  ArrowUp,
  ArrowUpDown,
  ChartNoAxesCombined,
  ChevronDown,
  ChevronRight,
  X,
} from "lucide-react";
import { Fragment, useRef, useState } from "react";

export interface LlmUsageSummary {
  from: string;
  to: string;
  /** ISO 4217 code for every `estimatedCost` in this report. */
  currency: string;
  requests: number;
  totalTokens: number;
  estimatedCost: number;
  breakdown: LlmUsageBreakdown[];
}

export type LlmUsageRange = "hour" | "day" | "week" | "month";

export interface LlmUsageBreakdown {
  model: string;
  agent: string;
  requests: number;
  totalTokens: number;
  estimatedCost: number;
}

export interface LlmUsageInteractions {
  /** ISO 4217 code for every `estimatedCost` in this page. */
  currency: string;
  interactions: LlmUsageInteraction[];
  nextCursor: string | null;
}

export interface LlmUsageInteraction {
  id: string;
  startedAt: string;
  completedAt: string | null;
  durationMs: number | null;
  httpStatus: number | null;
  failed: boolean;
  operation: string | null;
  provider: string | null;
  requestModel: string;
  responseModel: string | null;
  agent: string;
  inputTokens: number | null;
  outputTokens: number | null;
  totalTokens: number | null;
  estimatedCost: number | null;
}

export interface UsageReportProps {
  llmUsage: LlmUsageSummary | null;
  range: LlmUsageRange;
  loadedRange: LlmUsageRange;
  isRangeLoading: boolean;
  onRangeChange: (range: LlmUsageRange) => void;
  onLoadInteractions: (
    from: string,
    to: string,
    model: string,
    agent: string,
    cursor?: string | null,
  ) => Promise<LlmUsageInteractions | null>;
  /** Heading shown above the totals. Defaults to "Gateway usage". */
  title?: string;
  /** Text shown when `llmUsage` is null. */
  unavailableMessage?: string;
}

type SortColumn = "modelAgent" | "estimatedCost" | "totalTokens" | "requests";
type SortDirection = "ascending" | "descending";
type SortState = { column: SortColumn; direction: SortDirection };

const sortOptions: Array<SortState & { label: string }> = [
  {
    column: "estimatedCost",
    direction: "descending",
    label: "Cost, high to low",
  },
  {
    column: "estimatedCost",
    direction: "ascending",
    label: "Cost, low to high",
  },
  {
    column: "totalTokens",
    direction: "descending",
    label: "Tokens, high to low",
  },
  {
    column: "totalTokens",
    direction: "ascending",
    label: "Tokens, low to high",
  },
  {
    column: "requests",
    direction: "descending",
    label: "Calls, high to low",
  },
  {
    column: "requests",
    direction: "ascending",
    label: "Calls, low to high",
  },
  {
    column: "modelAgent",
    direction: "ascending",
    label: "Model, A to Z",
  },
  {
    column: "modelAgent",
    direction: "descending",
    label: "Model, Z to A",
  },
];

const rangeOptions: Array<{ value: LlmUsageRange; label: string }> = [
  { value: "hour", label: "Hour" },
  { value: "day", label: "Day" },
  { value: "week", label: "Week" },
  { value: "month", label: "Month" },
];

const rangeDescriptions: Record<LlmUsageRange, string> = {
  hour: "Past hour",
  day: "Past 24 hours",
  week: "Past 7 days",
  month: "Past 30 days",
};

export const usageRangeDescriptions = rangeDescriptions;

/** Segmented control for the trailing usage window. */
export function UsageRangePicker({
  range,
  disabled = false,
  onChange,
}: {
  range: LlmUsageRange;
  disabled?: boolean;
  onChange: (range: LlmUsageRange) => void;
}) {
  return (
    <fieldset className="usage-range">
      <legend>Usage period</legend>
      {rangeOptions.map((option) => (
        <label key={option.value}>
          <input
            type="radio"
            name="usage-range"
            value={option.value}
            checked={range === option.value}
            disabled={disabled}
            onChange={() => onChange(option.value)}
          />
          <span>{option.label}</span>
        </label>
      ))}
    </fieldset>
  );
}

/**
 * Totals, model/agent breakdown, and interaction drill-down for one usage
 * report. Renders a single `.card`; callers own the surrounding page layout.
 */
export function UsageReport({
  llmUsage,
  range,
  loadedRange,
  isRangeLoading,
  onRangeChange,
  onLoadInteractions,
  title = "Gateway usage",
  unavailableMessage = "Gateway usage is not configured or is currently unavailable.",
}: UsageReportProps) {
  const [modelFilter, setModelFilter] = useState("");
  const [agentFilter, setAgentFilter] = useState("");
  const [sort, setSort] = useState<SortState>({
    column: "estimatedCost",
    direction: "descending",
  });
  const [selectedBreakdown, setSelectedBreakdown] =
    useState<LlmUsageBreakdown | null>(null);
  const [interactions, setInteractions] = useState<LlmUsageInteraction[]>([]);
  const [nextCursor, setNextCursor] = useState<string | null>(null);
  const [interactionsError, setInteractionsError] = useState<string | null>(
    null,
  );
  const [interactionsLoading, setInteractionsLoading] = useState(false);
  const interactionRequest = useRef(0);

  if (!llmUsage) {
    return (
      <section className="card usage-empty">
        <ChartNoAxesCombined size={20} aria-hidden="true" />
        <div>
          <h2>No usage data</h2>
          <p>{unavailableMessage}</p>
        </div>
      </section>
    );
  }

  const models = uniqueSorted(llmUsage.breakdown.map((item) => item.model));
  const agents = uniqueSorted(
    llmUsage.breakdown.map((item) => item.agent),
    (value) => formatAgentName(value),
  );
  const filteredRows = llmUsage.breakdown.filter(
    (item) =>
      (!modelFilter || item.model === modelFilter) &&
      (!agentFilter || item.agent === agentFilter),
  );
  const rows = sortUsageRows(filteredRows, sort);
  const hasFilters = Boolean(modelFilter || agentFilter);
  const usageFrom = llmUsage.from;
  const usageTo = llmUsage.to;
  const currency = llmUsage.currency;

  function sortBy(column: SortColumn) {
    setSort((current) => nextSort(current, column));
  }

  function selectSort(value: string) {
    const selected = sortOptions.find((option) => sortValue(option) === value);
    if (selected) {
      setSort({ column: selected.column, direction: selected.direction });
    }
  }

  function clearFilters() {
    setModelFilter("");
    setAgentFilter("");
  }

  function closeInteractions() {
    interactionRequest.current += 1;
    setSelectedBreakdown(null);
    setInteractions([]);
    setNextCursor(null);
    setInteractionsError(null);
    setInteractionsLoading(false);
  }

  function changeRange(nextRange: LlmUsageRange) {
    closeInteractions();
    onRangeChange(nextRange);
  }

  function toggleInteractions(item: LlmUsageBreakdown) {
    if (
      selectedBreakdown &&
      breakdownKey(selectedBreakdown) === breakdownKey(item)
    ) {
      closeInteractions();
      return;
    }
    setSelectedBreakdown(item);
    setInteractions([]);
    setNextCursor(null);
    setInteractionsError(null);
    void loadInteractions(item, null, false);
  }

  async function loadInteractions(
    item: LlmUsageBreakdown,
    cursor: string | null,
    append: boolean,
  ) {
    const request = interactionRequest.current + 1;
    interactionRequest.current = request;
    setInteractionsLoading(true);
    setInteractionsError(null);
    try {
      const result = await onLoadInteractions(
        usageFrom,
        usageTo,
        item.model,
        item.agent,
        cursor,
      );
      if (interactionRequest.current !== request) return;
      if (!result) throw new Error("Interaction data is unavailable.");
      setInteractions((current) =>
        append ? [...current, ...result.interactions] : result.interactions,
      );
      setNextCursor(result.nextCursor);
    } catch (error: unknown) {
      if (interactionRequest.current !== request) return;
      setInteractionsError(
        error instanceof Error ? error.message : String(error),
      );
    } finally {
      if (interactionRequest.current === request) {
        setInteractionsLoading(false);
      }
    }
  }

  return (
    <section
      className="card usage-summary"
      aria-labelledby="usage-title"
      aria-busy={isRangeLoading}
    >
      <div className="usage-summary-heading">
        <span className="usage-summary-icon" aria-hidden="true">
          <ChartNoAxesCombined size={17} />
        </span>
        <div>
          <h2 id="usage-title">{title}</h2>
          <span
            className="usage-summary-period"
            title={`${llmUsage.from} to ${llmUsage.to}`}
          >
            {isRangeLoading
              ? `Updating to ${rangeDescriptions[range].toLowerCase()}…`
              : `All models and agents · ${rangeDescriptions[loadedRange]}`}
          </span>
        </div>
      </div>
      <UsageRangePicker
        range={range}
        disabled={isRangeLoading}
        onChange={changeRange}
      />
      <dl className="usage-summary-metrics">
        <div className="usage-summary-primary">
          <dt>Estimated cost</dt>
          <dd>{formatEstimatedCost(llmUsage.estimatedCost, currency)}</dd>
        </div>
        <div>
          <dt>Tokens</dt>
          <dd title={usageNumberTitle(llmUsage.totalTokens)}>
            {formatUsageNumber(llmUsage.totalTokens)}
          </dd>
        </div>
        <div>
          <dt>Calls</dt>
          <dd title={usageNumberTitle(llmUsage.requests)}>
            {formatUsageNumber(llmUsage.requests)}
          </dd>
        </div>
      </dl>
      <fieldset className="usage-controls">
        <legend>Usage table filters</legend>
        <label className="usage-filter">
          <span>Model</span>
          <select
            aria-label="Filter usage by model"
            value={modelFilter}
            onChange={(event) => setModelFilter(event.target.value)}
          >
            <option value="">All models</option>
            {models.map((model) => (
              <option key={model} value={model}>
                {model}
              </option>
            ))}
          </select>
        </label>
        <label className="usage-filter">
          <span>Agent</span>
          <select
            aria-label="Filter usage by agent"
            value={agentFilter}
            onChange={(event) => setAgentFilter(event.target.value)}
          >
            <option value="">All agents</option>
            {agents.map((agent) => (
              <option key={agent} value={agent}>
                {formatAgentName(agent)}
              </option>
            ))}
          </select>
        </label>
        <label className="usage-filter usage-mobile-sort">
          <span>Sort</span>
          <select
            aria-label="Sort usage"
            value={sortValue(sort)}
            onChange={(event) => selectSort(event.target.value)}
          >
            {sortOptions.map((option) => (
              <option key={sortValue(option)} value={sortValue(option)}>
                {option.label}
              </option>
            ))}
          </select>
        </label>
        <button
          className="usage-button usage-clear-filters"
          type="button"
          disabled={!hasFilters}
          onClick={clearFilters}
        >
          <X size={13} aria-hidden="true" />
          Clear filters
        </button>
      </fieldset>
      {llmUsage.breakdown.length ? (
        <div className="usage-breakdown">
          {rows.length ? (
            <table>
              <caption>
                <span className="usage-table-caption">
                  <span>Usage by model and agent</span>
                  <small>
                    {rows.length} of {llmUsage.breakdown.length} rows
                  </small>
                </span>
              </caption>
              <colgroup>
                <col />
                <col className="usage-breakdown-cost" />
                <col className="usage-breakdown-tokens" />
                <col className="usage-breakdown-calls" />
              </colgroup>
              <thead>
                <tr>
                  <SortableHeader
                    column="modelAgent"
                    label="Model / agent"
                    sort={sort}
                    onSort={sortBy}
                  />
                  <SortableHeader
                    column="estimatedCost"
                    label="Estimated cost"
                    sort={sort}
                    onSort={sortBy}
                  />
                  <SortableHeader
                    column="totalTokens"
                    label="Tokens"
                    sort={sort}
                    onSort={sortBy}
                  />
                  <SortableHeader
                    column="requests"
                    label="Calls"
                    sort={sort}
                    onSort={sortBy}
                  />
                </tr>
              </thead>
              <tbody>
                {rows.map((item) => {
                  const key = breakdownKey(item);
                  const drillable = item.requests > 0;
                  const expanded = selectedBreakdown
                    ? breakdownKey(selectedBreakdown) === key
                    : false;
                  return (
                    <Fragment key={key}>
                      <tr className="usage-summary-row">
                        <th scope="row">
                          {drillable ? (
                            <button
                              className="usage-row-toggle"
                              type="button"
                              aria-expanded={expanded}
                              aria-controls="usage-interaction-details"
                              onClick={() => toggleInteractions(item)}
                            >
                              {expanded ? (
                                <ChevronDown size={13} aria-hidden="true" />
                              ) : (
                                <ChevronRight size={13} aria-hidden="true" />
                              )}
                              <span>
                                <strong title={item.model}>{item.model}</strong>
                                <small>{formatAgentName(item.agent)}</small>
                              </span>
                            </button>
                          ) : (
                            <span className="usage-row-label">
                              <strong title={item.model}>{item.model}</strong>
                              <small>{formatAgentName(item.agent)}</small>
                            </span>
                          )}
                        </th>
                        <td data-label="Estimated cost">
                          {formatEstimatedCost(item.estimatedCost, currency)}
                        </td>
                        <td
                          data-label="Tokens"
                          title={usageNumberTitle(item.totalTokens)}
                        >
                          {formatUsageNumber(item.totalTokens)}
                        </td>
                        <td
                          data-label="Calls"
                          title={usageNumberTitle(item.requests)}
                        >
                          {formatUsageNumber(item.requests)}
                        </td>
                      </tr>
                      {expanded ? (
                        <tr className="usage-interaction-row">
                          <td colSpan={4}>
                            <InteractionDetails
                              item={item}
                              currency={currency}
                              interactions={interactions}
                              error={interactionsError}
                              loading={interactionsLoading}
                              nextCursor={nextCursor}
                              onClose={closeInteractions}
                              onLoadMore={() =>
                                void loadInteractions(item, nextCursor, true)
                              }
                            />
                          </td>
                        </tr>
                      ) : null}
                    </Fragment>
                  );
                })}
              </tbody>
            </table>
          ) : (
            <div className="usage-filter-empty" role="status">
              No usage matches these filters.
            </div>
          )}
        </div>
      ) : null}
    </section>
  );
}

function SortableHeader({
  column,
  label,
  sort,
  onSort,
}: {
  column: SortColumn;
  label: string;
  sort: SortState;
  onSort: (column: SortColumn) => void;
}) {
  const active = sort.column === column;
  const direction = active ? sort.direction : "none";
  const nextDirection = nextSort(sort, column).direction;

  return (
    <th scope="col" aria-sort={direction}>
      <button
        className="usage-sort"
        type="button"
        aria-label={`Sort by ${label}, ${nextDirection}`}
        onClick={() => onSort(column)}
      >
        <span>{label}</span>
        {active ? (
          sort.direction === "ascending" ? (
            <ArrowUp size={12} aria-hidden="true" />
          ) : (
            <ArrowDown size={12} aria-hidden="true" />
          )
        ) : (
          <ArrowUpDown size={12} aria-hidden="true" />
        )}
      </button>
    </th>
  );
}

function InteractionDetails({
  item,
  currency,
  interactions,
  error,
  loading,
  nextCursor,
  onClose,
  onLoadMore,
}: {
  item: LlmUsageBreakdown;
  currency: string;
  interactions: LlmUsageInteraction[];
  error: string | null;
  loading: boolean;
  nextCursor: string | null;
  onClose: () => void;
  onLoadMore: () => void;
}) {
  return (
    <section
      id="usage-interaction-details"
      className="usage-interactions"
      aria-label={`${item.model}, ${formatAgentName(item.agent)} interactions`}
      aria-busy={loading}
    >
      <header>
        <strong>Interactions</strong>
        <button
          className="usage-interactions-close"
          type="button"
          aria-label="Close interaction details"
          onClick={onClose}
        >
          <X size={14} aria-hidden="true" />
        </button>
      </header>
      {error ? (
        <p className="usage-interactions-message" role="alert">
          {error}
        </p>
      ) : interactions.length ? (
        <section
          className="usage-interactions-scroll"
          // biome-ignore lint/a11y/noNoninteractiveTabindex: Horizontal overflow must be keyboard scrollable.
          tabIndex={0}
          aria-label="Interaction metadata table"
        >
          <table>
            <thead>
              <tr>
                <th scope="col">Date / time</th>
                <th scope="col">Duration</th>
                <th scope="col">Status</th>
                <th scope="col">Input</th>
                <th scope="col">Output</th>
                <th scope="col">Total</th>
                <th scope="col">Cost</th>
              </tr>
            </thead>
            <tbody>
              {interactions.map((interaction) => (
                <tr key={interaction.id}>
                  <td title={interaction.startedAt}>
                    <time dateTime={interaction.startedAt}>
                      {formatInteractionTime(interaction.startedAt)}
                    </time>
                    {interaction.responseModel &&
                    interaction.responseModel !== interaction.requestModel ? (
                      <small>Response model: {interaction.responseModel}</small>
                    ) : null}
                  </td>
                  <td>{formatDuration(interaction.durationMs)}</td>
                  <td>
                    <span
                      className={`usage-interaction-status ${interactionStatusTone(interaction)}`}
                    >
                      {formatInteractionStatus(interaction)}
                    </span>
                  </td>
                  <td>{formatOptionalUsageNumber(interaction.inputTokens)}</td>
                  <td>{formatOptionalUsageNumber(interaction.outputTokens)}</td>
                  <td>{formatOptionalUsageNumber(interaction.totalTokens)}</td>
                  <td>
                    {formatOptionalCost(interaction.estimatedCost, currency)}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </section>
      ) : loading ? (
        <p className="usage-interactions-message" role="status">
          Loading interactions…
        </p>
      ) : (
        <p className="usage-interactions-message">No interactions found.</p>
      )}
      {nextCursor ? (
        <button
          className="usage-button usage-interactions-more"
          type="button"
          disabled={loading}
          onClick={onLoadMore}
        >
          {loading ? "Loading…" : "Load older"}
        </button>
      ) : null}
    </section>
  );
}

function breakdownKey(item: LlmUsageBreakdown): string {
  return `${item.model}:${item.agent}`;
}

function formatInteractionTime(value: string): string {
  const date = new Date(value);
  if (!Number.isFinite(date.getTime())) return value;
  return new Intl.DateTimeFormat(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
    second: "2-digit",
  }).format(date);
}

function formatDuration(value: number | null): string {
  if (value === null) return "—";
  if (value < 1000) return `${value} ms`;
  return `${new Intl.NumberFormat("en-US", { maximumFractionDigits: 1 }).format(
    value / 1000,
  )} s`;
}

function formatOptionalUsageNumber(value: number | null): string {
  return value === null ? "—" : formatUsageNumber(value);
}

function formatOptionalCost(value: number | null, currency: string): string {
  return value === null ? "—" : formatEstimatedCost(value, currency);
}

function formatInteractionStatus(interaction: LlmUsageInteraction): string {
  if (interaction.failed) {
    return interaction.httpStatus ? `${interaction.httpStatus} Error` : "Error";
  }
  return interaction.httpStatus?.toString() ?? "Unknown";
}

function interactionStatusTone(interaction: LlmUsageInteraction): string {
  if (interaction.failed || (interaction.httpStatus ?? 0) >= 400)
    return "error";
  return interaction.httpStatus ? "success" : "neutral";
}

function uniqueSorted(values: string[], label = (value: string) => value) {
  return [...new Set(values)].sort((left, right) =>
    label(left).localeCompare(label(right), undefined, { sensitivity: "base" }),
  );
}

function sortValue(sort: SortState): string {
  return `${sort.column}:${sort.direction}`;
}

/** Clicking the active column flips it; a new column starts at its natural order. */
function nextSort(current: SortState, column: SortColumn): SortState {
  if (current.column === column) {
    return {
      column,
      direction:
        current.direction === "ascending" ? "descending" : "ascending",
    };
  }
  return {
    column,
    direction: column === "modelAgent" ? "ascending" : "descending",
  };
}

function sortUsageRows(rows: LlmUsageBreakdown[], sort: SortState) {
  const direction = sort.direction === "ascending" ? 1 : -1;
  return [...rows].sort((left, right) => {
    let comparison = 0;
    switch (sort.column) {
      case "modelAgent":
        comparison =
          left.model.localeCompare(right.model, undefined, {
            sensitivity: "base",
          }) ||
          formatAgentName(left.agent).localeCompare(
            formatAgentName(right.agent),
            undefined,
            { sensitivity: "base" },
          );
        break;
      case "estimatedCost":
        comparison = left.estimatedCost - right.estimatedCost;
        break;
      case "totalTokens":
        comparison = left.totalTokens - right.totalTokens;
        break;
      case "requests":
        comparison = left.requests - right.requests;
        break;
    }
    return (
      comparison * direction ||
      left.model.localeCompare(right.model) ||
      left.agent.localeCompare(right.agent)
    );
  });
}

export function formatUsageNumber(value: number): string {
  if (value >= 1_000_000) {
    return new Intl.NumberFormat("en-US", {
      notation: "compact",
      compactDisplay: "short",
      maximumFractionDigits: 2,
    }).format(value);
  }
  return new Intl.NumberFormat("en-US").format(value);
}

export function usageNumberTitle(value: number): string | undefined {
  return value >= 1_000_000
    ? new Intl.NumberFormat("en-US").format(value)
    : undefined;
}

// Keys are AGW `user_agent.name` values: the product token of each harness's User-Agent.
// Codex reports its entry point as the product token ("originator").
const agentDisplayNames: Record<string, string> = {
  "claude-cli": "Claude Code",
  codex_cli_rs: "Codex",
  codex_exec: "Codex (exec)",
  codex_vscode: "Codex (VS Code)",
  codex_desktop: "Codex (Desktop)",
  opencode: "OpenCode",
  GitHubCopilotChat: "VS Code",
  vscode: "VS Code",
  curl: "curl",
};

export function formatAgentName(value: string): string {
  return (
    agentDisplayNames[value] ??
    value
      .split(/[-_]/)
      .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
      .join(" ")
  );
}

export function formatEstimatedCost(value: number, currency: string): string {
  const fractionDigits = value > 0 && value < 0.01 ? 6 : 2;
  try {
    return new Intl.NumberFormat("en-US", {
      style: "currency",
      currency,
      minimumFractionDigits: 2,
      maximumFractionDigits: fractionDigits,
    }).format(value);
  } catch {
    // Unknown or malformed currency code: fall back to a plain number with the code.
    const amount = new Intl.NumberFormat("en-US", {
      minimumFractionDigits: 2,
      maximumFractionDigits: fractionDigits,
    }).format(value);
    return currency ? `${amount} ${currency}` : amount;
  }
}
