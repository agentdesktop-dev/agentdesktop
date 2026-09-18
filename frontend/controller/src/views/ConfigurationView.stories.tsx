import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";

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

export const Defaults: Story = {
  play: async ({ canvas, canvasElement }) => {
    await expect(
      canvas.getByRole("checkbox", { name: /Enable LLM gateway/ }),
    ).toBeChecked();
    await expect(
      canvas.getByRole("checkbox", { name: /Controller JWT/ }),
    ).toBeChecked();
    await expect(canvas.getByLabelText("JWT audience")).toHaveValue(
      "agentgateway",
    );
    await expect(
      canvas.getByRole("textbox", { name: "Allowed client IDs" }),
    ).toHaveValue(
      "claude-code\nclaude-desktop\ncodex\nopencode\ngrok\ncopilot-cli",
    );
    await expect(canvas.getByRole("button", { name: "Copy" })).toBeEnabled();
    await expect(
      canvasElement.querySelector(".output-card code")?.textContent,
    ).toContain("programs:\n  claudeCode: {}");
  },
};

export const ActiveConfiguration: Story = {
  args: { initialConfig: activeDaemonConfig },
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
    await expect(output?.textContent).toContain(
      'allowedDomains:\n      - "api.github.com"',
    );
    await expect(output?.textContent).toContain('denied:\n      - "~/.ssh"');
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

    const output = canvasElement.querySelector(".output-card code");
    await expect(output?.textContent).toContain("sandbox:");
    await expect(output?.textContent).toContain(
      'allowedDomains:\n      - "api.github.com"\n      - "registry.npmjs.org"',
    );
    await expect(output?.textContent).toContain(
      'writable:\n      - "/tmp/build-cache"',
    );
    await expect(output?.textContent).toContain('denied:\n      - "~/.ssh"');

    await userEvent.click(canvas.getByText("Add agent"));
    await expect(
      canvas.getByRole("button", { name: /Claude Desktop/ }),
    ).toBeDisabled();
    await expect(
      canvas.getByRole("button", { name: /OpenCode/ }),
    ).toBeDisabled();
    await expect(
      canvas.getByRole("button", { name: "GitHub Copilot CLI" }),
    ).toBeDisabled();
    await expect(
      canvas.getByRole("button", { name: "VS Code" }),
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

export const BuildsCopilotCli: Story = {
  play: async ({ args, canvas, canvasElement }) => {
    await userEvent.click(canvas.getByText("Add agent"));
    await userEvent.click(
      canvas.getByRole("button", { name: "GitHub Copilot CLI" }),
    );
    const copy = canvas.getByRole("button", { name: "Copy" });
    await expect(copy).toBeDisabled();
    await expect(canvas.getByRole("status")).toHaveTextContent(
      "GitHub Copilot CLI requires a gateway model.",
    );
    const model = canvas.getByRole("textbox", { name: "Gateway model" });
    await expect(model).toBeRequired();
    await expect(model).toHaveAttribute("aria-invalid", "true");
    await userEvent.type(model, "claude-haiku-4-5");
    await expect(model).toHaveAttribute("aria-invalid", "false");
    const output = canvasElement.querySelector(".output-card code");
    await expect(output?.textContent).toContain(
      'copilotCli:\n    useLlmGateway: true\n    model: "claude-haiku-4-5"\n    wireApi: completions',
    );
    await expect(
      canvas.getByRole("textbox", { name: "Allowed client IDs" }),
    ).toHaveValue(
      "claude-code\nclaude-desktop\ncodex\nopencode\ngrok\ncopilot-cli",
    );
    await expect(copy).toBeEnabled();
    await userEvent.clear(model);
    await userEvent.type(model, "gpt-5.2");
    await userEvent.selectOptions(
      canvas.getByRole("combobox", { name: "Wire API" }),
      "responses",
    );
    await expect(output?.textContent).toContain("wireApi: responses");
    await userEvent.click(copy);
    await expect(args.onCopy).toHaveBeenCalledWith(output?.textContent);
  },
};

export const DisablesCopilotGateway: Story = {
  play: async ({ canvas, canvasElement }) => {
    await userEvent.click(canvas.getByText("Add agent"));
    await userEvent.click(
      canvas.getByRole("button", { name: "GitHub Copilot CLI" }),
    );
    await userEvent.type(
      canvas.getByRole("textbox", { name: "Gateway model" }),
      "gpt-5.2",
    );
    const card = canvas
      .getByRole("button", { name: "Remove GitHub Copilot CLI" })
      .closest("section");
    if (!card) throw new Error("Copilot CLI card is missing");
    await userEvent.click(
      within(card).getByRole("checkbox", { name: /Use LLM gateway/ }),
    );
    await expect(canvas.getByRole("button", { name: "Copy" })).toBeEnabled();
    await expect(
      canvasElement.querySelector(".output-card code")?.textContent,
    ).toContain('copilotCli:\n    useLlmGateway: false\n    model: "gpt-5.2"');
  },
};

export const DisabledImportedCopilotWithoutModel: Story = {
  args: {
    initialConfig: {
      llmGateway: { url: "https://gateway.example.internal" },
      programs: { copilotCli: { useLlmGateway: false } },
    },
  },
  play: async ({ args, canvas, canvasElement }) => {
    const model = canvas.getByRole("textbox", { name: "Gateway model" });
    await expect(model).toHaveValue("");
    await expect(model).not.toBeRequired();
    await expect(model).toHaveAttribute("aria-invalid", "false");
    await expect(
      canvas.getByRole("checkbox", { name: /Use LLM gateway/ }),
    ).not.toBeChecked();
    const copy = canvas.getByRole("button", { name: "Copy" });
    await expect(copy).toBeEnabled();
    const output = canvasElement.querySelector(".output-card code");
    await expect(output?.textContent).toContain(
      "copilotCli:\n    useLlmGateway: false",
    );
    await userEvent.click(copy);
    await expect(args.onCopy).toHaveBeenCalledWith(output?.textContent);
  },
};

export const CopilotWithoutSharedGateway: Story = {
  play: async ({ args, canvas, canvasElement }) => {
    await userEvent.click(
      canvas.getByRole("checkbox", { name: /Controller JWT/ }),
    );
    await userEvent.click(
      canvas.getByRole("checkbox", { name: /Enable LLM gateway/ }),
    );
    await userEvent.click(canvas.getByText("Add agent"));
    await userEvent.click(
      canvas.getByRole("button", { name: "GitHub Copilot CLI" }),
    );
    const model = canvas.getByRole("textbox", { name: "Gateway model" });
    await expect(model).toHaveValue("");
    await expect(model).not.toBeRequired();
    await expect(model).toHaveAttribute("aria-invalid", "false");
    const copy = canvas.getByRole("button", { name: "Copy" });
    await expect(copy).toBeEnabled();
    const output = canvasElement.querySelector(".output-card code");
    await expect(output?.textContent).not.toContain("llmGateway:");
    await expect(output?.textContent).toContain(
      "copilotCli:\n    useLlmGateway: false",
    );
    await userEvent.click(copy);
    await expect(args.onCopy).toHaveBeenCalledWith(output?.textContent);
  },
};

export const EnablingCopilotRequiresAuthentication: Story = {
  args: DisabledImportedCopilotWithoutModel.args,
  play: async ({ args, canvas, canvasElement }) => {
    const model = canvas.getByRole("textbox", { name: "Gateway model" });
    await userEvent.type(model, "gpt-5.2");
    const copy = canvas.getByRole("button", { name: "Copy" });
    await expect(copy).toBeEnabled();
    const useGateway = canvas.getByRole("checkbox", {
      name: /Use LLM gateway/,
    });
    await userEvent.click(useGateway);
    await expect(model).toBeRequired();
    await expect(model).toHaveAttribute("aria-invalid", "false");
    await expect(copy).toBeDisabled();
    await expect(canvas.getByRole("status")).toHaveTextContent(
      "GitHub Copilot CLI requires Controller JWT or OIDC authentication.",
    );
    await userEvent.click(copy);
    await expect(args.onCopy).not.toHaveBeenCalled();
    await userEvent.click(useGateway);
    await expect(copy).toBeEnabled();
    await userEvent.click(useGateway);
    await expect(copy).toBeDisabled();
    await userEvent.click(
      canvas.getByRole("checkbox", { name: /Controller JWT/ }),
    );
    const policy = canvas.getByRole("textbox", { name: "Allowed client IDs" });
    await expect(policy).toHaveValue("");
    await expect(copy).toBeDisabled();
    await userEvent.type(policy, "copilot-cli");
    await expect(copy).toBeEnabled();
    const output = canvasElement.querySelector(".output-card code");
    await expect(output?.textContent).toContain("type: controllerJwt");
    await userEvent.click(copy);
    await expect(args.onCopy).toHaveBeenCalledWith(output?.textContent);
  },
};

export const PreservesImportedClientPolicy: Story = {
  args: { initialConfig: activeDaemonConfig },
  play: async ({ args, canvas, canvasElement }) => {
    const policy = canvas.getByRole("textbox", { name: "Allowed client IDs" });
    await expect(policy).toHaveValue("claude-code");
    await userEvent.click(canvas.getByText("Add agent"));
    await userEvent.click(
      canvas.getByRole("button", { name: "GitHub Copilot CLI" }),
    );
    await userEvent.type(
      canvas.getByRole("textbox", { name: "Gateway model" }),
      "gpt-5.2",
    );
    await expect(policy).toHaveValue("claude-code");
    const output = canvasElement.querySelector(".output-card code");
    await expect(output?.textContent).toContain(
      'allowedClientIds:\n      - "claude-code"',
    );
    await expect(output?.textContent).not.toContain('- "copilot-cli"');
    const copy = canvas.getByRole("button", { name: "Copy" });
    await expect(copy).toBeEnabled();
    const controllerJwt = canvas.getByRole("checkbox", {
      name: /Controller JWT/,
    });
    await userEvent.click(controllerJwt);
    await expect(copy).toBeDisabled();
    await expect(canvas.getByRole("status")).toHaveTextContent(
      "GitHub Copilot CLI requires Controller JWT or OIDC authentication.",
    );
    await userEvent.click(controllerJwt);
    await expect(
      canvas.getByRole("textbox", { name: "Allowed client IDs" }),
    ).toHaveValue("claude-code");
    await expect(copy).toBeEnabled();
    await userEvent.click(copy);
    await expect(args.onCopy).toHaveBeenCalledWith(output?.textContent);
    await userEvent.click(
      canvas.getByRole("button", { name: "Remove Claude Code" }),
    );
    const restoredPolicy = canvas.getByRole("textbox", {
      name: "Allowed client IDs",
    });
    await expect(restoredPolicy).toHaveValue("claude-code");
    // Only an explicit policy edit authorizes the newly added launcher.
    await userEvent.clear(restoredPolicy);
    await userEvent.type(restoredPolicy, "claude-code{enter}copilot-cli");
    await expect(output?.textContent).toContain('- "copilot-cli"');
  },
};

export const DoesNotReplaceEmptyImportedPolicy: Story = {
  args: {
    initialConfig: {
      llmGateway: {
        url: "https://gateway.example.internal",
        authentication: {
          type: "controllerJwt",
          audience: "agentgateway",
          allowedClientIds: [],
        },
      },
    },
  },
  play: async ({ canvas }) => {
    await expect(
      canvas.getByRole("textbox", { name: "Allowed client IDs" }),
    ).toHaveValue("");
    await expect(canvas.getByRole("button", { name: "Copy" })).toBeDisabled();
  },
};

export const ImportsCopilotAndOidc: Story = {
  args: {
    initialConfig: {
      inventoryInterval: "30m",
      llmGateway: {
        url: "http://127.0.0.1:4001",
        authentication: {
          type: "oidc",
          issuer: "http://127.0.0.1:5557/dex",
          clientId: "agentdesktop-local",
          scopes: ["openid", "offline_access"],
          allowInsecure: true,
        },
      },
      programs: {
        copilotCli: {
          useLlmGateway: true,
          model: "gpt-5.2",
          wireApi: "responses",
        },
        vscode: {
          useLlmGateway: true,
          copilotProxyUrl: "http://127.0.0.1:4002/v1",
        },
      },
    },
  },
  play: async ({ args, canvas, canvasElement }) => {
    await expect(
      canvas.getByRole("textbox", { name: "Gateway model" }),
    ).toHaveValue("gpt-5.2");
    await expect(
      canvas.getByRole("combobox", { name: "Wire API" }),
    ).toHaveValue("responses");
    await expect(
      canvas.getByRole("textbox", { name: "Copilot proxy URL" }),
    ).toHaveValue("http://127.0.0.1:4002/v1");
    const output = canvasElement.querySelector(".output-card code");
    await expect(output?.textContent).toContain('type: "oidc"');
    await expect(output?.textContent).toContain(
      'clientId: "agentdesktop-local"',
    );
    await expect(output?.textContent).toContain('inventoryInterval: "30m"');
    await expect(output?.textContent).not.toContain("allowedClientIds:");
    const copy = canvas.getByRole("button", { name: "Copy" });
    await expect(copy).toBeEnabled();
    await userEvent.click(copy);
    await expect(args.onCopy).toHaveBeenCalledWith(output?.textContent);
  },
};

export const VscodeProxyWithoutSharedGateway: Story = {
  play: async ({ canvas, canvasElement }) => {
    await userEvent.click(
      canvas.getByRole("checkbox", { name: /Enable LLM gateway/ }),
    );
    await userEvent.click(canvas.getByText("Add agent"));
    await userEvent.click(canvas.getByRole("button", { name: "VS Code" }));
    const proxy = canvas.getByRole("textbox", { name: "Copilot proxy URL" });
    await expect(proxy).toHaveValue("http://127.0.0.1:4002/v1");
    const copy = canvas.getByRole("button", { name: "Copy" });
    await expect(copy).toBeEnabled();
    const output = canvasElement.querySelector(".output-card code");
    await expect(output?.textContent).not.toContain("llmGateway:");
    await expect(output?.textContent).toContain(
      'vscode:\n    useLlmGateway: true\n    copilotProxyUrl: "http://127.0.0.1:4002/v1"',
    );
    for (const invalid of [
      "http://gateway.example.com/v1",
      "http://127.0.0.1:4002",
      "http://127.0.0.1:4002/v1?key=placeholder",
    ]) {
      await userEvent.clear(proxy);
      await userEvent.type(proxy, invalid);
      await expect(copy).toBeDisabled();
    }
    await userEvent.clear(proxy);
    await userEvent.type(proxy, "http://127.0.0.1:4002/v1");
    await expect(copy).toBeEnabled();
  },
};

export const BlocksSandboxForCopilot: Story = {
  args: { initialConfig: ImportsCopilotAndOidc.args?.initialConfig },
  play: async ({ canvas }) => {
    await userEvent.click(canvas.getByText("Sandbox"));
    await expect(
      canvas.getByRole("checkbox", { name: /Require sandbox/ }),
    ).toBeDisabled();
    await expect(
      canvas.getByText(
        "Remove GitHub Copilot CLI and VS Code to enable sandboxing.",
      ),
    ).toBeVisible();
  },
};

export const Dark: Story = {
  ...ActiveSandbox,
  globals: { colorMode: "dark" },
};

export const DarkAgentMenu: Story = {
  globals: { colorMode: "dark" },
  play: async ({ canvas }) => {
    await userEvent.click(canvas.getByText("Add agent"));
    await expect(
      canvas.getByRole("button", { name: /OpenCode/ }),
    ).toBeVisible();
  },
};

export const DarkCopilot: Story = {
  ...ImportsCopilotAndOidc,
  globals: { colorMode: "dark" },
};
