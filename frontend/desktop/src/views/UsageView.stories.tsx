import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";

import { DesktopStoryFrame } from "../stories/DesktopStoryFrame";
import {
  deviceUsage,
  noDeviceUsage,
  usageNotCollected,
} from "../stories/fixtures";
import { UsageView } from "./UsageView";

const meta = {
  title: "Desktop/Usage",
  component: UsageView,
  decorators: [
    (Story) => (
      <DesktopStoryFrame pageTitle="Usage" showUsage view="usage">
        <Story />
      </DesktopStoryFrame>
    ),
  ],
  parameters: { layout: "fullscreen" },
  tags: ["test"],
  args: {
    error: null,
    loading: false,
    onRangeChange: fn(),
    range: "7d",
    usage: deviceUsage,
  },
} satisfies Meta<typeof UsageView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const ThisDevice: Story = {
  play: async ({ args, canvas }) => {
    const totals = within(canvas.getByRole("region", { name: "Usage totals" }));
    await expect(totals.getByText("$9.84")).toBeVisible();
    await expect(totals.getByText("Subscription (API rates)")).toBeVisible();
    await expect(totals.getByText("$14.62")).toBeVisible();
    await expect(
      canvas.getByText("VS Code uses GitHub's models, which aren't metered."),
    ).toBeVisible();
    const tools = within(canvas.getByRole("table", { name: "Usage by tool" }));
    await expect(tools.getByText("Claude Code")).toBeVisible();
    await expect(canvas.getByRole("img", { name: /^Cost from/ })).toBeVisible();
    await userEvent.click(canvas.getByRole("button", { name: "30d" }));
    await expect(args.onRangeChange).toHaveBeenCalledWith("30d");
  },
};

export const NoUsageYet: Story = {
  args: { usage: noDeviceUsage },
  play: async ({ canvas }) => {
    await expect(
      canvas.getByText("No usage recorded in this period."),
    ).toBeVisible();
    await expect(
      canvas.getByRole("img", { name: /none recorded$/ }),
    ).toBeVisible();
  },
};

export const NotCollected: Story = {
  args: { usage: usageNotCollected },
  play: async ({ canvas }) => {
    await expect(canvas.getByText("Usage isn’t recorded")).toBeVisible();
  },
};

export const Loading: Story = {
  args: { loading: true, usage: null },
};

export const Unavailable: Story = {
  args: {
    error: "controller at https://controller.example.com did not answer",
    usage: null,
  },
  play: async ({ canvas }) => {
    await expect(
      canvas.getByRole("heading", { name: "Usage is unavailable" }),
    ).toBeVisible();
  },
};

export const RefreshFailed: Story = {
  args: { error: "controller unavailable" },
  play: async ({ canvas }) => {
    await expect(canvas.getByRole("alert")).toHaveTextContent(
      "Couldn’t refresh usage. Showing the last result.",
    );
  },
};

export const ReflowAt320: Story = {
  globals: {
    viewport: { value: "reflow", isRotated: false },
  },
  play: async ({ canvasElement }) => {
    const documentElement = canvasElement.ownerDocument.documentElement;
    const viewportWidth = canvasElement.ownerDocument.defaultView?.innerWidth;
    await expect(documentElement.scrollWidth).toBeLessThanOrEqual(
      viewportWidth ?? 0,
    );
  },
};

export const Dark: Story = {
  globals: { colorMode: "dark" },
};

export const DarkReflow: Story = {
  ...ReflowAt320,
  globals: {
    colorMode: "dark",
    viewport: { value: "reflow", isRotated: false },
  },
};
