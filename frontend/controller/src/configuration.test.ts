import { describe, expect, it } from "vitest";
import { parse } from "yaml";

import builderFixture from "../../../crates/core/tests/fixtures/configuration-builder.json";
import {
  createConfigurationDraft,
  programSettingsYaml,
  renderConfiguration,
} from "./configuration";
import type { DaemonConfigDocument } from "./types";

const oidcConfig: DaemonConfigDocument = {
  controller: {
    address: "https://controller.example.com",
    heartbeatInterval: "1m",
  },
  llmGateway: {
    url: "https://gateway.example.com",
    authentication: {
      type: "oidc",
      issuer: "https://issuer.example.com",
      clientId: "desktop",
      redirectUri: "http://127.0.0.1:44120/callback",
      scopes: ["openid", "profile", "offline_access"],
      allowInsecure: false,
    },
  },
  telemetry: { events: ["session.new", "tool.use", "tool.use.input"] },
  programs: {
    codex: { useLlmGateway: false, managedConfig: { model: "company-model" } },
  },
};

describe("configuration documents", () => {
  it("matches the output fixture consumed by the daemon's parser", () => {
    const draft = createConfigurationDraft(builderFixture.input);
    expect(renderConfiguration(draft).yaml).toBe(builderFixture.yaml);
  });

  it.each([
    {},
    oidcConfig,
    {
      llmGateway: {
        url: "https://gateway.example.com",
        authentication: {
          type: "controllerJwt",
          audience: "restricted",
          allowedClientIds: ["codex", "custom-client"],
        },
      },
      programs: { codex: { useLlmGateway: true } },
    },
    {
      futureField: { preserve: "opaque data" },
      sandbox: { network: { futureRule: "preserved" } },
      telemetry: { events: ["future.event"], futureOption: true },
      programs: { futureHarness: { nativeField: "preserved" } },
    },
  ] satisfies DaemonConfigDocument[])(
    "preserves an untouched document %#",
    (config) => {
      const result = renderConfiguration(createConfigurationDraft(config));
      expect(result.errors).toEqual({});
      expect(parse(result.yaml ?? "")).toEqual(config);
    },
  );

  it("isolates edits from the source document and defaults", () => {
    const initial = structuredClone(oidcConfig);
    const draft = createConfigurationDraft(initial);
    if (!draft.config.llmGateway) throw new Error("fixture gateway missing");
    draft.config.llmGateway.url = "https://changed.example.com";
    const result = renderConfiguration(draft);
    expect(parse(result.yaml ?? "")).toEqual({
      ...oidcConfig,
      llmGateway: {
        ...oidcConfig.llmGateway,
        url: "https://changed.example.com",
      },
    });
    expect(initial).toEqual(oidcConfig);
    const first = createConfigurationDraft();
    if (!first.config.programs) throw new Error("default programs missing");
    first.config.programs.claudeCode = { changed: true };
    expect(createConfigurationDraft().config.programs).toEqual({
      claudeCode: {},
    });
  });

  it.each(["", "{}", "# no extra settings\n"])(
    "keeps empty mappings valid with a gateway opt-out: %j",
    (settings) => {
      const config = {
        llmGateway: { url: "https://gateway.example.com" },
        programs: { codex: { useLlmGateway: false } },
      };
      const draft = createConfigurationDraft(config);
      draft.settings.codex = settings;
      const result = renderConfiguration(draft);
      expect(result.errors).toEqual({});
      expect(parse(result.yaml ?? "")).toEqual(config);
    },
  );

  it("restores gateway policy after toggling the shared gateway", () => {
    const draft = createConfigurationDraft(oidcConfig);
    draft.gatewayEnabled = false;
    draft.settings.codex = "managedConfig: { model: company-model }";
    const disabled = parse(renderConfiguration(draft).yaml ?? "");
    expect(disabled.llmGateway).toBeUndefined();
    expect(disabled.programs.codex.useLlmGateway).toBe(false);
    draft.gatewayEnabled = true;
    expect(parse(renderConfiguration(draft).yaml ?? "")).toEqual(oidcConfig);
  });

  it("round-trips native mappings, sequences, quoted scalars and multiline text", () => {
    const managedConfig = {
      numericString: "123",
      falseString: "false",
      nullString: "null",
      unicode: "café 日本語",
      multiLine: "first\nsecond\n",
      true: "a quoted key",
      emptyObject: {},
      emptyList: [],
      list: [{ enabled: false, count: 0 }, null, "yes"],
    };
    const program = { useLlmGateway: false, managedConfig };
    const draft = createConfigurationDraft({ programs: { codex: program } });
    draft.settings.codex = programSettingsYaml(program);
    expect(draft.settings.codex).not.toContain("useLlmGateway");
    expect(parse(renderConfiguration(draft).yaml ?? "")).toEqual({
      programs: { codex: program },
    });
  });

  it("preserves whitespace in the final native block string while editing", () => {
    const program = { managedConfig: { instructions: "first\nsecond\n\n\n" } };
    const draft = createConfigurationDraft({ programs: { codex: program } });
    draft.settings.codex = programSettingsYaml(program);
    expect(parse(renderConfiguration(draft).yaml ?? "")).toEqual({
      programs: { codex: program },
    });
  });

  it("quotes strings that older YAML readers would interpret as scalars", () => {
    const config = {
      programs: {
        codex: {
          managedConfig: { "0b101": "0b101", mode: "on", date: "2026-09-11" },
        },
      },
    };
    const result = renderConfiguration(createConfigurationDraft(config));
    expect(parse(result.yaml ?? "", { version: "1.1" })).toEqual(config);
  });

  it("only changes edited sandbox fields", () => {
    const config = {
      sandbox: {
        network: {
          allowedDomains: ["old.example.com"],
          futureRule: "preserved",
        },
        filesystem: { denied: ["~/.ssh"] },
      },
    };
    const draft = createConfigurationDraft(config);
    draft.sandboxFields.allowedDomains =
      " api.github.com\r\napi.github.com\n\n";
    const result = parse(renderConfiguration(draft).yaml ?? "");
    expect(result.sandbox).toEqual({
      network: { allowedDomains: ["api.github.com"], futureRule: "preserved" },
      filesystem: { denied: ["~/.ssh"] },
    });
    expect(draft.config).toEqual(config);
    draft.sandboxEnabled = false;
    expect(parse(renderConfiguration(draft).yaml ?? "")).toEqual({});
  });

  it.each([
    "managedConfig: [",
    "managedConfig: {}\nmanagedConfig: {}",
    "null",
    "[]",
    "42",
    "useLlmGateway: false",
    "managedConfig: &cycle { self: *cycle }",
    "managedConfig: { limit: .inf }",
    "managedConfig: { value: !unknown tagged }",
    "%YAML 1.1\n---\nmanagedConfig: &m !!omap [{self: *m}]",
  ])(
    "rejects invalid native settings without losing the draft: %j",
    (settings) => {
      const draft = createConfigurationDraft({ programs: { codex: {} } });
      draft.settings.codex = settings;
      const result = renderConfiguration(draft);
      expect(result.yaml).toBeNull();
      expect(result.errors.codex).toBeTruthy();
      expect(draft.settings.codex).toBe(settings);
    },
  );
});
