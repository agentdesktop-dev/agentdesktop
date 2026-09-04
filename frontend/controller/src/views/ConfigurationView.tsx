import { ToolIcon } from "@agentdesktop/ui";
import {
  Check,
  ChevronRight,
  CircleAlert,
  Copy,
  Plus,
  Trash2,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";

import type { AgentDraft, AgentKind, DaemonConfigDocument } from "../types";

export interface ConfigurationViewProps {
  initialConfig?: DaemonConfigDocument | null;
  onCopy?: (yaml: string) => Promise<void> | void;
}

export function ConfigurationView({
  initialConfig,
  onCopy,
}: ConfigurationViewProps) {
  const addAgentMenu = useRef<HTMLDetailsElement>(null);
  const initializedFromController = useRef(false);
  const [gateway, setGateway] = useState(true);
  const [gatewayUrl, setGatewayUrl] = useState("https://gateway.example.com");
  const [controllerJwt, setControllerJwt] = useState(true);
  const [audience, setAudience] = useState("agentgateway");
  const [sessionNewTelemetry, setSessionNewTelemetry] = useState(false);
  const [toolUseTelemetry, setToolUseTelemetry] = useState(false);
  const [toolInputTelemetry, setToolInputTelemetry] = useState(false);
  const [sandboxEnabled, setSandboxEnabled] = useState(false);
  const [allowedDomains, setAllowedDomains] = useState("");
  const [writablePaths, setWritablePaths] = useState("");
  const [deniedPaths, setDeniedPaths] = useState("");
  const [agents, setAgents] = useState<AgentDraft[]>([
    { kind: "claudeCode", useGateway: true, settings: "" },
  ]);
  const [copied, setCopied] = useState(false);
  const incompatibleSandboxAgentNames = agents.flatMap((agent) => {
    if (!sandboxUnsupportedAgents.has(agent.kind)) return [];
    const definition = configurableAgents.find(
      (candidate) => candidate.kind === agent.kind,
    );
    return [definition?.label ?? agent.kind];
  });
  const sandboxUnavailable = incompatibleSandboxAgentNames.length > 0;
  const yaml = daemonConfigYaml({
    gateway,
    gatewayUrl,
    controllerJwt,
    audience,
    sandboxEnabled,
    allowedDomains,
    writablePaths,
    deniedPaths,
    sessionNewTelemetry,
    toolUseTelemetry,
    toolInputTelemetry,
    agents,
  });
  const availableAgents = configurableAgents.filter(
    (candidate) => !agents.some((agent) => agent.kind === candidate.kind),
  );

  useEffect(() => {
    if (initializedFromController.current || initialConfig === undefined) {
      return;
    }
    initializedFromController.current = true;
    if (!initialConfig) return;

    const llmGateway = initialConfig.llmGateway;
    const events = new Set(initialConfig.telemetry?.events ?? []);
    setGateway(Boolean(llmGateway));
    if (llmGateway) {
      setGatewayUrl(llmGateway.url);
      setControllerJwt(llmGateway.authentication?.type === "controllerJwt");
      setAudience(llmGateway.authentication?.audience ?? "agentgateway");
    }
    setSessionNewTelemetry(events.has("session.new"));
    setToolUseTelemetry(events.has("tool.use") || events.has("tool.use.input"));
    setToolInputTelemetry(events.has("tool.use.input"));
    setSandboxEnabled(Boolean(initialConfig.sandbox));
    setAllowedDomains(
      (initialConfig.sandbox?.network?.allowedDomains ?? []).join("\n"),
    );
    setWritablePaths(
      (initialConfig.sandbox?.filesystem?.writable ?? []).join("\n"),
    );
    setDeniedPaths(
      (initialConfig.sandbox?.filesystem?.denied ?? []).join("\n"),
    );
    setAgents(agentDrafts(initialConfig.programs));
  }, [initialConfig]);

  useEffect(() => {
    const closeMenu = (event: PointerEvent) => {
      const menu = addAgentMenu.current;
      if (menu?.open && !menu.contains(event.target as Node)) {
        menu.removeAttribute("open");
      }
    };
    document.addEventListener("pointerdown", closeMenu);
    return () => document.removeEventListener("pointerdown", closeMenu);
  }, []);

  async function copyYaml() {
    if (onCopy) {
      await onCopy(yaml);
    } else {
      await navigator.clipboard.writeText(yaml);
    }
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1500);
  }

  function updateAgent(kind: AgentKind, update: Partial<AgentDraft>) {
    setAgents((current) =>
      current.map((agent) =>
        agent.kind === kind ? { ...agent, ...update } : agent,
      ),
    );
  }

  function addAgent(selectedAgent: AgentKind) {
    if (sandboxEnabled && sandboxUnsupportedAgents.has(selectedAgent)) return;
    const definition = configurableAgents.find(
      (candidate) => candidate.kind === selectedAgent,
    );
    setAgents((current) => [
      ...current,
      {
        kind: selectedAgent,
        useGateway: true,
        settings: definition?.initialSettings ?? "",
      },
    ]);
  }

  return (
    <div className="stack">
      <section className="section-intro">
        <div>
          <h2>Build a configuration</h2>
          <p>Choose the settings to manage, then copy the generated YAML.</p>
        </div>
      </section>
      <div className="configuration-builder">
        <section className="card wizard-card">
          <details className="wizard-section" open={gateway}>
            <summary className="wizard-section-summary">
              <span className="wizard-section-title">
                <strong>LLM gateway</strong>
                <small>Shared connection settings for managed agents.</small>
              </span>
              <ChevronRight size={15} />
            </summary>
            <div className="wizard-section-content">
              <label className="toggle-row">
                <span>
                  <strong>Enable LLM gateway</strong>
                  <small>Agents can opt into these shared settings.</small>
                </span>
                <input
                  type="checkbox"
                  checked={gateway}
                  onChange={(event) => setGateway(event.target.checked)}
                />
              </label>
              {gateway && (
                <div className="form-grid">
                  <label className="field full-width">
                    <span>Gateway URL</span>
                    <input
                      value={gatewayUrl}
                      onChange={(event) => setGatewayUrl(event.target.value)}
                    />
                  </label>
                  <label className="toggle-row compact full-width">
                    <span>
                      <strong>Controller JWT</strong>
                      <small>Use identity-aware short-lived credentials.</small>
                    </span>
                    <input
                      type="checkbox"
                      checked={controllerJwt}
                      onChange={(event) =>
                        setControllerJwt(event.target.checked)
                      }
                    />
                  </label>
                  {controllerJwt && (
                    <label className="field full-width">
                      <span>JWT audience</span>
                      <input
                        value={audience}
                        onChange={(event) => setAudience(event.target.value)}
                      />
                    </label>
                  )}
                </div>
              )}
            </div>
          </details>
          <details className="wizard-section" open={sandboxEnabled}>
            <summary className="wizard-section-summary">
              <span className="wizard-section-title">
                <strong>Sandbox</strong>
                <small>
                  Restrict local command execution for managed agents.
                </small>
              </span>
              <ChevronRight size={15} />
            </summary>
            <div className="wizard-section-content">
              <label className="toggle-row">
                <span>
                  <strong>Require sandbox</strong>
                  <small>
                    Commands fail closed when sandboxing is unavailable.
                  </small>
                </span>
                <input
                  type="checkbox"
                  checked={sandboxEnabled}
                  disabled={sandboxUnavailable}
                  aria-describedby={
                    sandboxUnavailable ? "sandbox-compatibility" : undefined
                  }
                  onChange={(event) => setSandboxEnabled(event.target.checked)}
                />
              </label>
              {sandboxUnavailable && (
                <p className="sandbox-compatibility" id="sandbox-compatibility">
                  <CircleAlert size={14} />
                  Remove {incompatibleSandboxAgentNames.join(" and ")} to enable
                  sandboxing.
                </p>
              )}
              {sandboxEnabled && (
                <div className="form-grid sandbox-fields">
                  <label className="field full-width">
                    <span>Allowed domains</span>
                    <textarea
                      rows={4}
                      spellCheck={false}
                      placeholder={"api.github.com\nregistry.npmjs.org"}
                      value={allowedDomains}
                      onChange={(event) =>
                        setAllowedDomains(event.target.value)
                      }
                    />
                    <small>
                      One domain per line. Leave empty to block network access.
                    </small>
                  </label>
                  <label className="field full-width">
                    <span>Writable paths</span>
                    <textarea
                      rows={4}
                      spellCheck={false}
                      placeholder={"/tmp/build-cache\n/opt/project/output"}
                      value={writablePaths}
                      onChange={(event) => setWritablePaths(event.target.value)}
                    />
                    <small>One additional writable path per line.</small>
                  </label>
                  <label className="field full-width">
                    <span>Denied paths</span>
                    <textarea
                      rows={4}
                      spellCheck={false}
                      placeholder={"~/.ssh\n~/.aws"}
                      value={deniedPaths}
                      onChange={(event) => setDeniedPaths(event.target.value)}
                    />
                    <small>
                      One path per line. Denied paths cannot be read or changed.
                    </small>
                  </label>
                </div>
              )}
            </div>
          </details>
          <details
            className="wizard-section"
            open={sessionNewTelemetry || toolUseTelemetry || toolInputTelemetry}
          >
            <summary className="wizard-section-summary">
              <span className="wizard-section-title">
                <strong>Telemetry</strong>
                <small>Select the events to collect.</small>
              </span>
              <ChevronRight size={15} />
            </summary>
            <div className="wizard-section-content">
              <div className="telemetry-options">
                <label className="telemetry-option">
                  <span>
                    <strong>New session</strong>
                    <small>A developer-tool session starts.</small>
                  </span>
                  <code>session.new</code>
                  <input
                    type="checkbox"
                    checked={sessionNewTelemetry}
                    onChange={(event) =>
                      setSessionNewTelemetry(event.target.checked)
                    }
                  />
                </label>
                <label className="telemetry-option">
                  <span>
                    <strong>Tool use</strong>
                    <small>Agent, tool name, and invocation ID.</small>
                  </span>
                  <code>tool.use</code>
                  <input
                    type="checkbox"
                    checked={toolUseTelemetry}
                    onChange={(event) => {
                      setToolUseTelemetry(event.target.checked);
                      if (!event.target.checked) setToolInputTelemetry(false);
                    }}
                  />
                </label>
                <label className="telemetry-option">
                  <span>
                    <strong>Tool input</strong>
                    <small>May contain source code, prompts, or secrets.</small>
                  </span>
                  <code>tool.use.input</code>
                  <input
                    type="checkbox"
                    checked={toolInputTelemetry}
                    onChange={(event) => {
                      setToolInputTelemetry(event.target.checked);
                      if (event.target.checked) setToolUseTelemetry(true);
                    }}
                  />
                </label>
              </div>
            </div>
          </details>
          <details className="wizard-section" open={agents.length > 0}>
            <summary className="wizard-section-summary">
              <span className="wizard-section-title">
                <strong>Agents</strong>
                <small>Add the developer tools you want to manage.</small>
              </span>
              <ChevronRight size={15} />
            </summary>
            <div className="wizard-section-content">
              <div className="agent-list-heading">
                {availableAgents.length > 0 && (
                  <details className="add-agent-menu" ref={addAgentMenu}>
                    <summary className="button secondary">
                      <Plus size={14} /> Add agent
                      <ChevronRight className="menu-chevron" size={13} />
                    </summary>
                    <div className="add-agent-options">
                      {availableAgents.map((agent) => (
                        <button
                          type="button"
                          key={agent.kind}
                          disabled={
                            sandboxEnabled &&
                            sandboxUnsupportedAgents.has(agent.kind)
                          }
                          title={
                            sandboxEnabled &&
                            sandboxUnsupportedAgents.has(agent.kind)
                              ? "Unavailable while sandboxing is enabled"
                              : undefined
                          }
                          onClick={(event) => {
                            addAgent(agent.kind);
                            event.currentTarget
                              .closest("details")
                              ?.removeAttribute("open");
                          }}
                        >
                          <ToolIcon kind={agent.iconKind} />
                          <span>{agent.label}</span>
                          <Plus size={13} />
                        </button>
                      ))}
                    </div>
                  </details>
                )}
              </div>
              <div className="agent-drafts">
                {agents.map((agent) => {
                  const definition = configurableAgents.find(
                    (candidate) => candidate.kind === agent.kind,
                  );
                  if (!definition) return null;
                  return (
                    <section className="agent-draft" key={agent.kind}>
                      <div className="agent-draft-heading">
                        <span className="tool-cell">
                          <ToolIcon kind={definition.iconKind} />
                          <strong>{definition.label}</strong>
                        </span>
                        <button
                          type="button"
                          className="icon-button"
                          aria-label={`Remove ${definition.label}`}
                          onClick={() =>
                            setAgents((current) =>
                              current.filter(
                                (candidate) => candidate.kind !== agent.kind,
                              ),
                            )
                          }
                        >
                          <Trash2 size={14} />
                        </button>
                      </div>
                      <label className="toggle-row compact">
                        <span>
                          <strong>Use LLM gateway</strong>
                          <small>
                            Apply the general gateway settings above.
                          </small>
                        </span>
                        <input
                          type="checkbox"
                          disabled={!gateway}
                          checked={gateway && agent.useGateway}
                          onChange={(event) =>
                            updateAgent(agent.kind, {
                              useGateway: event.target.checked,
                            })
                          }
                        />
                      </label>
                      <label className="field">
                        <span>Additional settings (YAML)</span>
                        <textarea
                          rows={7}
                          spellCheck={false}
                          placeholder={definition.placeholder}
                          value={agent.settings}
                          onChange={(event) =>
                            updateAgent(agent.kind, {
                              settings: event.target.value,
                            })
                          }
                        />
                        <small>
                          Use the agent’s native configuration keys.
                        </small>
                      </label>
                    </section>
                  );
                })}
                {agents.length === 0 && (
                  <p className="agent-empty">No agents added.</p>
                )}
              </div>
            </div>
          </details>
        </section>
        <section className="card output-card">
          <div className="output-heading">
            <div>
              <h3>Generated YAML</h3>
              <p>Copy this into the daemon configuration file.</p>
            </div>
            <button
              type="button"
              className="button secondary"
              onClick={copyYaml}
            >
              {copied ? <Check size={14} /> : <Copy size={14} />}
              {copied ? "Copied" : "Copy"}
            </button>
          </div>
          <pre>
            <code>{yaml}</code>
          </pre>
        </section>
      </div>
    </div>
  );
}

const configurableAgents: Array<{
  kind: AgentKind;
  label: string;
  iconKind: string;
  placeholder: string;
  initialSettings?: string;
}> = [
  {
    kind: "claudeCode",
    label: "Claude Code",
    iconKind: "claude-code",
    placeholder: "permissions:\n  defaultMode: plan",
  },
  {
    kind: "claudeDesktop",
    label: "Claude Desktop",
    iconKind: "claude-desktop",
    placeholder: "isLocalDevMcpEnabled: true",
  },
  {
    kind: "codex",
    label: "Codex",
    iconKind: "codex",
    placeholder: "managedConfig:\n  model_reasoning_effort: high",
  },
  {
    kind: "openCode",
    label: "OpenCode",
    iconKind: "opencode",
    placeholder: "managedConfig:\n  autoupdate: false",
    initialSettings:
      "model: gpt-5.6-terra\nmodels:\n  gpt-5.6-terra:\n    name: GPT 5.6 Terra",
  },
  {
    kind: "grok",
    label: "Grok Build",
    iconKind: "grok",
    placeholder: "model: grok-4.6",
    initialSettings: "model: grok-4.6",
  },
];

const sandboxUnsupportedAgents = new Set<AgentKind>([
  "claudeDesktop",
  "openCode",
  "grok",
]);

function daemonConfigYaml(options: {
  gateway: boolean;
  gatewayUrl: string;
  controllerJwt: boolean;
  audience: string;
  sandboxEnabled: boolean;
  allowedDomains: string;
  writablePaths: string;
  deniedPaths: string;
  sessionNewTelemetry: boolean;
  toolUseTelemetry: boolean;
  toolInputTelemetry: boolean;
  agents: AgentDraft[];
}) {
  const lines: string[] = [];
  if (options.gateway) {
    lines.push("llmGateway:", `  url: ${yamlString(options.gatewayUrl)}`);
    if (options.controllerJwt) {
      lines.push(
        "  authentication:",
        "    type: controllerJwt",
        `    audience: ${yamlString(options.audience)}`,
        "    allowedClientIds: [claude-code, claude-desktop, codex, opencode, grok]",
      );
    }
    lines.push("");
  }
  if (options.sandboxEnabled) {
    lines.push(
      "sandbox:",
      "  network:",
      ...yamlStringList("allowedDomains", textList(options.allowedDomains), 4),
      "  filesystem:",
      ...yamlStringList("writable", textList(options.writablePaths), 4),
      ...yamlStringList("denied", textList(options.deniedPaths), 4),
      "",
    );
  }
  if (options.sessionNewTelemetry || options.toolUseTelemetry) {
    lines.push("telemetry:", "  events:");
    if (options.sessionNewTelemetry) lines.push("  - session.new");
    if (options.toolUseTelemetry) {
      lines.push(
        `  - ${options.toolInputTelemetry ? "tool.use.input" : "tool.use"}`,
      );
    }
    lines.push("");
  }
  if (options.agents.length === 0) {
    lines.push("programs: {}");
  } else {
    lines.push("programs:");
    for (const agent of options.agents) {
      const settings = agent.settings.trim();
      const disablesGateway = options.gateway && !agent.useGateway;
      if (!settings && !disablesGateway) {
        lines.push(`  ${agent.kind}: {}`);
        continue;
      }
      lines.push(`  ${agent.kind}:`);
      if (disablesGateway) lines.push("    useLlmGateway: false");
      if (settings) {
        lines.push(...settings.split("\n").map((line) => `    ${line}`));
      }
    }
  }
  return `${lines.join("\n")}\n`;
}

function yamlString(value: string) {
  return JSON.stringify(value);
}

function textList(value: string) {
  return [...new Set(value.split(/\r?\n/).map((item) => item.trim()))].filter(
    Boolean,
  );
}

function yamlStringList(key: string, values: string[], indent: number) {
  const padding = " ".repeat(indent);
  if (values.length === 0) return [`${padding}${key}: []`];
  return [
    `${padding}${key}:`,
    ...values.map((value) => `${padding}  - ${yamlString(value)}`),
  ];
}

function agentDrafts(programs: DaemonConfigDocument["programs"]): AgentDraft[] {
  if (!programs) return [];
  return configurableAgents.flatMap(({ kind }) => {
    const program = programs[kind];
    if (!program) return [];
    const { useLlmGateway, ...settings } = program;
    return [
      {
        kind,
        useGateway: useLlmGateway !== false,
        settings: objectYaml(settings),
      },
    ];
  });
}

function objectYaml(value: Record<string, unknown>) {
  return yamlLines(value, 0).join("\n");
}

function yamlLines(value: unknown, indent: number): string[] {
  const padding = " ".repeat(indent);
  if (Array.isArray(value)) {
    if (value.length === 0) return [`${padding}[]`];
    return value.flatMap((item) => {
      if (isNonEmptyCollection(item)) {
        return [`${padding}-`, ...yamlLines(item, indent + 2)];
      }
      return [`${padding}- ${yamlScalar(item)}`];
    });
  }
  if (value !== null && typeof value === "object") {
    const entries = Object.entries(value);
    if (entries.length === 0) return [`${padding}{}`];
    return entries.flatMap(([key, item]) => {
      const yamlKey = /^[A-Za-z_][A-Za-z0-9_.-]*$/.test(key)
        ? key
        : yamlString(key);
      if (isNonEmptyCollection(item)) {
        return [`${padding}${yamlKey}:`, ...yamlLines(item, indent + 2)];
      }
      return [`${padding}${yamlKey}: ${yamlScalar(item)}`];
    });
  }
  return [`${padding}${yamlScalar(value)}`];
}

function isNonEmptyCollection(value: unknown) {
  return (
    (Array.isArray(value) && value.length > 0) ||
    (value !== null &&
      typeof value === "object" &&
      Object.keys(value).length > 0)
  );
}

function yamlScalar(value: unknown) {
  if (typeof value === "string") return yamlString(value);
  if (value === null) return "null";
  if (typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  if (Array.isArray(value)) return "[]";
  if (typeof value === "object") return "{}";
  return yamlString(String(value));
}
