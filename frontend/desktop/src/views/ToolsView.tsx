import {
  CardHeader,
  friendlyBrowser,
  ModelRuntimeInventory,
  ToolInventory,
} from "@agentdesktop/ui";
import { AlertCircle, Box, Cpu, Globe } from "lucide-react";

import type { BrowserPac, Discovery } from "../types";

/** One PAC value for the device: the URL when every browser agrees. */
function devicePac(browsers: BrowserPac[]): string {
  const urls = new Set(browsers.map((browser) => browser.pacUrl ?? ""));
  if (urls.size > 1) {
    return "PAC: differs between browsers";
  }
  const [url] = urls;
  return url ? `PAC: found  ${url}` : "PAC: absent";
}

export interface ToolsViewProps {
  discovery: Discovery | null;
  unavailable: boolean;
}

export function ToolsView({ discovery, unavailable }: ToolsViewProps) {
  const agents = discovery?.agents ?? [];
  const modelRuntimes = discovery?.modelRuntimes ?? [];
  const browserPacs = discovery?.browserPacs ?? [];
  const pacFetches = discovery?.pacFetches ?? [];
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
      {agents.length || modelCount ? (
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
          <div className="stat-card">
            <strong>{modelCount}</strong>
            <span>Local models</span>
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
                Desktop, Codex, OpenCode, and Grok Build.
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
      {!unavailable ? (
        <section className="card table-card">
          <CardHeader
            title="Browser PAC"
            description="The proxy auto-config URL set by browser policy, as observed on this device"
          />
          {browserPacs.length ? (
            <div className="model-runtime-inventory">
              <div className="empty-inline">
                <Globe size={20} />
                <div>
                  <strong>{devicePac(browserPacs)}</strong>
                  {pacFetches.map((fetch) => (
                    <span key={fetch.url} title={fetch.sha256 ?? fetch.error}>
                      {fetch.sha256
                        ? `SHA-256 ${fetch.sha256.slice(0, 12)} · ${fetch.size} bytes`
                        : `PAC not reachable: ${fetch.error ?? "unknown error"}`}
                    </span>
                  ))}
                  {browserPacs.map((browser) => (
                    <span key={`${browser.browser}:${browser.packaging ?? ""}`}>
                      {friendlyBrowser(browser.browser)}:{" "}
                      {browser.pacUrl ?? browser.detail ?? "no PAC in policy"}
                      {browser.recommended
                        ? " (recommended, user can change)"
                        : ""}
                    </span>
                  ))}
                </div>
              </div>
            </div>
          ) : (
            <div className="empty-inline">
              <Globe size={20} />
              <div>
                <strong>No browser policy reported</strong>
                <span>
                  No supported browser was found, or browser policy is not read
                  on this platform yet.
                </span>
              </div>
            </div>
          )}
        </section>
      ) : null}
    </div>
  );
}
