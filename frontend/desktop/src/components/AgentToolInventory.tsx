import { ToolInventory } from "@agentdesktop/ui";
import {
  CircleAlert,
  Info,
  LoaderCircle,
  ShieldAlert,
  ShieldCheck,
} from "lucide-react";

import type { AgentAccessReport, DiscoveredAgent } from "../types";
import { AgentAccessPanel, agentAccessStatus } from "../views/AccessView";

interface AgentToolInventoryProps {
  access?: AgentAccessReport;
  accessLoading: boolean;
  accessStale: boolean;
  activateAccessRequest?: number;
  agent: DiscoveredAgent;
  onOpenAccessSource?: (path: string) => void | Promise<void>;
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
  onOpenAccessSource,
  unavailableDetail,
}: AgentToolInventoryProps) {
  const status = agentAccessStatus(access, accessLoading);
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
            loading={accessLoading}
            onOpenSource={onOpenAccessSource}
            stale={accessStale}
            unavailableDetail={unavailableDetail}
          />
        ),
      }}
    />
  );
}
