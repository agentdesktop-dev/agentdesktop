import type { Meta, StoryObj } from "@storybook/react-vite";
import { fn } from "storybook/test";

import { ControllerShell } from "./ControllerShell";
import { ErrorState, NotFound, PageSkeleton } from "./ViewStates";

const meta = {
  title: "Controller/Shell",
  component: ControllerShell,
  parameters: { layout: "fullscreen" },
  tags: ["test"],
  args: {
    children: <PageSkeleton />,
    onRefresh: fn(),
    path: "/",
  },
} satisfies Meta<typeof ControllerShell>;

export default meta;
type Story = StoryObj<typeof meta>;

export const LoadingOverview: Story = {};

export const LoadingDevices: Story = {
  args: { children: <PageSkeleton rows={5} />, path: "/devices" },
};

export const ControllerError: Story = {
  args: {
    children: <ErrorState message="The controller is unavailable." />,
    path: "/devices",
  },
};

export const UnknownRoute: Story = {
  args: { children: <NotFound />, path: "/missing" },
};
