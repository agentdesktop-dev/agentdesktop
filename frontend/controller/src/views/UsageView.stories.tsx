import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent } from "storybook/test";

import { ControllerStoryFrame } from "../stories/ControllerStoryFrame";
import { fleetUsage } from "../stories/fixtures";
import { UsageView } from "./UsageView";

const meta = {
  title: "Controller/Usage",
  component: UsageView,
  decorators: [
    (Story) => (
      <ControllerStoryFrame path="/usage">
        <Story />
      </ControllerStoryFrame>
    ),
  ],
  parameters: { layout: "fullscreen" },
  tags: ["test"],
  args: {
    usage: fleetUsage,
    range: "day",
    onRangeChange: fn(),
  },
} satisfies Meta<typeof UsageView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Populated: Story = {
  play: async ({ canvas }) => {
    await expect(canvas.getByText("$6.48")).toBeVisible();
    await expect(canvas.getByText("1.84M")).toBeVisible();
    await expect(canvas.getByText("dev-mac")).toBeVisible();
    await expect(canvas.getByText("Unknown device")).toBeVisible();
    await expect(canvas.getByText("Unattributed")).toBeVisible();
  },
};

export const ChangesRange: Story = {
  play: async ({ args, canvas }) => {
    await userEvent.click(canvas.getByRole("radio", { name: "Week" }));
    await expect(args.onRangeChange).toHaveBeenCalledWith("week");
  },
};

export const NoUsageInPeriod: Story = {
  args: { usage: { ...fleetUsage, devices: [] } },
  play: async ({ canvas }) => {
    await expect(canvas.getByText("No usage in this period")).toBeVisible();
  },
};

export const NotConfigured: Story = {
  args: { usage: null },
  play: async ({ canvas }) => {
    await expect(
      canvas.getByText("Usage reporting is not configured"),
    ).toBeVisible();
  },
};

export const Loading: Story = {
  args: { usage: null, loading: true },
};

export const LoadError: Story = {
  args: { usage: null, error: "LLM usage is unavailable" },
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
