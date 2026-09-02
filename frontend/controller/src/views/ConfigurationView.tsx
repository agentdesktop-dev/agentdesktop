import { ToolIcon } from "@agentdesktop/ui";
import { Check, ChevronRight, Copy, Plus, Save, Trash2 } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Document, isMap, isNode, parseDocument } from "yaml";

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
  const sourceConfiguration = useRef<ConfigurationSource | null>(null);
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
  const [saveMessage, setSaveMessage] = useState<string | null>(null);
  const [hydrationError, setHydrationError] = useState<string | null>(null);
  const draft = daemonConfigYaml(
    {
      gateway,
      gatewayUrl,
      controllerJwt,
      audience,
      allowedClientIds,
      preservedAuthentication,
      sessionNewTelemetry,
      toolUseTelemetry,
      toolInputTelemetry,
      agents,
    },
    sourceConfiguration.current,
  );
  const yaml =
    !hydrated && initialYaml !== undefined
      ? (initialYaml ?? draft.yaml)
      : hydrationError && initialYaml
        ? initialYaml
        : draft.yaml;
  const availableAgents = configurableAgents.filter(
    (candidate) => !agents.some((agent) => agent.kind === candidate.kind),
  );

  useEffect(() => {
    if (initializedFromController.current || initialYaml === undefined) {
      return;
    }
    initializedFromController.current = true;
    if (!initialYaml) {
      setHydrated(true);
      return;
    }
    try {
      const source = parseConfigurationSource(initialYaml);
      const document = source.document.toJS() as DaemonConfigDocument;
      sourceConfiguration.current = source;

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
      setToolUseTelemetry(
        events.has("tool.use") || events.has("tool.use.input"),
      );
      setToolInputTelemetry(events.has("tool.use.input"));
      setAgents(agentDrafts(source.document, document.programs));
    } catch (error) {
      sourceConfiguration.current = null;
      setHydrationError(errorMessage(error));
      setHydrated(true);
      return;
    }
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
    if (!onSave || !version || hydrationError || draft.error) return;
    setSaving(true);
    setSaveError(null);
    setSavedYaml(null);
    setSaveMessage(null);
    try {
      const previousRevision = revision;
      const result = await onSave(yaml, version);
      sourceConfiguration.current = parseConfigurationSource(yaml);
      setVersion(result.version);
      setRevision(result.revision);
      setSavedYaml(yaml);
      setSaveMessage(
        result.revision === previousRevision
          ? "No changes to roll out."
          : result.revision === null
            ? "Configuration saved."
            : `Revision ${result.revision} saved and queued for rollout.`,
      );
    } catch (error) {
      setSaveError(errorMessage(error));
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
      {hydrationError && (
        <p className="error-callout" role="alert">
          The active configuration cannot be edited: {hydrationError}
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
                  {!controllerJwt && preservedAuthentication && (
                    <p className="configuration-note full-width">
                      Existing OIDC authentication is preserved.
                    </p>
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
                  const settingsError =
                    draft.error?.kind === agent.kind
                      ? draft.error.message
                      : null;
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
                          aria-invalid={settingsError ? true : undefined}
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
                        {settingsError ? (
                          <small className="field-error" role="alert">
                            {settingsError}
                          </small>
                        ) : (
                          <small>
                            Use the agent’s native configuration keys.
                          </small>
                        )}
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
                disabled={Boolean(hydrationError || draft.error)}
                onClick={copyYaml}
              >
                {copied ? <Check size={14} /> : <Copy size={14} />}
                {copied ? "Copied" : "Copy"}
              </button>
              {writable && (
                <button
                  type="button"
                  className="button primary"
                  disabled={
                    saving ||
                    !version ||
                    !hydrated ||
                    Boolean(hydrationError || draft.error)
                  }
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
          {savedYaml === yaml && saveMessage && (
            <p className="configuration-save-message success" role="status">
              {saveMessage}
            </p>
          )}
          {draft.error && (
            <p className="configuration-save-message error" role="alert">
              Fix the invalid agent settings to generate a configuration.
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

type ConfigurationSource = {
  document: Document;
  yaml: string;
};

type ConfigurationDraft = {
  yaml: string;
  error: { kind: AgentKind; message: string } | null;
};

type ParsedAgentSettings = { document: Document } | { error: string };

function daemonConfigYaml(
  options: {
    gateway: boolean;
    gatewayUrl: string;
    controllerJwt: boolean;
    audience: string;
    allowedClientIds: string[];
    preservedAuthentication?: Record<string, unknown>;
    sessionNewTelemetry: boolean;
    toolUseTelemetry: boolean;
    toolInputTelemetry: boolean;
    agents: AgentDraft[];
  },
  source: ConfigurationSource | null,
): ConfigurationDraft {
  const programDocuments = new Map<AgentKind, Document>();
  for (const agent of options.agents) {
    const definition = configurableAgents.find(
      (candidate) => candidate.kind === agent.kind,
    );
    const parsed = parseAgentSettings(agent, definition?.label ?? agent.kind);
    if ("error" in parsed) {
      return {
        yaml: source?.yaml ?? "",
        error: { kind: agent.kind, message: parsed.error },
      };
    }
    if (!agent.useGateway) {
      parsed.document.set("useLlmGateway", false);
    } else {
      parsed.document.delete("useLlmGateway");
    }
    programDocuments.set(agent.kind, parsed.document);
  }

  const document = source?.document.clone() ?? new Document({});
  let changed = false;
  const setValue = (path: string[], value: unknown, node = value) => {
    if (yamlValuesEqual(documentValue(document, path), value)) return;
    document.setIn(path, isNode(node) ? node : document.createNode(node));
    changed = true;
  };
  const deleteValue = (path: string[]) => {
    if (document.deleteIn(path)) changed = true;
  };

  if (options.gateway) {
    if (!isMap(document.get("llmGateway", true))) {
      setValue(["llmGateway"], {});
    }
    setValue(["llmGateway", "url"], options.gatewayUrl);
    if (options.controllerJwt) {
      if (
        documentValue(document, ["llmGateway", "authentication", "type"]) !==
        "controllerJwt"
      ) {
        setValue(["llmGateway", "authentication"], {
          type: "controllerJwt",
          audience: options.audience,
          allowedClientIds: options.allowedClientIds,
        });
      } else {
        setValue(
          ["llmGateway", "authentication", "audience"],
          options.audience,
        );
        setValue(
          ["llmGateway", "authentication", "allowedClientIds"],
          options.allowedClientIds,
        );
      }
    } else if (options.preservedAuthentication) {
      setValue(
        ["llmGateway", "authentication"],
        options.preservedAuthentication,
      );
    } else {
      deleteValue(["llmGateway", "authentication"]);
    }
  } else {
    deleteValue(["llmGateway"]);
  }

  if (options.sessionNewTelemetry || options.toolUseTelemetry) {
    const events: string[] = [];
    if (options.sessionNewTelemetry) events.push("session.new");
    if (options.toolUseTelemetry) {
      events.push(options.toolInputTelemetry ? "tool.use.input" : "tool.use");
    }
    if (
      !stringSetsEqual(documentValue(document, ["telemetry", "events"]), events)
    ) {
      setValue(["telemetry", "events"], events);
    }
  } else {
    const currentEvents = documentValue(document, ["telemetry", "events"]);
    if (Array.isArray(currentEvents) && currentEvents.length > 0) {
      deleteValue(["telemetry"]);
    }
  }

  for (const { kind } of configurableAgents) {
    const programDocument = programDocuments.get(kind);
    if (!programDocument) {
      deleteValue(["programs", kind]);
      continue;
    }
    const contents = programDocument.contents;
    const current = documentValue(document, ["programs", kind]);
    const normalizedCurrent =
      current && typeof current === "object" && !Array.isArray(current)
        ? { ...(current as Record<string, unknown>) }
        : current;
    if (
      normalizedCurrent &&
      typeof normalizedCurrent === "object" &&
      !Array.isArray(normalizedCurrent) &&
      (normalizedCurrent as Record<string, unknown>).useLlmGateway === true
    ) {
      delete (normalizedCurrent as Record<string, unknown>).useLlmGateway;
    }
    if (!yamlValuesEqual(normalizedCurrent, programDocument.toJS())) {
      setValue(
        ["programs", kind],
        programDocument.toJS(),
        contents?.clone() ?? {},
      );
    }
  }
  if (options.agents.length === 0 && !source) setValue(["programs"], {});

  return {
    yaml:
      source && !changed ? source.yaml : document.toString({ lineWidth: 0 }),
    error: null,
  };
}

function agentDrafts(
  document: Document,
  programs: DaemonConfigDocument["programs"],
): AgentDraft[] {
  if (!programs) return [];
  return configurableAgents.flatMap(({ kind }) => {
    const program = programs[kind];
    if (!program) return [];
    const { useLlmGateway } = program;
    return [
      {
        kind,
        useGateway: useLlmGateway !== false,
        settings: agentSettingsYaml(document, kind),
      },
    ];
  });
}

function agentSettingsYaml(document: Document, kind: AgentKind) {
  const program = document.getIn(["programs", kind], true);
  if (!isMap(program)) return "";
  const settings = new Document(program.clone());
  settings.delete("useLlmGateway");
  return isMap(settings.contents) && settings.contents.items.length > 0
    ? settings.toString({ lineWidth: 0 }).trimEnd()
    : "";
}

function parseAgentSettings(
  agent: AgentDraft,
  label: string,
): ParsedAgentSettings {
  const document = agent.settings.trim()
    ? parseDocument(agent.settings, { intAsBigInt: true })
    : new Document({});
  if (document.errors.length > 0) {
    return { error: `${label}: ${document.errors[0].message}` };
  }
  if (!isMap(document.contents)) {
    return { error: `${label}: additional settings must be a YAML mapping.` };
  }
  return { document };
}

function parseConfigurationSource(yaml: string): ConfigurationSource {
  const document = parseDocument(yaml, { intAsBigInt: true });
  if (document.errors.length > 0) throw document.errors[0];
  if (!isMap(document.contents)) {
    throw new Error("Fleet configuration must be a YAML mapping.");
  }
  return { document, yaml };
}

function documentValue(document: Document, path: string[]) {
  const value = document.getIn(path, true);
  return isNode(value) ? value.toJS(document) : value;
}

function yamlValuesEqual(left: unknown, right: unknown): boolean {
  if (Object.is(left, right)) return true;
  if (Array.isArray(left) || Array.isArray(right)) {
    return (
      Array.isArray(left) &&
      Array.isArray(right) &&
      left.length === right.length &&
      left.every((value, index) => yamlValuesEqual(value, right[index]))
    );
  }
  if (
    !left ||
    !right ||
    typeof left !== "object" ||
    typeof right !== "object"
  ) {
    return false;
  }
  const leftRecord = left as Record<string, unknown>;
  const rightRecord = right as Record<string, unknown>;
  const leftKeys = Object.keys(leftRecord);
  const rightKeys = Object.keys(rightRecord);
  return (
    leftKeys.length === rightKeys.length &&
    leftKeys.every(
      (key) =>
        Object.hasOwn(rightRecord, key) &&
        yamlValuesEqual(leftRecord[key], rightRecord[key]),
    )
  );
}

function stringSetsEqual(left: unknown, right: string[]) {
  return (
    Array.isArray(left) &&
    left.every((value) => typeof value === "string") &&
    new Set(left).size === new Set(right).size &&
    left.every((value) => right.includes(value))
  );
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : "Save failed";
}
