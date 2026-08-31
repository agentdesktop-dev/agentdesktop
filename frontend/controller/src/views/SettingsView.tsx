import type { ControllerSettings } from "../types";

export function SettingsView({ data }: { data: ControllerSettings }) {
  return (
    <div className="stack">
      <section className="section-intro">
        <div>
          <h2>Controller settings</h2>
          <p>Runtime capabilities for this controller instance.</p>
        </div>
      </section>
      <section className="settings-list">
        <SettingRow title="Fleet API" description={data.fleet_listen} enabled />
        <SettingRow
          title="Admin UI"
          description={`${data.admin_listen} · loopback only`}
          enabled
        />
        <SettingRow
          title="TLS"
          description="Encrypted fleet transport"
          enabled={data.tls_enabled}
        />
        <SettingRow
          title="OIDC enrollment"
          description="Interactive SSO-based device enrollment"
          enabled={data.oidc_enabled}
        />
        <SettingRow
          title="Gateway JWT issuer"
          description="Short-lived LLM gateway credentials"
          enabled={data.gateway_jwt_enabled}
        />
      </section>
      <section className="local-notice">
        <div>
          <strong>Local access only</strong>
          <span>
            The controller rejects admin listener addresses that are not
            loopback interfaces.
          </span>
        </div>
      </section>
    </div>
  );
}

function SettingRow({
  title,
  description,
  enabled,
}: {
  title: string;
  description: string;
  enabled: boolean;
}) {
  return (
    <div className="setting-row">
      <div>
        <strong>{title}</strong>
        <span>{description}</span>
      </div>
      <span className={`badge ${enabled ? "success" : "neutral"}`}>
        {enabled ? "Enabled" : "Disabled"}
      </span>
    </div>
  );
}
