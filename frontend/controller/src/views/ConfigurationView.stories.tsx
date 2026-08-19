import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent } from "storybook/test";

import { ControllerStoryFrame } from "../stories/ControllerStoryFrame";
import { activeDaemonConfig } from "../stories/fixtures";
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

export const BuildsYaml: Story = {
  play: async ({ canvas, canvasElement }) => {
    const gatewayUrl = canvas.getByLabelText("Gateway URL");
    await userEvent.clear(gatewayUrl);
    await userEvent.type(gatewayUrl, "https://gateway.changed.example");
    await userEvent.click(canvas.getByText("Telemetry"));
    await userEvent.click(canvas.getByText("New session"));
    const output = canvasElement.querySelector(".output-card code");
    await expect(output).toHaveTextContent("https://gateway.changed.example");
    await expect(output).toHaveTextContent("session.new");
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
