import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent } from "storybook/test";

import { DesktopShell } from "./DesktopShell";
import { StatusLoading } from "./StatusLoading";

const meta = {
  title: "Desktop/Shell",
  component: DesktopShell,
  parameters: { layout: "fullscreen" },
  tags: ["test"],
  args: {
    children: <StatusLoading view="home" />,
    fullWidth: false,
    isRefreshing: false,
    notice: null,
    onNavigate: fn(),
    onRefresh: fn(),
    pageTitle: "Status",
    refreshError: null,
    view: "home",
  },
} satisfies Meta<typeof DesktopShell>;

export default meta;
type Story = StoryObj<typeof meta>;

export const CheckingStatus: Story = {
  play: async ({ args, canvas }) => {
    await expect(
      canvas.getByRole("heading", { name: "Checking status" }),
    ).toBeVisible();
    await userEvent.click(canvas.getByRole("button", { name: "Tools" }));
    await expect(args.onNavigate).toHaveBeenCalledWith("tools");
  },
};

export const DiscoveringTools: Story = {
  args: {
    children: <StatusLoading view="tools" />,
    pageTitle: "Tools",
    view: "tools",
  },
};

export const Refreshing: Story = {
  args: { isRefreshing: true },
};

export const PartialRefreshError: Story = {
  args: {
    refreshError:
      "Couldn’t refresh tool inventory. Showing available or last known information.",
  },
};
