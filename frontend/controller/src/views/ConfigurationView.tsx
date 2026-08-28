import { ToolIcon } from "@agentdesktop/ui";
import { Check, ChevronRight, Copy, Plus, Save, Trash2 } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { parse, stringify } from "yaml";

import type {
  AgentDraft,
  AgentKind,
  DaemonConfigDocument,
  FleetConfigurationResponse,
} from "../types";

export interface ConfigurationViewProps {
  initialYaml?: string | null;
  initialRevision?: number | null;
  initialVersion?: string | null;
  sourceError?: string | null;
  writable?: boolean;
  onCopy?: (yaml: string) => Promise<void> | void;
  onSave?: (
    yaml: string,
    version: string,
  ) => Promise<FleetConfigurationResponse>;
}

export function ConfigurationView({
  initialYaml,
  initialRevision,
  initialVersion,
  sourceError,
  writable = false,
  onCopy,
  onSave,
}: ConfigurationViewProps) {
  const addAgentMenu = useRef<HTMLDetailsElement>(null);
  const initializedFromController = useRef(false);
  const [gateway, setGateway] = useState(true);
  const [gatewayUrl, setGatewayUrl] = useState("https://gateway.example.com");
  const [controllerJwt, setControllerJwt] = useState(true);
  const [audience, setAudience] = useState("agentgateway");
  const [allowedClientIds, setAllowedClientIds] = useState([
    "claude-code",
    "claude-desktop",
    "codex",
    "opencode",
  ]);
  const [preservedAuthentication, setPreservedAuthentication] = useState<
    Record<string, unknown> | undefined
  >();
  const [preservedController, setPreservedController] = useState<
    Record<string, unknown> | undefined
  >();
  const [sessionNewTelemetry, setSessionNewTelemetry] = useState(false);
  const [toolUseTelemetry, setToolUseTelemetry] = useState(false);
  const [toolInputTelemetry, setToolInputTelemetry] = useState(false);
  const [agents, setAgents] = useState<AgentDraft[]>([
    { kind: "claudeCode", useGateway: true, settings: "" },
  ]);
  const [copied, setCopied] = useState(false);
  const [version, setVersion] = useState(initialVersion ?? null);
  const [revision, setRevision] = useState(initialRevision ?? null);
  const [saving, setSaving] = useState(false);
  const [hydrated, setHydrated] = useState(initialYaml === undefined);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [savedYaml, setSavedYaml] = useState<string | null>(null);
  const yaml = daemonConfigYaml({
    gateway,
    gatewayUrl,
    controllerJwt,
    audience,
    allowedClientIds,
    preservedAuthentication,
    preservedController,
    sessionNewTelemetry,
    toolUseTelemetry,
    toolInputTelemetry,
    agents,
  });
  const availableAgents = configurableAgents.filter(
    (candidate) => !agents.some((agent) => agent.kind === candidate.kind),
  );

  useEffect(() => {
    if (initializedFromController.current || initialYaml === undefined) {
      return;
    }
    initializedFromController.current = true;
    const document = initialYaml
      ? (parse(initialYaml, { intAsBigInt: true }) as DaemonConfigDocument)
      : null;
    if (!document) {
      setHydrated(true);
      return;
    }

    setPreservedController(document.controller);
    const llmGateway = document.llmGateway;
    const events = new Set(document.telemetry?.events ?? []);
    setGateway(Boolean(llmGateway));
    if (llmGateway) {
      setGatewayUrl(llmGateway.url);
      setControllerJwt(llmGateway.authentication?.type === "controllerJwt");
      setAudience(llmGateway.authentication?.audience ?? "agentgateway");
      if (llmGateway.authentication?.type === "controllerJwt") {
        setAllowedClientIds(llmGateway.authentication.allowedClientIds ?? []);
        setPreservedAuthentication(undefined);
      } else {
        setPreservedAuthentication(llmGateway.authentication);
      }
    }
    setSessionNewTelemetry(events.has("session.new"));
    setToolUseTelemetry(events.has("tool.use") || events.has("tool.use.input"));
    setToolInputTelemetry(events.has("tool.use.input"));
    setAgents(agentDrafts(document.programs));
    setHydrated(true);
  }, [initialYaml]);

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

  async function saveYaml() {
    if (!onSave || !version) return;
    setSaving(true);
    setSaveError(null);
    setSavedYaml(null);
    try {
      const result = await onSave(yaml, version);
      setVersion(result.version);
      setRevision(result.revision);
      setSavedYaml(yaml);
    } catch (error) {
      setSaveError(error instanceof Error ? error.message : "Save failed");
    } finally {
      setSaving(false);
    }
  }

  function updateAgent(kind: AgentKind, update: Partial<AgentDraft>) {
    setAgents((current) =>
      current.map((agent) =>
        agent.kind === kind ? { ...agent, ...update } : agent,
      ),
    );
  }

  function addAgent(selectedAgent: AgentKind) {
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
          <p>Choose the settings to manage, then review the generated YAML.</p>
        </div>
      </section>
      {sourceError && (
        <p className="error-callout" role="alert">
          The active configuration is shown, but its source is unavailable:{" "}
          {sourceError}
        </p>
      )}
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
              <p>
                {writable
                  ? "Save these settings to roll them out to the fleet."
                  : "Copy this into the daemon configuration file."}
              </p>
            </div>
            <div className="output-actions">
              <button
                type="button"
                className="button secondary"
                onClick={copyYaml}
              >
                {copied ? <Check size={14} /> : <Copy size={14} />}
                {copied ? "Copied" : "Copy"}
              </button>
              {writable && (
                <button
                  type="button"
                  className="button primary"
                  disabled={saving || !version || !hydrated}
                  onClick={saveYaml}
                >
                  {savedYaml === yaml ? (
                    <Check size={14} />
                  ) : (
                    <Save size={14} />
                  )}
                  {saving ? "Saving…" : "Save and roll out"}
                </button>
              )}
            </div>
          </div>
          {saveError && (
            <p className="configuration-save-message error" role="alert">
              {saveError}
            </p>
          )}
          {savedYaml === yaml && revision !== null && (
            <p className="configuration-save-message success" role="status">
              Revision {revision} saved and queued for rollout.
            </p>
          )}
          <textarea
            aria-label="Generated fleet configuration"
            className="configuration-yaml-preview"
            readOnly
            spellCheck={false}
            value={yaml}
          />
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
];

function daemonConfigYaml(options: {
  gateway: boolean;
  gatewayUrl: string;
  controllerJwt: boolean;
  audience: string;
  allowedClientIds: string[];
  preservedAuthentication?: Record<string, unknown>;
  preservedController?: Record<string, unknown>;
  sessionNewTelemetry: boolean;
  toolUseTelemetry: boolean;
  toolInputTelemetry: boolean;
  agents: AgentDraft[];
}) {
  const lines: string[] = [];
  if (options.preservedController) {
    lines.push("controller:", ...yamlBlock(options.preservedController, 2), "");
  }
  if (options.gateway) {
    lines.push("llmGateway:", `  url: ${yamlString(options.gatewayUrl)}`);
    if (options.controllerJwt) {
      lines.push(
        "  authentication:",
        "    type: controllerJwt",
        `    audience: ${yamlString(options.audience)}`,
        `    allowedClientIds: [${options.allowedClientIds.map(yamlString).join(", ")}]`,
      );
    } else if (options.preservedAuthentication) {
      lines.push(
        "  authentication:",
        ...yamlBlock(options.preservedAuthentication, 4),
      );
    }
    lines.push("");
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
        settings: stringify(settings, { lineWidth: 0 }).trimEnd(),
      },
    ];
  });
}

function yamlBlock(value: unknown, indent: number): string[] {
  const padding = " ".repeat(indent);
  return stringify(value, { lineWidth: 0 })
    .trimEnd()
    .split("\n")
    .map((line) => padding + line);
}
