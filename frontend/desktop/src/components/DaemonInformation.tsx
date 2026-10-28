import { Check, Copy } from "lucide-react";
import { useEffect, useId, useState } from "react";

import type { DaemonInfo } from "../types";

function CopyableValue({ label, value }: { label: string; value: string }) {
  const [copyState, setCopyState] = useState<
    "idle" | "copying" | "copied" | "error"
  >("idle");

  useEffect(() => {
    if (copyState !== "copied") return;
    const timeout = window.setTimeout(() => setCopyState("idle"), 2000);
    return () => window.clearTimeout(timeout);
  }, [copyState]);

  async function copy() {
    setCopyState("copying");
    try {
      await navigator.clipboard.writeText(value);
      setCopyState("copied");
    } catch {
      setCopyState("error");
    }
  }

  return (
    <div className="daemon-value">
      <span className="daemon-value-text" dir="ltr" title={value}>
        {value}
      </span>
      <button
        className="daemon-copy-button"
        type="button"
        onClick={copy}
        disabled={copyState === "copying"}
        aria-label={`Copy ${label.toLowerCase()}`}
        title={copyState === "copied" ? "Copied" : "Copy"}
      >
        {copyState === "copied" ? (
          <Check size={13} aria-hidden="true" />
        ) : (
          <Copy size={13} aria-hidden="true" />
        )}
      </button>
      <span
        className={
          copyState === "error" ? "daemon-copy-error" : "daemon-copy-feedback"
        }
        role="status"
        aria-atomic="true"
      >
        {copyState === "copied"
          ? `${label} copied`
          : copyState === "error"
            ? "Copy failed. Select and copy the value."
            : ""}
      </span>
    </div>
  );
}

export function DaemonInformation({
  info,
}: {
  info: DaemonInfo | null | undefined;
}) {
  const headingId = useId();
  const fields: [label: string, value: string, copyable?: boolean][] = info
    ? [
        ["Daemon scope", info.scope === "user" ? "User" : "System"],
        ["Configuration file", info.configPath, true],
        ["State directory", info.stateDirectory, true],
        ["Inventory refresh", info.inventoryInterval],
        [
          "Controller address",
          info.controller?.address ?? "Not configured (standalone)",
          Boolean(info.controller),
        ],
      ]
    : [];
  if (info?.controller) {
    fields.push(
      [
        "Controller CA certificate",
        info.controller.caCertificatePath ?? "No custom CA configured",
        Boolean(info.controller.caCertificatePath),
      ],
      ["Heartbeat interval", info.controller.heartbeatInterval],
    );
  }
  if (info?.llmProxy) {
    fields.push([
      "Local LLM proxy",
      info.llmProxy.bound
        ? `${info.llmProxy.listen} (client ${info.llmProxy.clientId})`
        : `${info.llmProxy.listen} (not bound${info.llmProxy.error ? `: ${info.llmProxy.error}` : ""})`,
    ]);
  }
  if (info?.usageReceiver) {
    fields.push([
      "Usage receiver",
      info.usageReceiver.bound
        ? info.usageReceiver.listen
        : `${info.usageReceiver.listen} (off${info.usageReceiver.error ? `: ${info.usageReceiver.error}` : ""})`,
    ]);
  }
  if (info?.intercept) {
    const intercept = info.intercept;
    fields.push([
      "Claude Desktop intercept",
      !intercept.enabled
        ? `${intercept.listen} (off)`
        : intercept.bound
          ? `${intercept.listen} (listening)`
          : `${intercept.listen} (not bound${intercept.error ? `: ${intercept.error}` : ""})`,
    ]);
    if (intercept.enabled && !intercept.caCertificate) {
      fields.push(["Intercept CA certificate", "Not generated"]);
    } else if (intercept.caCertificate) {
      fields.push(["Intercept CA certificate", intercept.caCertificate, true]);
    }
    if (intercept.caSha256) {
      fields.push(["Intercept CA SHA-256", intercept.caSha256, true]);
    }
    fields.push([
      "Intercept traffic",
      `${intercept.forwarded} forwarded, ${intercept.gatewayFailures} gateway failures, ${intercept.tunnels} tunnels, ${intercept.tunnelFailures} tunnel failures`,
    ]);
    if (intercept.lastFailure) {
      fields.push(["Last intercept failure", intercept.lastFailure]);
    }
  }

  return (
    <section className="daemon-information" aria-labelledby={headingId}>
      <div className="advanced-config-heading">
        <h3 id={headingId}>Daemon information</h3>
      </div>
      {info ? (
        <dl className="runtime-grid">
          {fields.map(([label, value, copyable]) => (
            <div className="definition-row daemon-information-row" key={label}>
              <dt>{label}</dt>
              <dd>
                {copyable && value ? (
                  <CopyableValue key={value} label={label} value={value} />
                ) : (
                  <span className="daemon-value-text">{value}</span>
                )}
              </dd>
            </div>
          ))}
        </dl>
      ) : (
        <p className="daemon-information-unavailable">
          Daemon information is unavailable. Refresh to retry.
        </p>
      )}
    </section>
  );
}
