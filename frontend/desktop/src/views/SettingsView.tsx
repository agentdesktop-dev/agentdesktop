import { AppearancePreference, CardHeader } from "@agentdesktop/ui";
import { useId } from "react";

import type { Settings } from "../types";

export function SettingsView({
  settings,
  disabled,
  onStartupChange,
}: {
  settings: Settings;
  disabled: boolean;
  onStartupChange: (checked: boolean) => void;
}) {
  const startupId = useId();
  return (
    <div className="page-stack settings-page">
      <section className="card">
        <CardHeader
          heading="h2"
          title="Appearance"
          description="Personalize the desktop client on this device."
        />
        <AppearancePreference disabled={disabled} />
      </section>
      <section className="card">
        <CardHeader
          heading="h2"
          title="Startup"
          description="Choose how the desktop client opens."
        />
        <div className="inline-preference">
          <div>
            <strong>
              <label htmlFor={startupId}>Open window at startup</label>
            </strong>
            <span id={`${startupId}-description`}>
              The tray application continues running when this is off.
            </span>
          </div>
          <label className="switch" htmlFor={startupId}>
            <input
              id={startupId}
              type="checkbox"
              aria-describedby={`${startupId}-description`}
              disabled={disabled}
              checked={settings.openOnStartup}
              onChange={(event) => onStartupChange(event.target.checked)}
            />
            <span className="switch-track" aria-hidden="true" />
          </label>
        </div>
      </section>
    </div>
  );
}
