import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, userEvent } from "storybook/test";

import { ControllerStoryFrame } from "../stories/ControllerStoryFrame";
import { devices } from "../stories/fixtures";
import { DevicesView } from "./DevicesView";

const meta = {
  title: "Controller/Devices",
  component: DevicesView,
  decorators: [
    (Story) => (
      <ControllerStoryFrame path="/devices">
        <Story />
      </ControllerStoryFrame>
    ),
  ],
  parameters: { layout: "fullscreen" },
  tags: ["test"],
  args: { devices },
} satisfies Meta<typeof DevicesView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Populated: Story = {};

export const SearchesDevices: Story = {
  play: async ({ canvas }) => {
    await userEvent.type(
      canvas.getByRole("textbox", { name: "Search devices" }),
      "linux",
    );
    await expect(canvas.getByText("build-linux")).toBeVisible();
    await expect(canvas.queryByText("dev-mac")).not.toBeInTheDocument();
  },
};

export const NoMatches: Story = {
  play: async ({ canvas }) => {
    await userEvent.type(
      canvas.getByRole("textbox", { name: "Search devices" }),
      "not-a-device",
    );
    await expect(canvas.getByText("No matching devices")).toBeVisible();
  },
};

export const Empty: Story = {
  args: { devices: [] },
};

export const Loading: Story = {
  args: { devices: [], loading: true },
};

export const LoadError: Story = {
  args: { devices: [], error: "The controller is unavailable." },
};

export const MobileReflow: Story = {
  globals: { viewport: { value: "mobile", isRotated: false } },
  play: async ({ canvasElement }) => {
    const documentElement = canvasElement.ownerDocument.documentElement;
    const viewportWidth = canvasElement.ownerDocument.defaultView?.innerWidth;
    await expect(documentElement.scrollWidth).toBeLessThanOrEqual(
      viewportWidth ?? 0,
    );
  },
};
