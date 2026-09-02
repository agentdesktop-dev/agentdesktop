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

const commentedFleetConfigurationYaml = `# Fleet-wide defaults
llmGateway:
  # Keep this endpoint private
  url: https://gateway.example.internal # production
telemetry:
  events: [tool.use, session.new]
programs:
  claudeCode:
    useLlmGateway: true
    # Native Claude setting
    permissions:
      defaultMode: plan
`;

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
      source: "database" as const,
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
      expect.stringMatching(
        /allowedClientIds:\s*\[(?:"|')?claude-code(?:"|')?\]/,
      ),
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
  play: async ({ canvas, canvasElement }) => {
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
    await expect(
      await canvas.findByText("Existing OIDC authentication is preserved."),
    ).toBeVisible();
  },
};

export const PreservesCommentsOnNoopSave: Story = {
  args: {
    initialYaml: commentedFleetConfigurationYaml,
    initialRevision: 3,
    initialVersion: "3",
    writable: true,
    onSave: fn(async (yaml: string, version: string) => ({
      yaml,
      revision: 3,
      version,
      source: "database" as const,
      sourceError: null,
      writable: true,
    })),
  },
  play: async ({ args, canvas }) => {
    await expect(
      canvas.getByLabelText("Generated fleet configuration"),
    ).toHaveValue(commentedFleetConfigurationYaml);
    await userEvent.click(
      canvas.getByRole("button", { name: "Save and roll out" }),
    );
    await expect(args.onSave).toHaveBeenCalledWith(
      commentedFleetConfigurationYaml,
      "3",
    );
    await expect(canvas.getByText("No changes to roll out.")).toBeVisible();
  },
};

export const PreservesCommentsWhileEditing: Story = {
  args: { initialYaml: commentedFleetConfigurationYaml },
  play: async ({ canvas }) => {
    const gatewayUrl = canvas.getByLabelText("Gateway URL");
    await userEvent.clear(gatewayUrl);
    await userEvent.type(gatewayUrl, "https://gateway.changed.example");
    const output = canvas.getByLabelText(
      "Generated fleet configuration",
    ) as HTMLTextAreaElement;
    await expect(output.value).toContain("# Fleet-wide defaults");
    await expect(output.value).toContain("# Keep this endpoint private");
    await expect(output.value).toContain("# Native Claude setting");
    await expect(output.value).toContain("https://gateway.changed.example");
  },
};

export const RejectsInvalidAgentSettings: Story = {
  args: {
    initialRevision: 1,
    initialVersion: "1",
    writable: true,
    onSave: fn(async () => ({
      yaml: null,
      revision: 1,
      version: "1",
      source: "database" as const,
      sourceError: null,
      writable: true,
    })),
  },
  play: async ({ args, canvas, canvasElement }) => {
    const settings = canvasElement.querySelector<HTMLTextAreaElement>(
      ".agent-draft textarea",
    );
    if (!settings)
      throw new Error("Claude Code settings editor was not rendered");
    await userEvent.click(settings);
    await userEvent.paste("permissions: [");
    await expect(canvas.getByText(/^Claude Code:/)).toBeVisible();
    await expect(
      canvas.getByRole("button", { name: "Save and roll out" }),
    ).toBeDisabled();
    await expect(args.onSave).not.toHaveBeenCalled();
  },
};

export const RejectsInvalidSourceYaml: Story = {
  args: {
    initialYaml: "programs: [",
    initialRevision: 1,
    initialVersion: "1",
    writable: true,
    onSave: fn(),
  },
  play: async ({ args, canvas }) => {
    await expect(
      await canvas.findByText(/The active configuration cannot be edited:/),
    ).toBeVisible();
    await expect(
      canvas.getByRole("button", { name: "Save and roll out" }),
    ).toBeDisabled();
    await expect(args.onSave).not.toHaveBeenCalled();
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
      source: "database" as const,
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
