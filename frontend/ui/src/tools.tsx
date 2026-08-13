import {
  ChevronLeft,
  ChevronRight,
  Code2,
  Server,
  Sparkles,
} from "lucide-react";
import { useState } from "react";
import claudeCodeIcon from "./assets/claude-code.svg";
import claudeDesktopIcon from "./assets/claude-desktop.svg";
import codexIcon from "./assets/codex.svg";
import copilotIcon from "./assets/copilot.svg";
import openCodeIcon from "./assets/opencode.svg";

export interface ToolMcpServer {
  name: string;
  transport: string;
  command?: string;
  url?: string;
  enabled: boolean;
  source: string;
}

export interface ToolSkill {
  path: string;
  frontMatter: Record<string, unknown>;
}

export interface ToolDiscovery {
  kind: string;
  version?: string | null;
  path: string;
  mcp_servers?: ToolMcpServer[];
  skills?: ToolSkill[];
}

const toolIcons: Record<string, string> = {
  codex: codexIcon,
  "claude-code": claudeCodeIcon,
  claude_code: claudeCodeIcon,
  "claude-desktop": claudeDesktopIcon,
  claude_desktop: claudeDesktopIcon,
  opencode: openCodeIcon,
  vscode: copilotIcon,
};

export function friendlyTool(kind: string) {
  const names: Record<string, string> = {
    codex: "Codex",
    claude_code: "Claude Code",
    "claude-code": "Claude Code",
    claude_desktop: "Claude Desktop",
    "claude-desktop": "Claude Desktop",
    opencode: "OpenCode",
    vscode: "VS Code",
  };
  return names[kind.toLowerCase()] ?? kind;
}

export function ToolIcon({ kind }: { kind: string }) {
  const icon = toolIcons[kind.toLowerCase()];
  return icon ? (
    <img className="tool-icon" src={icon} alt="" aria-hidden="true" />
  ) : (
    <Code2 className="tool-icon-fallback" size={16} aria-hidden="true" />
  );
}

function frontMatterText(value: unknown) {
  return typeof value === "string" && value.trim() ? value : null;
}

function CapabilitySection({
  icon,
  title,
  page,
  pages,
  onPageChange,
  children,
}: React.PropsWithChildren<{
  icon: React.ReactNode;
  title: string;
  page: number;
  pages: number;
  onPageChange: (page: number) => void;
}>) {
  return (
    <section className="capability-section">
      <div className="capability-heading">
        <h4>
          {icon}
          {title}
        </h4>
        {pages > 1 ? (
          <nav className="mini-pager" aria-label={`${title} pages`}>
            <button
              type="button"
              aria-label={`Previous ${title} page`}
              disabled={page === 0}
              onClick={() => onPageChange(page - 1)}
            >
              <ChevronLeft size={12} />
            </button>
            <span>
              {page + 1}/{pages}
            </span>
            <button
              type="button"
              aria-label={`Next ${title} page`}
              disabled={page + 1 === pages}
              onClick={() => onPageChange(page + 1)}
            >
              <ChevronRight size={12} />
            </button>
          </nav>
        ) : null}
      </div>
      {children}
    </section>
  );
}

export function ToolInventory({ discovery }: { discovery: ToolDiscovery }) {
  const servers = discovery.mcp_servers ?? [];
  const skills = discovery.skills ?? [];
  const [serverPage, setServerPage] = useState(0);
  const [skillPage, setSkillPage] = useState(0);
  const serverPages = Math.max(1, Math.ceil(servers.length / 5));
  const skillPages = Math.max(1, Math.ceil(skills.length / 5));
  const visibleServerPage = Math.min(serverPage, serverPages - 1);
  const visibleSkillPage = Math.min(skillPage, skillPages - 1);
  return (
    <details className="tool-inventory-item">
      <summary>
        <span className="tool-cell">
          <ToolIcon kind={discovery.kind} />
          <strong>{friendlyTool(discovery.kind)}</strong>
        </span>
        <span className="tool-version">{discovery.version || "Unknown"}</span>
        <code className="tool-path" title={discovery.path}>
          {discovery.path}
        </code>
        <span className="capability-counts">
          {servers.length} MCP · {skills.length} skills
        </span>
      </summary>
      <div className="capability-grid">
        <CapabilitySection
          icon={<Server size={14} />}
          title="MCP servers"
          page={visibleServerPage}
          pages={serverPages}
          onPageChange={setServerPage}
        >
          {servers.length ? (
            servers
              .slice(visibleServerPage * 5, visibleServerPage * 5 + 5)
              .map((server) => (
                <div
                  className="capability-row"
                  key={`${server.source}-${server.name}`}
                >
                  <div>
                    <strong>{server.name}</strong>
                    <span>
                      {server.transport}
                      {server.enabled ? "" : " · disabled"}
                    </span>
                  </div>
                  <code>
                    {server.url ?? server.command ?? "No endpoint reported"}
                  </code>
                  <small>{server.source}</small>
                </div>
              ))
          ) : (
            <p className="capability-empty">No MCP servers reported.</p>
          )}
        </CapabilitySection>
        <CapabilitySection
          icon={<Sparkles size={14} />}
          title="Skills"
          page={visibleSkillPage}
          pages={skillPages}
          onPageChange={setSkillPage}
        >
          {skills.length ? (
            skills
              .slice(visibleSkillPage * 5, visibleSkillPage * 5 + 5)
              .map((skill) => (
                <div className="capability-row" key={skill.path}>
                  <div>
                    <strong>
                      {frontMatterText(skill.frontMatter.name) ??
                        "Unnamed skill"}
                    </strong>
                  </div>
                  {frontMatterText(skill.frontMatter.description) ? (
                    <p>{frontMatterText(skill.frontMatter.description)}</p>
                  ) : null}
                  <small>{skill.path}</small>
                </div>
              ))
          ) : (
            <p className="capability-empty">No skills reported.</p>
          )}
        </CapabilitySection>
      </div>
    </details>
  );
}
