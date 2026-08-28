import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent } from "storybook/test";
import { parse } from "yaml";

import { ControllerStoryFrame } from "../stories/ControllerStoryFrame";
import { fleetConfigurationYaml } from "../stories/fixtures";
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
  args: { initialYaml: fleetConfigurationYaml },
};

export const BuildsYaml: Story = {
  play: async ({ canvas, canvasElement }) => {
    const gatewayUrl = canvas.getByLabelText("Gateway URL");
    await userEvent.clear(gatewayUrl);
    await userEvent.type(gatewayUrl, "https://gateway.changed.example");
    await userEvent.click(canvas.getByText("Telemetry"));
    await userEvent.click(canvas.getByText("New session"));
    const output = canvasElement.querySelector<HTMLTextAreaElement>(
      ".configuration-yaml-preview",
    );
    await expect(output?.value).toContain("llmGateway:");
    await expect(output?.value).toContain("https://gateway.changed.example");
    await expect(output?.value).toContain("session.new");
  },
};

export const CopiesYaml: Story = {
  play: async ({ args, canvas }) => {
    await userEvent.click(canvas.getByRole("button", { name: "Copy" }));
    await expect(args.onCopy).toHaveBeenCalledOnce();
    await expect(canvas.getByRole("button", { name: "Copied" })).toBeVisible();
  },
};

export const SavesYaml: Story = {
  args: {
    initialYaml: fleetConfigurationYaml,
    initialRevision: 3,
    initialVersion: "17",
    writable: true,
    onSave: fn(async (_yaml: string, _version: string) => ({
      yaml: "programs: {}\n",
      revision: 4,
      version: "18",
      source: "configMap" as const,
      sourceError: null,
      writable: true,
    })),
  },
  play: async ({ args, canvas }) => {
    await expect(
      canvas.getByRole("button", { name: "Save and roll out" }),
    ).toBeEnabled();
    await userEvent.click(
      canvas.getByRole("button", { name: "Save and roll out" }),
    );
    await expect(args.onSave).toHaveBeenCalledOnce();
    await expect(args.onSave).toHaveBeenCalledWith(
      expect.stringContaining('allowedClientIds: ["claude-code"]'),
      "17",
    );
    await expect(
      canvas.getByText("Revision 4 saved and queued for rollout."),
    ).toBeVisible();
  },
};

export const PreservesUnsupportedFields: Story = {
  args: {
    initialYaml: `controller:
  address: https://controller.example.com
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: oidc
    issuer: https://id.example.com
    clientId: agentdesktop
    scopes: [openid, offline_access]
programs:
  claudeCode: {}
`,
  },
  play: async ({ canvasElement }) => {
    const output = canvasElement.querySelector<HTMLTextAreaElement>(
      ".configuration-yaml-preview",
    );
    const document = parse(output?.value ?? "");
    await expect(document.controller).toEqual({
      address: "https://controller.example.com",
    });
    await expect(document.llmGateway.authentication).toEqual({
      type: "oidc",
      issuer: "https://id.example.com",
      clientId: "agentdesktop",
      scopes: ["openid", "offline_access"],
    });
  },
};

export const PreservesLargeIntegers: Story = {
  args: {
    initialYaml:
      "programs:\n  claudeCode:\n    customTokenBudget: 9007199254740993\n",
    initialRevision: 3,
    initialVersion: "17",
    writable: true,
    onSave: fn(async (_yaml: string, _version: string) => ({
      yaml: null,
      revision: 4,
      version: "18",
      source: "configMap" as const,
      sourceError: null,
      writable: true,
    })),
  },
  play: async ({ args, canvas }) => {
    await userEvent.click(
      canvas.getByRole("button", { name: "Save and roll out" }),
    );
    await expect(args.onSave).toHaveBeenCalledWith(
      expect.stringContaining("customTokenBudget: 9007199254740993"),
      "17",
    );
  },
};

export const AddsAgent: Story = {
  play: async ({ canvas }) => {
    await userEvent.click(canvas.getByText("Add agent"));
    await userEvent.click(canvas.getByRole("button", { name: /OpenCode/ }));
    await expect(canvas.getAllByText("OpenCode")).toHaveLength(1);
  },
};
