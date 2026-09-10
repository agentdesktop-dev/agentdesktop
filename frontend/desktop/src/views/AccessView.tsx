import {
  AlertCircle,
  Check,
  ChevronDown,
  ChevronRight,
  CircleAlert,
  ExternalLink,
  Filter,
  FolderLock,
  Globe2,
  Info,
  LoaderCircle,
  ShieldAlert,
  TerminalSquare,
} from "lucide-react";
import {
  type Dispatch,
  type ReactNode,
  type SetStateAction,
  useState,
} from "react";

import type {
  AccessCapability,
  AccessCategory,
  AccessDecision,
  AccessFinding,
  AccessObservation,
  AccessOperation,
  AgentAccessReport,
  NetworkRuleChange,
} from "../types";
import { AgentAccessEditor } from "./PolicyView";

type ReviewFinding = AccessFinding & { severity: "warning" | "critical" };

type UnifiedAccessCategory = "network" | "execution" | "filesystem";
type EvidenceOrigin = "configured" | "default" | "session";

const evidenceOrigins: EvidenceOrigin[] = ["default", "configured", "session"];
const originLabels: Record<EvidenceOrigin, string> = {
  default: "Default",
  configured: "Configured",
  session: "Session",
};
const originOrder: Record<EvidenceOrigin, number> = {
  default: 0,
  configured: 1,
  session: 2,
};

const categoryNames: Record<UnifiedAccessCategory, string> = {
  filesystem: "Filesystem",
  network: "Network",
  execution: "Commands",
};

const DEFAULT_EXPANDED_RESOURCE_LIMIT = 4;
const categoryOrder: UnifiedAccessCategory[] = [
  "network",
  "execution",
  "filesystem",
];
function unifiedCategory(
  category: AccessCategory,
): UnifiedAccessCategory | null {
  switch (category) {
    case "network":
    case "browser":
      return "network";
    case "execution":
    case "credential":
      return "execution";
    case "filesystem":
      return "filesystem";
    case "externalService":
      return null;
  }
}

function isMcpCapability(capability: AccessCapability): boolean {
  return (
    capability.source.kind === "mcp" ||
    capability.category === "externalService" ||
    capability.resource.startsWith("mcp:") ||
    capability.detail?.includes("MCP server") === true
  );
}

function capabilityOrigin(capability: AccessCapability): EvidenceOrigin | null {
  switch (capability.source.kind) {
    case "configuration":
      return "configured";
    case "default":
      return "default";
    case "history":
      return "session";
    case "mcp":
      return null;
  }
}

function isBroadGrant(capability: AccessCapability): boolean {
  return (
    capability.category === "network" &&
    capability.decision === "allow" &&
    (capability.resource === "*" || capability.resource.startsWith("*."))
  );
}

function isWildcardNetworkRule(capability: AccessCapability): boolean {
  return isBroadGrant(capability) && capability.resource.startsWith("*.");
}

function networkRiskLabel(capability: AccessCapability): string | null {
  if (!isBroadGrant(capability)) return null;
  return capability.resource === "*" ? "All destinations" : "Wildcard rule";
}

function categoryIcon(category: UnifiedAccessCategory): ReactNode {
  const properties = { size: 15, "aria-hidden": true as const };
  switch (category) {
    case "filesystem":
      return <FolderLock {...properties} />;
    case "network":
      return <Globe2 {...properties} />;
    case "execution":
      return <TerminalSquare {...properties} />;
  }
}

function isReviewFinding(finding: AccessFinding): finding is ReviewFinding {
  return finding.severity !== "notice";
}

function severityIcon(severity: ReviewFinding["severity"]) {
  if (severity === "critical") return <ShieldAlert size={17} />;
  return <CircleAlert size={17} />;
}

function formatObservedDate(timestamp?: number): string | null {
  if (!timestamp) return null;
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
  }).format(new Date(timestamp));
}

function capabilityDescription(capability: AccessCapability): string {
  const hasRead = capability.operations.includes("read");
  const hasWrite = capability.operations.includes("write");
  const action =
    capability.category === "filesystem"
      ? hasRead && hasWrite
        ? "read and write"
        : hasWrite
          ? "write"
          : "read"
      : capability.category === "execution"
        ? "run"
        : capability.category === "network" || capability.category === "browser"
          ? capability.operations.includes("connect")
            ? "connect"
            : "use"
          : "use";

  switch (capability.decision) {
    case "allow":
      if (capability.category === "credential") {
        return "Available to the agent environment";
      }
      return `Can ${action} without asking`;
    case "ask":
      return {
        "read and write": "Asks before reading or writing",
        write: "Asks before writing",
        read: "Asks before reading",
        run: "Asks before running",
        connect: "Asks before connecting",
        use: "Asks before use",
      }[action];
    case "deny":
      return "Blocked";
    case "autoReview":
      return "Checked automatically before use";
    case "unknown":
      return "Depends on the session settings";
  }
}

const operationOrder: AccessOperation[] = [
  "read",
  "write",
  "execute",
  "connect",
  "use",
];

const operationLabels: Record<AccessOperation, string> = {
  read: "Read",
  write: "Write",
  execute: "Run",
  connect: "Connect",
  use: "Use",
};

const decisionLabels: Record<AccessDecision, string> = {
  allow: "allowed",
  ask: "requires approval",
  deny: "blocked",
  autoReview: "reviewed automatically",
  unknown: "depends on session",
};

function groupedCapabilityDescription(
  capabilities: AccessCapability[],
): string {
  const [representative] = capabilities;
  if (!representative) return "";
  if (capabilities.length === 1) return capabilityDescription(representative);

  const decisions = new Set(
    capabilities.map((capability) => capability.decision),
  );
  const operations = operationOrder.filter((operation) =>
    capabilities.some((capability) =>
      capability.operations.includes(operation),
    ),
  );
  if (decisions.size === 1) {
    return capabilityDescription({ ...representative, operations });
  }

  return operations
    .map((operation) => {
      const capability = capabilities.find((candidate) =>
        candidate.operations.includes(operation),
      );
      return capability
        ? `${operationLabels[operation]} ${decisionLabels[capability.decision]}`
        : null;
    })
    .filter((description): description is string => Boolean(description))
    .join(" · ");
}

function enforcementDescription(capability: AccessCapability): string | null {
  switch (capability.enforcement) {
    case "sandbox":
      return "Sandbox enforced";
    case "none":
      return capability.category === "execution" &&
        capability.decision === "allow"
        ? "No sandbox boundary"
        : null;
    case "unknown":
      return "Isolation depends on the session";
    case "harness":
      return null;
  }
}

function sourceFileName(path?: string): string {
  return path?.split(/[\\/]/).pop() ?? "Settings source";
}

function hasCoverageLimits(agent: AgentAccessReport): boolean {
  return (agent.coverage ?? []).some(
    (coverage) => coverage.status !== "complete",
  );
}

type AgentAccessTone = "critical" | "warning" | "limited" | "clear" | "loading";

interface AgentAccessStatus {
  tone: AgentAccessTone;
  issueCount: number;
  limited: boolean;
}

export function agentAccessStatus(
  agent: AgentAccessReport | undefined,
  loading = false,
): AgentAccessStatus {
  if (loading && !agent) {
    return {
      tone: "loading",
      issueCount: 0,
      limited: false,
    };
  }
  if (!agent) {
    return {
      tone: "limited",
      issueCount: 0,
      limited: true,
    };
  }
  const findings = (agent.findings ?? []).filter(isReviewFinding);
  const critical = findings.filter(
    (finding) => finding.severity === "critical",
  ).length;
  const limited = hasCoverageLimits(agent);
  return {
    tone: critical
      ? "critical"
      : findings.length
        ? "warning"
        : limited
          ? "limited"
          : "clear",
    issueCount: findings.length,
    limited,
  };
}

function FindingRow({ finding }: { finding: ReviewFinding }) {
  return (
    <div className={`access-finding access-finding-${finding.severity}`}>
      <span className="access-finding-icon">
        {severityIcon(finding.severity)}
      </span>
      <div>
        <strong>{finding.title}</strong>
        <span>{finding.detail}</span>
        {finding.workspace ? <code>{finding.workspace}</code> : null}
      </div>
    </div>
  );
}

function OriginBadge({
  origin,
  sourcePath,
}: {
  origin: EvidenceOrigin;
  sourcePath?: string;
}) {
  return (
    <span
      className={`access-origin-badge access-origin-badge-${origin}`}
      title={
        origin === "configured" && sourcePath
          ? `Configured in ${sourceFileName(sourcePath)}`
          : originLabels[origin]
      }
    >
      {originLabels[origin]}
    </span>
  );
}

function ConfigurationControl({
  openedSource,
  openingSource,
  onOpenSource,
  paths,
}: {
  openedSource: string | null;
  openingSource: string | null;
  onOpenSource?: (path: string) => void | Promise<void>;
  paths: string[];
}) {
  if (!paths.length || !onOpenSource) return null;
  if (paths.length === 1) {
    const [path] = paths;
    return (
      <button
        className="access-configuration-link"
        disabled={openingSource !== null}
        onClick={() => onOpenSource(path)}
        title={path}
        type="button"
      >
        {openingSource === path ? (
          <LoaderCircle className="spin" size={13} aria-hidden="true" />
        ) : openedSource === path ? (
          <Check size={13} aria-hidden="true" />
        ) : (
          <ExternalLink size={13} aria-hidden="true" />
        )}
        Open configuration
      </button>
    );
  }
  return (
    <details className="access-configurations">
      <summary>
        <ExternalLink size={13} aria-hidden="true" />
        <span>Configurations</span>
        <ChevronDown size={13} aria-hidden="true" />
      </summary>
      <div>
        {paths.map((path) => (
          <button
            disabled={openingSource !== null}
            key={path}
            onClick={() => onOpenSource(path)}
            title={path}
            type="button"
          >
            {openingSource === path ? (
              <LoaderCircle className="spin" size={13} aria-hidden="true" />
            ) : openedSource === path ? (
              <Check size={13} aria-hidden="true" />
            ) : (
              <ExternalLink size={13} aria-hidden="true" />
            )}
            <span>{sourceFileName(path)}</span>
          </button>
        ))}
      </div>
    </details>
  );
}

function configurationPaths(
  capabilities: AccessCapability[],
  findings: AccessFinding[],
): string[] {
  const paths = new Set<string>();
  for (const source of [
    ...capabilities.map((capability) => capability.source),
    ...findings.map((finding) => finding.source),
  ]) {
    if (source?.kind === "configuration" && source.path) {
      paths.add(source.path);
    }
  }
  return [...paths].sort();
}

function SourceFilter({
  setVisibleOrigins,
  visibleOrigins,
}: {
  setVisibleOrigins: Dispatch<SetStateAction<Record<EvidenceOrigin, boolean>>>;
  visibleOrigins: Record<EvidenceOrigin, boolean>;
}) {
  const selectedOrigins = evidenceOrigins.filter(
    (origin) => visibleOrigins[origin],
  ).length;
  const filterLabel =
    selectedOrigins === evidenceOrigins.length
      ? "All sources"
      : `${selectedOrigins} of ${evidenceOrigins.length} sources`;
  return (
    <details className="access-filter">
      <summary aria-label={`Filter access sources: ${filterLabel}`}>
        <Filter size={13} aria-hidden="true" />
        <span>{filterLabel}</span>
        <ChevronDown size={13} aria-hidden="true" />
      </summary>
      <fieldset>
        <legend>Show sources</legend>
        {evidenceOrigins.map((origin) => (
          <label key={origin}>
            <input
              checked={visibleOrigins[origin]}
              onChange={(event) =>
                setVisibleOrigins((current) => ({
                  ...current,
                  [origin]: event.target.checked,
                }))
              }
              type="checkbox"
            />
            <OriginBadge origin={origin} />
          </label>
        ))}
      </fieldset>
    </details>
  );
}

function ReadOnlyDetail({ detail }: { detail?: string | null }) {
  if (!detail) return null;
  const advancedPrefix = "Advanced:";
  const advanced = detail.startsWith(advancedPrefix);
  return (
    <small className="access-read-only-detail">
      {advanced ? (
        <span className="access-advanced-badge">Advanced</span>
      ) : null}
      <span>
        {advanced ? detail.slice(advancedPrefix.length).trim() : detail}
      </span>
    </small>
  );
}

function readOnlyAccessDescription(
  detail: string | null | undefined,
  fallback: string,
): string {
  return detail?.startsWith("Advanced:")
    ? "Custom request and response approvals"
    : fallback;
}

function CapabilityRow({ capabilities }: { capabilities: AccessCapability[] }) {
  const [representative] = capabilities;
  if (!representative) return null;
  const origin = capabilityOrigin(representative);
  if (!origin) return null;
  const broadGrant = capabilities.some(isBroadGrant);
  const riskLabel = capabilities.map(networkRiskLabel).find(Boolean) ?? null;
  const enforcement = enforcementDescription(representative);
  const sourcePath =
    origin === "configured" ? representative.source.path : undefined;
  const readOnlyDetail =
    origin === "configured" &&
    representative.category === "network" &&
    !representative.rule
      ? representative.detail
      : null;
  return (
    <div
      className={`access-setting-row${broadGrant ? " access-setting-row-risk" : ""}`}
    >
      <div className="access-resource-main">
        <code title={representative.resource}>{representative.resource}</code>
        <span>
          {readOnlyAccessDescription(
            readOnlyDetail,
            groupedCapabilityDescription(capabilities),
          )}
        </span>
        {enforcement ? <small>{enforcement}</small> : null}
        <ReadOnlyDetail detail={readOnlyDetail} />
      </div>
      <div className="access-row-meta">
        {riskLabel ? <span className="access-risk">{riskLabel}</span> : null}
        <OriginBadge origin={origin} sourcePath={sourcePath} />
      </div>
    </div>
  );
}

interface WildcardRuleGroup {
  key: string;
  origin: EvidenceOrigin;
  sourcePath?: string;
  capabilities: AccessCapability[];
}

interface CapabilityRowGroup {
  key: string;
  capabilities: AccessCapability[];
  origin: EvidenceOrigin;
}

function capabilityRowGroups(
  capabilities: AccessCapability[],
): CapabilityRowGroup[] {
  const groupsByKey = new Map<string, CapabilityRowGroup[]>();
  for (const capability of capabilities) {
    const origin = capabilityOrigin(capability);
    if (!origin) continue;
    const baseKey = JSON.stringify([
      capability.category,
      capability.resource,
      origin,
      capability.source.path ?? null,
      capability.workspace ?? null,
      capability.enforcement,
    ]);
    const groups = groupsByKey.get(baseKey) ?? [];
    const group =
      capability.operations.length > 0
        ? groups.find((candidate) =>
            candidate.capabilities.every((existing) =>
              existing.operations.every(
                (operation) => !capability.operations.includes(operation),
              ),
            ),
          )
        : undefined;
    if (group) {
      group.capabilities.push(capability);
    } else {
      groups.push({
        key: `${baseKey}:${groups.length}`,
        capabilities: [capability],
        origin,
      });
      groupsByKey.set(baseKey, groups);
    }
  }
  return [...groupsByKey.values()].flat();
}

function wildcardRuleGroups(
  capabilities: AccessCapability[],
): WildcardRuleGroup[] {
  const groups = new Map<string, AccessCapability[]>();
  for (const capability of capabilities) {
    if (!isWildcardNetworkRule(capability)) continue;
    const key = JSON.stringify([
      capabilityOrigin(capability),
      capability.source.path ?? null,
      capability.decision,
      capability.enforcement,
      [...capability.operations].sort(),
      capability.workspace ?? null,
    ]);
    const group = groups.get(key) ?? [];
    group.push(capability);
    groups.set(key, group);
  }
  return [...groups.entries()].flatMap(([key, group]) => {
    const origin = capabilityOrigin(group[0]);
    return group.length > 1 && origin
      ? [
          {
            key,
            origin,
            sourcePath:
              origin === "configured" ? group[0]?.source.path : undefined,
            capabilities: group,
          },
        ]
      : [];
  });
}

function WildcardRules({
  capabilities,
  origin,
  sourcePath,
}: WildcardRuleGroup) {
  const [representative] = capabilities;
  if (!representative) return null;
  const enforcement = enforcementDescription(representative);
  const readOnlyDetail =
    origin === "configured" &&
    capabilities.every((capability) => !capability.rule)
      ? representative.detail
      : null;
  return (
    <div className="access-wildcard-group access-setting-row-risk">
      <div className="access-wildcard-summary">
        <div className="access-resource-main">
          <strong>{capabilities.length} wildcard domains</strong>
          <span>
            {readOnlyAccessDescription(
              readOnlyDetail,
              capabilityDescription(representative),
            )}
          </span>
          {enforcement ? <small>{enforcement}</small> : null}
          <ReadOnlyDetail detail={readOnlyDetail} />
        </div>
        <div className="access-row-meta">
          <OriginBadge origin={origin} sourcePath={sourcePath} />
        </div>
      </div>
      <ul aria-label="Wildcard domains" className="access-domain-list">
        {capabilities.map((capability, index) => (
          <li key={`${capability.resource}-${index}`}>
            <code title={capability.resource}>{capability.resource}</code>
          </li>
        ))}
      </ul>
    </div>
  );
}

function CategoryResourceList({
  capabilities,
  observations,
}: {
  capabilities: AccessCapability[];
  observations: AccessObservation[];
}) {
  const wildcardGroups = wildcardRuleGroups(capabilities);
  const groupedCapabilities = new Set(
    wildcardGroups.flatMap((wildcardGroup) => wildcardGroup.capabilities),
  );
  const capabilityGroups = capabilityRowGroups(
    capabilities.filter((capability) => !groupedCapabilities.has(capability)),
  );
  const items = [
    ...wildcardGroups.map((group) => ({
      kind: "wildcard" as const,
      origin: group.origin,
      sortKey: group.capabilities[0]?.resource ?? group.key,
      group,
    })),
    ...capabilityGroups.map((group) => ({
      kind: "capability" as const,
      origin: group.origin,
      sortKey: `${group.capabilities[0]?.resource}:${group.key}`,
      group,
    })),
    ...observations.map((observation, index) => ({
      kind: "observation" as const,
      origin: "session" as const,
      sortKey: `${observation.resource}:${index}`,
      observation,
    })),
  ].sort(
    (left, right) =>
      originOrder[left.origin] - originOrder[right.origin] ||
      left.sortKey.localeCompare(right.sortKey),
  );
  return (
    <div className="access-resource-list">
      {items.map((item) => {
        switch (item.kind) {
          case "wildcard":
            return (
              <WildcardRules
                {...item.group}
                key={`wildcard:${item.group.key}`}
              />
            );
          case "capability":
            return (
              <CapabilityRow
                capabilities={item.group.capabilities}
                key={`capability:${item.sortKey}`}
              />
            );
          case "observation":
            return (
              <ObservationRow
                key={`observation:${item.sortKey}`}
                observation={item.observation}
              />
            );
        }
        return null;
      })}
    </div>
  );
}

function capabilityRowCount(capabilities: AccessCapability[]): number {
  const wildcardGroups = wildcardRuleGroups(capabilities);
  const wildcardCapabilities = new Set(
    wildcardGroups.flatMap((group) => group.capabilities),
  );
  return (
    wildcardGroups.length +
    capabilityRowGroups(
      capabilities.filter(
        (capability) => !wildcardCapabilities.has(capability),
      ),
    ).length
  );
}

function categoryCapabilityCount(
  category: UnifiedAccessCategory,
  capabilities: AccessCapability[],
): number {
  return category === "network"
    ? capabilities.length
    : capabilityRowCount(capabilities);
}

function categoryCountLabel(
  category: UnifiedAccessCategory,
  count: number,
  total = count,
): string {
  const noun = category === "filesystem" ? "path" : "rule";
  const amount = count === total ? `${count}` : `${count} of ${total}`;
  return `${amount} ${noun}${total === 1 ? "" : "s"}`;
}

function ObservationRow({ observation }: { observation: AccessObservation }) {
  const observed = formatObservedDate(observation.evidenceUpdatedAtUnixMs);
  const operation = observation.operations.includes("read")
    ? observation.operations.includes("write")
      ? "read/write accesses"
      : "reads"
    : observation.operations.includes("write")
      ? "writes"
      : observation.operations.includes("execute")
        ? "runs"
        : observation.operations.includes("connect")
          ? "connections"
          : "uses";
  return (
    <div className="access-resource-row">
      <div className="access-resource-main">
        <code title={observation.resource}>{observation.resource}</code>
        <span>
          {observation.count} {operation} across {observation.sessionCount}{" "}
          {observation.sessionCount === 1 ? "session" : "sessions"}
          {observation.resourceCount > 1
            ? ` · ${observation.resourceCount} paths`
            : ""}
          {observation.workspaceCount > 1
            ? ` · ${observation.workspaceCount} workspaces`
            : ""}
          {observed ? ` · latest ${observed}` : ""}
        </span>
        {observation.workspace &&
        observation.workspace !== observation.resource ? (
          <small title={observation.workspace}>{observation.workspace}</small>
        ) : null}
      </div>
      <div className="access-row-meta">
        <OriginBadge origin="session" />
      </div>
    </div>
  );
}

function UnifiedCategory({
  alwaysVisibleCount = 0,
  category,
  capabilities,
  editor,
  observations,
  visibleOrigins,
}: {
  alwaysVisibleCount?: number;
  category: UnifiedAccessCategory;
  capabilities: AccessCapability[];
  editor?: ReactNode;
  observations: AccessObservation[];
  visibleOrigins: Record<EvidenceOrigin, boolean>;
}) {
  const visibleCapabilities = capabilities.filter((capability) => {
    const origin = capabilityOrigin(capability);
    return origin ? visibleOrigins[origin] : false;
  });
  const visibleObservations = visibleOrigins.session ? observations : [];
  const count =
    alwaysVisibleCount +
    categoryCapabilityCount(category, visibleCapabilities) +
    visibleObservations.length;
  const totalCount =
    alwaysVisibleCount +
    categoryCapabilityCount(category, capabilities) +
    observations.length;
  const [expanded, setExpanded] = useState(
    Boolean(editor) ||
      (category !== "execution" &&
        count > 0 &&
        count <= DEFAULT_EXPANDED_RESOURCE_LIMIT),
  );
  return (
    <details
      className="access-category-group access-category-disclosure"
      onToggle={(event) => setExpanded(event.currentTarget.open)}
      open={expanded}
    >
      <summary className="access-category-heading">
        <span className="access-category-title">
          {categoryIcon(category)}
          {categoryNames[category]}
        </span>
        <small className="access-category-count">
          {categoryCountLabel(category, count, totalCount)}
        </small>
        <ChevronRight
          className="access-category-disclosure-icon"
          size={13}
          aria-hidden="true"
        />
      </summary>
      {expanded ? (
        <div className="access-category-body">
          {editor}
          {visibleCapabilities.length || visibleObservations.length ? (
            <>
              {editor ? (
                <div className="access-network-evidence-heading">
                  <strong>Other network access</strong>
                  <span>Read-only settings, defaults, and recent activity</span>
                </div>
              ) : null}
              <CategoryResourceList
                capabilities={visibleCapabilities}
                observations={visibleObservations}
              />
            </>
          ) : editor ? null : (
            <p className="access-category-empty">No matching access.</p>
          )}
        </div>
      ) : null}
    </details>
  );
}

function UnifiedAccess({
  capabilities,
  configurationSources,
  networkEditor,
  observations,
  openingSource,
  onOpenSource,
  openedSource,
}: {
  capabilities: AccessCapability[];
  configurationSources: string[];
  networkEditor?: ReactNode;
  observations: AccessObservation[];
  openingSource: string | null;
  onOpenSource?: (path: string) => void | Promise<void>;
  openedSource: string | null;
}) {
  const [visibleOrigins, setVisibleOrigins] = useState<
    Record<EvidenceOrigin, boolean>
  >({ default: true, configured: true, session: true });
  return (
    <>
      <div className="access-toolbar">
        <ConfigurationControl
          openedSource={openedSource}
          openingSource={openingSource}
          onOpenSource={onOpenSource}
          paths={configurationSources}
        />
        <SourceFilter
          setVisibleOrigins={setVisibleOrigins}
          visibleOrigins={visibleOrigins}
        />
      </div>
      <div className="access-category-stack">
        {categoryOrder.map((category) => {
          const categoryCapabilities = capabilities.filter(
            (capability) => unifiedCategory(capability.category) === category,
          );
          const editableCapabilities =
            category === "network" && networkEditor
              ? categoryCapabilities.filter((capability) => capability.rule)
              : [];
          const evidenceCapabilities = editableCapabilities.length
            ? categoryCapabilities.filter((capability) => !capability.rule)
            : categoryCapabilities;
          return (
            <UnifiedCategory
              alwaysVisibleCount={editableCapabilities.length}
              capabilities={evidenceCapabilities}
              category={category}
              editor={category === "network" ? networkEditor : undefined}
              key={category}
              observations={observations.filter(
                (observation) =>
                  unifiedCategory(observation.category) === category,
              )}
              visibleOrigins={visibleOrigins}
            />
          );
        })}
      </div>
    </>
  );
}

interface AgentAccessPanelProps {
  agent?: AgentAccessReport;
  allowAccessEditing?: boolean;
  loading?: boolean;
  stale?: boolean;
  unavailableDetail?: string;
  onApplyNetworkRuleChange?: (change: NetworkRuleChange) => Promise<void>;
  onOpenSource?: (path: string) => void | Promise<void>;
}

export function AgentAccessPanel({
  agent,
  allowAccessEditing = false,
  loading = false,
  stale = false,
  unavailableDetail,
  onApplyNetworkRuleChange,
  onOpenSource,
}: AgentAccessPanelProps) {
  const [openingSource, setOpeningSource] = useState<string | null>(null);
  const [openedSource, setOpenedSource] = useState<string | null>(null);

  async function openSource(path: string) {
    if (!onOpenSource) return;
    setOpeningSource(path);
    try {
      await onOpenSource(path);
      setOpenedSource(path);
    } catch {
      setOpenedSource(null);
    } finally {
      setOpeningSource(null);
    }
  }

  if (loading && !agent) {
    return (
      <div className="agent-access-state-panel" role="status">
        <LoaderCircle className="spin" size={17} aria-hidden="true" />
        <div>
          <strong>Checking local access</strong>
          <span>Reading redacted settings and session evidence…</span>
        </div>
      </div>
    );
  }
  if (!agent) {
    return (
      <div className="agent-access-state-panel">
        <AlertCircle size={17} aria-hidden="true" />
        <div>
          <strong>Access audit unavailable</strong>
          <span>
            {unavailableDetail ?? "Refresh to retry the local assessment."}
          </span>
        </div>
      </div>
    );
  }

  const capabilities = agent.capabilities ?? [];
  const visibleCapabilities = capabilities.filter(
    (capability) =>
      !isMcpCapability(capability) &&
      unifiedCategory(capability.category) !== null,
  );
  const observations = (agent.observations ?? []).filter(
    (observation) => unifiedCategory(observation.category) !== null,
  );
  const reviewFindings = (agent.findings ?? [])
    .filter(isReviewFinding)
    .sort((left, right) =>
      left.severity === right.severity
        ? 0
        : left.severity === "critical"
          ? -1
          : 1,
    );
  const configurationSources = configurationPaths(
    capabilities,
    agent.findings ?? [],
  );

  return (
    <div className="agent-access-panel">
      {stale ? (
        <div className="agent-access-stale">
          <Info size={15} aria-hidden="true" />
          Showing the last successful audit because refresh failed.
        </div>
      ) : null}
      {reviewFindings.length ? (
        <section className="access-section access-review">
          <h3>Review</h3>
          <div className="access-findings">
            {reviewFindings.map((finding) => (
              <FindingRow
                finding={finding}
                key={`${finding.title}-${finding.detail}`}
              />
            ))}
          </div>
        </section>
      ) : null}
      <section className="access-section">
        <UnifiedAccess
          capabilities={visibleCapabilities}
          configurationSources={configurationSources}
          networkEditor={
            allowAccessEditing ? (
              <AgentAccessEditor
                agent={agent}
                onApplyNetworkRuleChange={onApplyNetworkRuleChange}
              />
            ) : undefined
          }
          observations={observations}
          onOpenSource={onOpenSource ? openSource : undefined}
          openedSource={openedSource}
          openingSource={openingSource}
        />
      </section>
    </div>
  );
}
