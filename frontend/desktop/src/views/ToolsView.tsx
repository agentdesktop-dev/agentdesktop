import { CardHeader, ToolInventory } from "@agentdesktop/ui";
import { AlertCircle, Box } from "lucide-react";

import type { Discovery } from "../types";

export interface ToolsViewProps {
  discovery: Discovery | null;
  unavailable: boolean;
}

export function ToolsView({ discovery, unavailable }: ToolsViewProps) {
  const agents = discovery?.agents ?? [];
  const mcpCount = agents.reduce(
    (total, agent) => total + (agent.mcpServers?.length ?? 0),
    0,
  );
  const skillCount = agents.reduce(
    (total, agent) => total + (agent.skills?.length ?? 0),
    0,
  );
  return (
    <div className="page-stack">
      <div className="page-heading">
        <div>
          <h2>Discovered tools</h2>
          <p>
            Developer tools and capabilities found locally by the Agent Desktop
            daemon.
          </p>
        </div>
      </div>
      {agents.length ? (
        <div className="stat-grid">
          <div className="stat-card">
            <strong>{agents.length}</strong>
            <span>Developer tools</span>
          </div>
          <div className="stat-card">
            <strong>{mcpCount}</strong>
            <span>MCP servers</span>
          </div>
          <div className="stat-card">
            <strong>{skillCount}</strong>
            <span>Skills</span>
          </div>
        </div>
      ) : null}
      <section className="card table-card">
        <CardHeader
          title="Local inventory"
          description={
            unavailable
              ? "Inventory unavailable"
              : `${agents.length} installation${agents.length === 1 ? "" : "s"} discovered`
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
              <ToolInventory
                key={`${agent.kind}-${agent.executable}`}
                discovery={{
                  kind: agent.kind,
                  version: agent.version,
                  path: agent.executable,
                  mcp_servers: agent.mcpServers,
                  skills: agent.skills,
                }}
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
    </div>
  );
}
