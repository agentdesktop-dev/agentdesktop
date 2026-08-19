import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent } from "storybook/test";

import { DesktopStoryFrame } from "../stories/DesktopStoryFrame";
import {
  approvedDevice,
  bootstrap,
  emptyDiscovery,
  managedConnector,
  offlineConnector,
  populatedDiscovery,
  remoteConfig,
  standaloneConnector,
  unconfiguredDevice,
} from "../stories/fixtures";
import { StatusView } from "./StatusView";

const meta = {
  title: "Desktop/Status",
  component: StatusView,
  decorators: [
    (Story) => (
      <DesktopStoryFrame pageTitle="Status">
        <Story />
      </DesktopStoryFrame>
    ),
  ],
  parameters: { layout: "fullscreen" },
  tags: ["test"],
  args: {
    bootstrap,
    connector: managedConnector,
    discovery: populatedDiscovery,
    isLoggingOut: false,
    isSaving: false,
    managedDevice: approvedDevice,
    onCopy: fn(),
    onCopyRemoteConfig: fn(),
    onLogout: fn(),
    onStartupChange: fn(),
    remoteConfig,
    settings: bootstrap.settings,
  },
} satisfies Meta<typeof StatusView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const ManagedReady: Story = {};

export const StandaloneReady: Story = {
  args: {
    connector: standaloneConnector,
    discovery: emptyDiscovery,
    managedDevice: unconfiguredDevice,
    remoteConfig: null,
  },
};

export const DaemonOffline: Story = {
  args: {
    connector: offlineConnector,
    discovery: emptyDiscovery,
    managedDevice: unconfiguredDevice,
    remoteConfig: null,
  },
};

export const PartiallyUnavailable: Story = {
  args: {
    connector: null,
    discovery: null,
    managedDevice: approvedDevice,
    remoteConfig: null,
  },
};

export const LongOrganizationName: Story = {
  args: {
    managedDevice: {
      ...approvedDevice,
      organizationName:
        "International Platform Engineering and Applied Intelligence Organization",
    },
  },
};

export const ChangesStartupPreference: Story = {
  play: async ({ args, canvas }) => {
    await userEvent.click(canvas.getByText("Runtime"));
    await userEvent.click(
      canvas.getByRole("checkbox", { name: "Open window at startup" }),
    );
    await expect(args.onStartupChange).toHaveBeenCalledWith(false);
  },
};

export const ConfirmsLogout: Story = {
  play: async ({ args, canvas }) => {
    await userEvent.click(canvas.getByText("Advanced"));
    await userEvent.click(canvas.getByRole("button", { name: "Sign out" }));
    await expect(canvas.getByText("Are you sure?")).toBeVisible();
    await userEvent.click(
      canvas.getByRole("button", { name: "Yes, sign out" }),
    );
    await expect(args.onLogout).toHaveBeenCalledOnce();
  },
};
