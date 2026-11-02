import { Check, CircleAlert, X } from "lucide-react";
import type { ReactNode } from "react";

import type { BrowserPacCompliance, DeviceBrowserPac } from "../types";

/** The device's PAC value from browser policy: absent, found (with URL), or differs. */
export function PacBadge({ pac }: { pac: DeviceBrowserPac }) {
  switch (pac.state) {
    case "found":
      return (
        <span className="badge neutral" title={pac.url ?? undefined}>
          PAC found
        </span>
      );
    case "absent":
      return <span className="badge warning">PAC absent</span>;
    case "differs":
      return (
        <span className="badge warning" title="Browsers set different PACs">
          PAC differs
        </span>
      );
    default:
      return <span className="mono-soft">—</span>;
  }
}

/** Fleet compliance check result; only shown when a profile is configured. */
export function PacComplianceBadge({
  compliance,
}: {
  compliance: BrowserPacCompliance;
}) {
  switch (compliance.state) {
    case "compliant":
      return <span className="badge success">Compliant</span>;
    case "not_compliant":
      return (
        <span className="badge danger" title={compliance.reason ?? undefined}>
          Not compliant
        </span>
      );
    default:
      return <span className="badge neutral">Not reported</span>;
  }
}

/**
 * One icon per device for tables: – no PAC, grey check PAC found (no
 * compliance profile), green check compliant, red cross not compliant,
 * amber alert when browsers differ or the PAC can't be fetched. Details are
 * in the tooltip and on the device page.
 */
export function PacCell({ pac }: { pac: DeviceBrowserPac }) {
  const { label, tone, icon } = pacStatus(pac);
  const details = [
    label,
    pac.url,
    pac.sha256 && `SHA-256 ${pac.sha256.slice(0, 12)}`,
    pac.compliance?.reason,
  ]
    .filter(Boolean)
    .join("\n");
  return (
    <span
      className={`pac-status ${tone}`}
      title={details}
      role="img"
      aria-label={details}
    >
      {icon}
    </span>
  );
}

function pacStatus(pac: DeviceBrowserPac): {
  label: string;
  tone: string;
  icon: ReactNode;
} {
  if (pac.state === "not_reported") {
    return { label: "Not reported", tone: "muted", icon: null };
  }
  // With a compliance profile the verdict decides: red is never hidden by
  // a warning.
  if (pac.compliance?.state === "not_compliant") {
    const icon = pac.state === "absent" ? "–" : <X size={16} />;
    return { label: "PAC not compliant", tone: "danger", icon };
  }
  if (pac.state === "absent") {
    return { label: "No PAC", tone: "muted", icon: "–" };
  }
  if (pac.state === "differs" || pac.fetch_error) {
    const label =
      pac.state === "differs"
        ? "Browsers set different PACs"
        : "PAC not reachable";
    return { label, tone: "warning", icon: <CircleAlert size={16} /> };
  }
  if (pac.compliance?.state === "compliant") {
    return {
      label: "PAC compliant",
      tone: "success",
      icon: <Check size={16} />,
    };
  }
  return { label: "PAC found", tone: "muted", icon: <Check size={16} /> };
}
