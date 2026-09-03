import {
  ChevronLeft,
  ChevronRight,
  Code2,
  Cpu,
  Server,
  Sparkles,
} from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import claudeCodeIcon from "./assets/claude-code.svg";
import claudeDesktopIcon from "./assets/claude-desktop.svg";
import codexIcon from "./assets/codex.svg";
import copilotIcon from "./assets/copilot.svg";
import ollamaIcon from "./assets/ollama.svg";
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

export interface ToolInventoryLeadingTab {
  label: string;
  count?: number;
  icon: React.ReactNode;
  content: React.ReactNode;
}

export interface ToolInventoryProps {
  discovery: ToolDiscovery;
  leadingTab?: ToolInventoryLeadingTab;
  activateLeadingTabRequest?: number;
}

export interface ModelRuntimeDiscovery {
  kind: string;
  models: Array<{ name: string }>;
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

export function friendlyModelRuntime(kind: string) {
  const names: Record<string, string> = {
    ollama: "Ollama",
  };
  return names[kind.toLowerCase()] ?? kind;
}

function ModelRuntimeIcon({ kind }: { kind: string }) {
  return kind.toLowerCase() === "ollama" ? (
    <img
      className="model-runtime-icon"
      src={ollamaIcon}
      alt=""
      aria-hidden="true"
    />
  ) : (
    <Cpu className="model-runtime-icon" size={18} aria-hidden="true" />
  );
}

export function ModelRuntimeInventory({
  runtime,
}: {
  runtime: ModelRuntimeDiscovery;
}) {
  return (
    <div className="model-runtime-item">
      <div className="model-runtime-heading">
        <span className="tool-cell">
          <ModelRuntimeIcon kind={runtime.kind} />
          <strong>{friendlyModelRuntime(runtime.kind)}</strong>
        </span>
        <span>
          {runtime.models.length} model{runtime.models.length === 1 ? "" : "s"}
        </span>
      </div>
      {runtime.models.length ? (
        <div className="model-name-list">
          {runtime.models.map((model) => (
            <code key={model.name}>{model.name}</code>
          ))}
        </div>
      ) : (
        <p className="model-runtime-empty">No models installed.</p>
      )}
    </div>
  );
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

function CapabilityPanel({
  id,
  labelledBy,
  hidden,
  title,
  count,
  page,
  pages,
  onPageChange,
  children,
}: React.PropsWithChildren<{
  id: string;
  labelledBy: string;
  hidden: boolean;
  title: string;
  count: number;
  page: number;
  pages: number;
  onPageChange: (page: number) => void;
}>) {
  const firstItem = page * 5 + 1;
  const lastItem = Math.min((page + 1) * 5, count);
  return (
    <section
      className="capability-panel"
      id={id}
      role="tabpanel"
      aria-labelledby={labelledBy}
      hidden={hidden}
    >
      {pages > 1 ? (
        <div className="capability-pagination">
          <span>
            {firstItem}–{lastItem} of {count}
          </span>
          <nav className="mini-pager" aria-label={`${title} pages`}>
            <button
              type="button"
              aria-label={`Previous ${title} page`}
              disabled={page === 0}
              onClick={() => onPageChange(page - 1)}
            >
              <ChevronLeft size={12} />
            </button>
            <button
              type="button"
              aria-label={`Next ${title} page`}
              disabled={page + 1 === pages}
              onClick={() => onPageChange(page + 1)}
            >
              <ChevronRight size={12} />
            </button>
          </nav>
        </div>
      ) : null}
      {children}
    </section>
  );
}

type InventoryTab = "leading" | "mcp" | "skills";

export function ToolInventory({
  discovery,
  leadingTab,
  activateLeadingTabRequest = 0,
}: ToolInventoryProps) {
  const servers = discovery.mcp_servers ?? [];
  const skills = discovery.skills ?? [];
  const [serverPage, setServerPage] = useState(0);
  const [skillPage, setSkillPage] = useState(0);
  const serverPages = Math.max(1, Math.ceil(servers.length / 5));
  const skillPages = Math.max(1, Math.ceil(skills.length / 5));
  const visibleServerPage = Math.min(serverPage, serverPages - 1);
  const visibleSkillPage = Math.min(skillPage, skillPages - 1);
  const [activeTab, setActiveTab] = useState<InventoryTab>(
    leadingTab
      ? "leading"
      : servers.length > 0 || skills.length === 0
        ? "mcp"
        : "skills",
  );
  const tabId = useId();
  const hasLeadingTab = Boolean(leadingTab);
  const details = useRef<HTMLDetailsElement>(null);
  const leadingTabRef = useRef<HTMLButtonElement>(null);
  const mcpTab = useRef<HTMLButtonElement>(null);
  const skillsTab = useRef<HTMLButtonElement>(null);
  const tabOrder: InventoryTab[] = leadingTab
    ? ["leading", "mcp", "skills"]
    : ["mcp", "skills"];

  useEffect(() => {
    if (!leadingTab || activateLeadingTabRequest <= 0) return;
    if (details.current) details.current.open = true;
    setActiveTab("leading");
    window.requestAnimationFrame(() => {
      details.current?.scrollIntoView({ block: "nearest" });
      leadingTabRef.current?.focus();
    });
  }, [activateLeadingTabRequest, hasLeadingTab]);

  function selectAdjacentTab(event: React.KeyboardEvent, tab: InventoryTab) {
    if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) {
      return;
    }
    event.preventDefault();
    const currentIndex = tabOrder.indexOf(tab);
    const nextTab =
      event.key === "Home"
        ? tabOrder[0]
        : event.key === "End"
          ? tabOrder[tabOrder.length - 1]
          : event.key === "ArrowRight"
            ? tabOrder[(currentIndex + 1) % tabOrder.length]
            : tabOrder[(currentIndex - 1 + tabOrder.length) % tabOrder.length];
    setActiveTab(nextTab);
    const nextRef = {
      leading: leadingTabRef,
      mcp: mcpTab,
      skills: skillsTab,
    }[nextTab];
    nextRef.current?.focus();
  }

  return (
    <details className="tool-inventory-item" ref={details}>
      <summary>
        <span className="tool-cell">
          <ToolIcon kind={discovery.kind} />
          <strong>{friendlyTool(discovery.kind)}</strong>
        </span>
        <span className="tool-version">{discovery.version || "Unknown"}</span>
        <code className="tool-path" title={discovery.path}>
          {discovery.path}
        </code>
        <span className="tool-summary-meta">
          <span className="capability-counts">
            {servers.length} MCP · {skills.length} skills
          </span>
        </span>
        <ChevronRight
          className="tool-disclosure"
          size={16}
          aria-hidden="true"
        />
      </summary>
      <div className="capability-stack">
        <div
          className="capability-tabs"
          role="tablist"
          aria-label={`${friendlyTool(discovery.kind)} capabilities`}
        >
          {leadingTab ? (
            <button
              ref={leadingTabRef}
              id={`${tabId}-leading-tab`}
              type="button"
              role="tab"
              aria-selected={activeTab === "leading"}
              aria-controls={`${tabId}-leading-panel`}
              tabIndex={activeTab === "leading" ? 0 : -1}
              onClick={() => setActiveTab("leading")}
              onKeyDown={(event) => selectAdjacentTab(event, "leading")}
            >
              {leadingTab.icon}
              {leadingTab.label}
              {leadingTab.count !== undefined ? (
                <span>{leadingTab.count}</span>
              ) : null}
            </button>
          ) : null}
          <button
            ref={mcpTab}
            id={`${tabId}-mcp-tab`}
            type="button"
            role="tab"
            aria-selected={activeTab === "mcp"}
            aria-controls={`${tabId}-mcp-panel`}
            tabIndex={activeTab === "mcp" ? 0 : -1}
            onClick={() => setActiveTab("mcp")}
            onKeyDown={(event) => selectAdjacentTab(event, "mcp")}
          >
            <Server size={14} />
            MCP servers
            <span>{servers.length}</span>
          </button>
          <button
            ref={skillsTab}
            id={`${tabId}-skills-tab`}
            type="button"
            role="tab"
            aria-selected={activeTab === "skills"}
            aria-controls={`${tabId}-skills-panel`}
            tabIndex={activeTab === "skills" ? 0 : -1}
            onClick={() => setActiveTab("skills")}
            onKeyDown={(event) => selectAdjacentTab(event, "skills")}
          >
            <Sparkles size={14} />
            Skills
            <span>{skills.length}</span>
          </button>
        </div>
        {leadingTab ? (
          <section
            className="capability-panel tool-leading-panel"
            id={`${tabId}-leading-panel`}
            role="tabpanel"
            aria-labelledby={`${tabId}-leading-tab`}
            hidden={activeTab !== "leading"}
          >
            {leadingTab.content}
          </section>
        ) : null}
        <CapabilityPanel
          id={`${tabId}-mcp-panel`}
          labelledBy={`${tabId}-mcp-tab`}
          hidden={activeTab !== "mcp"}
          title="MCP servers"
          count={servers.length}
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
        </CapabilityPanel>
        <CapabilityPanel
          id={`${tabId}-skills-panel`}
          labelledBy={`${tabId}-skills-tab`}
          hidden={activeTab !== "skills"}
          title="Skills"
          count={skills.length}
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
        </CapabilityPanel>
      </div>
    </details>
  );
}
