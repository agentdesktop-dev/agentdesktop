import type { Meta, StoryObj } from "@storybook/react-vite";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { StrictMode, useState } from "react";
import { expect, fn, userEvent } from "storybook/test";

import { Desktop } from "../Desktop";
import type { Settings } from "../types";
import {
  bootstrap,
  emptyDiscovery,
  pendingDevice,
  standaloneConnector,
  unconfiguredDevice,
} from "./fixtures";

let savedSettings: Settings;
const readyRequest = fn();
const saveRequest = fn();

function DesktopFixture() {
  const [instance, setInstance] = useState(0);
  return (
    <StrictMode>
      <button
        className="button button-secondary"
        type="button"
        onClick={() => setInstance((value) => value + 1)}
      >
        Restart client
      </button>
      <Desktop key={instance} />
    </StrictMode>
  );
}

const meta = {
  title: "Desktop/Appearance behavior",
  component: DesktopFixture,
  tags: ["test"],
  parameters: { themeProvider: false },
  beforeEach: ({ parameters }) => {
    savedSettings = {
      ...bootstrap.settings,
      colorMode: parameters.savedMode ?? "system",
    };
    readyRequest.mockClear();
    saveRequest.mockClear();
    mockIPC((command, payload) => {
      switch (command) {
        case "desktop_ready":
          readyRequest(document.documentElement.dataset.theme);
          return;
        case "get_bootstrap":
          return { ...bootstrap, settings: { ...savedSettings } };
        case "save_settings": {
          const next = (payload as { settings: Settings }).settings;
          saveRequest(next);
          if (parameters.saveFails) throw new Error("Cannot save settings");
          savedSettings = { ...next };
          return savedSettings;
        }
        case "get_connector_status":
          return standaloneConnector;
        case "get_managed_device_status":
          return parameters.needsEnrollment
            ? pendingDevice
            : unconfiguredDevice;
        case "get_discovery":
          return emptyDiscovery;
        case "get_remote_config":
          return null;
        default:
          throw new Error(`Unexpected desktop command: ${command}`);
      }
    });
    return () => clearMocks();
  },
} satisfies Meta<typeof DesktopFixture>;

export default meta;
type Story = StoryObj<typeof meta>;

export const SavesAndRestoresNativePreference: Story = {
  play: async ({ canvas, canvasElement }) => {
    await userEvent.click(
      await canvas.findByRole("button", { name: "Settings" }),
    );
    const select = canvas.getByRole("combobox", { name: "Color mode" });
    await expect(select).toBeEnabled();
    await userEvent.selectOptions(select, "dark");
    await expect(saveRequest).toHaveBeenCalledWith({
      openOnStartup: true,
      colorMode: "dark",
    });
    await expect(select).toBeEnabled();
    await userEvent.click(
      canvas.getByRole("checkbox", { name: "Open window at startup" }),
    );
    await expect(saveRequest).toHaveBeenLastCalledWith({
      openOnStartup: false,
      colorMode: "dark",
    });
    await userEvent.click(
      canvas.getByRole("button", { name: "Restart client" }),
    );
    await userEvent.click(
      await canvas.findByRole("button", { name: "Settings" }),
    );
    await expect(
      canvas.getByRole("combobox", { name: "Color mode" }),
    ).toHaveValue("dark");
    await expect(
      canvas.getByRole("checkbox", { name: "Open window at startup" }),
    ).not.toBeChecked();
    await expect(canvasElement.ownerDocument.documentElement).toHaveAttribute(
      "data-theme",
      "dark",
    );
  },
};

export const RestoresSavedDarkMode: Story = {
  parameters: { savedMode: "dark" },
  play: async ({ canvas, canvasElement }) => {
    await expect(canvasElement.ownerDocument.documentElement).toHaveAttribute(
      "data-theme",
      "dark",
    );
    await expect(readyRequest).toHaveBeenCalledWith("dark");
    await userEvent.click(canvas.getByRole("button", { name: "Settings" }));
    await expect(
      canvas.getByRole("combobox", { name: "Color mode" }),
    ).toHaveValue("dark");
  },
};

export const RollsBackWhenSaveFails: Story = {
  parameters: { savedMode: "light", saveFails: true },
  play: async ({ canvas, canvasElement }) => {
    await userEvent.click(canvas.getByRole("button", { name: "Settings" }));
    const select = canvas.getByRole("combobox", { name: "Color mode" });
    await expect(select).toBeEnabled();
    await userEvent.selectOptions(select, "dark");
    await expect(canvas.getByRole("alert")).toHaveTextContent(
      "Cannot save settings",
    );
    await expect(select).toHaveValue("light");
    await expect(canvasElement.ownerDocument.documentElement).toHaveAttribute(
      "data-theme",
      "light",
    );
    await expect(savedSettings.colorMode).toBe("light");
  },
};

export const AvailableBeforeEnrollment: Story = {
  parameters: { needsEnrollment: true },
  play: async ({ canvas, canvasElement }) => {
    await expect(
      await canvas.findByRole("heading", { name: "Enrollment" }),
    ).toBeVisible();
    await userEvent.click(canvas.getByRole("button", { name: "Settings" }));
    const select = canvas.getByRole("combobox", { name: "Color mode" });
    await expect(select).toBeEnabled();
    await userEvent.selectOptions(select, "dark");
    await expect(canvasElement.ownerDocument.documentElement).toHaveAttribute(
      "data-theme",
      "dark",
    );
    await userEvent.click(canvas.getByRole("button", { name: "Status" }));
    await expect(
      canvas.getByRole("heading", { name: "Enrollment" }),
    ).toBeVisible();
  },
};
