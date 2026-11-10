import { ChevronDown, ChevronRight, ScrollText } from "lucide-react";
import { Fragment, useState } from "react";

import { ErrorState, PageSkeleton } from "../components/ViewStates";
import type { AuditEvent, AuditOutcome } from "../types";

const outcomeTones: Record<AuditOutcome, string> = {
  authorized: "neutral",
  succeeded: "success",
  not_found: "warning",
  denied: "danger",
  failed: "danger",
  unaudited: "danger",
};

const outcomeLabels: Record<AuditOutcome, string> = {
  authorized: "Authorized",
  succeeded: "Succeeded",
  not_found: "Not found",
  denied: "Denied",
  failed: "Failed",
  unaudited: "Unaudited",
};

const eventLabels: Record<string, string> = {
  "authorization.denied": "Permission denied",
  "action.attempted": "Action started",
  "action.completed": "Action finished",
  "auth.login": "Signed in",
  "auth.logout": "Signed out",
  "auth.reauthenticate": "Reauthenticated",
};

export function AuditView({
  events,
  error = null,
  loading = false,
  hasMore = false,
  loadingMore = false,
  loadMoreError = null,
  onLoadMore,
}: {
  events: AuditEvent[];
  error?: string | null;
  loading?: boolean;
  hasMore?: boolean;
  loadingMore?: boolean;
  loadMoreError?: string | null;
  onLoadMore?: () => void;
}) {
  return (
    <div className="stack">
      <section className="section-intro">
        <div>
          <h2>Audit log</h2>
          <p>
            Every management change and denied request, newest first. The same
            events are streamed to your SIEM.
          </p>
        </div>
      </section>
      <section className="card table-card">
        {loading ? (
          <PageSkeleton rows={5} />
        ) : error ? (
          <ErrorState message={error} />
        ) : events.length ? (
          <AuditTable events={events} />
        ) : (
          <div className="empty-state">
            <ScrollText size={28} />
            <h3>No audit events yet</h3>
            <p>Management changes and denied requests will appear here.</p>
          </div>
        )}
      </section>
      {loadMoreError && (
        <p className="audit-load-error" role="alert">
          {loadMoreError}
        </p>
      )}
      {hasMore && !loading && !error && (
        <button
          type="button"
          className="button secondary audit-load-more"
          disabled={loadingMore}
          onClick={onLoadMore}
        >
          {loadingMore ? "Loading…" : "Load older events"}
        </button>
      )}
    </div>
  );
}

function AuditTable({ events }: { events: AuditEvent[] }) {
  const [expanded, setExpanded] = useState<string | null>(null);
  return (
    <section
      className="table-scroll"
      // biome-ignore lint/a11y/noNoninteractiveTabindex: Horizontal overflow must be keyboard scrollable.
      tabIndex={0}
      aria-label="Audit events table"
    >
      <table className="audit-table">
        <thead>
          <tr>
            <th>Time</th>
            <th>Actor</th>
            <th>Event</th>
            <th>Target</th>
            <th>Outcome</th>
            <th>
              <span className="sr-only">Details</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {events.map((event) => {
            const open = expanded === event.id;
            const toggle = () => setExpanded(open ? null : event.id);
            return (
              <Fragment key={event.id}>
                <tr onClick={toggle}>
                  <td>
                    <div className="cell-stack">
                      <strong>{formatAuditTime(event.time)}</strong>
                      <span>#{event.seq}</span>
                    </div>
                  </td>
                  <td>
                    <div className="cell-stack">
                      <strong>{actorLabel(event)}</strong>
                      <span>{event.actor.kind}</span>
                    </div>
                  </td>
                  <td>
                    <div className="cell-stack">
                      <strong>{eventLabels[event.event] ?? event.event}</strong>
                      <span>{event.action ?? event.request?.path ?? "—"}</span>
                    </div>
                  </td>
                  <td>
                    {event.target ? (
                      <div className="cell-stack">
                        <strong>{event.target.id ?? "All"}</strong>
                        <span>{event.target.class}</span>
                      </div>
                    ) : (
                      "—"
                    )}
                  </td>
                  <td>
                    <span
                      className={`badge ${outcomeTones[event.outcome] ?? "neutral"}`}
                      title={event.reason}
                    >
                      {outcomeLabels[event.outcome] ?? event.outcome}
                    </span>
                  </td>
                  <td>
                    <button
                      type="button"
                      className="icon-button"
                      aria-expanded={open}
                      aria-label={`${open ? "Hide" : "Show"} details for event ${event.seq}`}
                      onClick={(click) => {
                        click.stopPropagation();
                        toggle();
                      }}
                    >
                      {open ? (
                        <ChevronDown size={16} />
                      ) : (
                        <ChevronRight size={16} />
                      )}
                    </button>
                  </td>
                </tr>
                {open && (
                  <tr className="audit-detail-row">
                    <td colSpan={6}>
                      {/* biome-ignore lint/a11y/noNoninteractiveTabindex: Keyboard users need to focus this scrollable record. */}
                      <pre tabIndex={0}>
                        <code>{JSON.stringify(event, null, 2)}</code>
                      </pre>
                    </td>
                  </tr>
                )}
              </Fragment>
            );
          })}
        </tbody>
      </table>
    </section>
  );
}

function actorLabel(event: AuditEvent) {
  if (event.actor.kind === "local") return "Local access";
  return event.actor.email ?? event.actor.name ?? event.actor.subject;
}

function formatAuditTime(time: string) {
  const date = new Date(time);
  if (Number.isNaN(date.getTime())) return time;
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "medium",
  }).format(date);
}
