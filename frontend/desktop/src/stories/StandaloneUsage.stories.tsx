import type { Meta, StoryObj } from "@storybook/react-vite";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { expect, userEvent, within } from "storybook/test";

import { Desktop } from "../Desktop";
import {
  bootstrap,
  deviceUsage,
  emptyDiscovery,
  standaloneConnector,
  standaloneDaemonInfo,
  unconfiguredDevice,
  usageReceiverDaemonInfo,
} from "./fixtures";

const meta = {
  title: "Desktop/Standalone usage",
  component: Desktop,
  tags: ["test"],
  parameters: { themeProvider: false },
  beforeEach: ({ parameters }) => {
    mockIPC((command) => {
      switch (command) {
        case "desktop_ready":
          return;
        case "get_bootstrap":
          return bootstrap;
        case "get_connector_status":
          return {
            ...standaloneConnector,
            runtime: {
              ...standaloneConnector.runtime,
              daemon: parameters.usageReceiver
                ? usageReceiverDaemonInfo
                : standaloneDaemonInfo,
            },
          };
        case "get_managed_device_status":
          return unconfiguredDevice;
        case "get_discovery":
          return emptyDiscovery;
        case "get_remote_config":
          return null;
        case "get_device_usage":
          return deviceUsage;
        default:
          throw new Error(`Unexpected desktop command: ${command}`);
      }
    });
    return () => clearMocks();
  },
} satisfies Meta<typeof Desktop>;

export default meta;
type Story = StoryObj<typeof meta>;

export const GatewayOnThisMachine: Story = {
  parameters: { usageReceiver: true },
  play: async ({ canvas }) => {
    await userEvent.click(await canvas.findByRole("button", { name: "Usage" }));
    const tools = within(
      await canvas.findByRole("table", { name: "Usage by tool" }),
    );
    await expect(tools.getByText("Claude Code")).toBeVisible();
  },
};

export const NoUsageReceiver: Story = {
  parameters: { usageReceiver: false },
  play: async ({ canvas }) => {
    await expect(await canvas.findByText("Advanced")).toBeVisible();
    await expect(canvas.queryByRole("button", { name: "Usage" })).toBeNull();
  },
};
