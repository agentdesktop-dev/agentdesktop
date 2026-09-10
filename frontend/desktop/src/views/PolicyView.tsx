import {
  AlertCircle,
  Check,
  LoaderCircle,
  Plus,
  ShieldCheck,
  Trash2,
  X,
} from "lucide-react";
import { useState } from "react";

import type {
  AccessCapability,
  AgentAccessReport,
  NetworkRuleChange,
  NetworkRuleDecision,
} from "../types";

type EditableNetworkCapability = AccessCapability & {
  decision: NetworkRuleDecision;
  rule: NonNullable<AccessCapability["rule"]>;
};
type ExistingRuleDraft = NetworkRuleDecision | "remove";
type NewRuleDraft = {
  decision: NetworkRuleDecision;
  id: string;
  resource: string;
};

const decisionLabels: Record<NetworkRuleDecision, string> = {
  allow: "Always allow",
  ask: "Ask first",
  deny: "Block",
};

function isNetworkDecision(
  decision: AccessCapability["decision"],
): decision is NetworkRuleDecision {
  return decision === "allow" || decision === "ask" || decision === "deny";
}

function editableNetworkCapability(
  capability: AccessCapability,
): capability is EditableNetworkCapability {
  return (
    capability.category === "network" &&
    Boolean(capability.rule) &&
    isNetworkDecision(capability.decision)
  );
}

function networkDecisionOptions(
  agentKind: string,
  mechanism?: NonNullable<AccessCapability["rule"]>["mechanism"],
): NetworkRuleDecision[] {
  if (agentKind === "vscode" || mechanism === "vscodeUrlAutoApprove") {
    return ["allow", "ask"];
  }
  if (mechanism === "claudeSandboxDomain") return ["allow", "deny"];
  return ["allow", "ask", "deny"];
}

function mechanismLabel(
  agentKind: string,
  mechanism?: NonNullable<AccessCapability["rule"]>["mechanism"],
): string {
  switch (mechanism) {
    case "vscodeUrlAutoApprove":
      return "URL approval";
    case "claudePermission":
      return "Web request";
    case "claudeSandboxDomain":
      return "Sandbox";
    default:
      return agentKind === "vscode" ? "URL approval" : "Web request";
  }
}

function scopeLabel(resource: string): string {
  if (resource === "*") return "All destinations";
  return resource.startsWith("*.") ? "All subdomains" : "Exact host";
}

function configurationPaths(agent: AgentAccessReport): string[] {
  const paths = new Set<string>();
  for (const source of [
    ...(agent.capabilities ?? []).map((capability) => capability.source),
    ...(agent.findings ?? []).map((finding) => finding.source),
  ]) {
    if (source?.kind === "configuration" && source.path) {
      paths.add(source.path);
    }
  }
  return [...paths].sort();
}

function resourceValidationError(resource: string): string | null {
  const value = resource.trim().toLowerCase();
  if (!value) return "Enter a host or wildcard domain";
  if (value.length > 255) return "Network destination is too long";
  if (value === "*") return null;
  const host = value.startsWith("*.") ? value.slice(2) : value;
  if (!host || value.includes("://") || /[/@:#?]/.test(host)) {
    return "Enter only a host or leading wildcard, without a URL or port";
  }
  const valid = host
    .split(".")
    .every(
      (label) =>
        label.length > 0 &&
        label.length <= 63 &&
        /^[a-z0-9-]+$/.test(label) &&
        !label.startsWith("-") &&
        !label.endsWith("-"),
    );
  return valid ? null : "Network destination contains an invalid host label";
}

function DecisionControl({
  decision,
  label,
  onChange,
  options,
}: {
  decision: NetworkRuleDecision;
  label: string;
  onChange: (decision: NetworkRuleDecision) => void;
  options: NetworkRuleDecision[];
}) {
  return (
    <fieldset
      aria-label={label}
      className="policy-decision-control"
      style={{
        gridTemplateColumns: `repeat(${options.length}, minmax(52px, 1fr))`,
      }}
    >
      {options.map((option) => (
        <button
          aria-pressed={decision === option}
          className={decision === option ? "active" : undefined}
          key={option}
          onClick={() => onChange(option)}
          type="button"
        >
          {option === "allow" ? "Allow" : option === "ask" ? "Ask" : "Block"}
        </button>
      ))}
    </fieldset>
  );
}

interface AgentAccessEditorProps {
  agent?: AgentAccessReport;
  loading?: boolean;
  onApplyNetworkRuleChange?: (change: NetworkRuleChange) => Promise<void>;
}

export function AgentAccessEditor({
  agent,
  loading = false,
  onApplyNetworkRuleChange,
}: AgentAccessEditorProps) {
  const [drafts, setDrafts] = useState<Record<string, ExistingRuleDraft>>({});
  const [newRules, setNewRules] = useState<NewRuleDraft[]>([]);
  const [newResource, setNewResource] = useState("");
  const [applying, setApplying] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  if (loading && !agent) {
    return (
      <div className="agent-policy-state" role="status">
        <LoaderCircle className="spin" size={17} aria-hidden="true" />
        <div>
          <strong>Loading access controls</strong>
          <span>Checking editable agent settings.</span>
        </div>
      </div>
    );
  }
  if (!agent) {
    return (
      <div className="agent-policy-state">
        <AlertCircle size={17} aria-hidden="true" />
        <div>
          <strong>Access controls unavailable</strong>
          <span>Refresh the local access audit to try again.</span>
        </div>
      </div>
    );
  }

  const agentKind = agent.kind;
  const rules = (agent.capabilities ?? []).filter(editableNetworkCapability);
  const readOnlyConfiguredCount = (agent.capabilities ?? []).filter(
    (capability) =>
      capability.category === "network" &&
      capability.source.kind === "configuration" &&
      !capability.rule,
  ).length;
  const pendingCount = Object.keys(drafts).length + newRules.length;
  const paths = configurationPaths(agent);

  function stageDecision(
    capability: EditableNetworkCapability,
    decision: NetworkRuleDecision,
  ) {
    setSaved(false);
    setError(null);
    setDrafts((current) => {
      const next = { ...current };
      if (decision === capability.decision) {
        delete next[capability.rule.id];
      } else {
        next[capability.rule.id] = decision;
      }
      return next;
    });
  }

  function addRule() {
    const resource = newResource.trim().toLowerCase();
    const validationError = resourceValidationError(resource);
    if (validationError) {
      setError(validationError);
      return;
    }
    if (
      rules.some((rule) => rule.resource.toLowerCase() === resource) ||
      newRules.some((rule) => rule.resource === resource)
    ) {
      setError("A network rule already covers this destination");
      return;
    }
    setNewRules((current) => [
      ...current,
      {
        decision: "ask",
        id: `new-${Date.now()}-${resource}`,
        resource,
      },
    ]);
    setNewResource("");
    setError(null);
    setSaved(false);
  }

  function discardChanges() {
    setDrafts({});
    setNewRules([]);
    setError(null);
  }

  async function applyChanges() {
    setApplying(true);
    setError(null);
    setSaved(false);
    const appliedExisting = new Set<string>();
    const appliedNew = new Set<string>();
    let appliedCount = 0;
    try {
      if (
        (Object.keys(drafts).length || newRules.length) &&
        !onApplyNetworkRuleChange
      ) {
        throw new Error("Network settings cannot be changed right now");
      }
      const existingChanges = rules
        .map((capability) => ({
          capability,
          draft: drafts[capability.rule.id],
        }))
        .filter(
          (
            entry,
          ): entry is {
            capability: EditableNetworkCapability;
            draft: ExistingRuleDraft;
          } => Boolean(entry.draft),
        )
        .sort(
          (left, right) =>
            Number(right.draft === "remove") - Number(left.draft === "remove"),
        );
      for (const { capability, draft } of existingChanges) {
        const change: NetworkRuleChange =
          draft === "remove"
            ? {
                agentKind,
                operation: "remove",
                ruleId: capability.rule.id,
              }
            : {
                agentKind,
                operation: "setDecision",
                ruleId: capability.rule.id,
                decision: draft,
              };
        await onApplyNetworkRuleChange?.(change);
        appliedExisting.add(capability.rule.id);
        appliedCount += 1;
      }
      for (const rule of newRules) {
        await onApplyNetworkRuleChange?.({
          agentKind,
          operation: "add",
          resource: rule.resource,
          decision: rule.decision,
        });
        appliedNew.add(rule.id);
        appliedCount += 1;
      }
      setDrafts({});
      setNewRules([]);
      setSaved(true);
    } catch (reason: unknown) {
      setDrafts((current) =>
        Object.fromEntries(
          Object.entries(current).filter(([id]) => !appliedExisting.has(id)),
        ),
      );
      setNewRules((current) =>
        current.filter((rule) => !appliedNew.has(rule.id)),
      );
      const message = reason instanceof Error ? reason.message : String(reason);
      setError(
        appliedCount
          ? `${appliedCount} ${appliedCount === 1 ? "change was" : "changes were"} saved before the error. Review the remaining changes. ${message}`
          : message,
      );
    } finally {
      setApplying(false);
    }
  }

  const changeSummaries = [
    ...rules.flatMap((rule) => {
      const draft = drafts[rule.rule.id];
      if (!draft) return [];
      return [
        draft === "remove"
          ? `Remove ${rule.resource}`
          : `${rule.resource}: ${decisionLabels[draft]}`,
      ];
    }),
    ...newRules.map(
      (rule) => `Add ${rule.resource}: ${decisionLabels[rule.decision]}`,
    ),
  ];

  return (
    <div className="agent-policy-panel">
      {saved ? (
        <div className="policy-success" role="status">
          <ShieldCheck size={16} aria-hidden="true" />
          <span>
            <strong>Access updated</strong>
            Access was checked again against the saved settings.
          </span>
          <button
            aria-label="Dismiss"
            onClick={() => setSaved(false)}
            type="button"
          >
            <X size={14} aria-hidden="true" />
          </button>
        </div>
      ) : null}

      <section className="policy-destinations">
        <div className="policy-section-heading">
          <div>
            <h3>Configured destinations</h3>
            <p>
              Edit supported rules. Other configured and observed access appears
              below.
            </p>
          </div>
          <div className="policy-section-meta">
            {pendingCount ? (
              <span className="policy-pending-badge">
                {pendingCount} pending
              </span>
            ) : null}
            <span>{rules.length + newRules.length} editable</span>
          </div>
        </div>
        {pendingCount ? (
          <div className="policy-change-bar">
            <span>
              <strong>
                {pendingCount} pending{" "}
                {pendingCount === 1 ? "change" : "changes"}
              </strong>
              <small>Nothing is written until you apply.</small>
            </span>
            <details>
              <summary>Review</summary>
              <div>
                {paths[0] ? <code>{paths[0]}</code> : null}
                {changeSummaries.map((summary) => (
                  <span key={summary}>{summary}</span>
                ))}
              </div>
            </details>
            <button
              className="secondary"
              disabled={applying}
              onClick={discardChanges}
              type="button"
            >
              Discard
            </button>
            <button
              className="primary"
              disabled={applying}
              onClick={applyChanges}
              type="button"
            >
              {applying ? (
                <LoaderCircle className="spin" size={14} aria-hidden="true" />
              ) : (
                <Check size={14} aria-hidden="true" />
              )}
              {applying ? "Applying" : "Apply changes"}
            </button>
          </div>
        ) : null}
        <div className="policy-rule-table">
          <div className="policy-rule-header" aria-hidden="true">
            <span>Destination</span>
            <span>Scope</span>
            <span>Access</span>
            <span />
          </div>
          {!rules.length && !newRules.length ? (
            <div className="policy-rule-empty">
              {readOnlyConfiguredCount ? (
                <>
                  <strong>
                    {readOnlyConfiguredCount} configured{" "}
                    {readOnlyConfiguredCount === 1
                      ? "destination is"
                      : "destinations are"}{" "}
                    Advanced and read-only here
                  </strong>
                  <span>
                    Advanced rules use separate request and response approvals
                    or URL paths. Use Open configuration to edit them directly.
                  </span>
                </>
              ) : (
                <span>No configured destinations found.</span>
              )}
            </div>
          ) : null}
          {rules.map((rule) => {
            const draft = drafts[rule.rule.id];
            const removed = draft === "remove";
            const decision =
              draft && draft !== "remove" ? draft : rule.decision;
            return (
              <div
                className={`policy-rule-row ${removed ? "removed" : ""}`}
                key={rule.rule.id}
              >
                <span>
                  <code
                    className={removed ? "policy-removed-resource" : undefined}
                    title={rule.resource}
                  >
                    {rule.resource}
                  </code>
                  <small title={rule.source.path}>
                    {mechanismLabel(agent.kind, rule.rule.mechanism)}
                  </small>
                </span>
                <span
                  className={
                    scopeLabel(rule.resource) !== "Exact host"
                      ? "broad"
                      : undefined
                  }
                >
                  {scopeLabel(rule.resource)}
                </span>
                {removed ? (
                  <button
                    className="policy-undo"
                    disabled={applying}
                    onClick={() =>
                      setDrafts((current) => {
                        const next = { ...current };
                        delete next[rule.rule.id];
                        return next;
                      })
                    }
                    type="button"
                  >
                    Keep rule
                  </button>
                ) : (
                  <DecisionControl
                    decision={decision}
                    label={`Access for ${rule.resource}`}
                    onChange={(next) => stageDecision(rule, next)}
                    options={networkDecisionOptions(
                      agent.kind,
                      rule.rule.mechanism,
                    )}
                  />
                )}
                <button
                  aria-label={`Remove ${rule.resource}`}
                  className="policy-icon-button"
                  disabled={applying || removed}
                  onClick={() => {
                    setDrafts((current) => ({
                      ...current,
                      [rule.rule.id]: "remove",
                    }));
                    setSaved(false);
                    setError(null);
                  }}
                  title={`Remove ${rule.resource}`}
                  type="button"
                >
                  <Trash2 size={14} aria-hidden="true" />
                </button>
              </div>
            );
          })}
          {newRules.map((rule) => (
            <div className="policy-rule-row new" key={rule.id}>
              <span>
                <code title={rule.resource}>{rule.resource}</code>
                <small>{mechanismLabel(agent.kind)}</small>
              </span>
              <span
                className={
                  scopeLabel(rule.resource) !== "Exact host"
                    ? "broad"
                    : undefined
                }
              >
                {scopeLabel(rule.resource)}
              </span>
              <DecisionControl
                decision={rule.decision}
                label={`Access for ${rule.resource}`}
                onChange={(decision) =>
                  setNewRules((current) =>
                    current.map((candidate) =>
                      candidate.id === rule.id
                        ? { ...candidate, decision }
                        : candidate,
                    ),
                  )
                }
                options={networkDecisionOptions(agent.kind)}
              />
              <button
                aria-label={`Remove ${rule.resource}`}
                className="policy-icon-button"
                disabled={applying}
                onClick={() =>
                  setNewRules((current) =>
                    current.filter((candidate) => candidate.id !== rule.id),
                  )
                }
                title={`Remove ${rule.resource}`}
                type="button"
              >
                <Trash2 size={14} aria-hidden="true" />
              </button>
            </div>
          ))}
        </div>
        <form
          className="policy-add-rule"
          onSubmit={(event) => {
            event.preventDefault();
            addRule();
          }}
        >
          <label>
            <span>New destination</span>
            <input
              disabled={applying}
              onChange={(event) => {
                setNewResource(event.target.value);
                setError(null);
              }}
              placeholder="api.example.com or *.example.com"
              value={newResource}
            />
          </label>
          <button disabled={applying || !newResource.trim()} type="submit">
            <Plus size={14} aria-hidden="true" />
            Add as Ask first
          </button>
        </form>
      </section>

      {error ? (
        <div className="policy-error" role="alert">
          <AlertCircle size={14} aria-hidden="true" />
          {error}
        </div>
      ) : null}
    </div>
  );
}
