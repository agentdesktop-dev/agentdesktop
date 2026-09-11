import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent } from "storybook/test";
import { parse } from "yaml";

import { ControllerStoryFrame } from "../stories/ControllerStoryFrame";
import { activeDaemonConfig, sandboxDaemonConfig } from "../stories/fixtures";
import { ConfigurationView } from "./ConfigurationView";

const meta = {
  title: "Controller/Configuration",
  component: ConfigurationView,
  decorators: [
    (Story) => (
      <ControllerStoryFrame path="/configuration">
        <Story />
      </ControllerStoryFrame>
    ),
  ],
  parameters: { layout: "fullscreen" },
  tags: ["test"],
  args: { onCopy: fn() },
} satisfies Meta<typeof ConfigurationView>;

export default meta;
type Story = StoryObj<typeof meta>;

function generatedConfig(element: HTMLElement) {
  return parse(element.querySelector(".output-card code")?.textContent ?? "");
}

export const Defaults: Story = {};

export const ActiveConfiguration: Story = {
  args: { initialConfig: activeDaemonConfig },
};

const restrictedJwtConfig = {
  llmGateway: {
    url: "https://restricted.example.com",
    authentication: {
      type: "controllerJwt",
      audience: "restricted",
      allowedClientIds: ["codex"],
    },
  },
  programs: { codex: {} },
};

export const PreservesImportedJwtPolicy: Story = {
  args: { initialConfig: restrictedJwtConfig },
  play: async ({ canvas, canvasElement }) => {
    const output = () => generatedConfig(canvasElement);
    await expect(output()).toEqual(restrictedJwtConfig);
    await userEvent.clear(canvas.getByLabelText("Gateway URL"));
    await userEvent.type(
      canvas.getByLabelText("Gateway URL"),
      "https://changed.example.com",
    );
    await expect(output()).toEqual({
      ...restrictedJwtConfig,
      llmGateway: {
        ...restrictedJwtConfig.llmGateway,
        url: "https://changed.example.com",
      },
    });
  },
};

const importedOidcConfig = {
  controller: { address: "https://controller.example.com" },
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
  programs: {
    codex: { useLlmGateway: false, managedConfig: { model: "company-model" } },
  },
};

export const PreservesImportedOidc: Story = {
  args: { initialConfig: importedOidcConfig },
  play: async ({ args, canvas, canvasElement }) => {
    await expect(generatedConfig(canvasElement)).toEqual(importedOidcConfig);
    await expect(
      canvas.getByText(/OIDC authentication is preserved/),
    ).toBeVisible();
    await userEvent.click(
      canvas.getByRole("checkbox", { name: /Enable LLM gateway/ }),
    );
    const disabled = generatedConfig(canvasElement);
    await expect(disabled.llmGateway).toBeUndefined();
    await expect(disabled.programs).toEqual(importedOidcConfig.programs);
    await userEvent.click(
      canvas.getByRole("checkbox", { name: /Enable LLM gateway/ }),
    );
    await expect(generatedConfig(canvasElement)).toEqual(importedOidcConfig);
    await userEvent.click(canvas.getByRole("button", { name: "Copy" }));
    await expect(args.onCopy).toHaveBeenCalledWith(
      canvasElement.querySelector(".output-card code")?.textContent,
    );
  },
};

export const ImportedGatewayOptOutWithEmptySettings: Story = {
  args: {
    initialConfig: {
      ...restrictedJwtConfig,
      programs: { codex: { useLlmGateway: false } },
    },
  },
  play: async ({ args, canvas, canvasElement }) => {
    const settings = canvas.getByRole("textbox", {
      name: /Additional settings/,
    });
    await expect(settings).toHaveValue("");
    await expect(generatedConfig(canvasElement)).toEqual(args.initialConfig);
    await userEvent.type(settings, "{{}");
    await expect(generatedConfig(canvasElement)).toEqual(args.initialConfig);
    await expect(canvas.getByRole("button", { name: "Copy" })).toBeEnabled();
  },
};

export const InvalidSettingsBlockCopyUntilCorrected: Story = {
  play: async ({ args, canvas, canvasElement }) => {
    const settings = canvas.getByRole("textbox", {
      name: /Additional settings/,
    });
    await userEvent.type(settings, "- not a mapping");
    await expect(settings).toHaveAttribute("aria-invalid", "true");
    await expect(canvas.getByText(/Enter a YAML mapping/)).toBeVisible();
    await expect(canvas.getByRole("button", { name: "Copy" })).toBeDisabled();
    await expect(canvasElement.querySelector(".output-card code")).toBeNull();
    await expect(args.onCopy).not.toHaveBeenCalled();
    await userEvent.clear(settings);
    await userEvent.type(settings, "permissions: {{ defaultMode: plan }");
    await expect(settings).toHaveAttribute("aria-invalid", "false");
    await expect(generatedConfig(canvasElement).programs.claudeCode).toEqual({
      permissions: { defaultMode: "plan" },
    });
    await userEvent.click(canvas.getByRole("button", { name: "Copy" }));
    await expect(args.onCopy).toHaveBeenCalledOnce();
  },
};

export const ActiveSandbox: Story = {
  args: { initialConfig: sandboxDaemonConfig },
  play: async ({ canvas, canvasElement }) => {
    await expect(
      canvas.getByRole("checkbox", { name: /Require sandbox/ }),
    ).toBeChecked();
    await expect(
      canvas.getByRole("textbox", { name: /Allowed domains/ }),
    ).toHaveValue("api.github.com\nregistry.npmjs.org");
    await expect(
      canvas.getByRole("textbox", { name: /Writable paths/ }),
    ).toHaveValue("/tmp/build-cache\n/opt/project/output");
    await expect(
      canvas.getByRole("textbox", { name: /Denied paths/ }),
    ).toHaveValue("~/.ssh\n~/.aws");
    const output = canvasElement.querySelector(".output-card code");
    await expect(parse(output?.textContent ?? "")).toEqual(sandboxDaemonConfig);
  },
};

export const BuildsYaml: Story = {
  play: async ({ canvas, canvasElement }) => {
    const gatewayUrl = canvas.getByLabelText("Gateway URL");
    await userEvent.clear(gatewayUrl);
    await userEvent.type(gatewayUrl, "https://gateway.changed.example");
    await userEvent.click(canvas.getByText("Telemetry"));
    await userEvent.click(canvas.getByText("New session"));
    const output = canvasElement.querySelector(".output-card code");
    await expect(output).toHaveTextContent("llmGateway:");
    await expect(output).toHaveTextContent("https://gateway.changed.example");
    await expect(output).toHaveTextContent("session.new");
  },
};

export const BuildsSandboxYaml: Story = {
  play: async ({ canvas, canvasElement }) => {
    await userEvent.click(canvas.getByText("Sandbox"));
    await userEvent.click(
      canvas.getByRole("checkbox", { name: /Require sandbox/ }),
    );
    await userEvent.type(
      canvas.getByRole("textbox", { name: /Allowed domains/ }),
      "api.github.com{enter}registry.npmjs.org",
    );
    await userEvent.type(
      canvas.getByRole("textbox", { name: /Writable paths/ }),
      "/tmp/build-cache",
    );
    await userEvent.type(
      canvas.getByRole("textbox", { name: /Denied paths/ }),
      "~/.ssh",
    );

    const output = parse(
      canvasElement.querySelector(".output-card code")?.textContent ?? "",
    );
    await expect(output.sandbox).toEqual({
      network: { allowedDomains: ["api.github.com", "registry.npmjs.org"] },
      filesystem: { writable: ["/tmp/build-cache"], denied: ["~/.ssh"] },
    });

    await userEvent.click(canvas.getByText("Add agent"));
    await expect(
      canvas.getByRole("button", { name: /Claude Desktop/ }),
    ).toBeDisabled();
    await expect(
      canvas.getByRole("button", { name: /OpenCode/ }),
    ).toBeDisabled();
    await expect(canvas.getByRole("button", { name: /Codex/ })).toBeEnabled();
  },
};

export const BlocksSandboxForUnsupportedAgents: Story = {
  args: { initialConfig: activeDaemonConfig },
  play: async ({ canvas }) => {
    await userEvent.click(canvas.getByText("Sandbox"));
    await expect(
      canvas.getByRole("checkbox", { name: /Require sandbox/ }),
    ).toBeDisabled();
    await expect(
      canvas.getByText("Remove OpenCode to enable sandboxing."),
    ).toBeVisible();
  },
};

export const CopiesYaml: Story = {
  play: async ({ args, canvas }) => {
    await userEvent.click(canvas.getByRole("button", { name: "Copy" }));
    await expect(args.onCopy).toHaveBeenCalledOnce();
    await expect(canvas.getByRole("button", { name: "Copied" })).toBeVisible();
  },
};

export const AddsAgent: Story = {
  play: async ({ canvas }) => {
    await userEvent.click(canvas.getByText("Add agent"));
    await userEvent.click(canvas.getByRole("button", { name: /OpenCode/ }));
    await expect(canvas.getAllByText("OpenCode")).toHaveLength(1);
  },
};

export const PreservesAgentChoicesAndAuthorization: Story = {
  play: async ({ canvas, canvasElement }) => {
    const output = () => generatedConfig(canvasElement);
    const llmGateway = {
      url: "https://gateway.example.com",
      authentication: {
        type: "controllerJwt",
        audience: "agentgateway",
        allowedClientIds: [
          "claude-code",
          "claude-desktop",
          "codex",
          "opencode",
        ],
      },
    };
    await expect(output()).toEqual({
      llmGateway,
      programs: { claudeCode: {} },
    });

    const additionalAgents = ["Claude Desktop", "Codex", "OpenCode"];
    for (const [index, name] of additionalAgents.entries()) {
      await userEvent.click(canvas.getByText("Add agent"));
      await expect(
        Array.from(
          canvasElement.querySelectorAll(".add-agent-options button"),
          (button) => button.textContent,
        ),
      ).toEqual(additionalAgents.slice(index));
      await userEvent.click(canvas.getByRole("button", { name }));
    }

    const allAgents = ["Claude Code", ...additionalAgents];
    await expect(
      Array.from(
        canvasElement.querySelectorAll(".agent-draft-heading strong"),
        (heading) => heading.textContent,
      ),
    ).toEqual(allAgents);
    await expect(canvas.queryByText("Add agent")).not.toBeInTheDocument();
    await expect(canvas.queryByText("VS Code")).not.toBeInTheDocument();

    const programs = {
      claudeCode: {},
      claudeDesktop: {},
      codex: {},
      openCode: {
        model: "gpt-5.6-terra",
        models: { "gpt-5.6-terra": { name: "GPT 5.6 Terra" } },
      },
    };
    await expect(output()).toEqual({ llmGateway, programs });

    await userEvent.click(
      canvas.getByRole("button", { name: "Remove Claude Desktop" }),
    );
    const { claudeDesktop: _, ...remaining } = programs;
    await expect(output()).toEqual({ llmGateway, programs: remaining });
    await userEvent.click(canvas.getByText("Add agent"));
    await expect(
      Array.from(
        canvasElement.querySelectorAll(".add-agent-options button"),
        (button) => button.textContent,
      ),
    ).toEqual(["Claude Desktop"]);
    await userEvent.click(canvas.getByText("Add agent"));

    for (const name of ["Claude Code", "Codex", "OpenCode"]) {
      await userEvent.click(
        canvas.getByRole("button", { name: `Remove ${name}` }),
      );
    }
    await expect(output()).toEqual({ llmGateway, programs: {} });
    await userEvent.click(canvas.getByText("Agents", { exact: true }));
    await expect(canvas.getByText("No agents added.")).toBeVisible();
    await userEvent.click(canvas.getByText("Add agent"));
    await expect(
      Array.from(
        canvasElement.querySelectorAll(".add-agent-options button"),
        (button) => button.textContent,
      ),
    ).toEqual(allAgents);
  },
};
