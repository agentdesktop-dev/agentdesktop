import {
  AppearancePreference,
  CardHeader,
  friendlyTool,
  ToolIcon,
} from "@agentdesktop/ui";
import { AlertCircle, Box, LoaderCircle } from "lucide-react";
import { type ReactNode, useId } from "react";

import type {
  ManagedProgram,
  ProgramChoice,
  ProgramManagement,
  Settings,
} from "../types";

type Tone = "success" | "warning" | "danger" | "neutral";

const programStates: Record<string, { label: string; tone: Tone }> = {
  applied: { label: "Applied", tone: "success" },
  unchanged: { label: "Up to date", tone: "success" },
  removed: { label: "Removed", tone: "neutral" },
  opted_out: { label: "Opted out", tone: "neutral" },
  inactive: { label: "Inactive", tone: "warning" },
  blocked: { label: "Blocked", tone: "warning" },
  conflict: { label: "Conflict", tone: "danger" },
  failed: { label: "Failed", tone: "danger" },
};

const choiceModes: Array<{
  mode: ProgramChoice["mode"];
  label: string;
  description: string;
}> = [
  {
    mode: "all",
    label: "All",
    description: "Every configured tool, including tools configured later.",
  },
  {
    mode: "selected",
    label: "Only these",
    description:
      "The tools switched on below. Tools configured later stay unmanaged until you switch them on.",
  },
  {
    mode: "none",
    label: "None",
    description:
      "Agent Desktop removes the settings it wrote from every configured tool.",
  },
];

function programStatus(program: ManagedProgram): { label: string; tone: Tone } {
  if (program.state) {
    return (
      programStates[program.state] ?? { label: program.state, tone: "neutral" }
    );
  }
  return { label: program.managed ? "Pending" : "Opted out", tone: "neutral" };
}

function includes(choice: ProgramChoice, program: string): boolean {
  return (
    choice.mode === "all" ||
    (choice.mode === "selected" && choice.programs.includes(program))
  );
}

function selection(programs: Iterable<string>): ProgramChoice {
  return { mode: "selected", programs: [...new Set(programs)].sort() };
}

/** A configured program; with `toggle`, a switch to manage it. */
function ProgramRow({
  program,
  toggle,
}: {
  program: ManagedProgram;
  toggle?: {
    checked: boolean;
    disabled: boolean;
    onChange: (checked: boolean) => void;
  };
}) {
  const id = useId();
  const status = programStatus(program);
  return (
    <li className="inline-preference program-row">
      <div>
        {toggle ? (
          <strong>
            <label className="tool-cell" htmlFor={id}>
              <ToolIcon kind={program.program} />
              {friendlyTool(program.program)}
            </label>
          </strong>
        ) : (
          <strong className="tool-cell">
            <ToolIcon kind={program.program} />
            {friendlyTool(program.program)}
          </strong>
        )}
        {program.detail ? (
          <span id={`${id}-detail`}>{program.detail}</span>
        ) : null}
      </div>
      <div className="program-row-controls">
        <span id={`${id}-state`} className={`badge ${status.tone}`}>
          {status.label}
        </span>
        {toggle ? (
          <label className="switch" htmlFor={id}>
            <input
              id={id}
              type="checkbox"
              aria-describedby={
                program.detail ? `${id}-state ${id}-detail` : `${id}-state`
              }
              disabled={toggle.disabled}
              checked={toggle.checked}
              onChange={(event) => toggle.onChange(event.target.checked)}
            />
            <span className="switch-track" aria-hidden="true" />
          </label>
        ) : null}
      </div>
    </li>
  );
}

function ManagedTools({
  management,
  loading,
  saving,
  onChoiceChange,
}: {
  management: ProgramManagement | null;
  loading: boolean;
  saving: boolean;
  onChoiceChange: (choice: ProgramChoice) => void;
}) {
  const id = useId();
  const allowed = Boolean(management?.userChoiceAllowed);
  let body: ReactNode;

  if (!management) {
    body = loading ? (
      <div className="empty-inline" role="status">
        <LoaderCircle className="spin" size={20} />
        <div>
          <strong>Checking managed tools</strong>
        </div>
      </div>
    ) : (
      <div className="empty-inline">
        <AlertCircle size={20} />
        <div>
          <strong>Managed tools are unavailable</strong>
          <span>Use Refresh to try again.</span>
        </div>
      </div>
    );
  } else if (!management.programs.length) {
    body = (
      <div className="empty-inline">
        <Box size={20} />
        <div>
          <strong>No tools are configured</strong>
          <span>
            This device’s configuration doesn’t set up any tools for Agent
            Desktop to manage.
          </span>
        </div>
      </div>
    );
  } else if (!allowed) {
    body = (
      <>
        <div className="program-policy">
          <h3 id={`${id}-policy`}>Managed by your organization</h3>
          <p>Your organization’s policy manages every tool it configures.</p>
        </div>
        <ul className="program-list" aria-labelledby={`${id}-policy`}>
          {management.programs.map((program) => (
            <ProgramRow key={program.program} program={program} />
          ))}
        </ul>
      </>
    );
  } else {
    const { choice, programs } = management;
    const chooseMode = (mode: ProgramChoice["mode"]) =>
      onChoiceChange(
        mode === "selected"
          ? selection(
              programs
                .map((program) => program.program)
                .filter((program) => includes(choice, program)),
            )
          : { mode },
      );
    const chooseProgram = (program: string, managed: boolean) => {
      const current = choice.mode === "selected" ? choice.programs : [];
      onChoiceChange(
        selection(
          managed
            ? [...current, program]
            : current.filter((selected) => selected !== program),
        ),
      );
    };
    body = (
      <>
        <div className="inline-preference program-choice">
          <div>
            <strong id={`${id}-mode`}>Tools to manage</strong>
            <span id={`${id}-mode-description`}>
              {
                choiceModes.find((option) => option.mode === choice.mode)
                  ?.description
              }
            </span>
          </div>
          <div
            className="program-modes"
            role="radiogroup"
            aria-labelledby={`${id}-mode`}
            aria-describedby={`${id}-mode-description`}
            aria-busy={saving}
          >
            {choiceModes.map((option) => (
              <label key={option.mode} className="program-mode">
                <input
                  type="radio"
                  name={`${id}-mode`}
                  value={option.mode}
                  disabled={saving}
                  checked={choice.mode === option.mode}
                  onChange={() => chooseMode(option.mode)}
                />
                {option.label}
              </label>
            ))}
          </div>
        </div>
        <ul className="program-list" aria-label="Configured tools">
          {programs.map((program) => (
            <ProgramRow
              key={program.program}
              program={program}
              toggle={{
                checked: includes(choice, program.program),
                disabled: saving || choice.mode !== "selected",
                onChange: (managed) => chooseProgram(program.program, managed),
              }}
            />
          ))}
        </ul>
      </>
    );
  }

  return (
    <section className="card">
      <CardHeader
        heading="h2"
        title="Managed tools"
        description={
          allowed
            ? "Choose which configured tools Agent Desktop manages on this device."
            : "The tools Agent Desktop configures on this device."
        }
        action={
          saving ? (
            <span className="badge neutral" role="status">
              Saving…
            </span>
          ) : null
        }
      />
      {body}
    </section>
  );
}

export function SettingsView({
  settings,
  disabled,
  onStartupChange,
  programManagement,
  programsLoading,
  savingPrograms,
  onProgramChoiceChange,
}: {
  settings: Settings;
  disabled: boolean;
  onStartupChange: (checked: boolean) => void;
  programManagement: ProgramManagement | null;
  programsLoading: boolean;
  savingPrograms: boolean;
  onProgramChoiceChange: (choice: ProgramChoice) => void;
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
      <ManagedTools
        management={programManagement}
        loading={programsLoading}
        saving={savingPrograms}
        onChoiceChange={onProgramChoiceChange}
      />
    </div>
  );
}
