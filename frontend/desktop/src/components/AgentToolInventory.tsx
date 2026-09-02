import { ToolInventory } from "@agentdesktop/ui";
import {
  CircleAlert,
  Info,
  LoaderCircle,
  ShieldAlert,
  ShieldCheck,
} from "lucide-react";

import type {
  AgentAccessReport,
  DiscoveredAgent,
  NetworkRuleChange,
} from "../types";
import { AgentAccessPanel, agentAccessStatus } from "../views/AccessView";

interface AgentToolInventoryProps {
  access?: AgentAccessReport;
  accessLoading: boolean;
  accessStale: boolean;
  activateAccessRequest?: number;
  agent: DiscoveredAgent;
  onApplyNetworkRuleChange?: (change: NetworkRuleChange) => Promise<void>;
  onOpenAccessSource?: (path: string) => void | Promise<void>;
  allowAccessEditing: boolean;
  unavailableDetail?: string;
}

function accessIcon(tone: ReturnType<typeof agentAccessStatus>["tone"]) {
  const properties = { size: 14, "aria-hidden": true as const };
  switch (tone) {
    case "critical":
      return <ShieldAlert {...properties} />;
    case "warning":
      return <CircleAlert {...properties} />;
    case "limited":
      return <Info {...properties} />;
    case "loading":
      return <LoaderCircle className="spin" {...properties} />;
    case "clear":
      return <ShieldCheck {...properties} />;
  }
}

export function AgentToolInventory({
  access,
  accessLoading,
  accessStale,
  activateAccessRequest,
  agent,
  allowAccessEditing,
  onApplyNetworkRuleChange,
  onOpenAccessSource,
  unavailableDetail,
}: AgentToolInventoryProps) {
  const status = agentAccessStatus(access, accessLoading);
  const canEditAccess =
    allowAccessEditing &&
    (agent.kind === "claude-code" || agent.kind === "vscode") &&
    Boolean(onApplyNetworkRuleChange);
  return (
    <ToolInventory
      activateLeadingTabRequest={activateAccessRequest}
      discovery={{
        kind: agent.kind,
        version: agent.version,
        path: agent.executable,
        mcp_servers: agent.mcpServers,
        skills: agent.skills,
      }}
      leadingTab={{
        label: "Access",
        count: status.issueCount,
        icon: accessIcon(status.tone),
        content: (
          <AgentAccessPanel
            agent={access}
            allowAccessEditing={canEditAccess}
            loading={accessLoading}
            onApplyNetworkRuleChange={onApplyNetworkRuleChange}
            onOpenSource={onOpenAccessSource}
            stale={accessStale}
            unavailableDetail={unavailableDetail}
          />
        ),
      }}
    />
  );
}
