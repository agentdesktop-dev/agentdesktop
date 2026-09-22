import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent } from "storybook/test";

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

export const Defaults: Story = {};

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

export const GeneratedYamlScrolls: Story = {
  args: { initialConfig: activeDaemonConfig },
  play: async ({ canvasElement }) => {
    const preview = canvasElement.querySelector(".output-card pre");
    await expect(preview).toBeTruthy();
    const overflowY = getComputedStyle(preview as Element).overflowY;
    await expect(["auto", "scroll"]).toContain(overflowY);
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

export const ImportsVscodeProxy: Story = {
  args: {
    initialConfig: {
      programs: {
        vscode: {
          useLlmGateway: true,
          copilotProxyUrl: "http://127.0.0.1:4002/v1",
        },
      },
    },
  },
  play: async ({ args, canvas, canvasElement }) => {
    await expect(
      canvas.getByRole("textbox", { name: "Copilot proxy URL" }),
    ).toHaveValue("http://127.0.0.1:4002/v1");
    await expect(
      canvas.getByRole("checkbox", { name: /Use Copilot proxy/ }),
    ).toBeChecked();
    const output = canvasElement.querySelector(".output-card code");
    await expect(output?.textContent).not.toContain("llmGateway:");
    await expect(output?.textContent).toContain(
      'vscode:\n    useLlmGateway: true\n    copilotProxyUrl: "http://127.0.0.1:4002/v1"',
    );
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

export const BlocksSandboxForVscode: Story = {
  args: { initialConfig: ImportsVscodeProxy.args?.initialConfig },
  play: async ({ canvas }) => {
    await userEvent.click(canvas.getByText("Sandbox"));
    await expect(
      canvas.getByRole("checkbox", { name: /Require sandbox/ }),
    ).toBeDisabled();
    await expect(
      canvas.getByText("Remove VS Code to enable sandboxing."),
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

export const DarkVscode: Story = {
  ...ImportsVscodeProxy,
  globals: { colorMode: "dark" },
};
