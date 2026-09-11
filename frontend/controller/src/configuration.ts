import {
  type CreateNodeOptions,
  parseDocument,
  type SchemaOptions,
  stringify,
  type ToStringOptions,
} from "yaml";

import type { AgentKind, DaemonConfigDocument } from "./types";

type SandboxField = "allowedDomains" | "writable" | "denied";

// Keep strings unambiguous for both the browser and the daemon's YAML reader.
const yamlOptions: CreateNodeOptions & SchemaOptions & ToStringOptions = {
  compat: "yaml-1.1",
  lineWidth: 0,
  aliasDuplicateObjects: false,
};

export interface ConfigurationDraft {
  config: DaemonConfigDocument;
  gatewayEnabled: boolean;
  sandboxEnabled: boolean;
  settings: Partial<Record<AgentKind, string>>;
  sandboxFields: Partial<Record<SandboxField, string>>;
}

export function defaultLlmGateway() {
  return {
    url: "https://gateway.example.com",
    authentication: {
      type: "controllerJwt",
      audience: "agentgateway",
      // Authorization policy is independent of presentation and builder choices.
      allowedClientIds: ["claude-code", "claude-desktop", "codex", "opencode"],
    },
  };
}

export function createConfigurationDraft(
  initial?: DaemonConfigDocument | null,
): ConfigurationDraft {
  const config: DaemonConfigDocument = structuredClone(
    initial ?? {
      llmGateway: defaultLlmGateway(),
      programs: { claudeCode: {} },
    },
  );
  return {
    config,
    gatewayEnabled: Boolean(config.llmGateway),
    sandboxEnabled: Boolean(config.sandbox),
    settings: {},
    sandboxFields: {},
  };
}

export function programSettingsYaml(program: Record<string, unknown>) {
  const { useLlmGateway: _, ...settings } = program;
  return Object.keys(settings).length ? stringify(settings, yamlOptions) : "";
}

/** Preserve the source document; only fields edited by the form are replaced. */
export function renderConfiguration(draft: ConfigurationDraft): {
  yaml: string | null;
  errors: Partial<Record<AgentKind, string>>;
} {
  const config = { ...draft.config };
  const errors: Partial<Record<AgentKind, string>> = {};
  if (!draft.gatewayEnabled) delete config.llmGateway;
  else config.llmGateway ??= defaultLlmGateway();

  if (!draft.sandboxEnabled) delete config.sandbox;
  else {
    const { allowedDomains, writable, denied } = draft.sandboxFields;
    const sandbox = { ...config.sandbox };
    if (allowedDomains !== undefined) {
      sandbox.network = {
        ...sandbox.network,
        allowedDomains: textList(allowedDomains),
      };
    }
    if (writable !== undefined || denied !== undefined) {
      sandbox.filesystem = {
        ...sandbox.filesystem,
        ...(writable === undefined ? {} : { writable: textList(writable) }),
        ...(denied === undefined ? {} : { denied: textList(denied) }),
      };
    }
    config.sandbox = sandbox;
  }

  if (config.programs) {
    config.programs = { ...config.programs };
    for (const kind of Object.keys(draft.settings) as AgentKind[]) {
      const program = config.programs[kind];
      const input = draft.settings[kind];
      if (!program || input === undefined) continue;
      try {
        const settings = parseSettings(input);
        // The gateway checkbox owns this field, even when the gateway is disabled.
        if (program.useLlmGateway !== undefined) {
          settings.useLlmGateway = program.useLlmGateway;
        }
        config.programs[kind] = settings;
      } catch (error) {
        errors[kind] = error instanceof Error ? error.message : "Invalid YAML.";
      }
    }
  }

  return {
    yaml: Object.keys(errors).length ? null : stringify(config, yamlOptions),
    errors,
  };
}

function parseSettings(input: string): Record<string, unknown> {
  const document = parseDocument(input, {
    prettyErrors: false,
    stringKeys: true,
    schema: "core",
    resolveKnownTags: false,
  });
  const problem = document.errors[0] ?? document.warnings[0];
  if (problem) throw new Error(problem.message);
  if (document.contents === null) return {};
  const value: unknown = document.toJS({ maxAliasCount: 100 });
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("Enter a YAML mapping of setting names to values.");
  }
  if (Object.hasOwn(value, "useLlmGateway")) {
    throw new Error(
      "Use the “Use LLM gateway” checkbox instead of useLlmGateway.",
    );
  }
  // Native pass-through settings are JSON values; reject cycles and non-finite numbers.
  JSON.stringify(value, (_key, item) => {
    if (typeof item === "number" && !Number.isFinite(item)) {
      throw new Error("Setting numbers must be finite.");
    }
    return item;
  });
  return value as Record<string, unknown>;
}

function textList(value: string) {
  return [...new Set(value.split(/\r?\n/).map((item) => item.trim()))].filter(
    Boolean,
  );
}
