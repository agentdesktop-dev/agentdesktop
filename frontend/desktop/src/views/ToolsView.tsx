import {
  CardHeader,
  friendlyTool,
  ModelRuntimeInventory,
  ToolIcon,
} from "@agentdesktop/ui";
import {
  AlertCircle,
  Box,
  ChevronRight,
  CircleAlert,
  Cpu,
  Info,
  LoaderCircle,
  ShieldAlert,
  ShieldCheck,
} from "lucide-react";
import { useState } from "react";

import { AgentToolInventory } from "../components/AgentToolInventory";
import type {
  AccessReport,
  AgentAccessReport,
  DiscoveredAgent,
  Discovery,
  NetworkRuleChange,
} from "../types";
import { agentAccessStatus } from "./AccessView";

interface ToolsViewProps {
  accessLoaded: boolean;
  accessLoading: boolean;
  accessReport: AccessReport | null;
  accessStale: boolean;
  discovery: Discovery | null;
  onApplyNetworkRuleChange?: (change: NetworkRuleChange) => Promise<void>;
  onOpenAccessSource?: (path: string) => void | Promise<void>;
  allowAccessEditing: boolean;
  unavailable: boolean;
}

function accessForAgent(
  report: AccessReport | null,
  agent: DiscoveredAgent,
): AgentAccessReport | undefined {
  return (
    report?.agents.find(
      (candidate) =>
        candidate.kind === agent.kind &&
        candidate.executable === agent.executable,
    ) ?? report?.agents.find((candidate) => candidate.kind === agent.kind)
  );
}

function formatAuditTime(timestamp: number): string {
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(timestamp));
}

function AuditOverview({
  agents,
  loaded,
  loading,
  onSelectAgent,
  report,
  stale,
}: {
  agents: DiscoveredAgent[];
  loaded: boolean;
  loading: boolean;
  onSelectAgent: (agentKind: string) => void;
  report: AccessReport | null;
  stale: boolean;
}) {
  const ready = report?.status === "ready";
  const statuses = ready
    ? agents
        .map((agent) => ({
          agent: accessForAgent(report, agent),
          discovery: agent,
        }))
        .map(({ agent, discovery }) => ({
          agent: discovery,
          status: agentAccessStatus(agent),
        }))
    : [];
  const issues = statuses
    .filter(({ status }) => status.issueCount > 0)
    .sort(
      (left, right) =>
        Number(right.status.tone === "critical") -
        Number(left.status.tone === "critical"),
    );
  const limited = statuses.filter(({ status }) => status.limited).length;
  const issueCount = issues.reduce(
    (total, item) => total + item.status.issueCount,
    0,
  );
  const hasCritical = issues.some(({ status }) => status.tone === "critical");
  const tone = !ready
    ? loading || !loaded
      ? "loading"
      : "limited"
    : issueCount
      ? hasCritical
        ? "critical"
        : "warning"
      : limited
        ? "limited"
        : "clear";
  const iconProperties = { size: 18, "aria-hidden": true as const };
  const icon =
    tone === "critical" ? (
      <ShieldAlert {...iconProperties} />
    ) : tone === "warning" ? (
      <CircleAlert {...iconProperties} />
    ) : tone === "clear" ? (
      <ShieldCheck {...iconProperties} />
    ) : tone === "loading" ? (
      <LoaderCircle className="spin" {...iconProperties} />
    ) : (
      <Info {...iconProperties} />
    );
  const title = !ready
    ? loading || !loaded
      ? "Checking local access"
      : "Local access audit unavailable"
    : issueCount
      ? `${issueCount} access ${issueCount === 1 ? "issue needs" : "issues need"} review`
      : limited
        ? "No risks found in checked sources"
        : "No current access risks found";
  const detail = !ready
    ? loading || !loaded
      ? "Inventory is ready while settings and session evidence are checked."
      : (report?.detail ?? "Use Refresh to retry the local audit.")
    : issueCount
      ? null
      : limited
        ? `${limited} ${limited === 1 ? "agent has" : "agents have"} incomplete or unsupported evidence.`
        : "Supported evidence sources show no current configuration risks.";

  return (
    <section className={`tools-audit-overview ${tone}`}>
      <div className="tools-audit-heading">
        {icon}
        <div>
          <strong>{title}</strong>
          {detail ? <span>{detail}</span> : null}
        </div>
        {ready ? (
          <small className={stale ? "stale" : undefined}>
            {stale ? "Last successful check" : "Checked"}{" "}
            {formatAuditTime(report.generatedAtUnixMs)}
          </small>
        ) : null}
      </div>
      {issues.length ? (
        <div className="tools-audit-queue">
          {issues.map(({ agent, status }) => (
            <button
              key={`${agent.kind}-${agent.executable}`}
              onClick={() => onSelectAgent(agent.kind)}
              type="button"
            >
              <ToolIcon kind={agent.kind} />
              <span>
                <strong>{friendlyTool(agent.kind)}</strong>
                <small>
                  {status.tone === "critical"
                    ? `${status.issueCount} critical ${status.issueCount === 1 ? "issue" : "issues"}`
                    : `${status.issueCount} access ${status.issueCount === 1 ? "issue" : "issues"}`}
                </small>
              </span>
              <ChevronRight size={14} aria-hidden="true" />
            </button>
          ))}
        </div>
      ) : null}
    </section>
  );
}

export function ToolsView({
  accessLoaded,
  accessLoading,
  accessReport,
  accessStale,
  allowAccessEditing,
  discovery,
  onApplyNetworkRuleChange,
  onOpenAccessSource,
  unavailable,
}: ToolsViewProps) {
  const agents = discovery?.agents ?? [];
  const modelRuntimes = discovery?.modelRuntimes ?? [];
  const modelCount = modelRuntimes.reduce(
    (total, runtime) => total + runtime.models.length,
    0,
  );
  const mcpCount = agents.reduce(
    (total, agent) => total + (agent.mcpServers?.length ?? 0),
    0,
  );
  const skillCount = agents.reduce(
    (total, agent) => total + (agent.skills?.length ?? 0),
    0,
  );
  const [accessTarget, setAccessTarget] = useState({ kind: "", request: 0 });
  function selectAgentAccess(kind: string) {
    setAccessTarget((current) => ({ kind, request: current.request + 1 }));
  }
  return (
    <div className="page-stack">
      <div className="page-heading">
        <div>
          <h2>Local tools</h2>
          <p>
            Review each agent’s access, MCP servers, and skills in one place.
          </p>
        </div>
      </div>
      {!unavailable && agents.length ? (
        <AuditOverview
          agents={agents}
          loaded={accessLoaded}
          loading={accessLoading}
          onSelectAgent={selectAgentAccess}
          report={accessReport}
          stale={accessStale}
        />
      ) : null}
      <section className="card table-card">
        <CardHeader
          title="Agents"
          description={
            unavailable
              ? "Inventory unavailable"
              : `${agents.length} agent${agents.length === 1 ? "" : "s"} · ${mcpCount} MCP · ${skillCount} skills`
          }
        />
        {unavailable ? (
          <div className="empty-inline">
            <AlertCircle size={20} />
            <div>
              <strong>Tool inventory is unavailable</strong>
              <span>Use Refresh to try again.</span>
            </div>
          </div>
        ) : agents.length ? (
          <div className="tool-inventory">
            {agents.map((agent) => (
              <AgentToolInventory
                access={accessForAgent(accessReport, agent)}
                accessLoading={accessLoading && !accessReport}
                accessStale={accessStale}
                activateAccessRequest={
                  accessTarget.kind === agent.kind
                    ? accessTarget.request
                    : undefined
                }
                agent={agent}
                allowAccessEditing={allowAccessEditing}
                key={`${agent.kind}-${agent.executable}`}
                onApplyNetworkRuleChange={onApplyNetworkRuleChange}
                onOpenAccessSource={onOpenAccessSource}
                unavailableDetail={accessReport?.detail}
              />
            ))}
          </div>
        ) : (
          <div className="empty-inline">
            <Box size={20} />
            <div>
              <strong>No supported tools found</strong>
              <span>
                Agent Desktop can inventory VS Code, Claude Code, Claude
                Desktop, Codex, and OpenCode.
              </span>
            </div>
          </div>
        )}
      </section>
      {!unavailable ? (
        <section className="card table-card">
          <CardHeader
            title="Local models"
            description={`${modelCount} model${modelCount === 1 ? "" : "s"} across ${modelRuntimes.length} runtime${modelRuntimes.length === 1 ? "" : "s"}`}
          />
          {modelRuntimes.length ? (
            <div className="model-runtime-inventory">
              {modelRuntimes.map((runtime) => (
                <ModelRuntimeInventory key={runtime.kind} runtime={runtime} />
              ))}
            </div>
          ) : (
            <div className="empty-inline">
              <Cpu size={20} />
              <div>
                <strong>No local models found</strong>
                <span>Start Ollama before restarting Agent Desktop.</span>
              </div>
            </div>
          )}
        </section>
      ) : null}
    </div>
  );
}
