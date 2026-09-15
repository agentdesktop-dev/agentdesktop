import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, spyOn, userEvent, waitFor, within } from "storybook/test";

import { DesktopStoryFrame } from "../stories/DesktopStoryFrame";
import {
  approvedDevice,
  bootstrap,
  emptyDiscovery,
  managedConnector,
  managedDaemonInfo,
  offlineConnector,
  populatedDiscovery,
  standaloneConnector,
  standaloneDaemonInfo,
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
    managedDevice: approvedDevice,
    onCopy: fn(),
    onLogout: fn(),
  },
} satisfies Meta<typeof StatusView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const ManagedReady: Story = {};

export const DaemonInformation: Story = {
  play: async ({ args, canvas }) => {
    await userEvent.click(canvas.getByText("Advanced"));
    const local = within(
      canvas.getByRole("region", { name: "Daemon information" }),
    );
    await expect(local.getByText("System", { exact: true })).toBeVisible();
    await expect(local.getByText(managedDaemonInfo.configPath)).toBeVisible();
    await expect(
      local.getByText("https://controller.example.internal:8443/"),
    ).toBeVisible();
    await expect(
      local.getByText("/etc/agentdesktop/controller-ca.pem"),
    ).toBeVisible();
    await expect(local.getByText("30s")).toBeVisible();
    await expect(local.getByText("15m")).toBeVisible();
    await expect(
      canvas.queryByRole("region", {
        name: "Controller-provided configuration",
      }),
    ).not.toBeInTheDocument();
    await expect(
      canvas.queryByRole("button", { name: "Copy YAML" }),
    ).not.toBeInTheDocument();
    await expect(
      canvas.getByRole("button", { name: "Sign out" }),
    ).toBeVisible();
    await userEvent.click(canvas.getByText("Runtime"));
    await expect(
      canvas.getByText(bootstrap.version, { exact: true }),
    ).toBeVisible();
    await expect(
      canvas.getByText(managedDaemonInfo.version, { exact: true }),
    ).toBeVisible();
    await userEvent.click(
      canvas.getByRole("button", { name: "Copy diagnostics" }),
    );
    await expect(args.onCopy).toHaveBeenCalledOnce();
  },
};

export const StandaloneReady: Story = {
  args: {
    connector: standaloneConnector,
    discovery: emptyDiscovery,
    managedDevice: unconfiguredDevice,
  },
  play: async ({ canvas }) => {
    await userEvent.click(canvas.getByText("Advanced"));
    const local = within(
      canvas.getByRole("region", { name: "Daemon information" }),
    );
    await expect(local.getByText("User", { exact: true })).toBeVisible();
    await expect(
      local.getByText(standaloneDaemonInfo.configPath),
    ).toBeVisible();
    await expect(local.getByText("Not configured (standalone)")).toBeVisible();
    await expect(local.getAllByRole("button", { name: /^Copy / })).toHaveLength(
      2,
    );
    await expect(
      local.queryByRole("button", { name: "Copy controller address" }),
    ).not.toBeInTheDocument();
    await expect(
      local.queryByText("Heartbeat interval"),
    ).not.toBeInTheDocument();
    await expect(
      canvas.queryByRole("region", {
        name: "Controller-provided configuration",
      }),
    ).not.toBeInTheDocument();
  },
};

export const NoCustomControllerCa: Story = {
  args: {
    connector: {
      ...managedConnector,
      runtime: {
        ...managedConnector.runtime,
        daemon: {
          ...managedDaemonInfo,
          controller: {
            address: "https://controller.example.com/",
            caCertificatePath: null,
            heartbeatInterval: "30s",
          },
        },
      },
    },
  },
  play: async ({ canvas }) => {
    await userEvent.click(canvas.getByText("Advanced"));
    await expect(canvas.getByText("No custom CA configured")).toBeVisible();
    await expect(
      canvas.queryByRole("button", { name: "Copy controller ca certificate" }),
    ).not.toBeInTheDocument();
  },
};

export const CopiesDaemonValues: Story = {
  play: async ({ canvas }) => {
    const user = userEvent.setup();
    const writeText = spyOn(navigator.clipboard, "writeText").mockResolvedValue(
      undefined,
    );
    try {
      await user.click(canvas.getByText("Advanced"));
      const local = within(
        canvas.getByRole("region", { name: "Daemon information" }),
      );
      const configurationCopy = local.getByRole("button", {
        name: "Copy configuration file",
      });
      await user.tab();
      await expect(configurationCopy).toHaveFocus();
      await user.keyboard("{Enter}");
      await expect(writeText).toHaveBeenLastCalledWith(
        managedDaemonInfo.configPath,
      );
      await waitFor(() =>
        expect(configurationCopy).toHaveAttribute("title", "Copied"),
      );

      for (const [label, value] of [
        ["state directory", managedDaemonInfo.stateDirectory],
        ["controller address", "https://controller.example.internal:8443/"],
        ["controller ca certificate", "/etc/agentdesktop/controller-ca.pem"],
      ]) {
        await user.click(local.getByRole("button", { name: `Copy ${label}` }));
        await expect(writeText).toHaveBeenLastCalledWith(value);
      }
      await expect(writeText).toHaveBeenCalledTimes(4);
      await expect(
        local.queryByRole("button", { name: "Copy daemon scope" }),
      ).not.toBeInTheDocument();
    } finally {
      writeText.mockRestore();
    }
  },
};

export const ClipboardFailureCanBeRetried: Story = {
  play: async ({ canvas }) => {
    const user = userEvent.setup();
    const writeText = spyOn(navigator.clipboard, "writeText")
      .mockRejectedValueOnce(new Error("Clipboard permission denied"))
      .mockResolvedValue(undefined);
    try {
      await user.click(canvas.getByText("Advanced"));
      const button = canvas.getByRole("button", {
        name: "Copy configuration file",
      });
      await user.click(button);
      await expect(
        await canvas.findByText("Copy failed. Select and copy the value."),
      ).toBeVisible();
      await expect(button).toBeEnabled();
      await expect(button).toHaveAttribute("title", "Copy");
      await expect(
        canvas.getByText(managedDaemonInfo.configPath),
      ).toBeVisible();
      await user.click(button);
      await waitFor(() => expect(button).toHaveAttribute("title", "Copied"));
      await expect(writeText).toHaveBeenLastCalledWith(
        managedDaemonInfo.configPath,
      );
      await expect(
        canvas.queryByText("Copy failed. Select and copy the value."),
      ).not.toBeInTheDocument();
    } finally {
      writeText.mockRestore();
    }
  },
};

export const DaemonInformationUnavailable: Story = {
  args: {
    connector: {
      state: "offline",
      detail: "Cannot read daemon state: daemon returned 404 Not Found",
      runtime: null,
    },
  },
  play: async ({ canvas }) => {
    await userEvent.click(canvas.getByText("Advanced"));
    await expect(
      canvas.getByText(/Daemon information is unavailable/),
    ).toBeVisible();
    await expect(
      canvas.queryByRole("region", {
        name: "Controller-provided configuration",
      }),
    ).not.toBeInTheDocument();
    await userEvent.click(canvas.getByText("Runtime"));
    await expect(
      canvas.getByText("Daemon version").nextElementSibling,
    ).toHaveTextContent("Unavailable");
    await expect(
      canvas.getByText(
        "Cannot read daemon state: daemon returned 404 Not Found",
      ),
    ).toBeVisible();
    await expect(
      canvas.getByRole("heading", { name: "Agent Desktop needs attention" }),
    ).toBeVisible();
  },
};

export const DaemonInformationReflow: Story = {
  globals: { viewport: { value: "reflow", isRotated: false } },
  args: {
    connector: {
      ...managedConnector,
      runtime: {
        ...managedConnector.runtime,
        daemon: {
          ...managedDaemonInfo,
          configPath:
            "/Library/Application Support/International Platform Engineering/Agentdesktop/configuration/production/研发团队/config.yaml",
          stateDirectory: `/var/lib/agentdesktop/${"long-directory-name-".repeat(12)}`,
          controller: {
            address: `https://controller.example.internal/${"fleet-path-".repeat(15)}`,
            caCertificatePath: String.raw`C:\ProgramData\International Platform Engineering\Agentdesktop\Certificates\Production\controller-ca.pem`,
            heartbeatInterval: "30s",
          },
        },
      },
    },
  },
  play: async ({ args, canvas }) => {
    const user = userEvent.setup();
    const writeText = spyOn(navigator.clipboard, "writeText").mockResolvedValue(
      undefined,
    );
    try {
      await user.click(canvas.getByText("Advanced"));
      const region = canvas.getByRole("region", { name: "Daemon information" });
      await expect(region).toBeVisible();
      const local = within(region);
      for (const value of region.querySelectorAll<HTMLElement>(
        ".daemon-value-text",
      )) {
        await expect(value.scrollWidth).toBeLessThanOrEqual(
          value.clientWidth + 1,
        );
        await expect(value.scrollHeight).toBeLessThanOrEqual(
          value.clientHeight + 1,
        );
        await expect(value.getBoundingClientRect().right).toBeLessThanOrEqual(
          region.getBoundingClientRect().right + 1,
        );
      }
      const path = args.connector?.runtime?.daemon.configPath;
      await expect(local.getByTitle(path ?? "")).toHaveTextContent(path ?? "");
      await user.click(
        local.getByRole("button", { name: "Copy configuration file" }),
      );
      await expect(writeText).toHaveBeenLastCalledWith(path);
      await user.click(
        local.getByRole("button", { name: "Copy state directory" }),
      );
      await expect(writeText).toHaveBeenLastCalledWith(
        args.connector?.runtime?.daemon.stateDirectory,
      );
    } finally {
      writeText.mockRestore();
    }
  },
};

export const DaemonOffline: Story = {
  args: {
    connector: offlineConnector,
    discovery: emptyDiscovery,
    managedDevice: unconfiguredDevice,
  },
  play: async ({ canvas }) => {
    await userEvent.click(canvas.getByText("Advanced"));
    await expect(
      canvas.getByText(/Daemon information is unavailable/),
    ).toBeVisible();
    await expect(
      canvas.queryByText("Not configured (standalone)"),
    ).not.toBeInTheDocument();
  },
};

export const PartiallyUnavailable: Story = {
  args: {
    connector: null,
    discovery: null,
    managedDevice: approvedDevice,
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

export const Dark: Story = {
  globals: { colorMode: "dark" },
};

export const DarkOffline: Story = {
  ...DaemonOffline,
  globals: { colorMode: "dark" },
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
