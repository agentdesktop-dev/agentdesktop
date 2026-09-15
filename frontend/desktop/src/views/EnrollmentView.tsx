import { AlertCircle, Copy, LoaderCircle, ShieldCheck } from "lucide-react";

import { DaemonInformation } from "../components/DaemonInformation";
import type { DaemonInfo, ManagedDeviceSnapshot } from "../types";

export interface EnrollmentViewProps {
  busy: boolean;
  daemon: DaemonInfo | null;
  enrollment: ManagedDeviceSnapshot;
  onCopy: () => void;
  onEnroll: () => void;
}

export function EnrollmentView({
  enrollment,
  busy,
  daemon,
  onCopy,
  onEnroll,
}: EnrollmentViewProps) {
  const waiting =
    enrollment.enrollment === "pending" || enrollment.enrollment === "issuing";
  const failed =
    enrollment.enrollment === "unavailable" ||
    enrollment.enrollment === "rejected";
  return (
    <>
      <section className="enrollment-welcome">
        <div
          className={`enrollment-mark ${failed ? "enrollment-mark-error" : ""}`}
        >
          {failed ? <AlertCircle size={28} /> : <ShieldCheck size={28} />}
        </div>
        <p className="eyebrow">Agent Desktop</p>
        <h2>
          {waiting
            ? "Enrollment is in progress"
            : failed
              ? "Enrollment needs attention"
              : "Enroll this device"}
        </h2>
        <p>
          {waiting
            ? "Complete approval in your browser. This window will update automatically when access is ready."
            : failed
              ? (enrollment.detail ??
                "The device could not be enrolled. Try again or contact your administrator.")
              : "Connect this device to your organization to receive managed AI tool configuration, gateway access, and policy updates."}
        </p>
        {waiting ? (
          <div className="enrollment-progress" role="status">
            <LoaderCircle className="spin" size={15} />
            Waiting for approval
          </div>
        ) : (
          <button
            className="button button-primary enrollment-action"
            type="button"
            onClick={onEnroll}
            disabled={busy}
          >
            {busy ? "Opening sign-in…" : failed ? "Try again" : "Enroll device"}
          </button>
        )}
        <small>
          {waiting
            ? "You can leave this window open while approval completes."
            : "Sign-in opens securely in your default browser."}
        </small>
      </section>
      <details className="card advanced-config enrollment-details">
        <summary>
          <span>
            <strong>Advanced</strong>
            <small>Daemon information and controller connection settings</small>
          </span>
          <span>View</span>
        </summary>
        <div className="advanced-config-body">
          <DaemonInformation info={daemon} />
          <div>
            <button
              className="button button-secondary"
              type="button"
              onClick={onCopy}
            >
              <Copy size={13} /> Copy diagnostics
            </button>
          </div>
        </div>
      </details>
    </>
  );
}
