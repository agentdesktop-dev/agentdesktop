import { friendlyTool, ToolIcon } from "@agentdesktop/ui";
import {
  Check,
  ChevronRight,
  CircleAlert,
  Copy,
  Plus,
  Trash2,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";

import {
  createConfigurationDraft,
  defaultLlmGateway,
  programSettingsYaml,
  renderConfiguration,
} from "../configuration";
import type { AgentKind, DaemonConfigDocument } from "../types";

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
  const [draft, setDraft] = useState(() =>
    createConfigurationDraft(initialConfig),
  );
  const [copied, setCopied] = useState(false);
  const { config, gatewayEnabled: gateway, sandboxEnabled } = draft;
  const llmGateway = config.llmGateway ?? defaultLlmGateway();
  const controllerJwt = llmGateway.authentication?.type === "controllerJwt";
  const events = new Set(config.telemetry?.events ?? []);
  const sessionNewTelemetry = events.has("session.new");
  const toolInputTelemetry = events.has("tool.use.input");
  const toolUseTelemetry = events.has("tool.use") || toolInputTelemetry;
  const agents = Object.entries(config.programs ?? {}).flatMap(
    ([kind, program]) => {
      const definition = configurableAgents.find(
        (candidate) => candidate.kind === kind,
      );
      return definition && program ? [{ ...definition, program }] : [];
    },
  );
  const incompatibleSandboxAgentNames = agents
    .filter((agent) => sandboxUnsupportedAgents.has(agent.kind))
    .map((agent) => friendlyTool(agent.iconKind));
  const sandboxUnavailable = incompatibleSandboxAgentNames.length > 0;
  const { yaml, errors } = renderConfiguration(draft);
  const availableAgents = configurableAgents.filter(
    (candidate) => !agents.some((agent) => agent.kind === candidate.kind),
  );

  useEffect(() => {
    if (initializedFromController.current || initialConfig === undefined) {
      return;
    }
    initializedFromController.current = true;
    if (!initialConfig) return;
    setDraft(createConfigurationDraft(initialConfig));
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
    if (yaml === null) return;
    if (onCopy) {
      await onCopy(yaml);
    } else {
      await navigator.clipboard.writeText(yaml);
    }
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1500);
  }

  function updateConfig(
    update: (config: DaemonConfigDocument) => DaemonConfigDocument,
  ) {
    setDraft((current) => ({ ...current, config: update(current.config) }));
  }

  function updateGateway(
    update: Partial<NonNullable<DaemonConfigDocument["llmGateway"]>>,
  ) {
    updateConfig((current) => ({
      ...current,
      llmGateway: { ...(current.llmGateway ?? defaultLlmGateway()), ...update },
    }));
  }

  function updateSandbox(
    field: "allowedDomains" | "writable" | "denied",
    value: string,
  ) {
    setDraft((current) => ({
      ...current,
      sandboxFields: { ...current.sandboxFields, [field]: value },
    }));
  }

  function updateTelemetry(event: string, enabled: boolean) {
    updateConfig((current) => {
      const next = new Set(current.telemetry?.events ?? []);
      if (enabled) next.add(event);
      else next.delete(event);
      if (event === "tool.use" && !enabled) next.delete("tool.use.input");
      if (event === "tool.use.input") {
        if (enabled) next.delete("tool.use");
        else next.add("tool.use");
      }
      return {
        ...current,
        telemetry: { ...current.telemetry, events: [...next] },
      };
    });
  }

  function addAgent(selectedAgent: AgentKind) {
    if (sandboxEnabled && sandboxUnsupportedAgents.has(selectedAgent)) return;
    const definition = configurableAgents.find(
      (candidate) => candidate.kind === selectedAgent,
    );
    updateConfig((current) => ({
      ...current,
      programs: {
        ...current.programs,
        [selectedAgent]: definition?.initialSettings ?? {},
      },
    }));
  }

  function removeAgent(kind: AgentKind) {
    setDraft((current) => {
      const programs = { ...current.config.programs };
      const settings = { ...current.settings };
      delete programs[kind];
      delete settings[kind];
      return { ...current, config: { ...current.config, programs }, settings };
    });
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
                  onChange={(event) =>
                    setDraft({ ...draft, gatewayEnabled: event.target.checked })
                  }
                />
              </label>
              {gateway && (
                <div className="form-grid">
                  <label className="field full-width">
                    <span>Gateway URL</span>
                    <input
                      value={llmGateway.url}
                      onChange={(event) =>
                        updateGateway({ url: event.target.value })
                      }
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
                        updateGateway({
                          authentication: event.target.checked
                            ? defaultLlmGateway().authentication
                            : undefined,
                        })
                      }
                    />
                  </label>
                  {llmGateway.authentication && !controllerJwt && (
                    <p className="sandbox-compatibility full-width">
                      {llmGateway.authentication.type.toUpperCase()}{" "}
                      authentication is preserved. Enable Controller JWT to
                      replace it.
                    </p>
                  )}
                  {controllerJwt && (
                    <label className="field full-width">
                      <span>JWT audience</span>
                      <input
                        value={llmGateway.authentication?.audience ?? ""}
                        onChange={(event) =>
                          updateGateway({
                            authentication: {
                              ...llmGateway.authentication,
                              type: "controllerJwt",
                              audience: event.target.value,
                            },
                          })
                        }
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
                  onChange={(event) =>
                    setDraft({ ...draft, sandboxEnabled: event.target.checked })
                  }
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
                      value={
                        draft.sandboxFields.allowedDomains ??
                        (config.sandbox?.network?.allowedDomains ?? []).join(
                          "\n",
                        )
                      }
                      onChange={(event) =>
                        updateSandbox("allowedDomains", event.target.value)
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
                      value={
                        draft.sandboxFields.writable ??
                        (config.sandbox?.filesystem?.writable ?? []).join("\n")
                      }
                      onChange={(event) =>
                        updateSandbox("writable", event.target.value)
                      }
                    />
                    <small>One additional writable path per line.</small>
                  </label>
                  <label className="field full-width">
                    <span>Denied paths</span>
                    <textarea
                      rows={4}
                      spellCheck={false}
                      placeholder={"~/.ssh\n~/.aws"}
                      value={
                        draft.sandboxFields.denied ??
                        (config.sandbox?.filesystem?.denied ?? []).join("\n")
                      }
                      onChange={(event) =>
                        updateSandbox("denied", event.target.value)
                      }
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
                      updateTelemetry("session.new", event.target.checked)
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
                    onChange={(event) =>
                      updateTelemetry("tool.use", event.target.checked)
                    }
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
                    onChange={(event) =>
                      updateTelemetry("tool.use.input", event.target.checked)
                    }
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
                          <span>{friendlyTool(agent.iconKind)}</span>
                          <Plus size={13} />
                        </button>
                      ))}
                    </div>
                  </details>
                )}
              </div>
              <div className="agent-drafts">
                {agents.map((agent) => {
                  const name = friendlyTool(agent.iconKind);
                  return (
                    <section className="agent-draft" key={agent.kind}>
                      <div className="agent-draft-heading">
                        <span className="tool-cell">
                          <ToolIcon kind={agent.iconKind} />
                          <strong>{name}</strong>
                        </span>
                        <button
                          type="button"
                          className="icon-button"
                          aria-label={`Remove ${name}`}
                          onClick={() => removeAgent(agent.kind)}
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
                          checked={
                            gateway && agent.program.useLlmGateway !== false
                          }
                          onChange={(event) =>
                            updateConfig((current) => ({
                              ...current,
                              programs: {
                                ...current.programs,
                                [agent.kind]: {
                                  ...current.programs?.[agent.kind],
                                  useLlmGateway: event.target.checked,
                                },
                              },
                            }))
                          }
                        />
                      </label>
                      <label className="field">
                        <span>Additional settings (YAML)</span>
                        <textarea
                          rows={7}
                          spellCheck={false}
                          placeholder={agent.placeholder}
                          value={
                            draft.settings[agent.kind] ??
                            programSettingsYaml(agent.program)
                          }
                          aria-invalid={Boolean(errors[agent.kind])}
                          aria-describedby={`${agent.kind}-settings-help`}
                          onChange={(event) =>
                            setDraft({
                              ...draft,
                              settings: {
                                ...draft.settings,
                                [agent.kind]: event.target.value,
                              },
                            })
                          }
                        />
                        <small
                          id={`${agent.kind}-settings-help`}
                          className={
                            errors[agent.kind] ? "field-error" : undefined
                          }
                        >
                          {errors[agent.kind] ??
                            "Use the agent’s native configuration keys."}
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
              disabled={yaml === null}
              onClick={copyYaml}
            >
              {copied ? <Check size={14} /> : <Copy size={14} />}
              {copied ? "Copied" : "Copy"}
            </button>
          </div>
          {yaml === null ? (
            <p className="error-callout" role="status">
              Fix the additional settings above to generate YAML.
            </p>
          ) : (
            // biome-ignore lint/a11y/noNoninteractiveTabindex: The YAML preview scrolls and must be keyboard-accessible.
            <pre tabIndex={0}>
              <code>{yaml}</code>
            </pre>
          )}
        </section>
      </div>
    </div>
  );
}

const configurableAgents: Array<{
  kind: AgentKind;
  iconKind: string;
  placeholder: string;
  initialSettings?: Record<string, unknown>;
}> = [
  {
    kind: "claudeCode",
    iconKind: "claude-code",
    placeholder: "permissions:\n  defaultMode: plan",
  },
  {
    kind: "claudeDesktop",
    iconKind: "claude-desktop",
    placeholder: "isLocalDevMcpEnabled: true",
  },
  {
    kind: "codex",
    iconKind: "codex",
    placeholder: "managedConfig:\n  model_reasoning_effort: high",
  },
  {
    kind: "openCode",
    iconKind: "opencode",
    placeholder: "managedConfig:\n  autoupdate: false",
    initialSettings: {
      model: "gpt-5.6-terra",
      models: { "gpt-5.6-terra": { name: "GPT 5.6 Terra" } },
    },
  },
  {
    kind: "grok",
    iconKind: "grok",
    placeholder: "model: grok-4.6",
    initialSettings: { model: "grok-4.6" },
  },
];

const sandboxUnsupportedAgents = new Set<AgentKind>([
  "claudeDesktop",
  "openCode",
  "grok",
]);
