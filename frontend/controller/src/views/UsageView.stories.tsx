import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";

import { ControllerStoryFrame } from "../stories/ControllerStoryFrame";
import {
  disabledUsageReport,
  emptyUsageReport,
  macDevice,
  usageReport,
} from "../stories/fixtures";
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
    dimension: "user",
    error: null,
    filters: {},
    loading: false,
    onDimensionChange: fn(),
    onFiltersChange: fn(),
    onRangeChange: fn(),
    range: "7d",
    report: usageReport,
  },
} satisfies Meta<typeof UsageView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Fleet: Story = {
  play: async ({ args, canvas }) => {
    const totals = within(canvas.getByRole("region", { name: "Usage totals" }));
    await expect(totals.getByText("Subscription (API rates)")).toBeVisible();
    await expect(canvas.getByText("3 users on 4 devices")).toBeVisible();
    await expect(canvas.getByRole("img", { name: /^Cost from/ })).toBeVisible();
    await expect(
      canvas.getByText(
        "Cost excludes 12 requests to models without a gateway price.",
      ),
    ).toBeVisible();

    await userEvent.click(
      canvas.getByRole("button", { name: "maria@example.com" }),
    );
    await expect(args.onFiltersChange).toHaveBeenCalledWith({
      user: "user-maria",
    });
    await userEvent.click(canvas.getByRole("button", { name: "30d" }));
    await expect(args.onRangeChange).toHaveBeenCalledWith("30d");
    await userEvent.click(canvas.getByRole("button", { name: "Tools" }));
    await expect(args.onDimensionChange).toHaveBeenCalledWith("tool");
    await userEvent.selectOptions(
      canvas.getByRole("combobox", { name: "User" }),
      "sam@example.com",
    );
    await expect(args.onFiltersChange).toHaveBeenLastCalledWith({
      user: "user-sam",
    });
  },
};

export const ByTool: Story = {
  args: { dimension: "tool" },
  play: async ({ canvas }) => {
    const table = within(canvas.getByRole("table", { name: "Usage by tool" }));
    await expect(table.getByText("Claude Code")).toBeVisible();
    await expect(table.getByText("VS Code")).toBeVisible();
    await expect(table.getByText("Unknown tool")).toBeVisible();
    await expect(
      table.getByRole("columnheader", { name: "Subscription (API rates)" }),
    ).toBeVisible();
  },
};

export const ChartShowsTokens: Story = {
  play: async ({ canvas }) => {
    await userEvent.click(canvas.getByRole("button", { name: "Tokens" }));
    await expect(
      canvas.getByRole("img", { name: /^Tokens from/ }),
    ).toBeVisible();
    await expect(
      canvas.getByRole("button", { name: "Tokens" }),
    ).toHaveAttribute("aria-pressed", "true");
  },
};

export const Filtered: Story = {
  args: {
    filters: { user: "user-maria", device: macDevice.id, tool: "claude-code" },
  },
  play: async ({ args, canvas }) => {
    const user = canvas.getByRole("combobox", { name: "User" });
    await expect(user).toHaveDisplayValue("maria@example.com");
    await expect(
      canvas.getByRole("combobox", { name: "Device" }),
    ).toHaveDisplayValue(macDevice.hostname);
    await expect(
      canvas.queryByRole("button", { name: "maria@example.com" }),
    ).toBeNull();

    await userEvent.click(
      canvas.getByRole("button", { name: "Remove tool filter" }),
    );
    await expect(args.onFiltersChange).toHaveBeenLastCalledWith({
      user: "user-maria",
      device: macDevice.id,
    });
    // The device list belongs to the user, so changing the user resets it.
    await userEvent.selectOptions(user, "All users");
    await expect(args.onFiltersChange).toHaveBeenLastCalledWith({
      tool: "claude-code",
    });
  },
};

export const FilteredWithoutUsage: Story = {
  args: { filters: { device: "0d7f6c1e-5b3a-4f2e-9c8d-1a2b3c4d5e6f" } },
  play: async ({ canvas }) => {
    await expect(
      canvas.getByRole("combobox", { name: "Device" }),
    ).toHaveDisplayValue("0d7f6c1e-5b3a-4f2e-9c8d-1a2b3c4d5e6f");
  },
};

export const NoUsage: Story = {
  args: { report: emptyUsageReport },
  play: async ({ canvas }) => {
    await expect(
      canvas.getByText("No usage recorded in this period."),
    ).toBeVisible();
  },
};

export const CollectionOff: Story = {
  args: { report: disabledUsageReport },
  play: async ({ canvas }) => {
    await expect(canvas.getByText("Usage collection is off")).toBeVisible();
  },
};

export const Loading: Story = {
  args: { loading: true, report: null },
};

export const LoadError: Story = {
  args: { error: "The controller is unavailable.", report: null },
};

export const MobileReflow: Story = {
  args: { dimension: "tool" },
  globals: { viewport: { value: "mobile", isRotated: false } },
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
