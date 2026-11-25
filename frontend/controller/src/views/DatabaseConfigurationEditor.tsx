import { CircleAlert, RotateCcw, Save } from "lucide-react";
import { useEffect, useState } from "react";
import { lt, valid } from "semver";
import { parseDocument } from "yaml";

import { ApiError } from "../auth";
import { registerNavigationGuard } from "../router";
import type {
  ActiveDaemonConfig,
  DaemonConfigDocument,
  DaemonConfigRevision,
  DaemonConfigRevisionSummary,
  Device,
} from "../types";
import { ConfigurationView } from "./ConfigurationView";

type SaveDaemonConfig = {
  base_revision: number;
  yaml: string;
  message?: string;
};

type DiffLine = { kind: "added" | "removed"; text: string };

export interface DatabaseConfigurationEditorProps {
  active: ActiveDaemonConfig;
  devices: Device[] | null;
  devicesLoading: boolean;
  devicesError: string | null;
  canUpdate: boolean;
  history: DaemonConfigRevisionSummary[];
  historyError: string | null;
  historyLoading: boolean;
  historyHasMore: boolean;
  selectedRevision: number | null;
  selectedRevisionData: DaemonConfigRevision | null;
  selectedRevisionError: string | null;
  selectedRevisionLoading: boolean;
  previousRevisionData: DaemonConfigRevision | null;
  previousRevisionError: string | null;
  previousRevisionLoading: boolean;
  onLoadMore: () => void;
  onSelectRevision: (revision: number | null) => void;
  onSave: (request: SaveDaemonConfig) => Promise<ActiveDaemonConfig>;
  onLoadLatest: () => Promise<ActiveDaemonConfig>;
  onSaved: () => void;
}

export function DatabaseConfigurationEditor({
  active,
  devices,
  devicesLoading,
  devicesError,
  canUpdate,
  history,
  historyError,
  historyLoading,
  historyHasMore,
  selectedRevision,
  selectedRevisionData,
  selectedRevisionError,
  selectedRevisionLoading,
  previousRevisionData,
  previousRevisionError,
  previousRevisionLoading,
  onLoadMore,
  onSelectRevision,
  onSave,
  onLoadLatest,
  onSaved,
}: DatabaseConfigurationEditorProps) {
  const [baseYaml, setBaseYaml] = useState(active.yaml ?? "");
  const [draft, setDraft] = useState(active.yaml ?? "");
  const [baseRevision, setBaseRevision] = useState(active.revision ?? 0);
  const [activeConfig, setActiveConfig] = useState(active);
  const [tab, setTab] = useState<"yaml" | "builder">("yaml");
  const [builderConfig, setBuilderConfig] =
    useState<DaemonConfigDocument | null>(null);
  const [builderError, setBuilderError] = useState<string | null>(null);
  const [builderDirty, setBuilderDirty] = useState(false);
  const [message, setMessage] = useState("");
  const [reviewOpen, setReviewOpen] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [reauthenticate, setReauthenticate] = useState(false);
  const [conflict, setConflict] = useState<ActiveDaemonConfig | null>(null);
  const [savedRevision, setSavedRevision] = useState<number | null>(null);
  const dirty = draft !== baseYaml;
  const hasUnsavedChanges = dirty || builderDirty || Boolean(message.trim());
  const devicesOnline = (devices ?? []).filter((device) => {
    const now = Math.floor(Date.now() / 1000);
    return device.last_seen_at !== null && now - device.last_seen_at <= 90;
  }).length;
  const controllerVersion = valid(active.controller_version ?? "");
  const olderDaemons =
    reviewOpen && controllerVersion
      ? (devices ?? []).filter((device) => {
          const version = valid(device.agent_version);
          return version !== null && lt(version, controllerVersion);
        }).length
      : 0;
  const applied = devices?.filter(
    (device) =>
      device.config_revision === savedRevision && device.config_state === 1,
  ).length;
  const failed = devices?.filter(
    (device) =>
      device.config_revision === savedRevision && device.config_state === 2,
  ).length;
  const waiting =
    devices && savedRevision !== null
      ? devices.length - (applied ?? 0) - (failed ?? 0)
      : null;

  useEffect(
    () =>
      registerNavigationGuard(
        () =>
          !hasUnsavedChanges ||
          window.confirm("Discard your unsaved configuration changes?"),
      ),
    [hasUnsavedChanges],
  );

  useEffect(() => {
    if (!hasUnsavedChanges) return;
    const warnBeforeUnload = (event: BeforeUnloadEvent) => {
      event.preventDefault();
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", warnBeforeUnload);
    return () => window.removeEventListener("beforeunload", warnBeforeUnload);
  }, [hasUnsavedChanges]);

  function openBuilder() {
    if (tab === "builder") return;
    setBuilderError(null);
    setBuilderDirty(false);
    try {
      setBuilderConfig(parseDaemonConfig(draft));
      setTab("builder");
    } catch (reason) {
      setBuilderError(
        reason instanceof Error
          ? reason.message
          : "The YAML cannot be edited in the builder.",
      );
      setTab("builder");
    }
  }

  function applyBuilderYaml(yaml: string, initialYaml: string) {
    try {
      setDraft(mergeBuilderYaml(draft, initialYaml, yaml));
      setTab("yaml");
      setBuilderConfig(null);
      setBuilderDirty(false);
      setBuilderError(null);
      setError(null);
    } catch (reason) {
      setBuilderError(
        reason instanceof Error
          ? reason.message
          : "Could not apply builder changes.",
      );
      return false;
    }
  }

  async function save() {
    if (!canUpdate || !dirty || builderDirty || saving) return;
    const submittedDraft = draft;
    const submittedMessage = message;
    setSaving(true);
    setError(null);
    setReauthenticate(false);
    try {
      const saved = await onSave({
        base_revision: baseRevision,
        yaml: submittedDraft,
        ...(submittedMessage.trim()
          ? { message: submittedMessage.trim() }
          : {}),
      });
      const savedYaml = saved.yaml ?? submittedDraft;
      setBaseYaml(savedYaml);
      setDraft((current) => (current === submittedDraft ? savedYaml : current));
      setBaseRevision(saved.revision ?? baseRevision);
      setActiveConfig(saved);
      setMessage((current) => (current === submittedMessage ? "" : current));
      setReviewOpen(false);
      setConflict(null);
      setSavedRevision(saved.revision);
      onSaved();
    } catch (reason) {
      if (reason instanceof ApiError && reason.status === 409) {
        try {
          setConflict(await onLoadLatest());
          setReviewOpen(false);
        } catch (loadError) {
          setError(
            loadError instanceof Error
              ? `A newer revision exists, but it could not be loaded: ${loadError.message}`
              : "A newer revision exists, but it could not be loaded.",
          );
        }
      } else {
        setError(reason instanceof Error ? reason.message : "Save failed.");
        setReauthenticate(
          reason instanceof ApiError &&
            reason.code === "reauthentication_required",
        );
      }
    } finally {
      setSaving(false);
    }
  }

  function reloadConflict() {
    if (!conflict) return;
    if (!discardBuilderChanges()) return;
    const latestYaml = conflict.yaml ?? "";
    setActiveConfig(conflict);
    setBaseYaml(latestYaml);
    setDraft(latestYaml);
    setTab("yaml");
    setBaseRevision(conflict.revision ?? 0);
    setConflict(null);
    setMessage("");
    setError(null);
  }

  function rebaseDraft() {
    if (!conflict) return;
    setActiveConfig(conflict);
    setBaseYaml(conflict.yaml ?? "");
    setBaseRevision(conflict.revision ?? 0);
    setConflict(null);
    setError(null);
  }

  function restoreRevision(yaml: string) {
    if (!discardBuilderChanges()) return;
    setDraft(yaml);
    setTab("yaml");
    setReviewOpen(false);
    setError(null);
  }

  function discardBuilderChanges() {
    if (
      builderDirty &&
      !window.confirm("Discard your unapplied builder changes?")
    ) {
      return false;
    }
    setBuilderDirty(false);
    setBuilderConfig(null);
    return true;
  }

  return (
    <div className="stack">
      <section className="section-intro config-page-intro">
        <h2>Fleet configuration</h2>
        <span className="config-current-revision">
          {activeConfig.revision === null
            ? "Not configured"
            : `Revision ${activeConfig.revision}`}
        </span>
      </section>

      {error && (
        <p className="config-notice config-error" role="alert">
          <CircleAlert size={14} />
          {error}
          {reauthenticate && (
            <a
              className="text-button"
              href="/auth/login?reauthenticate=true"
              target="_blank"
              rel="noopener noreferrer"
            >
              Sign in again in a new tab, then return to save
            </a>
          )}
        </p>
      )}

      {conflict && (
        <section className="config-conflict" role="alert">
          <div>
            <strong>
              Revision {conflict.revision} is newer than your draft.
            </strong>
            <p>
              {conflict.author?.display ??
                conflict.author?.subject ??
                "Another admin"}
              {conflict.created_at
                ? ` · ${formatTimestamp(conflict.created_at)}`
                : ""}
            </p>
          </div>
          <div className="config-conflict-actions">
            <button
              type="button"
              className="button secondary"
              onClick={reloadConflict}
            >
              Reload latest
            </button>
            <button
              type="button"
              className="button secondary"
              onClick={rebaseDraft}
            >
              Keep my draft
            </button>
          </div>
          <DiffView before={conflict.yaml ?? ""} after={draft} />
        </section>
      )}

      <div className="configuration-workspace">
        <div className="config-editor-column">
          <section className="config-editor">
            <header className="config-editor-header">
              <div
                className="config-tabs"
                role="tablist"
                aria-label="Configuration editor"
              >
                <button
                  type="button"
                  role="tab"
                  id="database-config-tab-yaml"
                  aria-controls="database-config-panel"
                  aria-selected={tab === "yaml"}
                  className={tab === "yaml" ? "active" : ""}
                  onClick={() => {
                    if (tab === "builder" && !discardBuilderChanges()) return;
                    setTab("yaml");
                  }}
                >
                  YAML
                </button>
                {canUpdate && (
                  <button
                    type="button"
                    role="tab"
                    id="database-config-tab-builder"
                    aria-controls="database-config-panel"
                    aria-selected={tab === "builder"}
                    className={tab === "builder" ? "active" : ""}
                    onClick={openBuilder}
                  >
                    Builder
                  </button>
                )}
              </div>
              {(dirty || builderDirty) && (
                <span className="config-draft-state dirty" aria-live="polite">
                  Unsaved
                </span>
              )}
            </header>

            <div
              id="database-config-panel"
              role="tabpanel"
              aria-labelledby={`database-config-tab-${tab}`}
            >
              {tab === "yaml" ? (
                <label className="config-yaml-editor">
                  <span className="sr-only">Daemon configuration YAML</span>
                  <textarea
                    aria-label="Daemon configuration YAML"
                    spellCheck={false}
                    value={draft}
                    readOnly={!canUpdate}
                    onChange={(event) => {
                      setDraft(event.target.value);
                      setError(null);
                    }}
                    placeholder={"programs:\n  claudeCode: {}"}
                  />
                </label>
              ) : builderConfig ? (
                <>
                  {builderError && (
                    <p
                      className="config-notice config-editor-error"
                      role="alert"
                    >
                      <CircleAlert size={14} />
                      {builderError}
                    </p>
                  )}
                  <ConfigurationView
                    embedded
                    initialConfig={builderConfig}
                    onCopy={applyBuilderYaml}
                    onDirtyChange={setBuilderDirty}
                    copyLabel="Apply to draft"
                    copySuccessLabel="Applied"
                  />
                </>
              ) : (
                <p className="config-notice config-editor-error" role="alert">
                  <CircleAlert size={14} />
                  {builderError ?? "The builder is unavailable for this YAML."}
                </p>
              )}
            </div>

            {canUpdate && (
              <footer className="config-editor-footer">
                <button
                  type="button"
                  className="button primary"
                  disabled={!dirty || builderDirty || saving}
                  onClick={() => setReviewOpen((open) => !open)}
                >
                  <Save size={14} />
                  Review changes
                </button>
              </footer>
            )}
          </section>

          {reviewOpen && canUpdate && (
            <section className="config-review">
              <div className="config-review-heading">
                <div>
                  <h3>Review</h3>
                  <p>
                    {devicesLoading
                      ? "Checking device status…"
                      : devicesError
                        ? `Device status unavailable: ${devicesError} Saving still updates connected devices.`
                        : `${devicesOnline} online · updates now`}
                  </p>
                </div>
                <button
                  type="button"
                  className="text-button"
                  onClick={() => setReviewOpen(false)}
                >
                  Close
                </button>
              </div>
              {olderDaemons > 0 && (
                <p className="config-review-warning" role="status">
                  <CircleAlert
                    className="config-review-warning-icon"
                    size={14}
                    aria-hidden="true"
                  />
                  {olderDaemons === 1
                    ? "1 enrolled daemon is older than this controller and may not understand the new configuration."
                    : `${olderDaemons} enrolled daemons are older than this controller and may not understand the new configuration.`}
                </p>
              )}
              <DiffView before={baseYaml} after={draft} />
              <details className="config-message-disclosure">
                <summary>
                  {message.trim() ? "Edit revision note" : "Add revision note"}
                </summary>
                <label className="field config-message-field">
                  <span className="sr-only">Revision note</span>
                  <input
                    maxLength={1024}
                    value={message}
                    onChange={(event) => setMessage(event.target.value)}
                    placeholder="Optional"
                  />
                </label>
              </details>
              <div className="config-review-actions">
                <button
                  type="button"
                  className="button secondary"
                  onClick={() => setReviewOpen(false)}
                >
                  Back to editing
                </button>
                <button
                  type="button"
                  className="button primary"
                  disabled={saving || builderDirty || !dirty}
                  onClick={() => void save()}
                >
                  {saving ? "Saving…" : "Save configuration"}
                </button>
              </div>
            </section>
          )}

          {savedRevision !== null && (
            <section className="config-rollout" aria-live="polite">
              <strong>Revision {savedRevision} saved</strong>
              {devices && (
                <div>
                  <span>{applied ?? 0} applied</span>
                  <span>{failed ?? 0} failed</span>
                  <span>{waiting ?? 0} not reported</span>
                </div>
              )}
              {devicesError && (
                <p className="config-history-error">
                  Could not refresh device status: {devicesError}
                </p>
              )}
            </section>
          )}
        </div>

        <aside
          className="config-history"
          aria-label="Configuration revision history"
        >
          <header className="config-history-header">
            <h3>Revision history</h3>
          </header>
          {historyError && (
            <p className="config-history-error">{historyError}</p>
          )}
          {!historyError && history.length === 0 && !historyLoading && (
            <p className="config-history-empty">No saved revisions.</p>
          )}
          <ol className="config-history-list">
            {history.map((revision) => (
              <li key={revision.revision}>
                <button
                  type="button"
                  className={
                    selectedRevision === revision.revision ? "selected" : ""
                  }
                  onClick={() =>
                    onSelectRevision(
                      selectedRevision === revision.revision
                        ? null
                        : revision.revision,
                    )
                  }
                >
                  <strong>Revision {revision.revision}</strong>
                  {revision.message && (
                    <small
                      className="config-history-message"
                      title={revision.message}
                    >
                      {revision.message}
                    </small>
                  )}
                  <span className="config-history-meta">
                    <span>
                      {revision.author.display ?? revision.author.subject}
                    </span>
                    <time
                      dateTime={new Date(
                        revision.created_at * 1000,
                      ).toISOString()}
                    >
                      {formatTimestamp(revision.created_at)}
                    </time>
                  </span>
                </button>
              </li>
            ))}
          </ol>
          {historyLoading && (
            <p className="config-history-empty">Loading history…</p>
          )}
          {historyHasMore && !historyLoading && (
            <button
              type="button"
              className="text-button config-history-more"
              onClick={onLoadMore}
            >
              Load earlier revisions
            </button>
          )}
          {selectedRevision !== null && (
            <section className="config-history-detail">
              <div className="config-history-detail-heading">
                <h4>Revision {selectedRevision}</h4>
                {selectedRevision === 1 ? (
                  <span>Initial</span>
                ) : (
                  previousRevisionData && (
                    <span>vs. r{previousRevisionData.revision}</span>
                  )
                )}
              </div>
              {selectedRevisionError && (
                <p className="config-history-error">{selectedRevisionError}</p>
              )}
              {selectedRevisionLoading && <p>Loading revision…</p>}
              {selectedRevisionData && (
                <>
                  {selectedRevision === 1 ? (
                    <DiffView before="" after={selectedRevisionData.yaml} />
                  ) : previousRevisionLoading ? (
                    <p>Loading revision {selectedRevision - 1}…</p>
                  ) : previousRevisionError ? (
                    <p className="config-history-error" role="alert">
                      Could not load the previous revision:{" "}
                      {previousRevisionError}
                    </p>
                  ) : previousRevisionData ? (
                    <DiffView
                      before={previousRevisionData.yaml}
                      after={selectedRevisionData.yaml}
                    />
                  ) : (
                    <p className="config-history-error" role="alert">
                      The previous revision is unavailable.
                    </p>
                  )}
                  <details>
                    <summary>View YAML</summary>
                    <pre className="config-history-yaml">
                      <code>{selectedRevisionData.yaml}</code>
                    </pre>
                  </details>
                  {canUpdate && (
                    <button
                      type="button"
                      className="button secondary"
                      onClick={() => restoreRevision(selectedRevisionData.yaml)}
                    >
                      <RotateCcw size={14} /> Restore to draft
                    </button>
                  )}
                </>
              )}
            </section>
          )}
        </aside>
      </div>
    </div>
  );
}

function DiffView({ before, after }: { before: string; after: string }) {
  const lines = lineDiff(before, after);
  const occurrences = new Map<string, number>();
  return lines.length === 0 ? (
    <p className="config-diff-empty">No content changes.</p>
  ) : (
    <pre className="config-diff">
      <code>
        {lines.map((line) => {
          const baseKey = `${line.kind}-${line.text}`;
          const occurrence = occurrences.get(baseKey) ?? 0;
          occurrences.set(baseKey, occurrence + 1);
          return (
            <span className={line.kind} key={`${baseKey}-${occurrence}`}>
              {line.kind === "added" ? "+" : "-"} {line.text}
              {"\n"}
            </span>
          );
        })}
      </code>
    </pre>
  );
}

function lineDiff(before: string, after: string): DiffLine[] {
  if (before === after) return [];
  const oldLines = before.split("\n");
  const newLines = after.split("\n");
  let prefix = 0;
  while (
    prefix < oldLines.length &&
    prefix < newLines.length &&
    oldLines[prefix] === newLines[prefix]
  ) {
    prefix += 1;
  }
  let suffix = 0;
  while (
    suffix < oldLines.length - prefix &&
    suffix < newLines.length - prefix &&
    oldLines[oldLines.length - suffix - 1] ===
      newLines[newLines.length - suffix - 1]
  ) {
    suffix += 1;
  }
  return [
    ...oldLines
      .slice(prefix, oldLines.length - suffix)
      .map((text) => ({ kind: "removed" as const, text })),
    ...newLines
      .slice(prefix, newLines.length - suffix)
      .map((text) => ({ kind: "added" as const, text })),
  ];
}

function parseDaemonConfig(yaml: string): DaemonConfigDocument {
  const document = parseDocument(yaml);
  if (document.errors.length > 0) {
    throw new Error(document.errors.map((error) => error.message).join("\n"));
  }
  const value: unknown = document.toJS();
  if (value === null) return {};
  if (typeof value !== "object" || Array.isArray(value)) {
    throw new Error("The YAML root must be a mapping.");
  }
  const ancestors = new WeakSet<object>();
  const visited = new WeakSet<object>();
  const checkCycles = (item: unknown) => {
    if (item === null || typeof item !== "object") return;
    if (ancestors.has(item)) {
      throw new Error("Cyclic YAML aliases cannot be edited in the builder.");
    }
    if (visited.has(item)) return;
    ancestors.add(item);
    for (const child of Object.values(item)) checkCycles(child);
    ancestors.delete(item);
    visited.add(item);
  };
  checkCycles(value);
  const root = value as Record<string, unknown>;
  const mapping = (value: unknown, path: string): Record<string, unknown> => {
    if (value === null || typeof value !== "object" || Array.isArray(value)) {
      throw new Error(`${path} must be a mapping to use the builder.`);
    }
    return value as Record<string, unknown>;
  };
  const optionalMapping = (value: unknown, path: string) =>
    value === undefined || value === null ? undefined : mapping(value, path);
  const optionalString = (value: unknown, path: string) => {
    if (value !== undefined && value !== null && typeof value !== "string") {
      throw new Error(`${path} must be a string to use the builder.`);
    }
  };
  const optionalStringList = (value: unknown, path: string) => {
    if (
      value !== undefined &&
      value !== null &&
      (!Array.isArray(value) ||
        !value.every((item) => typeof item === "string"))
    ) {
      throw new Error(`${path} must be a list of strings to use the builder.`);
    }
  };
  const gateway = optionalMapping(root.llmGateway, "llmGateway");
  if (gateway) {
    if (typeof gateway.url !== "string") {
      throw new Error("llmGateway.url must be a string to use the builder.");
    }
    optionalString(gateway.proxyUrl, "llmGateway.proxyUrl");
    const auth = optionalMapping(
      gateway.authentication,
      "llmGateway.authentication",
    );
    if (auth) {
      optionalString(auth.type, "llmGateway.authentication.type");
      optionalString(auth.audience, "llmGateway.authentication.audience");
    }
  }
  const sandbox = optionalMapping(root.sandbox, "sandbox");
  const network = optionalMapping(sandbox?.network, "sandbox.network");
  optionalStringList(network?.allowedDomains, "sandbox.network.allowedDomains");
  const filesystem = optionalMapping(sandbox?.filesystem, "sandbox.filesystem");
  optionalStringList(filesystem?.writable, "sandbox.filesystem.writable");
  optionalStringList(filesystem?.denied, "sandbox.filesystem.denied");
  const telemetry = optionalMapping(root.telemetry, "telemetry");
  optionalStringList(telemetry?.events, "telemetry.events");
  optionalMapping(root.programSelection, "programSelection");
  const programs = optionalMapping(root.programs, "programs");
  if (programs) {
    for (const [name, program] of Object.entries(programs)) {
      optionalMapping(program, `programs.${name}`);
    }
  }
  return value as DaemonConfigDocument;
}

export function mergeBuilderYaml(
  source: string,
  initialBuilderYaml: string,
  builderYaml: string,
): string {
  const sourceDocument = parseDocument(source);
  const initialDocument = parseDocument(initialBuilderYaml);
  const builderDocument = parseDocument(builderYaml);
  for (const document of [sourceDocument, initialDocument, builderDocument]) {
    if (document.errors.length > 0) {
      throw new Error(document.errors.map((error) => error.message).join("\n"));
    }
  }
  const isMapping = (value: unknown): value is Record<string, unknown> =>
    value !== null && typeof value === "object" && !Array.isArray(value);
  function applyChanges(before: unknown, after: unknown, path: string[]) {
    if (isMapping(before) && isMapping(after)) {
      for (const key of new Set([
        ...Object.keys(before),
        ...Object.keys(after),
      ])) {
        applyChanges(before[key], after[key], [...path, key]);
      }
    } else if (JSON.stringify(before) !== JSON.stringify(after)) {
      if (after === undefined) {
        sourceDocument.deleteIn(path);
      } else {
        sourceDocument.setIn(path, after);
      }
    }
  }
  applyChanges(initialDocument.toJS(), builderDocument.toJS(), []);
  return sourceDocument.toString();
}

function formatTimestamp(seconds: number) {
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(seconds * 1000));
}
