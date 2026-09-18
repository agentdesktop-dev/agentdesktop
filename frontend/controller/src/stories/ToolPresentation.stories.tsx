import { friendlyTool, ToolIcon, ToolInventory } from "@agentdesktop/ui";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect } from "storybook/test";

import { ControllerStoryFrame } from "./ControllerStoryFrame";

const meta = {
  title: "Controller/Tool presentation",
  component: ToolInventory,
  decorators: [
    (Story) => (
      <ControllerStoryFrame path="/devices">
        <section className="card">
          <Story />
        </section>
      </ControllerStoryFrame>
    ),
  ],
  parameters: { layout: "fullscreen" },
  tags: ["test"],
  args: {
    discovery: {
      kind: "copilot-cli",
      version: "1.0.85",
      path: "/usr/local/bin/copilot",
    },
  },
} satisfies Meta<typeof ToolInventory>;

export default meta;
type Story = StoryObj<typeof meta>;

export const CopilotCli: Story = {
  play: async ({ canvas, canvasElement }) => {
    await expect(canvas.getByText("GitHub Copilot CLI")).toBeVisible();
    const icon = canvasElement.querySelector(".tool-icon");
    await expect(icon).toHaveAttribute("aria-hidden", "true");
    await expect(icon).toHaveClass("theme-monochrome");
    await expect(icon).toHaveAttribute("alt", "");
  },
};

export const Aliases: Story = {
  render: () => (
    <div className="stack">
      {["copilot-cli", "copilot_cli", "copilot", "COPILOT-CLI", "vscode"].map(
        (kind) => (
          <span className="tool-cell" key={kind}>
            <ToolIcon kind={kind} />
            <strong>{friendlyTool(kind)}</strong>
          </span>
        ),
      )}
    </div>
  ),
  play: async ({ canvas, canvasElement }) => {
    await expect(canvas.getAllByText("GitHub Copilot CLI")).toHaveLength(4);
    await expect(canvas.getByText("VS Code")).toBeVisible();
    const icons = canvasElement.querySelectorAll(".tool-icon");
    await expect(icons).toHaveLength(5);
    for (const icon of icons) {
      await expect(icon).toHaveClass("theme-monochrome");
      await expect(icon).toHaveAttribute("src", icons[0].getAttribute("src"));
    }
    await expect(friendlyTool("claude_code")).toBe("Claude Code");
    await expect(friendlyTool("grok-build")).toBe("Grok Build");
    await expect(friendlyTool("unknown-tool")).toBe("unknown-tool");
  },
};

export const Dark: Story = {
  ...CopilotCli,
  globals: { colorMode: "dark" },
};
