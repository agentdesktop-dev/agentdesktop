import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";

import { ControllerStoryFrame } from "../stories/ControllerStoryFrame";
import {
  deviceDetail,
  emptyDeviceDetail,
  failedDeviceDetail,
} from "../stories/fixtures";
import { DeviceView } from "./DeviceView";

const meta = {
  title: "Controller/Device details",
  component: DeviceView,
  decorators: [
    (Story) => (
      <ControllerStoryFrame path="/devices/device-mac-12345678">
        <Story />
      </ControllerStoryFrame>
    ),
  ],
  parameters: { layout: "fullscreen" },
  tags: ["test"],
  args: {
    deleteError: null,
    deleteOpen: false,
    deleting: false,
    device: deviceDetail,
    onDeleteCancel: fn(),
    onDeleteConfirm: fn(),
    onDeleteRequest: fn(),
  },
} satisfies Meta<typeof DeviceView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Healthy: Story = {
  play: async ({ canvas }) => {
    await expect(canvas.getByText("Ollama")).toBeVisible();
    await expect(canvas.getByText("qwen3:8b")).toBeVisible();
  },
};

export const BrowsesDiscoveredCapabilities: Story = {
  play: async ({ canvas }) => {
    await userEvent.click(
      canvas.getByText("VS Code", { selector: ".tool-inventory-item strong" }),
    );
    await expect(
      canvas.getByRole("tab", { name: "MCP servers 7" }),
    ).toHaveAttribute("aria-selected", "true");
    await userEvent.click(canvas.getByRole("tab", { name: "Skills 11" }));
    const skillsPanel = within(
      await canvas.findByRole("tabpanel", { name: "Skills 11" }),
    );
    await expect(skillsPanel.getByText("Release workflow")).toBeVisible();
    await userEvent.click(
      skillsPanel.getByRole("button", { name: "Next Skills page" }),
    );
    await expect(skillsPanel.getByText("Performance diagnosis")).toBeVisible();
  },
};

export const ConfigurationFailed: Story = {
  args: { device: failedDeviceDetail },
};

export const NoActivityOrTools: Story = {
  args: { device: emptyDeviceDetail },
};

export const RequestsDeletion: Story = {
  play: async ({ args, canvas }) => {
    await userEvent.click(
      canvas.getByRole("button", { name: "Delete device" }),
    );
    await expect(args.onDeleteRequest).toHaveBeenCalledOnce();
  },
};

export const DeleteConfirmation: Story = {
  args: { deleteOpen: true },
  play: async ({ args, canvas }) => {
    const dialog = within(canvas.getByRole("dialog"));
    await expect(
      dialog.getByRole("heading", { name: "Delete dev-mac?" }),
    ).toBeVisible();
    await userEvent.click(
      dialog.getByRole("button", { name: "Delete device" }),
    );
    await expect(args.onDeleteConfirm).toHaveBeenCalledOnce();
  },
};

export const DeleteFailed: Story = {
  args: {
    deleteError: "The controller could not remove this device.",
    deleteOpen: true,
  },
};

export const Deleting: Story = {
  args: { deleteOpen: true, deleting: true },
};
